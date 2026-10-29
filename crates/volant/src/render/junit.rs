// SPDX-License-Identifier: GPL-3.0-or-later
//! The `junit` callback of ansible-core 2.19.12: a JUnit XML file per playbook, written when the
//! run ends, with a test case per task and host. Its options are environment variables alone, as
//! the reference reads them.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use super::recap_json::dumps_indented;
use crate::stats::Outcome;

/// What the callback collected, and its options.
pub struct Junit {
    dir: PathBuf,
    task_class: bool,
    relative_to: Option<String>,
    out_of_tree: Option<String>,
    fail_on_change: bool,
    fail_on_ignore: bool,
    include_setup: bool,
    hide_arguments: bool,
    prefix: String,
    playbook: String,
    play: String,
    /// Each task once, in the order it first started, keyed as `profile_tasks` keys them.
    tasks: Vec<TaskData>,
    keys: HashMap<String, usize>,
    current: Option<usize>,
}

struct TaskData {
    name: String,
    path: Option<String>,
    play: String,
    action: String,
    start: f64,
    hosts: Vec<HostData>,
}

struct HostData {
    name: String,
    status: Status,
    finish: f64,
    body: Map<String, Value>,
    pretty: bool,
    /// The error block's message of a failure, which the reference files as an error.
    error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Failed,
    Skipped,
}

/// The wall clock as `time.time()` reads it.
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64())
}

/// A `JUNIT_*` flag, read as the reference reads it: `true` in any case is on.
fn flag(name: &str, default: bool) -> bool {
    std::env::var(name).map_or(default, |value| value.eq_ignore_ascii_case("true"))
}

impl Junit {
    /// The callback with its options from the environment, its directory made as the reference
    /// makes it when it loads.
    ///
    /// # Errors
    /// The directory cannot be made.
    pub fn new() -> Result<Self, String> {
        let dir = std::env::var("JUNIT_OUTPUT_DIR").map_or_else(
            |_| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default()
                    .join(".ansible.log")
            },
            PathBuf::from,
        );
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        Ok(Self {
            dir,
            task_class: flag("JUNIT_TASK_CLASS", false),
            relative_to: std::env::var("JUNIT_TASK_RELATIVE_PATH")
                .ok()
                .filter(|path| !path.is_empty()),
            out_of_tree: std::env::var("JUNIT_REPLACE_OUT_OF_TREE_PATH").ok(),
            fail_on_change: flag("JUNIT_FAIL_ON_CHANGE", false),
            fail_on_ignore: flag("JUNIT_FAIL_ON_IGNORE", false),
            include_setup: flag("JUNIT_INCLUDE_SETUP_TASKS_IN_REPORT", true),
            hide_arguments: flag("JUNIT_HIDE_TASK_ARGUMENTS", false),
            prefix: std::env::var("JUNIT_TEST_CASE_PREFIX").unwrap_or_default(),
            playbook: String::new(),
            play: String::new(),
            tasks: Vec::new(),
            keys: HashMap::new(),
            current: None,
        })
    }

    pub(super) fn playbook(&mut self, file: &std::path::Path) {
        self.playbook = file
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
    }

    pub(super) fn play(&mut self, name: &str) {
        self.play = name.to_string();
    }

    /// A task starts: recorded once, its name followed by its arguments unless they are hidden.
    pub(super) fn task(&mut self, key: &str, begun: &super::Begun<'_>) {
        if let Some(index) = self.keys.get(key) {
            self.current = Some(*index);
            return;
        }
        let mut name = begun.name.trim().to_string();
        if let Some(args) = begun
            .args
            .filter(|_| !self.hide_arguments && !args_empty(begun.args))
        {
            name.push(' ');
            name.push_str(args);
        }
        self.tasks.push(TaskData {
            name,
            path: begun.path.clone(),
            play: self.play.clone(),
            action: begun.action.to_string(),
            start: now(),
            hosts: Vec::new(),
        });
        self.keys.insert(key.to_string(), self.tasks.len() - 1);
        self.current = Some(self.tasks.len() - 1);
    }

    /// One host's result for the task running. An unreachable host is not recorded, as there.
    pub(super) fn result(
        &mut self,
        host: &str,
        outcome: Outcome,
        body: Map<String, Value>,
        pretty: bool,
        error: Option<&str>,
    ) {
        let Some(task) = self.current.and_then(|index| self.tasks.get_mut(index)) else {
            return;
        };
        let changed = body
            .get("changed")
            .is_some_and(|c| c.as_bool() == Some(true));
        let mut status = match outcome {
            Outcome::Ok | Outcome::Changed => Status::Ok,
            Outcome::Ignored if !self.fail_on_ignore => Status::Ok,
            Outcome::Skipped => Status::Skipped,
            Outcome::Ignored | Outcome::Failed | Outcome::Rescued => Status::Failed,
        };
        if self.fail_on_change && status == Status::Ok && changed {
            status = Status::Failed;
        }
        if status == Status::Failed && task.name.contains("EXPECTED FAILURE") {
            status = Status::Ok;
        } else if task.name.contains("TOGGLE RESULT") {
            status = match status {
                Status::Failed => Status::Ok,
                Status::Ok => Status::Failed,
                Status::Skipped => Status::Skipped,
            };
        }
        if !task.name.starts_with(&self.prefix) && status != Status::Failed {
            return;
        }
        task.hosts.retain(|h| h.name != host);
        task.hosts.push(HostData {
            name: host.to_string(),
            status,
            finish: now(),
            body,
            pretty,
            error: error
                .filter(|_| status == Status::Failed)
                .map(str::to_string),
        });
    }

    /// `_generate_report`: the file `<playbook>-<time>.xml` in the output directory.
    pub(super) fn report(&self) -> Result<(), String> {
        let mut cases = Vec::new();
        for task in &self.tasks {
            let setup = matches!(
                task.action.as_str(),
                "setup"
                    | "ansible.builtin.setup"
                    | "ansible.legacy.setup"
                    | "gather_facts"
                    | "ansible.builtin.gather_facts"
                    | "ansible.legacy.gather_facts"
            );
            if setup && !self.include_setup {
                continue;
            }
            for host in &task.hosts {
                cases.push(self.case(task, host));
            }
        }
        let xml = suites(&self.playbook, &cases);
        let file = self.dir.join(format!("{}-{}.xml", self.playbook, now()));
        std::fs::write(&file, xml).map_err(|e| format!("cannot write {}: {e}", file.display()))
    }

    fn case(&self, task: &TaskData, host: &HostData) -> Case {
        let name = format!("[{}] {}: {}", host.name, task.play, task.name);
        let mut classname = task.path.clone().unwrap_or_default();
        if let (Some(base), Some(path)) = (&self.relative_to, &task.path) {
            classname = relative(path, base);
        }
        if let Some(placeholder) = &self.out_of_tree
            && classname.starts_with("../")
        {
            let base = classname.rsplit('/').next().unwrap_or_default().to_string();
            classname = format!("{placeholder}{base}");
        }
        if self.task_class
            && let Some(at) = classname.rfind(".yml:")
            && classname[at + 5..].bytes().all(|b| b.is_ascii_digit())
            && at + 5 < classname.len()
        {
            classname.truncate(at);
        }
        let dump = dumps_indented(
            &Value::Object(host.body.clone()),
            0,
            if host.pretty { 4 } else { 0 },
        );
        let mut case = Case {
            name,
            classname,
            time: exact(host.finish - task.start),
            body: Body::Out(dump.clone()),
        };
        match host.status {
            Status::Ok => {}
            Status::Skipped => {
                let reason = host
                    .body
                    .get("skip_reason")
                    .map_or_else(|| "skipped".to_string(), text);
                case.body = Body::Skipped(reason);
            }
            Status::Failed => {
                // A failure the task raised carries its error, as every failed result does in
                // ansible-core 2.19.12; one an option turned into a failure has none.
                case.body = match &host.error {
                    Some(message) => Body::Error {
                        message: message.clone(),
                        output: traceback(message, host.body.get("exception")),
                    },
                    None => Body::Failure {
                        message: host.body.get("msg").map_or_else(
                            || {
                                format!(
                                    "rc={}",
                                    host.body.get("rc").map_or_else(|| "0".to_string(), text)
                                )
                            },
                            text,
                        ),
                        output: dump,
                    },
                };
            }
        }
        case
    }
}

/// `format_event_traceback` of a failure's error: each link of its chain, the module's own
/// traceback when it sent one, and `(traceback missing)` for the others.
fn traceback(message: &str, exception: Option<&Value>) -> String {
    let first = exception.filter(|e| truthy(e)).map_or_else(
        || "(traceback missing)\n".to_string(),
        |e| {
            let text = text(e);
            if text.ends_with('\n') {
                text
            } else {
                format!("{text}\n")
            }
        },
    );
    let cause = "The above exception was the direct cause of the following exception:";
    // A failure a module or an action plugin returned was captured: one link more.
    let captured = if message.starts_with("Task failed: Module failed") {
        Some("target")
    } else if message.starts_with("Task failed: Action failed") {
        Some("action")
    } else {
        None
    };
    match captured {
        Some(context) => format!(
            "{first}\nThe above {context} exception was the direct cause of the following \
             controller exception:\n\n(traceback missing)\n\n{cause}\n\n(traceback missing)\n"
        ),
        None => format!("{first}\n{cause}\n\n(traceback missing)\n"),
    }
}

/// Whether a task's argument text says nothing.
fn args_empty(args: Option<&str>) -> bool {
    args.is_none_or(str::is_empty)
}

fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
        && value.as_str().is_none_or(|s| !s.is_empty())
}

/// A value as Python's `str()` writes it.
fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => crate::playbook::python_repr(other),
    }
}

/// `os.path.relpath(path, base)` for two absolute or two relative paths, path separators `/`.
fn relative(path: &str, base: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let absolute = |p: &str| {
        let p = std::path::Path::new(p);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    };
    let (path, base) = (absolute(path), absolute(base));
    let (from, to): (Vec<_>, Vec<_>) = (base.components().collect(), path.components().collect());
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - common];
    parts.extend(
        to[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

struct Case {
    name: String,
    classname: String,
    time: Dec,
    body: Body,
}

enum Body {
    Out(String),
    Skipped(String),
    Error { message: String, output: String },
    Failure { message: String, output: String },
}

/// `TestSuites(suites=[TestSuite(name, cases)]).to_pretty_xml()`, as minidom writes it.
fn suites(playbook: &str, cases: &[Case]) -> String {
    let count = |want: fn(&Body) -> bool| cases.iter().filter(|c| want(&c.body)).count();
    let errors = count(|b| matches!(b, Body::Error { .. }));
    let failures = count(|b| matches!(b, Body::Failure { .. }));
    let skipped = count(|b| matches!(b, Body::Skipped(_)));
    let mut total = Dec::zero();
    for case in cases {
        if !case.time.is_zero() {
            total = total.add(&case.time);
        }
    }
    // `decimal.Decimal(sum(...))` of the suites' times, one suite here.
    let all = Dec::zero().add(&total);
    let mut out = String::from("<?xml version=\"1.0\" ?>\n");
    let _ = writeln!(
        out,
        "<testsuites disabled=\"0\" errors=\"{errors}\" failures=\"{failures}\" tests=\"{}\" time=\"{}\">",
        cases.len(),
        all.text()
    );
    let open = format!(
        "\t<testsuite disabled=\"0\" errors=\"{errors}\" failures=\"{failures}\" name=\"{}\" skipped=\"{skipped}\" tests=\"{}\" time=\"{}\"",
        attr(playbook),
        cases.len(),
        total.text()
    );
    if cases.is_empty() {
        let _ = writeln!(out, "{open}/>");
    } else {
        let _ = writeln!(out, "{open}>");
        for case in cases {
            case_xml(&mut out, case);
        }
        out.push_str("\t</testsuite>\n");
    }
    out.push_str("</testsuites>\n");
    out
}

fn case_xml(out: &mut String, case: &Case) {
    let open = format!(
        "\t\t<testcase classname=\"{}\" name=\"{}\" time=\"{}\"",
        attr(&case.classname),
        attr(&case.name),
        case.time.text()
    );
    let child = match &case.body {
        Body::Out(text) if text.is_empty() => None,
        Body::Out(text) => Some(format!("<system-out>{}</system-out>", data(text))),
        Body::Skipped(text) if text.is_empty() => Some("<skipped/>".to_string()),
        Body::Skipped(text) => Some(format!("<skipped>{}</skipped>", data(text))),
        Body::Error { message, output } => Some(element("error", message, output)),
        Body::Failure { message, output } => Some(element("failure", message, output)),
    };
    match child {
        None => {
            let _ = writeln!(out, "{open}/>");
        }
        Some(child) => {
            let _ = writeln!(out, "{open}>\n\t\t\t{child}\n\t\t</testcase>");
        }
    }
}

fn element(tag: &str, message: &str, output: &str) -> String {
    let attrs = format!("message=\"{}\" type=\"{tag}\"", attr(message));
    if output.is_empty() {
        format!("<{tag} {attrs}/>")
    } else {
        format!("<{tag} {attrs}>{}</{tag}>", data(output))
    }
}

/// minidom's escaping of text.
fn data(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// minidom's escaping of an attribute value.
fn attr(text: &str) -> String {
    data(text)
        .replace('"', "&quot;")
        .replace('\r', "&#13;")
        .replace('\n', "&#10;")
        .replace('\t', "&#9;")
}

/// A Python `Decimal`: the integer `digits` times ten to `exp`.
#[derive(Clone, Debug, PartialEq)]
struct Dec {
    digits: Vec<u8>,
    exp: i64,
}

/// `Decimal(f)`: the exact value of a float, `n * 5**k` over `10**k` for `n / 2**k`.
fn exact(f: f64) -> Dec {
    if f == 0.0 || !f.is_finite() {
        return Dec::zero();
    }
    let bits = f.abs().to_bits();
    let exponent = i64::try_from((bits >> 52) & 0x7ff).unwrap_or(0);
    let fraction = bits & ((1 << 52) - 1);
    let (mut mantissa, mut power) = if exponent == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1 << 52), exponent - 1075)
    };
    while mantissa % 2 == 0 {
        mantissa /= 2;
        power += 1;
    }
    let mut digits: Vec<u8> = mantissa.to_string().bytes().map(|b| b - b'0').collect();
    if power >= 0 {
        for _ in 0..power {
            digits = times(&digits, 2);
        }
        return Dec { digits, exp: 0 };
    }
    for _ in 0..-power {
        digits = times(&digits, 5);
    }
    Dec { digits, exp: power }
}

/// `digits` times a small factor.
fn times(digits: &[u8], factor: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(digits.len() + 1);
    let mut carry = 0u8;
    for d in digits.iter().rev() {
        let v = d * factor + carry;
        out.push(v % 10);
        carry = v / 10;
    }
    if carry > 0 {
        out.push(carry);
    }
    out.reverse();
    out
}

impl Dec {
    fn zero() -> Self {
        Self {
            digits: vec![0],
            exp: 0,
        }
    }

    fn is_zero(&self) -> bool {
        self.digits.iter().all(|d| *d == 0)
    }

    /// `self + other` under the default context: exact, then rounded half-even to 28 digits.
    fn add(&self, other: &Dec) -> Dec {
        let exp = self.exp.min(other.exp);
        let align = |d: &Dec| {
            let mut v = d.digits.clone();
            v.extend(std::iter::repeat_n(
                0,
                usize::try_from(d.exp - exp).unwrap_or(0),
            ));
            v
        };
        let (a, b) = (align(self), align(other));
        let len = a.len().max(b.len());
        let mut sum = Vec::with_capacity(len + 1);
        let mut carry = 0u8;
        for i in 0..len {
            let x = if i < a.len() { a[a.len() - 1 - i] } else { 0 };
            let y = if i < b.len() { b[b.len() - 1 - i] } else { 0 };
            let v = x + y + carry;
            sum.push(v % 10);
            carry = v / 10;
        }
        if carry > 0 {
            sum.push(carry);
        }
        sum.reverse();
        while sum.len() > 1 && sum[0] == 0 {
            sum.remove(0);
        }
        Dec { digits: sum, exp }.rounded(28)
    }

    fn rounded(mut self, precision: usize) -> Dec {
        if self.digits.len() <= precision {
            return self;
        }
        let dropped = self.digits.split_off(precision);
        self.exp += i64::try_from(dropped.len()).unwrap_or(0);
        let rest_nonzero = dropped[1..].iter().any(|d| *d != 0);
        let up = dropped[0] > 5
            || (dropped[0] == 5
                && (rest_nonzero || self.digits.last().is_some_and(|d| d % 2 == 1)));
        if up {
            let mut i = self.digits.len();
            loop {
                if i == 0 {
                    self.digits.insert(0, 1);
                    self.digits.pop();
                    self.exp += 1;
                    break;
                }
                i -= 1;
                if self.digits[i] == 9 {
                    self.digits[i] = 0;
                } else {
                    self.digits[i] += 1;
                    break;
                }
            }
        }
        self
    }

    /// `str(Decimal)`.
    fn text(&self) -> String {
        let coefficient: String = self.digits.iter().map(|d| char::from(b'0' + d)).collect();
        let left = self.exp + i64::try_from(coefficient.len()).unwrap_or(0);
        if self.exp <= 0 && left > -6 {
            if self.exp == 0 {
                return coefficient;
            }
            if left <= 0 {
                let zeros = "0".repeat(usize::try_from(-left).unwrap_or(0));
                return format!("0.{zeros}{coefficient}");
            }
            let at = usize::try_from(left).unwrap_or(0);
            return format!("{}.{}", &coefficient[..at], &coefficient[at..]);
        }
        let (head, tail) = coefficient.split_at(1);
        let dot = if tail.is_empty() {
            String::new()
        } else {
            format!(".{tail}")
        };
        format!("{head}{dot}E{:+}", left - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_read_as_python_writes_them() {
        assert_eq!(exact(0.25).text(), "0.25");
        assert_eq!(exact(1.0).text(), "1");
        assert_eq!(
            exact(0.1).text(),
            "0.1000000000000000055511151231257827021181583404541015625"
        );
        // `sum` rounds each step to 28 digits.
        assert_eq!(
            Dec::zero().add(&exact(0.1)).text(),
            "0.1000000000000000055511151231"
        );
        assert_eq!(
            exact(1e-7).text(),
            "9.99999999999999954748111825886258685613938723690807819366455078125E-8"
        );
    }

    #[test]
    fn the_xml_is_minidom_s() {
        let cases = vec![Case {
            name: "[h1] all: first _raw_params=echo hi".into(),
            classname: "/p/play.yml:4".into(),
            time: exact(0.5),
            body: Body::Out("{\n\"changed\": true\n}".into()),
        }];
        assert_eq!(
            suites("play", &cases),
            "<?xml version=\"1.0\" ?>\n\
             <testsuites disabled=\"0\" errors=\"0\" failures=\"0\" tests=\"1\" time=\"0.5\">\n\
             \t<testsuite disabled=\"0\" errors=\"0\" failures=\"0\" name=\"play\" skipped=\"0\" tests=\"1\" time=\"0.5\">\n\
             \t\t<testcase classname=\"/p/play.yml:4\" name=\"[h1] all: first _raw_params=echo hi\" time=\"0.5\">\n\
             \t\t\t<system-out>{\n\"changed\": true\n}</system-out>\n\
             \t\t</testcase>\n\
             \t</testsuite>\n\
             </testsuites>\n"
        );
    }
}
