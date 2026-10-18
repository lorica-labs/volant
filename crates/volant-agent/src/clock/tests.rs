// SPDX-License-Identifier: GPL-3.0-or-later

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
