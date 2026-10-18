// SPDX-License-Identifier: GPL-3.0-or-later
//! The expansion of `$name` and `~` in an argument, the way Python does it.

/// `os.path.expandvars`: `$name` and `${name}` whose name `var` knows; any other stays as written.
pub(super) fn expand_vars(text: &str, var: &dyn Fn(&str) -> Option<String>) -> String {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let (name, len) = if let Some(braced) = after.strip_prefix('{') {
            braced
                .find('}')
                .map_or(("", 0), |end| (&braced[..end], end + 2))
        } else {
            let end = after.find(|c| !word(c)).unwrap_or(after.len());
            (&after[..end], end)
        };
        match (len > 0).then(|| var(name)).flatten() {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[at..=at + len]),
        }
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// `os.path.expanduser`: a leading `~` or `~name` becomes the home directory of the module's
/// `HOME`, or of the account; an unknown account leaves the word as written.
#[cfg(unix)]
pub(super) fn expand_user(path: &str, var: &dyn Fn(&str) -> Option<String>) -> String {
    let Some(tail) = path.strip_prefix('~') else {
        return path.to_string();
    };
    let uid = unsafe { libc::getuid() };
    let end = tail.find('/').unwrap_or(tail.len());
    let (name, rest) = tail.split_at(end);
    let home = if name.is_empty() {
        var("HOME").or_else(|| passwd_home(&uid.to_string(), true))
    } else {
        passwd_home(name, false)
    };
    let Some(home) = home else {
        return path.to_string();
    };
    let joined = format!("{}{rest}", home.trim_end_matches('/'));
    if joined.is_empty() {
        "/".into()
    } else {
        joined
    }
}

/// The home directory of an account, by name or by uid. The agent is a static musl binary, whose
/// `getpwnam` reads `/etc/passwd` and nothing else, as this does.
#[cfg(unix)]
fn passwd_home(key: &str, by_uid: bool) -> Option<String> {
    let text = std::fs::read_to_string("/etc/passwd").ok()?;
    text.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.len() >= 6 && fields[usize::from(by_uid) * 2] == key).then(|| fields[5].to_string())
    })
}
