// SPDX-License-Identifier: GPL-3.0-or-later
//! Timestamps in the shape Ansible's command module reports (`start`, `end`, `delta`).
//! Always UTC; Ansible uses the host's local time, which is a documented difference.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now() -> String {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_timestamp(since)
}

pub fn delta(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    format!(
        "{}:{:02}:{:02}.{:06}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        elapsed.subsec_micros()
    )
}

fn format_timestamp(since_epoch: Duration) -> String {
    let secs = since_epoch.as_secs() as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let day_secs = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:06}",
        day_secs / 3600,
        (day_secs % 3600) / 60,
        day_secs % 60,
        since_epoch.subsec_micros()
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_a_known_instant() {
        // 2026-09-06 12:34:56.000007 UTC
        let t = Duration::new(1_788_698_096, 7_000);
        assert_eq!(format_timestamp(t), "2026-09-06 12:34:56.000007");
    }

    #[test]
    fn formats_a_delta() {
        assert_eq!(delta(Duration::new(3_725, 500)), "1:02:05.000000");
        assert_eq!(delta(Duration::from_micros(12_345)), "0:00:00.012345");
    }
}
