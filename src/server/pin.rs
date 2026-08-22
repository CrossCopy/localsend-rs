//! Receiver-side PIN enforcement: 401 on mismatch, 3 failures -> 429 + 5 min cooldown
//! (matches official LocalSend app behavior).

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

pub const MAX_FAILURES: u32 = 3;
pub const LOCKOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, PartialEq, Eq)]
pub enum PinVerdict {
    Ok,
    Unauthorized,
    LockedOut,
}

#[derive(Debug)]
pub struct PinGate {
    pin: Option<String>,
    failures: HashMap<IpAddr, (u32, Instant)>, // (count, last_failure)
}

impl PinGate {
    pub fn new(pin: Option<String>) -> Self {
        Self {
            pin,
            failures: HashMap::new(),
        }
    }

    /// Replaces the PIN while the server is running. `None` takes the gate off.
    ///
    /// **The lockout table is cleared with it.** A person changing the PIN has
    /// decided the old value no longer means anything, and leaving the counters
    /// behind would keep a peer locked out for guessing at a secret that has
    /// since been retired — which reads to them as "the new PIN does not work".
    /// Nothing is loosened by this that the caller did not already control: only
    /// something holding the gate can call it.
    pub fn set(&mut self, pin: Option<String>) {
        self.pin = pin;
        self.failures.clear();
    }

    /// Whether a sender has to present a PIN at all.
    pub fn required(&self) -> bool {
        self.pin.is_some()
    }

    /// The PIN itself, for a consumer that gates a *second* surface with the
    /// same secret — a Web Share on the same port, in practice.
    ///
    /// Kept here rather than copied into the consumer because two copies of a
    /// mutable secret is how one of them ends up stale after [`Self::set`].
    /// Never put this in a log line or a response body.
    pub fn pin(&self) -> Option<&str> {
        self.pin.as_deref()
    }

    pub fn check(&mut self, provided: Option<&str>, peer: IpAddr) -> PinVerdict {
        let Some(expected) = self.pin.as_deref() else {
            return PinVerdict::Ok;
        };

        if let Some((count, at)) = self.failures.get(&peer)
            && *count >= MAX_FAILURES
        {
            if at.elapsed() < LOCKOUT {
                return PinVerdict::LockedOut;
            }
            self.failures.remove(&peer);
        }

        // **Presenting nothing is not a guess.** A sender that sent no PIN at
        // all has not tried a value, cannot be enumerating, and most often is a
        // client with no PIN field on the screen it was driven from. Counting
        // that as a strike means three innocent requests spend the lockout
        // budget of a sender who knows the PIN perfectly well, and the peer is
        // then locked out for `LOCKOUT` from a machine that could have got in.
        // Only a wrong *value* moves the counter.
        let Some(provided) = provided else {
            return PinVerdict::Unauthorized;
        };

        if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
            self.failures.remove(&peer);
            PinVerdict::Ok
        } else {
            let entry = self.failures.entry(peer).or_insert((0, Instant::now()));
            entry.0 += 1;
            entry.1 = Instant::now();
            PinVerdict::Unauthorized
        }
    }
}

/// Length-leaking-free comparison without extra deps.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 7));
    const OTHER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 8));

    #[test]
    fn no_pin_configured_always_ok() {
        let mut g = PinGate::new(None);
        assert_eq!(g.check(None, PEER), PinVerdict::Ok);
        assert_eq!(g.check(Some("anything"), PEER), PinVerdict::Ok);
    }

    #[test]
    fn wrong_or_missing_pin_is_unauthorized() {
        let mut g = PinGate::new(Some("123456".to_string()));
        assert_eq!(g.check(None, PEER), PinVerdict::Unauthorized);
        assert_eq!(g.check(Some("000000"), PEER), PinVerdict::Unauthorized);
        assert_eq!(g.check(Some("123456"), PEER), PinVerdict::Ok);
    }

    #[test]
    fn a_sender_that_offered_no_pin_never_spends_the_lockout_budget() {
        let mut g = PinGate::new(Some("123456".to_string()));
        for _ in 0..10 {
            assert_eq!(g.check(None, PEER), PinVerdict::Unauthorized);
        }
        // Ten refusals later the peer is still allowed to present the real one.
        // A client with no PIN field cannot lock its user out of a receiver.
        assert_eq!(g.check(Some("123456"), PEER), PinVerdict::Ok);
    }

    #[test]
    fn three_failures_lock_out_that_peer_only() {
        let mut g = PinGate::new(Some("123456".to_string()));
        for _ in 0..3 {
            assert_eq!(g.check(Some("bad"), PEER), PinVerdict::Unauthorized);
        }
        // 4th attempt: locked, even with the right PIN
        assert_eq!(g.check(Some("123456"), PEER), PinVerdict::LockedOut);
        // a different peer is unaffected
        assert_eq!(g.check(Some("123456"), OTHER), PinVerdict::Ok);
    }

    #[test]
    fn success_resets_failure_count() {
        let mut g = PinGate::new(Some("123456".to_string()));
        g.check(Some("bad"), PEER);
        g.check(Some("bad"), PEER);
        assert_eq!(g.check(Some("123456"), PEER), PinVerdict::Ok);
        // counter reset: two more failures don't lock
        g.check(Some("bad"), PEER);
        g.check(Some("bad"), PEER);
        assert_eq!(g.check(Some("123456"), PEER), PinVerdict::Ok);
    }
}
