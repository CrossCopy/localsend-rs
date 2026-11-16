//! The number, size and time formatting every surface reads.
//!
//! These are pure functions with no GPUI and no network in them, so the same
//! spellings are asserted by the unit tests below and used by list rows, progress
//! lines and history alike. Sizes are decimal (1000), the way Finder/`ls -lh`
//! report them and the way the official LocalSend UI shows them; a binary
//! division here would make a 1.0 GB transfer disagree with the file's own size.

use std::time::{Duration, SystemTime};

/// `1.2 MB`, `840 KB`, `12 B`. One fractional digit, `-` for a missing size.
pub fn bytes(size: u64) -> String {
    format_size(size as f64)
}

fn format_size(value: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = value.max(0.0);
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", value.round() as u64, UNITS[0])
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `12.4 MB/s`, `860 KB/s`.
pub fn rate(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return "—".to_string();
    }
    format!("{}/s", format_size(bytes_per_second))
}

/// `0:04`, `1:23`, `1:02:03` — a countdown/elapsed clock, never a rounded "4s".
pub fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (hours, minutes, secs) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

/// The time a transfer has left, given how much is left and how fast it is moving.
/// `None` before a rate is known — a progress line with no estimate yet says so
/// rather than printing an infinity.
pub fn eta(remaining_bytes: u64, bytes_per_second: f64) -> Option<String> {
    if remaining_bytes == 0 {
        return Some(clock(0.0));
    }
    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return None;
    }
    Some(clock(remaining_bytes as f64 / bytes_per_second))
}

/// `just now`, `12s ago`, `4 min ago`, `3 h ago`, `yesterday`, `5 days ago`,
/// then an ISO date for anything older than a month. `then` in the future (a
/// clock that stepped backwards) reads as `just now`, never as a negative age.
pub fn time_ago(then: SystemTime, now: SystemTime) -> String {
    let elapsed = now.duration_since(then).unwrap_or(Duration::ZERO);
    let secs = elapsed.as_secs();
    match secs {
        0..=4 => "just now".to_string(),
        5..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        86_400..=172_799 => "yesterday".to_string(),
        _ if secs < 2_592_000 => format!("{} days ago", secs / 86_400),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// A fingerprint as a person compares it: the first 8 hex characters, grouped
/// `ABCD-EF01`. Peers are pinned by the full value; this is for the eye only.
pub fn short_fingerprint(fingerprint: &str) -> String {
    let cleaned: String = fingerprint
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(8)
        .collect::<String>()
        .to_uppercase();
    if cleaned.len() <= 4 {
        cleaned
    } else {
        format!("{}-{}", &cleaned[..4], &cleaned[4..])
    }
}

/// Whole-percent progress for a bar, guarded against a zero total.
pub fn percent(done: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    ((done.min(total) as f64 / total as f64) * 100.0) as f32
}

/// `1 file`, `3 files`.
pub fn files(count: usize) -> String {
    if count == 1 {
        "1 file".to_string()
    } else {
        format!("{count} files")
    }
}

/// `1 device`, `3 devices` — the peer count, which is not a file count.
pub fn devices(count: usize) -> String {
    if count == 1 {
        "1 device".to_string()
    } else {
        format!("{count} devices")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_decimal_units_and_one_decimal() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_000), "1.0 KB");
        assert_eq!(bytes(12_400), "12.4 KB");
        assert_eq!(bytes(123_400_000), "123 MB");
        assert_eq!(bytes(1_500_000_000), "1.5 GB");
    }

    #[test]
    fn rates_read_per_second_and_refuse_a_zero_denominator() {
        assert_eq!(rate(12_400_000.0), "12.4 MB/s");
        assert_eq!(rate(0.0), "—");
        assert_eq!(rate(f64::INFINITY), "—");
    }

    #[test]
    fn clocks_roll_into_hours_only_when_needed() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(4.4), "0:04");
        assert_eq!(clock(83.0), "1:23");
        assert_eq!(clock(3_723.0), "1:02:03");
    }

    #[test]
    fn eta_needs_a_known_rate() {
        assert_eq!(eta(1_000, 500.0).as_deref(), Some("0:02"));
        assert_eq!(eta(1_000, 0.0), None);
        assert_eq!(eta(0, 0.0).as_deref(), Some("0:00"));
    }

    #[test]
    fn ages_are_coarse_but_monotonic() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let at = |secs: u64| now - Duration::from_secs(secs);
        assert_eq!(time_ago(now, now), "just now");
        assert_eq!(time_ago(at(12), now), "12s ago");
        assert_eq!(time_ago(at(240), now), "4 min ago");
        assert_eq!(time_ago(at(3 * 3600), now), "3 h ago");
        assert_eq!(time_ago(at(90_000), now), "yesterday");
        assert_eq!(time_ago(at(5 * 86_400), now), "5 days ago");
        // A clock that stepped backwards is "just now", not a negative age.
        assert_eq!(time_ago(now + Duration::from_secs(60), now), "just now");
    }

    #[test]
    fn fingerprints_group_the_first_eight_hex_digits() {
        assert_eq!(short_fingerprint("ab12cd34ef56"), "AB12-CD34");
        assert_eq!(short_fingerprint("ab-12-cd-34"), "AB12-CD34");
        assert_eq!(short_fingerprint("xyz"), "");
    }

    #[test]
    fn percentages_never_divide_by_zero_or_exceed_a_hundred() {
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(1, 2), 50.0);
        assert_eq!(percent(9, 4), 100.0);
    }

    #[test]
    fn counts_agree_with_their_noun() {
        assert_eq!(files(0), "0 files");
        assert_eq!(files(1), "1 file");
        assert_eq!(files(4), "4 files");
        // The peer list is devices; "1 file" under "Send files" was a real misread.
        assert_eq!(devices(0), "0 devices");
        assert_eq!(devices(1), "1 device");
        assert_eq!(devices(3), "3 devices");
    }
}
