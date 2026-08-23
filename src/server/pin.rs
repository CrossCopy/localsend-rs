//! Receiver-side PIN enforcement: 401 on mismatch, 3 failures -> 429 + 5 min cooldown
//! (matches official LocalSend app behavior).

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

pub const MAX_FAILURES: u32 = 3;
pub const LOCKOUT: Duration = Duration::from_secs(5 * 60);

/// How many peer addresses the lockout table remembers at once.
///
/// **A bound, because the key is the sender's own address.** Every wrong PIN
/// *value* from an address the gate has not seen adds an entry, and one machine
/// on the segment holds a whole IPv6 `/64` — so without a bound this is memory
/// an unauthenticated stranger allocates on this device.
///
/// The table is swept for entries older than [`LOCKOUT`] first, since those
/// decide nothing any more. Only if that frees nothing is a live entry evicted,
/// and it is the **oldest** one: evicting loosens the gate for whoever is
/// evicted, so it takes the entry closest to expiring anyway.
///
/// The alternative — refusing every unknown peer while the table is full —
/// fails closed and is worse: it hands anyone on the segment a way to lock this
/// device's receiver against everybody by filling the table.
pub const MAX_TRACKED_PEERS: usize = 1024;

/// How many wrong PIN *values* this gate tolerates, across every address, before
/// [`PinGate::under_guessing_pressure`] starts saying so.
///
/// **Deliberately not a refusal.** The per-address lockout is what refuses; this
/// counter exists so a consumer can decline to treat a cleared PIN as *standing*
/// evidence — falling back to asking a person — while somebody is working
/// through values. Making it refuse would hand anyone who can reach the port a
/// way to lock the receiver against everybody, which is strictly worse than the
/// guessing it would prevent.
///
/// It is global because the per-address counter cannot see the shape of the
/// attack: three tries per address is a budget set by how many addresses the
/// sender has, and one machine on the segment holds an IPv6 `/64`.
///
/// **No window and no timer.** It clears on the next correct PIN, which is the
/// owner's own device sending a file, and until then the consumer asks — the
/// safe direction, and one that needs no clock.
pub const GUESS_PRESSURE: u32 = 10;

#[derive(Debug, PartialEq, Eq)]
pub enum PinVerdict {
    /// The request may proceed.
    ///
    /// `cleared` says the stronger thing: a PIN **was** required and this
    /// request presented the right one. It is `false` when no PIN is configured,
    /// where the request proceeds because there was nothing to prove.
    ///
    /// **On the verdict rather than read beside it.** A caller that wants "did
    /// this sender know the secret" used to answer it with a second call —
    /// `gate.required()` next to `gate.check()` — which gives the same answer
    /// only because every failing verdict returns early above it. That is a
    /// property of the caller's control flow, not of the gate, and it is
    /// invisible when it stops holding. Carried here, the fact cannot be
    /// obtained without the check that establishes it.
    Ok {
        cleared: bool,
    },
    Unauthorized,
    LockedOut,
}

#[derive(Debug)]
pub struct PinGate {
    pin: Option<String>,
    failures: HashMap<IpAddr, (u32, Instant)>, // (count, last_failure)
    /// Wrong PIN values seen since the last correct one, across every address.
    /// See [`GUESS_PRESSURE`].
    wrong_values: u32,
}

impl PinGate {
    pub fn new(pin: Option<String>) -> Self {
        Self {
            pin,
            failures: HashMap::new(),
            wrong_values: 0,
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
        // Same argument as the table: guesses at a retired secret say nothing
        // about the new one, and leaving the count behind would keep a consumer
        // asking about every offer for a reason nobody could see.
        self.wrong_values = 0;
    }

    /// Whether wrong PIN values have been arriving faster than ordinary use
    /// explains. See [`GUESS_PRESSURE`] for what this is and is not for.
    pub fn under_guessing_pressure(&self) -> bool {
        self.wrong_values >= GUESS_PRESSURE
    }

    /// How many wrong values have arrived since the last correct one. Exposed
    /// for a receiver that wants to report it; it decides nothing on its own.
    pub fn wrong_values(&self) -> u32 {
        self.wrong_values
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

    /// How many peer addresses the lockout table is holding.
    ///
    /// Exposed so a receiver can report it; it never affects a verdict.
    pub fn tracked_peers(&self) -> usize {
        self.failures.len()
    }

    pub fn check(&mut self, provided: Option<&str>, peer: IpAddr) -> PinVerdict {
        let Some(expected) = self.pin.as_deref() else {
            return PinVerdict::Ok { cleared: false };
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
            self.wrong_values = 0;
            PinVerdict::Ok { cleared: true }
        } else {
            self.wrong_values = self.wrong_values.saturating_add(1);
            // Only for an address not already tracked: a peer working through
            // its three guesses must not be able to evict anybody, or the bound
            // becomes the attack instead of the defence.
            if !self.failures.contains_key(&peer) {
                self.make_room();
            }
            let entry = self.failures.entry(peer).or_insert((0, Instant::now()));
            entry.0 += 1;
            entry.1 = Instant::now();
            PinVerdict::Unauthorized
        }
    }

    /// Keeps the lockout table under [`MAX_TRACKED_PEERS`]. See that constant
    /// for why a bound exists and why eviction is the lesser of the two ways to
    /// hold it.
    fn make_room(&mut self) {
        if self.failures.len() < MAX_TRACKED_PEERS {
            return;
        }
        // Expired first. An entry past `LOCKOUT` decides nothing — `check`
        // removes it on the next request from that peer anyway — so dropping it
        // costs nothing at all.
        self.failures.retain(|_, (_, at)| at.elapsed() < LOCKOUT);
        if self.failures.len() < MAX_TRACKED_PEERS {
            return;
        }
        if let Some(oldest) = self
            .failures
            .iter()
            .min_by_key(|(_, (_, at))| *at)
            .map(|(peer, _)| *peer)
        {
            self.failures.remove(&oldest);
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
        // `cleared: false` in both: the request proceeds because there was
        // nothing to prove, which is not the same fact as having proved it.
        assert_eq!(g.check(None, PEER), PinVerdict::Ok { cleared: false });
        assert_eq!(
            g.check(Some("anything"), PEER),
            PinVerdict::Ok { cleared: false }
        );
    }

    #[test]
    fn wrong_or_missing_pin_is_unauthorized() {
        let mut g = PinGate::new(Some("123456".to_string()));
        assert_eq!(g.check(None, PEER), PinVerdict::Unauthorized);
        assert_eq!(g.check(Some("000000"), PEER), PinVerdict::Unauthorized);
        assert_eq!(g.check(Some("123456"), PEER), PinVerdict::Ok { cleared: true });
    }

    #[test]
    fn a_sender_that_offered_no_pin_never_spends_the_lockout_budget() {
        let mut g = PinGate::new(Some("123456".to_string()));
        for _ in 0..10 {
            assert_eq!(g.check(None, PEER), PinVerdict::Unauthorized);
        }
        // Ten refusals later the peer is still allowed to present the real one.
        // A client with no PIN field cannot lock its user out of a receiver.
        assert_eq!(g.check(Some("123456"), PEER), PinVerdict::Ok { cleared: true });
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
        assert_eq!(g.check(Some("123456"), OTHER), PinVerdict::Ok { cleared: true });
    }

    #[test]
    fn success_resets_failure_count() {
        let mut g = PinGate::new(Some("123456".to_string()));
        g.check(Some("bad"), PEER);
        g.check(Some("bad"), PEER);
        assert_eq!(g.check(Some("123456"), PEER), PinVerdict::Ok { cleared: true });
        // counter reset: two more failures don't lock
        g.check(Some("bad"), PEER);
        g.check(Some("bad"), PEER);
        assert_eq!(g.check(Some("123456"), PEER), PinVerdict::Ok { cleared: true });
    }

    #[test]
    fn wrong_values_across_addresses_add_up_to_pressure() {
        let mut g = PinGate::new(Some("123456".to_string()));
        // Each from its own address, so the per-address lockout never fires and
        // nothing here would be visible to it. That is the point: three tries
        // per address is a budget the sender sets, not this device.
        for n in 0..super::GUESS_PRESSURE {
            assert!(!g.under_guessing_pressure(), "pressure at {n} guesses");
            let peer = IpAddr::V4(Ipv4Addr::from((n + 1).to_be_bytes()));
            assert_eq!(g.check(Some("000000"), peer), PinVerdict::Unauthorized);
        }
        assert!(g.under_guessing_pressure());
        assert_eq!(g.wrong_values(), super::GUESS_PRESSURE);

        // **The correct PIN clears it**, which is the owner's own device sending
        // a file and is why this needs no timer.
        assert_eq!(
            g.check(Some("123456"), PEER),
            PinVerdict::Ok { cleared: true }
        );
        assert!(!g.under_guessing_pressure());
    }

    #[test]
    fn a_sender_that_offered_no_pin_does_not_raise_the_pressure_either() {
        // The same argument as the lockout budget: presenting nothing is not a
        // guess, and a client with no PIN field must not be able to make this
        // device ask about every offer.
        let mut g = PinGate::new(Some("123456".to_string()));
        for _ in 0..(super::GUESS_PRESSURE * 3) {
            assert_eq!(g.check(None, PEER), PinVerdict::Unauthorized);
        }
        assert!(!g.under_guessing_pressure());
        assert_eq!(g.wrong_values(), 0);
    }

    #[test]
    fn changing_the_pin_clears_the_pressure_with_the_table() {
        let mut g = PinGate::new(Some("123456".to_string()));
        for n in 0..super::GUESS_PRESSURE {
            let peer = IpAddr::V4(Ipv4Addr::from((n + 1).to_be_bytes()));
            g.check(Some("000000"), peer);
        }
        assert!(g.under_guessing_pressure());
        g.set(Some("654321".to_string()));
        assert!(
            !g.under_guessing_pressure(),
            "guesses at a retired secret still counted against the new one"
        );
    }

    /// One address per wrong guess is a table an unauthenticated stranger
    /// grows. `MAX_TRACKED_PEERS` is what stops it, and this is the row that
    /// fails if the bound is removed.
    #[test]
    fn the_lockout_table_is_bounded() {
        let mut g = PinGate::new(Some("123456".to_string()));
        for n in 0..(super::MAX_TRACKED_PEERS as u32 + 500) {
            let peer = IpAddr::V4(Ipv4Addr::from(n.to_be_bytes()));
            assert_eq!(g.check(Some("bad"), peer), PinVerdict::Unauthorized);
        }
        assert!(
            g.tracked_peers() <= super::MAX_TRACKED_PEERS,
            "the lockout table grew to {}",
            g.tracked_peers()
        );
    }

    /// Eviction must not become the attack. A peer part-way through its three
    /// guesses keeps its count while addresses it has never seen arrive, so
    /// filling the table does not reset anybody's budget on the way past.
    #[test]
    fn a_peer_working_through_its_guesses_evicts_nobody() {
        let mut g = PinGate::new(Some("123456".to_string()));
        for n in 0..(super::MAX_TRACKED_PEERS as u32 + 500) {
            let peer = IpAddr::V4(Ipv4Addr::from(n.to_be_bytes()));
            g.check(Some("bad"), peer);
        }
        let held = g.tracked_peers();
        // The last address the loop used is still tracked and still counting.
        let recent = IpAddr::V4(Ipv4Addr::from(
            (super::MAX_TRACKED_PEERS as u32 + 499).to_be_bytes(),
        ));
        for _ in 0..2 {
            assert_eq!(g.check(Some("bad"), recent), PinVerdict::Unauthorized);
        }
        assert_eq!(g.check(Some("123456"), recent), PinVerdict::LockedOut);
        assert_eq!(
            g.tracked_peers(),
            held,
            "three more guesses from a tracked peer changed the table's size"
        );
    }
}
