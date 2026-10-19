// SPDX-License-Identifier: GPL-3.0-or-later
//! `now()`, and the Python `datetime` it returns.
//!
//! The reference's `now(utc=False, fmt=None)` is `datetime.datetime.now()`, or the same moment in
//! UTC with the zone dropped, and `strftime(fmt)` of it when `fmt` is given. The value here is
//! naive too: its fields, no zone. It prints as ISO 8601 with a `T`, which is how it leaves a
//! template (a task result, `to_json`). Spliced into text, `string` or `~` it prints as Python's
//! `str` does, with a space.

use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use minijinja::value::{Object, ObjectRepr, Rest, ValueKind, ValueOrKwargs};
use minijinja::{Error, ErrorKind, State, Value};

use super::globals::{Callee, bind, python_type};

mod strftime;
#[cfg(test)]
mod tests;

fn raise(message: String) -> Error {
    Error::new(ErrorKind::InvalidOperation, message)
}

/// How much of the time `isoformat` writes.
#[derive(Clone, Copy)]
enum Timespec {
    Auto,
    Hours,
    Minutes,
    Seconds,
    Milliseconds,
    Microseconds,
}

/// A moment as a calendar reads it: no zone, microsecond precision. Field order is time order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct DateTime {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    microsecond: u32,
}

impl DateTime {
    /// The calendar fields of the instant `secs` seconds after the Unix epoch, read in UTC.
    fn from_utc(secs: i64, microsecond: u32) -> Self {
        let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
        let of_day = secs.rem_euclid(86_400);
        Self {
            year,
            month,
            day,
            hour: u32::try_from(of_day / 3600).unwrap_or(0),
            minute: u32::try_from(of_day % 3600 / 60).unwrap_or(0),
            second: u32::try_from(of_day % 60).unwrap_or(0),
            microsecond,
        }
    }

    /// The same instant as the controller's local clock reads it.
    fn from_local(secs: i64, microsecond: u32) -> Self {
        local_fields(secs).map_or_else(
            || Self::from_utc(secs, microsecond),
            |utc| Self { microsecond, ..utc },
        )
    }

    /// The current time, in UTC or in the controller's zone.
    pub(super) fn now(utc: bool) -> Self {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = i64::try_from(since.as_secs()).unwrap_or(0);
        let micros = since.subsec_micros();
        if utc {
            Self::from_utc(secs, micros)
        } else {
            Self::from_local(secs, micros)
        }
    }

    fn days(&self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }

    /// Days since Monday, 0 to 6.
    fn weekday(&self) -> u32 {
        u32::try_from((self.days() + 3).rem_euclid(7)).unwrap_or(0)
    }

    /// Seconds since the epoch if the fields are read as UTC.
    fn utc_seconds(&self) -> i64 {
        self.days() * 86_400
            + i64::from(self.hour) * 3600
            + i64::from(self.minute) * 60
            + i64::from(self.second)
    }

    /// Seconds since the epoch if the fields are the controller's local time, as `timestamp()`
    /// and `%s` read a naive `datetime`.
    fn local_seconds(&self) -> i64 {
        local_epoch(self).unwrap_or_else(|| self.utc_seconds())
    }

    /// `isoformat(sep, timespec)`.
    fn isoformat(&self, sep: char, timespec: Timespec) -> String {
        let timespec = match timespec {
            Timespec::Auto if self.microsecond == 0 => Timespec::Seconds,
            Timespec::Auto => Timespec::Microseconds,
            other => other,
        };
        let clock = match timespec {
            Timespec::Hours => format!("{:02}", self.hour),
            Timespec::Minutes => format!("{:02}:{:02}", self.hour, self.minute),
            Timespec::Seconds | Timespec::Auto => {
                format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
            }
            Timespec::Milliseconds => format!(
                "{:02}:{:02}:{:02}.{:03}",
                self.hour,
                self.minute,
                self.second,
                self.microsecond / 1000
            ),
            Timespec::Microseconds => format!(
                "{:02}:{:02}:{:02}.{:06}",
                self.hour, self.minute, self.second, self.microsecond
            ),
        };
        format!(
            "{:04}-{:02}-{:02}{sep}{clock}",
            self.year, self.month, self.day
        )
    }

    /// Python's `str()`: ISO 8601 with a space.
    pub(super) fn python_str(&self) -> String {
        self.isoformat(' ', Timespec::Auto)
    }
}

/// The year, month and day of a day count from 1970-01-01 (the proleptic Gregorian calendar).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// The day count from 1970-01-01 of a calendar date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let month = i64::from(month);
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The local calendar fields of an instant, microseconds left at 0.
#[cfg(unix)]
fn local_fields(secs: i64) -> Option<DateTime> {
    // `time_t` is not named: `libc` deprecates the alias on musl.
    let seconds = secs as _;
    // SAFETY: `tm` is plain data that `localtime_r` fills, and `seconds` is only read.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&seconds, &mut tm).is_null() {
            return None;
        }
        tm
    };
    let field = |value: libc::c_int| u32::try_from(value).ok();
    Some(DateTime {
        year: i64::from(tm.tm_year) + 1900,
        month: field(tm.tm_mon + 1)?,
        day: field(tm.tm_mday)?,
        hour: field(tm.tm_hour)?,
        minute: field(tm.tm_min)?,
        second: field(tm.tm_sec)?,
        microsecond: 0,
    })
}

#[cfg(not(unix))]
fn local_fields(_secs: i64) -> Option<DateTime> {
    None
}

/// `mktime` of the fields: the instant they name on the controller's local clock.
#[cfg(unix)]
fn local_epoch(at: &DateTime) -> Option<i64> {
    let field = |value: u32| libc::c_int::try_from(value).ok();
    // SAFETY: `tm` is plain data, and `mktime` only reads and normalises it.
    let seconds = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = libc::c_int::try_from(at.year - 1900).ok()?;
        tm.tm_mon = field(at.month)? - 1;
        tm.tm_mday = field(at.day)?;
        tm.tm_hour = field(at.hour)?;
        tm.tm_min = field(at.minute)?;
        tm.tm_sec = field(at.second)?;
        tm.tm_isdst = -1;
        libc::mktime(&mut tm)
    };
    Some(seconds as i64)
}

#[cfg(not(unix))]
fn local_epoch(_at: &DateTime) -> Option<i64> {
    None
}

impl Object for DateTime {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.isoformat('T', Timespec::Auto))
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        Some(match key.as_str()? {
            "year" => Value::from(self.year),
            "month" => Value::from(self.month),
            "day" => Value::from(self.day),
            "hour" => Value::from(self.hour),
            "minute" => Value::from(self.minute),
            "second" => Value::from(self.second),
            "microsecond" => Value::from(self.microsecond),
            "tzinfo" => Value::from(()),
            _ => return None,
        })
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &mut State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        match method {
            "strftime" => self.strftime_method(args),
            "isoformat" => self.isoformat_method(args),
            "timestamp" => {
                no_arguments(method, args)?;
                let seconds = self.local_seconds() as f64;
                Ok(Value::from(seconds + f64::from(self.microsecond) / 1e6))
            }
            "weekday" => {
                no_arguments(method, args)?;
                Ok(Value::from(self.weekday()))
            }
            "isoweekday" => {
                no_arguments(method, args)?;
                Ok(Value::from(self.weekday() + 1))
            }
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

fn no_arguments(method: &str, args: &[Value]) -> Result<(), Error> {
    if args.is_empty() {
        return Ok(());
    }
    Err(raise(format!(
        "{method}() takes no arguments ({} given)",
        args.len()
    )))
}

/// The text of a `str` argument, or Python's refusal of another type.
fn text_argument(method: &str, position: usize, value: &Value) -> Result<String, Error> {
    if value.kind() == ValueKind::String {
        return Ok(value.to_string());
    }
    Err(raise(format!(
        "{method}() argument {position} must be str, not {}",
        python_type(value)
    )))
}

impl DateTime {
    /// `strftime(format)`.
    fn strftime_method(&self, args: &[Value]) -> Result<Value, Error> {
        let [format] = bind(Callee::Native, "strftime", &["format"], args)?;
        let Some(format) = format else {
            return Err(raise(
                "strftime() missing required argument 'format' (pos 1)".into(),
            ));
        };
        let format = text_argument("strftime", 1, &format)?;
        Ok(Value::from(strftime::format(self, &format)))
    }

    /// `isoformat(sep='T', timespec='auto')`.
    fn isoformat_method(&self, args: &[Value]) -> Result<Value, Error> {
        let [sep, timespec] = bind(Callee::Native, "isoformat", &["sep", "timespec"], args)?;
        let sep = match sep {
            None => 'T',
            Some(sep) => {
                let text = text_argument("isoformat", 1, &sep)?;
                let mut chars = text.chars();
                match (chars.next(), chars.next()) {
                    (Some(only), None) => only,
                    _ => {
                        return Err(raise(
                            "isoformat() argument 1 must be a unicode character, not str".into(),
                        ));
                    }
                }
            }
        };
        let timespec = match timespec {
            None => Timespec::Auto,
            Some(spec) => match text_argument("isoformat", 2, &spec)?.as_str() {
                "auto" => Timespec::Auto,
                "hours" => Timespec::Hours,
                "minutes" => Timespec::Minutes,
                "seconds" => Timespec::Seconds,
                "milliseconds" => Timespec::Milliseconds,
                "microseconds" => Timespec::Microseconds,
                _ => return Err(raise("Unknown timespec value".into())),
            },
        };
        Ok(Value::from(self.isoformat(sep, timespec)))
    }
}

/// `now(utc=False, fmt=None)`. Measured on ansible-core 2.19.12: any true `utc` takes UTC, any
/// false or empty `fmt` leaves the `datetime` as it is, and a true `fmt` that is not text fails
/// in `strftime`.
pub(super) fn now(args: Rest<ValueOrKwargs>) -> Result<Value, Error> {
    let args = args.into_values();
    let [utc, format] = bind(Callee::Python, "_now", &["utc", "fmt"], &args)?;
    let now = DateTime::now(utc.is_some_and(|utc| utc.is_true()));
    match format.filter(Value::is_true) {
        None => Ok(Value::from_object(now)),
        Some(format) => {
            let format = text_argument("strftime", 1, &format)?;
            Ok(Value::from(strftime::format(&now, &format)))
        }
    }
}
