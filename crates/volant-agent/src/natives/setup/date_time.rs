// SPDX-License-Identifier: GPL-3.0-or-later
//! The `date_time` collector: one clock reading, formatted in local time and in UTC as the
//! reference's `strftime` calls format it. Local time, `%Z` and `%z` come from the C library,
//! which reads the same `TZ` and `/etc/localtime` the module's interpreter reads; `tz_dst` is
//! the interpreter's own `time.tzname[1]`, which musl would not give. Weekday names are
//! English: a locale that would name them otherwise hands back.

use std::ffi::CStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use super::Host;

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

pub fn collect(host: &Host) -> Result<Map<String, Value>, String> {
    match host.probe.lc_time.as_deref() {
        Some(locale) if english(locale) => {}
        Some(locale) => return Err(format!("weekday names follow the {locale} locale")),
        None => {
            return Err("the module's locale cannot be set, and it would replace it".into());
        }
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "the clock is before 1970")?;
    let seconds = libc::time_t::try_from(now.as_secs()).map_err(|_| "the clock is out of range")?;
    let micros = now.subsec_micros();
    // Safety: the two conversions write only the process's time zone state and the structures
    // given.
    let (local, utc) = unsafe {
        let mut local: libc::tm = std::mem::zeroed();
        let mut utc: libc::tm = std::mem::zeroed();
        libc::localtime_r(&raw const seconds, &raw mut local);
        libc::gmtime_r(&raw const seconds, &raw mut utc);
        (local, utc)
    };
    let dst_name = host.probe.tz_dst.clone();
    let zone = unsafe { CStr::from_ptr(local.tm_zone) }
        .to_string_lossy()
        .into_owned();
    let offset = local.tm_gmtoff / 60;
    let two = |n: libc::c_int| format!("{n:02}");
    let year = (local.tm_year + 1900).to_string();
    let month = two(local.tm_mon + 1);
    let day = two(local.tm_mday);
    let (hour, minute, second) = (two(local.tm_hour), two(local.tm_min), two(local.tm_sec));
    let epoch = seconds.to_string();
    let utc_stamp = format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}",
        utc.tm_year + 1900,
        utc.tm_mon + 1,
        utc.tm_mday,
        utc.tm_hour,
        utc.tm_min,
        utc.tm_sec
    );
    let basic_short = format!("{year}{month}{day}T{hour}{minute}{second}");
    let facts: Map<String, Value> = [
        ("year", year.clone()),
        ("month", month.clone()),
        ("weekday", WEEKDAYS[local.tm_wday as usize].to_string()),
        ("weekday_number", local.tm_wday.to_string()),
        ("weeknumber", two(week_number(local.tm_yday, local.tm_wday))),
        ("day", day.clone()),
        ("hour", hour.clone()),
        ("minute", minute.clone()),
        ("second", second.clone()),
        ("epoch", epoch.clone()),
        ("epoch_int", epoch),
        ("date", format!("{year}-{month}-{day}")),
        ("time", format!("{hour}:{minute}:{second}")),
        ("iso8601_micro", format!("{utc_stamp}.{micros:06}Z")),
        ("iso8601", format!("{utc_stamp}Z")),
        ("iso8601_basic", format!("{basic_short}{micros:06}")),
        ("iso8601_basic_short", basic_short),
        ("tz", zone),
        ("tz_dst", dst_name),
        (
            "tz_offset",
            format!(
                "{}{:02}{:02}",
                if offset < 0 { '-' } else { '+' },
                offset.abs() / 60,
                offset.abs() % 60
            ),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), Value::from(value)))
    .collect();
    let mut result = Map::new();
    result.insert("date_time".into(), Value::Object(facts));
    Ok(result)
}

/// `%W`: weeks start on Monday, and the days before the year's first Monday are week 0.
fn week_number(yday: libc::c_int, wday: libc::c_int) -> libc::c_int {
    (yday + 7 - (wday + 6) % 7) / 7
}

/// A locale whose `%A` is the English weekday.
fn english(locale: &str) -> bool {
    matches!(locale, "C" | "POSIX") || locale.starts_with("C.") || locale.starts_with("en_")
}

#[cfg(test)]
mod tests {
    use super::super::tests::{FakeRoot, probe};
    use super::*;

    /// What would make this red: a French `LC_TIME` answered in English, or a locale the module
    /// cannot set answered at all (it would replace `LANG` and `LC_ALL` in the module's
    /// environment).
    #[test]
    fn the_date_is_formatted_in_an_english_locale_only() {
        let fake = FakeRoot::new("date-time");
        let root = fake.root();
        let mut probe = probe();
        let facts = collect(&fake.host(&root, &probe)).unwrap();
        let date_time = facts["date_time"].as_object().unwrap();
        assert_eq!(date_time.len(), 20);
        let date = date_time["date"].as_str().unwrap();
        assert_eq!(date.len(), 10);
        assert_eq!(date_time["epoch"], date_time["epoch_int"]);
        assert!(WEEKDAYS.contains(&date_time["weekday"].as_str().unwrap()));
        assert!(date_time["iso8601_micro"].as_str().unwrap().ends_with('Z'));
        assert_eq!(date_time["iso8601_basic"].as_str().unwrap().len(), 21);
        probe.lc_time = Some("en_US.UTF-8".into());
        assert!(collect(&fake.host(&root, &probe)).is_ok());
        probe.lc_time = Some("fr_FR.UTF-8".into());
        assert!(collect(&fake.host(&root, &probe)).is_err());
        probe.lc_time = None;
        assert!(collect(&fake.host(&root, &probe)).is_err());
    }

    /// `tz_dst` is the interpreter's `time.tzname[1]`, whatever the C library the agent is linked
    /// against says. Measured: for `Etc/UTC` the reference says `UTC`, and musl, which the
    /// uploaded agent is built with, leaves its own `tzname[1]` empty.
    ///
    /// What would make this red: `tz_dst` read from the agent's C library again, which on the
    /// musl agent answers `""` for every zone without daylight time.
    #[test]
    fn the_daylight_zone_name_is_the_interpreter_s() {
        let fake = FakeRoot::new("tz-dst");
        let root = fake.root();
        let mut probe = probe();
        for name in ["UTC", "XDT"] {
            probe.tz_dst = name.into();
            let facts = collect(&fake.host(&root, &probe)).unwrap();
            assert_eq!(facts["date_time"]["tz_dst"], name);
        }
    }

    /// `%W` against dates whose week number is known: 2026-01-01 is a Thursday (week 00), the
    /// first Monday 2026-01-05 opens week 01, and 2026-09-24 is in week 38.
    #[test]
    fn the_week_number_counts_from_the_first_monday() {
        assert_eq!(week_number(0, 4), 0);
        assert_eq!(week_number(3, 0), 0);
        assert_eq!(week_number(4, 1), 1);
        assert_eq!(week_number(266, 4), 38);
    }
}
