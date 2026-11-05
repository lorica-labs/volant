// SPDX-License-Identifier: GPL-3.0-or-later
//! The `-vvvv` lines of the `ssh` commands the controller runs to reach a host, in the shape of
//! the reference's `ESTABLISH SSH CONNECTION`, `SSH: EXEC` and result lines, and the `-vvv` lines
//! of a local link. They name the commands this controller runs (the probe, the upload, the link
//! to the agent), never the reference's own. A log with the verbosity for a line gets it too.

use std::fmt::Write as _;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

use anstream::{AutoStream, ColorChoice};

use crate::render::log;

use super::bootstrap::Captured;
use super::defaults::SshTarget;

/// The level the connection lines show from: the reference prints them one step lower, but
/// they describe other commands, so they stay with the rest of the `-vvvv` detail.
const LEVEL_FROM: u8 = 4;

/// The level a local link's lines show from, the reference's for its local connection.
const LOCAL_LEVEL: u8 = 3;

static VERBOSITY: AtomicU8 = AtomicU8::new(0);
static COLOR: Mutex<ColorChoice> = Mutex::new(ColorChoice::Auto);

/// Sets how many `-v`s the run was given and the colour choice, once, before any link opens.
pub fn configure(verbosity: u8, choice: ColorChoice) {
    VERBOSITY.store(verbosity, Ordering::Relaxed);
    *COLOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = choice;
}

/// How many `-v`s the run was given.
pub fn verbosity() -> u8 {
    VERBOSITY.load(Ordering::Relaxed)
}

fn enabled() -> bool {
    VERBOSITY.load(Ordering::Relaxed) >= LEVEL_FROM || log::logs_level(LEVEL_FROM)
}

/// Whether the connection lines are shown, for a line that costs a command to write.
pub(super) fn connection_lines() -> bool {
    enabled()
}

fn line(text: &str) {
    line_from(LEVEL_FROM, text);
}

/// `text`, on the screen from `level` `-v`s up and in the log the way the screen's lines go there.
fn line_from(level: u8, text: &str) {
    let shown = VERBOSITY.load(Ordering::Relaxed) >= level;
    if shown {
        let choice = *COLOR
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = AutoStream::new(std::io::stdout(), choice);
        let style = crate::render::VERBOSE;
        let _ = writeln!(out, "{style}{text}{style:#}");
    }
    if shown || log::logs_level(level) {
        log::record(text);
    }
}

/// `<host> <text>` for a command or an upload an action sends over a link, from `-vvv`.
pub(crate) fn action_line(host: &str, text: &str) {
    line_from(LOCAL_LEVEL, &format!("<{host}> {text}"));
}

/// `<host> ESTABLISH LOCAL CONNECTION FOR USER: <user>` and `<host> EXEC <the command line>` for
/// the command a local link starts. The password is never in it: it travels on stdin.
pub(super) fn local_link(host: &str, command: &tokio::process::Command) {
    let wanted = VERBOSITY.load(Ordering::Relaxed) >= LOCAL_LEVEL || log::logs_level(LOCAL_LEVEL);
    if wanted {
        let user = ["LOGNAME", "USER", "LNAME", "USERNAME"]
            .iter()
            .find_map(|key| std::env::var(key).ok())
            .unwrap_or_else(|| "None".to_string());
        line_from(
            LOCAL_LEVEL,
            &format!("<{host}> ESTABLISH LOCAL CONNECTION FOR USER: {user}"),
        );
        let command = command.as_std();
        let words = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|word| word.to_string_lossy().into_owned());
        line_from(LOCAL_LEVEL, &local_exec_line(host, words));
    }
}

fn local_exec_line(host: &str, words: impl Iterator<Item = String>) -> String {
    let words: Vec<String> = words.map(|word| shell_quote(&word)).collect();
    format!("<{host}> EXEC {}", words.join(" "))
}

/// `<address> ESTABLISH SSH CONNECTION FOR USER: <user>`, `None` when no user is set. A
/// `paramiko_ssh` host gets that plugin's line, which names the port and the address too.
pub(super) fn established(target: &SshTarget) {
    if enabled() {
        let user = target.user.as_deref().unwrap_or("None");
        line(&if target.own.paramiko {
            format!(
                "<{0}> ESTABLISH PARAMIKO SSH CONNECTION FOR USER: {user} on PORT {1} TO {0}",
                target.address,
                target.port.unwrap_or(22)
            )
        } else {
            format!(
                "<{}> ESTABLISH SSH CONNECTION FOR USER: {user}",
                target.address
            )
        });
    }
}

/// `<address> SSH: EXEC <the command line>`.
pub(super) fn exec(target: &SshTarget, argv: &[String]) {
    if enabled() {
        let words: Vec<String> = argv.iter().map(|word| shell_quote(word)).collect();
        line(&format!(
            "<{}> SSH: EXEC {}",
            target.address,
            words.join(" ")
        ));
    }
}

/// `<name> ESTABLISH <KIND> CONNECTION FOR USER: <user>` for a target reached through an exec
/// command, `None` when no user is set.
pub(super) fn established_as(place: &str, kind: &str, user: Option<&str>) {
    if enabled() {
        line(&established_line(place, kind, user));
    }
}

fn established_line(place: &str, kind: &str, user: Option<&str>) -> String {
    format!(
        "<{place}> ESTABLISH {kind} CONNECTION FOR USER: {}",
        user.unwrap_or("None")
    )
}

/// `<name> EXEC [<the command line as a Python list>]` for a command an exec target runs.
pub(super) fn exec_list(place: &str, argv: &[String]) {
    if enabled() {
        line(&exec_list_line(place, argv));
    }
}

fn exec_list_line(place: &str, argv: &[String]) -> String {
    let words: Vec<String> = argv.iter().map(|word| str_repr(word)).collect();
    format!("<{place}> EXEC [{}]", words.join(", "))
}

/// `<name> <text>`, a plugin's own note on a link.
pub(super) fn note(place: &str, text: &str) {
    if enabled() {
        line(&format!("<{place}> {text}"));
    }
}

/// `<dir> THIS IS A LOCAL CHROOT DIR`, the chroot plugin's line for a link.
pub(super) fn local_chroot(place: &str) {
    if enabled() {
        line(&format!("<{place}> THIS IS A LOCAL CHROOT DIR"));
    }
}

/// `<name> RUN [<the command line as a list of Python bytes>]`, podman's line for a command.
pub(super) fn run_list(place: &str, argv: &[String]) {
    if enabled() {
        line(&run_list_line(place, argv));
    }
}

fn run_list_line(place: &str, argv: &[String]) -> String {
    let words: Vec<String> = argv
        .iter()
        .map(|word| bytes_repr(word.as_bytes()))
        .collect();
    format!("<{place}> RUN [{}]", words.join(", "))
}

/// A Python `str` literal for `text`, as `repr` writes one.
fn str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => {
                let _ = write!(out, "\\x{:02x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `<address> (code, b'stdout', b'stderr')` once a command that was waited for has ended.
pub(super) fn result(place: &str, captured: &Captured) {
    if enabled() {
        line(&format!(
            "<{}> ({}, {}, {})",
            place,
            captured.code.unwrap_or(-1),
            bytes_repr(captured.stdout.as_bytes()),
            bytes_repr(captured.stderr.as_bytes())
        ));
    }
}

/// `shlex.quote`: a word of safe characters stays bare, any other is single-quoted.
pub(crate) fn shell_quote(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\"'\"'"))
    }
}

/// A Python `bytes` literal for `bytes`: the quote Python picks, and its escapes.
pub(crate) fn bytes_repr(bytes: &[u8]) -> String {
    let quote = if bytes.contains(&b'\'') && !bytes.contains(&b'"') {
        '"'
    } else {
        '\''
    };
    let mut text = format!("b{quote}");
    for &byte in bytes {
        match byte {
            b'\\' => text.push_str("\\\\"),
            b'\t' => text.push_str("\\t"),
            b'\n' => text.push_str("\\n"),
            b'\r' => text.push_str("\\r"),
            b if char::from(b) == quote => {
                text.push('\\');
                text.push(quote);
            }
            0x20..=0x7e => text.push(char::from(byte)),
            _ => {
                let _ = write!(text, "\\x{byte:02x}");
            }
        }
    }
    text.push(quote);
    text
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::{
        bytes_repr, established_line, exec_list_line, local_exec_line, run_list_line, shell_quote,
    };

    #[test]
    fn a_local_link_s_line_reads_as_a_shell_command() {
        let argv = ["sudo", "-H", "-S", "-n", "/bin/sh", "-c", "exec '/a b'"].map(String::from);
        assert_eq!(
            local_exec_line("web1", argv.into_iter()),
            r#"<web1> EXEC sudo -H -S -n /bin/sh -c 'exec '"'"'/a b'"'"''"#
        );
    }

    #[test]
    fn a_podman_line_writes_bytes() {
        let argv = ["/usr/bin/podman", "exec", "app1"].map(String::from);
        assert_eq!(
            run_list_line("app1", &argv),
            "<app1> RUN [b'/usr/bin/podman', b'exec', b'app1']"
        );
    }

    #[test]
    fn an_exec_target_s_lines_read_as_the_reference_s() {
        assert_eq!(
            established_line("app1", "DOCKER", Some("root")),
            "<app1> ESTABLISH DOCKER CONNECTION FOR USER: root"
        );
        assert_eq!(
            established_line("app1", "DOCKER", None),
            "<app1> ESTABLISH DOCKER CONNECTION FOR USER: None"
        );
        let argv = [
            "docker",
            "exec",
            "-i",
            "app1",
            "/bin/sh",
            "-c",
            "it's \"x\"",
        ]
        .map(String::from);
        assert_eq!(
            exec_list_line("app1", &argv),
            r#"<app1> EXEC ['docker', 'exec', '-i', 'app1', '/bin/sh', '-c', 'it\'s "x"']"#
        );
    }

    #[rstest]
    #[case(b"ok\n", "b'ok\\n'")]
    #[case(b"it's", "b\"it's\"")]
    #[case(b"a\\b\x01\xc3", "b'a\\\\b\\x01\\xc3'")]
    #[case(b"", "b''")]
    fn a_bytes_literal_reads_as_python_prints_it(#[case] bytes: &[u8], #[case] want: &str) {
        assert_eq!(bytes_repr(bytes), want);
    }

    #[rstest]
    #[case("-o", "-o")]
    #[case("/bin/sh -c 'x'", "'/bin/sh -c '\"'\"'x'\"'\"''")]
    #[case("", "''")]
    fn a_word_is_quoted_as_shlex_quotes_it(#[case] word: &str, #[case] want: &str) {
        assert_eq!(shell_quote(word), want);
    }
}
