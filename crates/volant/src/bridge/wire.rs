// SPDX-License-Identifier: GPL-3.0-or-later
//! What crosses between the controller and a helper: the asks, the replies, the kinds of plugin and the errors.

use std::fmt;
use std::path::PathBuf;

use serde_json::{Value, json};

/// What an action plugin asks of the host while it runs, done by the controller over the host's
/// own link.
#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    /// Run a module the controller's way. `files` pairs an argument with the controller file it
    /// stages.
    Module {
        name: String,
        args: Value,
        files: Vec<(String, PathBuf)>,
    },
    /// `Connection.exec_command`: a command line, already wrapped in its shell.
    Command {
        cmd: String,
        stdin: Option<Vec<u8>>,
        sudoable: bool,
    },
    /// `Connection.put_file`: a controller file written at a path of the host.
    PutFile {
        local: PathBuf,
        remote: String,
        mode: Option<u32>,
    },
    /// `Connection.fetch_file`: a file of the host written at a controller path.
    FetchFile { remote: String, local: PathBuf },
    /// The action's temporary directory on the host.
    Tmp(TmpOp),
    /// The variables of another host, which `task_vars` leaves out until a plugin reads them.
    HostVar { host: String },
}

/// What happens to an action's temporary directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmpOp {
    /// Make one, as `remote_user` when the plugin names one; the reply is its path.
    Make {
        remote_user: Option<String>,
    },
    Remove {
        path: String,
    },
}

/// The controller's answer to an [`Ask`]. For a `Command` the value is `{rc, stdout, stderr}`,
/// for a `Module` its result, for `Tmp(Make)` the path, for a `HostVar` the variables or `null`
/// when there is no such host.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Value(Value),
    Error(String),
}

impl Ask {
    pub(super) fn read(ask: &Value) -> Result<Ask, String> {
        let text = |key: &str| {
            ask.get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("{key} is not text"))
        };
        let maybe_text = |key: &str| match ask.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(format!("{key} is not text")),
        };
        Ok(match ask.get("op").and_then(Value::as_str) {
            Some("module") => Ask::Module {
                name: text("name")?,
                args: ask
                    .get("args")
                    .filter(|args| args.is_object())
                    .cloned()
                    .ok_or("the module arguments are not a mapping")?,
                files: match ask.get("files") {
                    None => Vec::new(),
                    Some(Value::Array(files)) => files
                        .iter()
                        .map(|pair| match pair.as_array().map(Vec::as_slice) {
                            Some([Value::String(arg), Value::String(path)]) => {
                                Ok((arg.clone(), PathBuf::from(path)))
                            }
                            _ => Err("a staged file is not an argument and a path".to_string()),
                        })
                        .collect::<Result<_, _>>()?,
                    Some(_) => return Err("the staged files are not a list".into()),
                },
            },
            Some("command") => Ask::Command {
                cmd: text("cmd")?,
                stdin: maybe_text("in_data")?.map(String::into_bytes),
                sudoable: ask
                    .get("sudoable")
                    .and_then(Value::as_bool)
                    .ok_or("sudoable is not a boolean")?,
            },
            Some("put_file") => Ask::PutFile {
                local: text("local")?.into(),
                remote: text("remote")?,
                mode: match ask.get("mode") {
                    None | Some(Value::Null) => None,
                    Some(mode) => Some(
                        mode.as_u64()
                            .and_then(|mode| u32::try_from(mode).ok())
                            .filter(|mode| *mode <= 0o7777)
                            .ok_or("the mode is not a file mode")?,
                    ),
                },
            },
            Some("fetch_file") => Ask::FetchFile {
                remote: text("remote")?,
                local: text("local")?.into(),
            },
            Some("tmp") => Ask::Tmp(match ask.get("action").and_then(Value::as_str) {
                Some("make") => TmpOp::Make {
                    remote_user: maybe_text("remote_user")?,
                },
                Some("remove") => TmpOp::Remove {
                    path: text("path")?,
                },
                other => return Err(format!("the temporary directory action {other:?}")),
            }),
            Some("hostvar") => Ask::HostVar {
                host: text("host")?,
            },
            other => return Err(format!("the operation {other:?}")),
        })
    }
}

impl Reply {
    pub(super) fn to_json(&self) -> Value {
        match self {
            Reply::Value(value) => json!({ "value": value }),
            Reply::Error(text) => json!({ "error": text }),
        }
    }
}

/// What a bridged call runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginKind {
    Filter,
    Test,
    Lookup,
}

impl PluginKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PluginKind::Filter => "filter",
            PluginKind::Test => "test",
            PluginKind::Lookup => "lookup",
        }
    }
}

/// Why a bridged call gave no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    /// The plugin ran and failed; the text is ansible-core's, `The filter plugin 'x' failed: …`.
    Plugin(String),
    /// ansible-core knows no plugin of that name; the text is its sentence.
    NotFound(String),
    /// The helper could not start; the text is why.
    Start(String),
    /// The helper died or broke the protocol.
    Helper(String),
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BridgeError::Start(why) => write!(f, "the Python helper could not start: {why}"),
            BridgeError::Plugin(text) | BridgeError::NotFound(text) | BridgeError::Helper(text) => {
                f.write_str(text)
            }
        }
    }
}

impl std::error::Error for BridgeError {}
