// SPDX-License-Identifier: GPL-3.0-or-later
//! `vars_prompt`: the variables a play asks for when it starts, the way ansible-playbook asks.
//!
//! On a terminal each entry is a prompt; anywhere else the entry takes its default, with one
//! warning for the run. A name given with `-e` is never asked, hashed or defaulted.

use anyhow::{anyhow, bail};
use serde_json::{Map, Value};

use crate::executor::as_bool_value;
use crate::playbook::Play;
use crate::template::Templar;

/// How one prompt is put to the operator: the text, and whether what is typed stays hidden.
pub(crate) type Ask<'a> = &'a mut dyn FnMut(&str, bool) -> anyhow::Result<String>;

/// The variables a play's `vars_prompt` adds to its own, in the order they are asked.
///
/// `ask` is `None` when standard input is not a terminal; `warned` keeps the warning to once for
/// the run, as the reference's own display does. The entries are templated first over `vars`,
/// the play's variables under the extra ones, as the reference templates them before asking.
///
/// # Errors
/// An entry the reference would not take, a prompt that ended without an answer, or a hash that
/// could not be made.
pub(crate) fn answers(
    play: &Play,
    templar: &Templar,
    vars: &Map<String, Value>,
    extra: &Map<String, Value>,
    mut ask: Option<Ask<'_>>,
    warned: &mut bool,
) -> anyhow::Result<Vec<(String, Value)>> {
    if play.vars_prompt.is_empty() {
        return Ok(Vec::new());
    }
    // All the entries are templated before the first is asked, as the reference does.
    let entries = render_once(
        templar,
        &Value::Array(
            play.vars_prompt
                .iter()
                .cloned()
                .map(Value::Object)
                .collect(),
        ),
        vars,
    )
    .map_err(|err| anyhow!("Error processing keyword 'vars_prompt': {}", err.0))?;
    let mut out = Vec::new();
    for entry in entries.as_array().into_iter().flatten() {
        let entry = Entry::read(entry)?;
        if extra.contains_key(&entry.name) {
            continue;
        }
        let mut typed = None;
        if let Some(ask) = ask.as_mut() {
            typed = Some(entry.ask(ask)?);
        } else if !std::mem::replace(warned, true) {
            eprintln!("[WARNING]: Not prompting as we are not in interactive mode");
        }
        // `if not result and default is not None`: an empty answer takes the default too.
        if typed.as_deref().is_none_or(str::is_empty)
            && let Some(default) = &entry.default
        {
            typed = Some(default.clone());
        }
        let mut text = match (typed, &entry.encrypt) {
            (Some(text), _) => text,
            (None, Some(_)) => bail!("vars_prompt '{}' has no value to encrypt", entry.name),
            // What the reference's `to_text(None)` makes of an entry nobody answered.
            (None, None) => "None".to_string(),
        };
        if let Some(algorithm) = &entry.encrypt {
            text = hash(
                templar,
                &text,
                algorithm,
                entry.salt.as_deref(),
                entry.salt_size,
            )?;
        }
        let value = Value::String(text);
        // The reference marks what it asked for as safe to template unless `unsafe` says not.
        out.push((
            entry.name,
            if entry.unsafe_text {
                crate::yaml::unsafe_data(value)
            } else {
                value
            },
        ));
    }
    Ok(out)
}

/// The play with its prompted variables among its own, asked on the terminal when standard input
/// is one. They are play variables, as in the reference, so a task variable or a `set_fact`
/// still beats them and the extra variables beat them all.
///
/// # Errors
/// See [`answers`].
pub(crate) fn prompted<'a>(
    play: &'a Play,
    templar: &Templar,
    vars: &Map<String, Value>,
    extra: &Map<String, Value>,
    warned: &mut bool,
) -> anyhow::Result<std::borrow::Cow<'a, Play>> {
    use std::io::IsTerminal as _;
    if play.vars_prompt.is_empty() {
        return Ok(std::borrow::Cow::Borrowed(play));
    }
    let mut terminal = at_terminal;
    let ask: Option<Ask<'_>> = if std::io::stdin().is_terminal() {
        Some(&mut terminal)
    } else {
        None
    };
    let mut play = play.clone();
    for (name, value) in answers(&play, templar, vars, extra, ask, warned)? {
        play.vars.insert(name, value);
    }
    Ok(std::borrow::Cow::Owned(play))
}

/// Every string of `value` rendered once; a result that holds a template stays as it is.
fn render_once(
    templar: &Templar,
    value: &Value,
    vars: &Map<String, Value>,
) -> Result<Value, crate::template::TemplateError> {
    Ok(match value {
        Value::String(text) => templar.render_one_pass(text, vars)?,
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| render_once(templar, item, vars))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| Ok((key.clone(), render_once(templar, item, vars)?)))
                .collect::<Result<_, crate::template::TemplateError>>()?,
        ),
        other => other.clone(),
    })
}

/// One `vars_prompt` entry after templating, with the reference's defaults.
struct Entry {
    name: String,
    prompt: String,
    default: Option<String>,
    private: bool,
    confirm: bool,
    encrypt: Option<String>,
    salt_size: Option<i64>,
    salt: Option<String>,
    unsafe_text: bool,
}

impl Entry {
    fn read(entry: &Value) -> anyhow::Result<Self> {
        let get = |key: &str| entry.get(key).filter(|v| !v.is_null());
        let name = match get("name") {
            Some(Value::String(name)) => name.clone(),
            _ => bail!("vars_prompt 'name' must be a string"),
        };
        let flag = |key: &str, default: bool| match get(key) {
            None => Ok(default),
            Some(value) => as_bool_value(value)
                .ok_or_else(|| anyhow!("vars_prompt '{name}': '{key}' is not a boolean: {value}")),
        };
        let text = |key: &str| -> anyhow::Result<Option<String>> {
            get(key)
                .map(|value| match value {
                    Value::String(text) => Ok(text.clone()),
                    Value::Bool(b) => Ok(if *b { "True" } else { "False" }.to_string()),
                    Value::Number(n) => Ok(n.to_string()),
                    other => bail!(
                        "vars_prompt '{name}': '{key}' must be text, found {}",
                        crate::executor::python_type(other)
                    ),
                })
                .transpose()
        };
        let salt_size = match get("salt_size") {
            None => None,
            Some(value) => Some(
                value
                    .as_i64()
                    .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
                    .ok_or_else(|| anyhow!("vars_prompt '{name}': 'salt_size' is not a number"))?,
            ),
        };
        Ok(Self {
            prompt: text("prompt")?.unwrap_or_else(|| name.clone()),
            default: text("default")?,
            private: flag("private", true)?,
            confirm: flag("confirm", false)?,
            encrypt: text("encrypt")?.filter(|e| !e.is_empty()),
            salt_size,
            salt: text("salt")?,
            unsafe_text: flag("unsafe", false)?,
            name,
        })
    }

    /// The prompt line: `prompt [default]: `, `prompt: `, or `input for name: `.
    fn message(&self) -> String {
        match (self.prompt.as_str(), &self.default) {
            ("", _) => format!("input for {}: ", self.name),
            (prompt, Some(default)) => format!("{prompt} [{default}]: "),
            (prompt, None) => format!("{prompt}: "),
        }
    }

    /// Asks, twice when `confirm` is set, until the two answers agree.
    fn ask(&self, ask: Ask<'_>) -> anyhow::Result<String> {
        let message = self.message();
        loop {
            let first = ask(&message, self.private)?;
            if !self.confirm {
                return Ok(first);
            }
            if first == ask(&format!("confirm {message}"), self.private)? {
                return Ok(first);
            }
            println!("***** VALUES ENTERED DO NOT MATCH ****");
        }
    }
}

/// `text` hashed as the `password_hash` filter hashes it, which is ansible-core's own routine
/// and needs passlib in the controller's Python. The text travels as a variable and is never
/// templated: a password with braces in it stays what was typed.
fn hash(
    templar: &Templar,
    text: &str,
    algorithm: &str,
    salt: Option<&str>,
    salt_size: Option<i64>,
) -> anyhow::Result<String> {
    let vars = Map::from_iter([
        (
            "secret".to_string(),
            crate::yaml::unsafe_data(Value::String(text.to_string())),
        ),
        ("algorithm".to_string(), Value::from(algorithm)),
        ("salt".to_string(), salt.map_or(Value::Null, Value::from)),
        (
            "salt_size".to_string(),
            salt_size.map_or(Value::Null, Value::from),
        ),
    ]);
    match templar
        .render(
            "{{ secret | password_hash(algorithm, salt=salt, salt_size=salt_size) }}",
            &vars,
        )
        .map_err(|err| anyhow!("{}", err.0))?
    {
        Value::String(hashed) => Ok(hashed),
        other => bail!("the hash of a vars_prompt answer came back as {other}"),
    }
}

/// Reads one line from the terminal with the echo off, the prompt written to the terminal as
/// `getpass` does (stderr where there is none) so that a redirected stdout still carries only the
/// run's own output, and the line break after the answer with it. `None` when nothing was read.
///
/// The echo is turned off by the shell that reads the line rather than from here, and that
/// shell arms its `trap` before touching the terminal: a Ctrl-C, a Ctrl-\ or a `SIGTERM` while
/// the operator is typing reaches the whole foreground process group, so the shell restores the
/// echo on its way out even though this process is dying too. `QUIT` is in the list because
/// Ctrl-\ is a key an operator reaches for at a prompt that seems stuck, and it would otherwise
/// kill the shell with the echo still off, leaving a terminal that types nothing back. Doing
/// this from Rust would need a `Drop` that a signal never runs, and a terminal left with the
/// echo off is a poor parting gift. Where stdin is not a terminal, `stty` fails, its complaint
/// is dropped and the line is read as it comes.
#[cfg(unix)]
pub(crate) fn hidden_line(prompt: &str) -> anyhow::Result<Option<Vec<u8>>> {
    use std::process::Stdio;
    const READ_LINE: &str = "trap 'stty echo 2>/dev/null' EXIT INT QUIT TERM HUP\n\
         stty -echo 2>/dev/null\n\
         if ( : >/dev/tty ) 2>/dev/null; then t=/dev/tty; else t=/dev/stderr; fi\n\
         printf %s \"$1\" >\"$t\"\n\
         IFS= read -r password\n\
         status=$?\n\
         printf '\\n' >\"$t\"\n\
         [ $status = 0 ] || exit 1\n\
         printf %s \"$password\"\n";
    // The shell prints the prompt itself, once the echo is off: a person (or a test) answering
    // as soon as it shows would otherwise see the answer typed back.
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(READ_LINE)
        .arg("sh")
        .arg(prompt)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| anyhow!("reading a hidden line: {e}"))?;
    Ok(out.status.success().then_some(out.stdout))
}

#[cfg(not(unix))]
pub(crate) fn hidden_line(prompt: &str) -> anyhow::Result<Option<Vec<u8>>> {
    eprint!("{prompt}");
    Ok(visible_line()?.map(String::into_bytes))
}

/// One line from standard input, without its line ending; `None` at the end of the input.
fn visible_line() -> anyhow::Result<Option<String>> {
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
}

/// The prompt as the reference puts it on a terminal: hidden for a private entry, otherwise on
/// stdout in front of a visible line.
pub(crate) fn at_terminal(message: &str, private: bool) -> anyhow::Result<String> {
    use std::io::Write as _;
    let line = if private {
        hidden_line(message)?
            .map(|bytes| String::from_utf8(bytes).map_err(|_| anyhow!("the answer is not UTF-8")))
            .transpose()?
    } else {
        print!("{message}");
        std::io::stdout().flush()?;
        visible_line()?
    };
    line.ok_or_else(|| {
        anyhow!(
            "no answer was given to '{}'",
            message.trim_end_matches([' ', ':'])
        )
    })
}

#[cfg(test)]
mod tests;
