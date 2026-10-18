// SPDX-License-Identifier: GPL-3.0-or-later
//! How a link escalates through `sudo`: the command forms and the exchange that precedes the agent.

use super::super::defaults::{ConnectError, Escalation};
use super::super::shell::single_quoted;

/// Which `sudo` form a link uses. The escalation probe settles it and the link itself then
/// uses the same one, so the two can never disagree about whether a password is offered.
///
/// `-n` is the form for a `sudo` that will not ask for anything: it never reads stdin, so
/// nothing may be written to a link opened this way. That covers a `NOPASSWD` rule as much as
/// an authentication still cached, and neither can be told from the other before asking.
///
/// `-k -S -p ''` is the form for a `sudo` that does ask, and `-k` there is not optional.
/// Measured on `sudo` 1.9.15p5 and `sudo-rs` 0.2.13: while a previous authentication is still
/// cached, `-S` does not read stdin at all, and the password written there would be read by the
/// agent as the first bytes of its first frame. `-k` drops that cached authentication, so `-S`
/// always consumes exactly the one line written for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::transport) enum SudoForm {
    NoPrompt,
    ReadStdin,
}

impl SudoForm {
    fn flags(self) -> &'static [&'static str] {
        match self {
            SudoForm::NoPrompt => &["-n"],
            SudoForm::ReadStdin => &["-k", "-S", "-p", ""],
        }
    }
}

/// One escalation and the `sudo` form settled for it, or `None` for a link that does not
/// escalate at all.
type Escalated<'a> = Option<(&'a Escalation, SudoForm)>;

/// The `sudo` password, as the bytes written to the child before any frame. Built here and
/// nowhere else, so the only thing that ever holds it is a `Vec<u8>` on its way to a pipe.
///
/// Nothing is written unless the probe settled on the form that reads stdin. A `sudo` that
/// needs no authentication leaves the line on the pipe, where the agent reads it as its first
/// frame header and then never answers.
pub(in crate::transport) fn preamble(escalated: Escalated<'_>) -> Option<Vec<u8>> {
    let (escalation, form) = escalated?;
    if form != SudoForm::ReadStdin {
        return None;
    }
    let password = escalation.password.as_ref()?;
    let mut bytes = Vec::with_capacity(password.len() + 1);
    bytes.extend_from_slice(password.as_bytes());
    bytes.push(b'\n');
    Some(bytes)
}

/// `sudo`'s words up to `-u`: `become_exe` or `sudo`, then `become_flags` or `-H`, then the
/// probed form's own. The reference's default flags are `-H -S -n`; the last two are what the
/// probed form decides here, so only `-H` stands in for them.
///
/// The form that writes a password drops `-n` from `become_flags` first, as the reference does
/// when it has a password to send: a `sudo` told never to ask refuses the line it is given.
fn sudo_words(escalation: &Escalation, form: SudoForm) -> Vec<String> {
    let mut words = if escalation.exe.is_empty() {
        vec!["sudo".to_string()]
    } else {
        escalation.exe.clone()
    };
    match &escalation.flags {
        None => words.push("-H".to_string()),
        Some(flags) if form == SudoForm::ReadStdin => {
            words.extend(flags.iter().filter_map(|flag| interactive(flag)));
        }
        Some(flags) => words.extend(flags.iter().cloned()),
    }
    words.extend(form.flags().iter().map(ToString::to_string));
    words
}

/// One `become_flags` word with its non-interactive switch taken out, the way sudo.py rewrites
/// it: `-n` and `--non-interactive` go, and a cluster of short flags loses its `n`
/// (its `^(-\w*)n(\w*.*)` pattern, whose greedy `\w*` finds the last `n` of the leading run
/// of word characters).
pub(in crate::transport) fn interactive(flag: &str) -> Option<String> {
    if flag == "-n" || flag == "--non-interactive" {
        return None;
    }
    let Some(cluster) = flag.strip_prefix('-').filter(|rest| !rest.starts_with('-')) else {
        return Some(flag.to_string());
    };
    let run = cluster
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(cluster.len());
    Some(match cluster[..run].rfind('n') {
        Some(at) => format!("-{}{}", &cluster[..at], &cluster[at + 1..]),
        None => flag.to_string(),
    })
}

/// The command line that starts a local agent, under `sudo` when escalation is asked for.
pub(in crate::transport) fn local_argv(agent: &str, escalated: Escalated<'_>) -> Vec<String> {
    let Some((escalation, form)) = escalated else {
        return vec![agent.to_string()];
    };
    let mut argv = sudo_words(escalation, form);
    argv.extend([
        "-u".to_string(),
        escalation.user.clone(),
        "--".to_string(),
        agent.to_string(),
    ]);
    argv
}

/// One remote command as the escalated user, or unchanged when the link does not escalate.
///
/// The command travels as a single word to an inner `sh -c` rather than straight to `sudo`,
/// because `remote_tmp` starts with `~`: the outer remote shell would expand it to the
/// *connecting* user's home, which is the one directory an unprivileged `become_user` may not
/// be able to enter. Inside the shell `sudo -H` starts, `~` is the escalated user's own home,
/// so the agent is cached where that user can read it and nowhere else.
pub(in crate::transport) fn escalated_command(escalated: Escalated<'_>, command: &str) -> String {
    match escalated {
        None => command.to_string(),
        Some(_) => format!("{}sh -c {}", sudo_prefix(escalated), single_quoted(command)),
    }
}

/// `sudo -H ... --`, up to but not including the agent, as shell words for a remote command.
/// Empty without escalation, so one `format!` covers both cases at every call site.
pub(in crate::transport) fn sudo_prefix(escalated: Escalated<'_>) -> String {
    let Some((escalation, form)) = escalated else {
        return String::new();
    };
    let words = sudo_words(escalation, form)
        .iter()
        .map(|word| single_quoted(word))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{words} -u {} -- ", single_quoted(&escalation.user))
}

/// Who a bootstrap command runs as: the connecting user, or the escalated one through `sudo`
/// in its settled form, both over plain pipes; or the escalated one through `su`, over a
/// terminal.
#[derive(Clone, Copy)]
pub(in crate::transport) enum RunAs<'a> {
    Direct(Escalated<'a>),
    Su(&'a Escalation),
}

/// A bootstrap failure on an escalated link, said to be about that user's agent.
pub(in crate::transport) fn preparing_for(
    err: ConnectError,
    escalation: Option<&Escalation>,
) -> ConnectError {
    match (err, escalation) {
        (ConnectError::Unreachable(msg), Some(e)) => {
            ConnectError::Unreachable(format!("preparing the agent for user {}: {msg}", e.user))
        }
        (err, _) => err,
    }
}

/// The line the command behind `su` prints once `su` has let it in and its terminal is raw:
/// everything before it is `su` talking, everything after it is the command's own. A fresh one
/// for every `su`, like the reference's `BECOME-SUCCESS-<random>`, so nothing printed before
/// `su` lets the command in can pass for it.
pub(in crate::transport) fn become_success() -> String {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seed = format!(
        "{:?} {} {}",
        std::time::SystemTime::now(),
        std::process::id(),
        COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    format!(
        "VOLANT-BECOME-SUCCESS-{}",
        &blake3::hash(seed.as_bytes()).to_hex()[..32]
    )
}
