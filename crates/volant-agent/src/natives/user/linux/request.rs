// SPDX-License-Identifier: GPL-3.0-or-later

//! The options `user` reads, and the request read from them.

use serde_json::{Map, Value};

use crate::natives::common::{ArgSpec, bool_param, check_names, path_param, str_param};
use crate::natives::group::{id_param, outside_subset, plain_name, present};

const fn arg(name: &'static str, default: fn() -> Value) -> ArgSpec {
    ArgSpec {
        name,
        aliases: &[],
        default,
    }
}

/// `user`'s `argument_spec` in ansible-core 2.19.12.
pub(super) const SPEC: &[ArgSpec] = &[
    arg("state", || Value::from("present")),
    ArgSpec {
        name: "name",
        aliases: &["user"],
        default: || Value::Null,
    },
    arg("uid", || Value::Null),
    arg("non_unique", || Value::Bool(false)),
    arg("group", || Value::Null),
    arg("groups", || Value::Null),
    arg("comment", || Value::Null),
    arg("home", || Value::Null),
    arg("shell", || Value::Null),
    arg("password", || Value::Null),
    arg("login_class", || Value::Null),
    arg("password_expire_max", || Value::Null),
    arg("password_expire_min", || Value::Null),
    arg("password_expire_warn", || Value::Null),
    arg("hidden", || Value::Null),
    arg("seuser", || Value::Null),
    arg("force", || Value::Bool(false)),
    arg("remove", || Value::Bool(false)),
    ArgSpec {
        name: "create_home",
        aliases: &["createhome"],
        default: || Value::Bool(true),
    },
    arg("skeleton", || Value::Null),
    arg("system", || Value::Bool(false)),
    arg("move_home", || Value::Bool(false)),
    arg("append", || Value::Bool(false)),
    arg("generate_ssh_key", || Value::Null),
    arg("ssh_key_bits", || Value::from(0)),
    arg("ssh_key_type", || Value::from("rsa")),
    arg("ssh_key_file", || Value::Null),
    // `'ansible-generated on %s' % socket.gethostname()`.
    arg("ssh_key_comment", || {
        Value::from(format!("ansible-generated on {}", hostname()))
    }),
    arg("ssh_key_passphrase", || Value::Null),
    arg("update_password", || Value::from("always")),
    arg("expires", || Value::Null),
    arg("password_lock", || Value::Null),
    arg("local", || Value::Null),
    arg("profile", || Value::Null),
    arg("authorization", || Value::Null),
    arg("role", || Value::Null),
    arg("umask", || Value::Null),
    arg("password_expire_account_disable", || Value::Null),
    arg("uid_min", || Value::Null),
    arg("uid_max", || Value::Null),
];

/// The arguments the native answers for; any other one given hands the task back.
const SUBSET: &[&str] = &[
    "name",
    "user",
    "state",
    "uid",
    "group",
    "groups",
    "append",
    "comment",
    "home",
    "shell",
    "system",
    "create_home",
    "createhome",
];

/// `socket.gethostname()`.
pub(super) fn hostname() -> String {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

pub(super) struct Request<'a> {
    pub(super) name: &'a str,
    pub(super) present: bool,
    pub(super) uid: Option<u32>,
    pub(super) group: Option<&'a str>,
    /// `','.join(groups)`, as the module keeps it and answers it.
    pub(super) groups: Option<String>,
    pub(super) append: bool,
    pub(super) comment: Option<&'a str>,
    pub(super) home: Option<&'a str>,
    pub(super) shell: Option<&'a str>,
    pub(super) system: bool,
    pub(super) create_home: bool,
}

pub(super) fn request<'a>(
    args: &Map<String, Value>,
    params: &'a Map<String, Value>,
) -> Result<Request<'a>, String> {
    check_names(SPEC, args)?;
    outside_subset(args, SUBSET)?;
    let name = str_param(params, "name")?
        .filter(|name| plain_name(name))
        .ok_or("the module reads this account name differently")?;
    // `type='list', elements='str'`, a string already split by `answer`: the module also
    // converts numbers.
    let groups = match &params["groups"] {
        Value::Null => None,
        Value::Array(items) => Some(
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .ok_or("groups holds a value that is not a string")
                })
                .collect::<Result<Vec<&str>, _>>()?
                .join(","),
        ),
        _ => return Err("groups is not a list".into()),
    };
    let path = |key: &str| match &params[key] {
        Value::Null => Ok(None),
        _ => path_param(params, key).map(Some),
    };
    let append = bool_param(params, "append")?;
    if append && groups.is_none() {
        return Err("append without groups makes the module warn".into());
    }
    Ok(Request {
        name,
        present: present(params)?,
        uid: id_param(params, "uid")?,
        group: str_param(params, "group")?,
        groups,
        append,
        comment: str_param(params, "comment")?,
        home: path("home")?,
        shell: path("shell")?,
        system: bool_param(params, "system")?,
        create_home: bool_param(params, "create_home")?,
    })
}
