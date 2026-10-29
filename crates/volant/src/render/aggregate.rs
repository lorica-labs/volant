// SPDX-License-Identifier: GPL-3.0-or-later
//! The callbacks that print beside the stdout one: `timer` and `profile_tasks` of ansible.posix
//! 2.2.2. `profile_tasks` puts a line under each task banner and both print a block after the
//! recap, in the order `callbacks_enabled` names them.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Instant;

use super::Renderer;

/// One callback that prints beside the stdout one.
pub enum Aggregate {
    /// `timer`: how long the run took, after the recap.
    Timer(Instant),
    Profile(Box<Profile>),
}

/// `profile_tasks`: the time of each task, under its banner and sorted after the recap.
pub struct Profile {
    /// When the callback was loaded, and when the last line was printed.
    start: Instant,
    last: Instant,
    /// Each task once, in the order it first ran: the reference keys them by the task's uuid, so a
    /// task a later `serial` batch runs again adds to its first entry.
    tasks: Vec<Timed>,
    keys: HashMap<String, usize>,
    current: Option<usize>,
    /// `true` for `descending`, `false` for `ascending`, `None` for `none`.
    sort: Option<bool>,
    /// `output_limit`, `None` for `all`.
    limit: Option<usize>,
    summary_only: bool,
    datetime_format: String,
}

struct Timed {
    name: String,
    path: Option<String>,
    started: Instant,
    elapsed: f64,
}

impl Profile {
    /// The callback with its options, each as `Callbacks::option` reads it.
    ///
    /// # Errors
    /// An `output_limit` that is neither a number nor `all`, or a `sort_order` it does not know.
    pub fn new(option: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let sort = match option("sort_order").as_deref().unwrap_or("descending") {
            "descending" => Some(true),
            "ascending" => Some(false),
            "none" => None,
            other => {
                return Err(format!(
                    "Invalid value \"{other}\" for configuration option \"callback_profile_tasks : \
                     sort_order\", valid values are: descending, ascending, none"
                ));
            }
        };
        let limit = match option("output_limit").as_deref().unwrap_or("20") {
            "all" => None,
            text => Some(
                text.trim()
                    .parse()
                    .map_err(|_| format!("invalid literal for int() with base 10: '{text}'"))?,
            ),
        };
        let summary_only = option("summary_only").is_some_and(|flag| {
            matches!(
                flag.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on" | "y" | "t"
            )
        });
        let datetime_format = match option("datetime_format") {
            Some(format) if format == "iso8601" => "%Y-%m-%dT%H:%M:%S.%f".to_string(),
            Some(format) => format,
            None => "%A %d %B %Y  %H:%M:%S %z".to_string(),
        };
        let now = Instant::now();
        Ok(Self {
            start: now,
            last: now,
            tasks: Vec::new(),
            keys: HashMap::new(),
            current: None,
            sort,
            limit,
            summary_only,
            datetime_format,
        })
    }

    /// `tasktime()`: the wall clock, the time since the last line and since the start, starred.
    fn line(&mut self) -> String {
        let now = Instant::now();
        let line = format!(
            "{} ({}){}{}",
            crate::template::local_now_zoned(&self.datetime_format),
            seconds_text(now.duration_since(self.last).as_secs_f64()),
            " ".repeat(7),
            seconds_text(now.duration_since(self.start).as_secs_f64()),
        );
        self.last = now;
        filled(&line, '*')
    }

    /// `timestamp()`: the task running so far gets the time since it started.
    fn stop_current(&mut self) {
        if let Some(index) = self.current {
            let task = &mut self.tasks[index];
            task.elapsed += task.started.elapsed().as_secs_f64();
        }
    }
}

/// `secondsToStr`: `H:MM:SS.mmm`, each part truncated.
fn seconds_text(seconds: f64) -> String {
    let millis = seconds * 1000.0;
    let whole = (millis / 1000.0).floor();
    let ms = millis - whole * 1000.0;
    let minutes = (whole / 60.0).floor();
    let secs = whole - minutes * 60.0;
    let hours = (minutes / 60.0).floor();
    let mins = minutes - hours * 60.0;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "Python's `%d` truncates a float the same way"
    )]
    let parts = (hours as i64, mins as i64, secs as i64, ms as i64);
    format!("{}:{:02}:{:02}.{:03}", parts.0, parts.1, parts.2, parts.3)
}

/// `filled`: the text and a space, then `fill` up to 79 characters (three at least), and a space.
fn filled(text: &str, fill: char) -> String {
    let text = if text.is_empty() {
        String::new()
    } else {
        format!("{text} ")
    };
    let width = 79usize.saturating_sub(text.chars().count()).max(3);
    format!("{text}{} ", fill.to_string().repeat(width))
}

impl Renderer {
    /// Turns on the callbacks that print beside the stdout one, in `callbacks_enabled` order.
    pub fn set_aggregates(&mut self, aggregates: Vec<Aggregate>) {
        self.aggregates = aggregates;
    }

    /// What `profile_tasks` does as task `name` starts, after its banner: its time line, and the
    /// task's clock started. `key` names the task across batches.
    pub fn task_timed(&mut self, key: &str, name: &str, path: Option<String>) {
        let verbose = self.verbosity >= 2;
        let mut lines = Vec::new();
        for aggregate in &mut self.aggregates {
            let Aggregate::Profile(profile) = aggregate else {
                continue;
            };
            if !profile.summary_only {
                lines.push(profile.line());
            }
            profile.stop_current();
            let index = if let Some(index) = profile.keys.get(key) {
                *index
            } else {
                profile.tasks.push(Timed {
                    name: name.to_string(),
                    path: None,
                    started: Instant::now(),
                    elapsed: 0.0,
                });
                profile
                    .keys
                    .insert(key.to_string(), profile.tasks.len() - 1);
                profile.tasks.len() - 1
            };
            profile.tasks[index].started = Instant::now();
            if verbose {
                profile.tasks[index].path.clone_from(&path);
            }
            profile.current = Some(index);
        }
        for line in lines {
            let _ = writeln!(self.out, "{line}");
        }
    }

    /// What each callback prints after the recap.
    pub(super) fn aggregates_recap(&mut self) {
        let mut aggregates = std::mem::take(&mut self.aggregates);
        for aggregate in &mut aggregates {
            match aggregate {
                Aggregate::Timer(start) => {
                    let secs = start.elapsed().as_secs();
                    let (days, of_day) = (secs / 86_400, secs % 86_400);
                    self.banner("PLAYBOOK RECAP");
                    let _ = write!(
                        self.out,
                        "Playbook run took {days} days, {} hours, {} minutes, {} seconds\n\r\n",
                        of_day / 3600,
                        of_day / 60 % 60,
                        of_day % 60,
                    );
                }
                Aggregate::Profile(profile) => self.profile_recap(profile),
            }
        }
        self.aggregates = aggregates;
    }

    fn profile_recap(&mut self, profile: &mut Profile) {
        self.banner("TASKS RECAP");
        let line = profile.line();
        let _ = writeln!(self.out, "{line}");
        let _ = writeln!(self.out, "{}", filled("", '='));
        profile.stop_current();
        profile.current = None;
        let mut order: Vec<usize> = (0..profile.tasks.len()).collect();
        if let Some(descending) = profile.sort {
            order.sort_by(|a, b| {
                let (a, b) = (profile.tasks[*a].elapsed, profile.tasks[*b].elapsed);
                if descending {
                    b.total_cmp(&a)
                } else {
                    a.total_cmp(&b)
                }
            });
        }
        let width = self.width;
        for index in order.into_iter().take(profile.limit.unwrap_or(usize::MAX)) {
            let task = &profile.tasks[index];
            let time = format!(" {:.2}s", task.elapsed);
            let mut line = format!(
                "{:-<name$}{time:->9}",
                format!("{} ", task.name),
                name = width.saturating_sub(9)
            );
            if let Some(path) = &task.path {
                let _ = write!(line, "\n{:-<width$}", format!("{path} "));
            }
            let _ = writeln!(self.out, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_fills_read_as_the_reference_writes_them() {
        assert_eq!(seconds_text(0.008_123), "0:00:00.008");
        assert_eq!(seconds_text(3_725.5), "1:02:05.500");
        let starred = filled(
            "Thursday 29 October 2026  21:53:40 +0100 (0:00:00.008)       0:00:00.008",
            '*',
        );
        assert_eq!(starred.chars().count(), 80);
        assert!(starred.ends_with("****** "));
        assert_eq!(filled("", '='), format!("{} ", "=".repeat(79)));
    }

    #[test]
    fn a_wrong_option_is_refused() {
        let option = |key: &str| (key == "sort_order").then(|| "sideways".to_string());
        assert!(Profile::new(option).is_err());
        let limit = |key: &str| (key == "output_limit").then(|| "all".to_string());
        assert!(Profile::new(limit).unwrap().limit.is_none());
    }
}
