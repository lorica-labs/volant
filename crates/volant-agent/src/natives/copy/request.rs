// SPDX-License-Identifier: GPL-3.0-or-later

//! The options `copy` reads, and the request read from them.

use serde_json::{Map, Value};

use crate::natives::common::{
    Account, ArgSpec, ModeError, bool_param, flag, group_account, null, owner_account, parse_mode,
    path_param, str_param, umask,
};
use crate::natives::runtime::{Clock, Stop as Halt};

/// `copy`'s own options, then `add_file_common_args`.
pub(super) const SPEC: &[ArgSpec] = &[
    null("src"),
    null("_original_basename"),
    null("content"),
    null("dest"),
    flag("backup", false),
    flag("force", true),
    null("validate"),
    null("directory_mode"),
    flag("remote_src", false),
    null("local_follow"),
    null("checksum"),
    flag("follow", false),
    null("mode"),
    null("owner"),
    null("group"),
    null("seuser"),
    null("serole"),
    null("selevel"),
    null("setype"),
    ArgSpec {
        name: "attributes",
        aliases: &["attr"],
        default: || Value::Null,
    },
    flag("unsafe_writes", false),
];

/// Options whose every value other than `null` is outside the native. The action plugin never
/// sends `content`: it stages it as `src`.
const UNSUPPORTED: &[&str] = &[
    "content",
    "directory_mode",
    "attributes",
    "seuser",
    "serole",
    "selevel",
    "setype",
];

/// Why the answer stopped short of a result.
pub(super) enum Stop {
    /// The reference's `fail_json`, with these keys.
    Fail(Map<String, Value>),
    /// Outside the subset. Only ever returned before anything on the host changed.
    Back(String),
    /// The task's `timeout` ran out, or the controller cancelled it.
    Clock(Halt),
}

impl From<String> for Stop {
    fn from(reason: String) -> Self {
        Stop::Back(reason)
    }
}

impl From<&str> for Stop {
    fn from(reason: &str) -> Self {
        Stop::Back(reason.to_string())
    }
}

impl From<Halt> for Stop {
    fn from(halt: Halt) -> Self {
        match halt {
            Halt::HandBack(reason) => Stop::Back(reason),
            other => Stop::Clock(other),
        }
    }
}

/// What the task asks for, read and checked before anything is touched.
pub(super) struct Request<'a> {
    pub(super) src: &'a str,
    pub(super) dest: &'a str,
    pub(super) original_basename: Option<&'a str>,
    pub(super) backup: bool,
    pub(super) force: bool,
    pub(super) follow: bool,
    pub(super) validate: Option<&'a str>,
    pub(super) checksum: Option<&'a str>,
    pub(super) owner: Option<Account>,
    pub(super) group: Option<Account>,
    pub(super) mode: Option<Value>,
    pub(super) umask: u32,
}

/// The parameters as `AnsibleModule` would validate them. Whatever it would convert, warn about
/// or refuse goes back to the Python module, which says it in its own words.
pub(super) fn request<'a>(
    params: &'a Map<String, Value>,
    clock: Clock,
) -> Result<Request<'a>, Halt> {
    if let Some(name) = UNSUPPORTED.iter().find(|name| !params[**name].is_null()) {
        return Err(format!("{name} is set").into());
    }
    for name in ["backup", "force", "remote_src", "follow", "unsafe_writes"] {
        bool_param(params, name)?;
    }
    if bool_param(params, "remote_src")? {
        return Err("remote_src is set".into());
    }
    if bool_param(params, "unsafe_writes")? {
        return Err("unsafe_writes is set".into());
    }
    if !matches!(params["local_follow"], Value::Null | Value::Bool(_)) {
        return Err("local_follow is not a boolean".into());
    }
    let text = |name| str_param(params, name);
    let dest = path_param(params, "dest")?;
    // `os.path.dirname` folds repeated slashes, which the messages would show.
    if dest.contains("//") {
        return Err("dest has repeated slashes".into());
    }
    let mode = Some(params["mode"].clone()).filter(|mode| !mode.is_null());
    if let Some(mode) = &mode {
        if *mode == "preserve" {
            return Err("mode is preserve".into());
        }
        if let Err(ModeError::Unsupported(why)) = parse_mode(mode, 0, false) {
            return Err(why.into());
        }
    }
    Ok(Request {
        src: path_param(params, "src")?,
        dest,
        // Read as the reference reads them: an empty one is none at all.
        original_basename: text("_original_basename")?.filter(|base| !base.is_empty()),
        backup: bool_param(params, "backup")?,
        force: bool_param(params, "force")?,
        follow: bool_param(params, "follow")?,
        validate: text("validate")?.filter(|validate| !validate.is_empty()),
        checksum: text("checksum")?.filter(|checksum| !checksum.is_empty()),
        owner: text("owner")?
            .map(|owner| owner_account(owner, clock))
            .transpose()?,
        group: text("group")?
            .map(|group| group_account(group, clock))
            .transpose()?,
        mode,
        umask: umask().map_err(|_| "the umask cannot be read")?,
    })
}
