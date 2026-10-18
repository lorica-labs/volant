// SPDX-License-Identifier: GPL-3.0-or-later
//! Quoting remote paths for a shell, and checking `remote_tmp`.

use anyhow::bail;

/// One remote path as a single shell word. A leading `~` or `~user` stays bare, up to and
/// including the first `/`, so the remote shell, the only thing that can, expands it; the rest
/// is single-quoted, so a `remote_tmp` holding a space or a shell metacharacter cannot split
/// into several words or run anything. That bare segment cannot be quoted without stopping the
/// shell from expanding it, so this depends on [`validate_remote_tmp`] having already refused
/// everything between the `~` and the first `/` that is not a user name.
///
/// That validator runs at every source `remote_tmp` has: `Config::load` for `[defaults]` and for
/// `ANSIBLE_REMOTE_TMP`, and [`super::Transport::for_vars`] for a host's own `ansible_remote_tmp`, which
/// is where a `group_vars`, a task's `vars:`, a `set_fact` or a `-e` arrives. A fourth source
/// added without a fourth call is how this guarantee rots, so the list is here rather than left
/// to a grep.
pub(super) fn shell_word(path: &str) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return single_quoted(path);
    };
    match rest.find('/') {
        Some(slash) => format!("~{}/{}", &rest[..slash], single_quoted(&rest[slash + 1..])),
        // Just `~` or `~user`, with nothing after it to hold a space or a metacharacter.
        None => path.to_string(),
    }
}

/// The characters a user name may hold in the `~user` part of a remote path. Everything else --
/// a space, a quote, a `$`, a backtick -- would reach the remote shell unquoted, because that
/// segment is the one thing `shell_word` cannot quote: quoting it would stop the shell from
/// expanding it, which is the only reason it is there.
fn is_home_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// Refuses a `remote_tmp` whose `~user` part -- up to the first `/`, or the whole value if there
/// is none -- holds anything but a user name. A value with no leading `~` at all is untouched:
/// `shell_word` single-quotes it whole, so nothing there reaches the shell unquoted.
pub(crate) fn validate_remote_tmp(value: &str) -> anyhow::Result<()> {
    let Some(rest) = value.strip_prefix('~') else {
        return Ok(());
    };
    let user = rest.split('/').next().unwrap_or("");
    if user.chars().all(is_home_char) {
        return Ok(());
    }
    bail!(
        "remote_tmp '{value}': only a user name may follow '~', because that part reaches the \
         remote shell unquoted"
    )
}

pub(super) fn single_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}
