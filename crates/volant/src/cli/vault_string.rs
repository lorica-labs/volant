// SPDX-License-Identifier: GPL-3.0-or-later
//! `vault encrypt_string`: each string as a `!vault |` block to paste into a vars file.

use std::io::{IsTerminal as _, Read as _, Write as _};

use super::vault::{Failure, bad_option, write_data};
use crate::vault;

/// What the command line asked `encrypt_string` for.
pub(super) struct Job {
    pub(super) prompt: bool,
    pub(super) show_input: bool,
    pub(super) names: Vec<String>,
    pub(super) stdin_name: Option<String>,
    pub(super) strings: Vec<String>,
}

impl Job {
    /// The text comes from standard input: a `-`, a `--stdin-name`, or nothing else to encrypt.
    fn reads_stdin(&self) -> bool {
        self.strings.iter().any(|s| s == "-")
            || self.stdin_name.is_some()
            || (self.strings.is_empty() && !self.prompt)
    }

    /// The check the reference makes before it looks at any secret.
    pub(super) fn check(&self) -> Result<(), Failure> {
        if self.prompt && self.reads_stdin() {
            return Err(bad_option(
                "The --prompt option is not supported if also reading input from stdin",
            ));
        }
        Ok(())
    }
}

/// An exception the reference does not word: its first line, and exit code 250.
fn exception(message: &str) -> Failure {
    Failure {
        message: format!("Unexpected Exception, this is probably a bug: {message}"),
        code: 250,
    }
}

/// One line of standard input, read a byte at a time so that nothing past it is taken from a
/// later reader (the hidden prompt reads the same descriptor). `None` at the end of the input.
fn read_line() -> Option<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        #[cfg(unix)]
        // SAFETY: `byte` is valid for one byte.
        let read = unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) };
        #[cfg(not(unix))]
        let read = std::io::stdin()
            .lock()
            .read(&mut byte)
            .map_or(-1, |n| isize::try_from(n).unwrap_or(-1));
        if read <= 0 {
            return (!line.is_empty()).then_some(line);
        }
        if byte[0] == b'\n' {
            return Some(line);
        }
        line.push(byte[0]);
    }
}

/// What Python's `str` decoding says about text that is not UTF-8.
fn not_utf8(error: &std::str::Utf8Error, bytes: &[u8]) -> Failure {
    let at = error.valid_up_to();
    let reason = match error.error_len() {
        None => "unexpected end of data",
        Some(_) if !(0xC2..=0xF4).contains(&bytes[at]) => "invalid start byte",
        Some(_) => "invalid continuation byte",
    };
    exception(&format!(
        "'utf-8' codec can't decode byte 0x{:02x} in position {at}: {reason}",
        bytes[at]
    ))
}

type Item = (Vec<u8>, &'static str, Option<String>);

/// The answers to `--prompt`: the variable name, echoed, then the string.
fn prompted(show_input: bool) -> Result<Item, Failure> {
    let eof = || exception("EOF when reading a line");
    let ask = |text: &str| {
        print!("{text}");
        let _ = std::io::stdout().flush();
        read_line()
    };
    let name = ask("Variable name (enter for no name): ").ok_or_else(eof)?;
    let typed = if show_input {
        ask("String to encrypt:").ok_or_else(eof)?
    } else {
        crate::prompt::hidden_line("String to encrypt (hidden): ")
            .map_err(|e| exception(&e.to_string()))?
            .ok_or_else(eof)?
    };
    if typed.is_empty() {
        return Err(bad_option(
            "The plaintext provided from the prompt was empty, not encrypting",
        ));
    }
    let name = String::from_utf8_lossy(&name).into_owned();
    Ok((
        typed,
        "the interactive prompt",
        Some(name).filter(|n| !n.is_empty()),
    ))
}

/// Standard input to its end, as one string.
fn from_stdin(name: Option<String>, terminal: bool) -> Result<Item, Failure> {
    if terminal {
        eprintln!(
            "Reading plaintext input from stdin. (ctrl-d to end input, twice if your content does not already have a newline)"
        );
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .read_to_end(&mut bytes)
        .map_err(|e| exception(&e.to_string()))?;
    if let Err(error) = std::str::from_utf8(&bytes) {
        return Err(not_utf8(&error, &bytes));
    }
    if bytes.is_empty() {
        return Err(bad_option("stdin was empty, not encrypting"));
    }
    if terminal && !bytes.ends_with(b"\n") {
        println!();
    }
    Ok((bytes, "stdin", name))
}

/// The strings of the command line, paired with the names in order. Names left over are told
/// about; strings left over have none.
fn from_arguments(job: &Job) -> Result<Vec<Item>, Failure> {
    let strings: Vec<&String> = job.strings.iter().filter(|s| *s != "-").collect();
    let paired = job.names.len().min(strings.len());
    if !job.names.is_empty() && strings.len() > paired {
        // The text is vague on purpose, so the plaintext never shows in the output.
        eprintln!("The number of --name options do not match the number of args.");
        eprintln!(
            "The last named variable will be \"{}\". The rest will not have names.",
            job.names[job.names.len() - 1]
        );
    }
    strings
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            if text.is_empty() {
                return Err(bad_option(
                    "The plaintext provided from the command line args was empty, not encrypting",
                ));
            }
            let name = (i < paired).then(|| job.names[i].clone());
            Ok((text.as_bytes().to_vec(), "the command line args", name))
        })
        .collect()
}

/// `encrypt_string`: the blocks to standard output, or to `output`.
pub(super) fn run(
    job: &Job,
    label: &str,
    secret: &[u8],
    output: Option<&str>,
) -> Result<(), Failure> {
    let terminal = std::io::stdout().is_terminal();
    let mut items = Vec::new();
    if job.prompt {
        items.push(prompted(job.show_input)?);
    }
    if job.reads_stdin() {
        items.push(from_stdin(job.stdin_name.clone(), terminal)?);
    }
    items.extend(from_arguments(job)?);
    let several = items.len() > 1;
    let mut blocks = Vec::new();
    for (index, (plain, source, name)) in items.iter().enumerate() {
        let text = vault::encrypt(plain, secret, Some(label)).map_err(|e| bad_option(e.0))?;
        let mut block = name
            .as_ref()
            .map_or_else(|| "!vault |".to_string(), |n| format!("{n}: !vault |"));
        for line in text.lines() {
            block.push_str("\n          ");
            block.push_str(line);
        }
        blocks.push(block);
        if several {
            let number = index + 1;
            // The stray parenthesis of the unnamed form is the reference's.
            match name {
                Some(n) => eprintln!(
                    "# The encrypted version of variable (\"{n}\", the string #{number} from {source})."
                ),
                None => {
                    eprintln!("# The encrypted version of the string #{number} from {source}.)");
                }
            }
        }
    }
    let mut text = blocks.join("\n");
    text.push('\n');
    // On a terminal the reference's message comes first: its output is buffered until it exits.
    if terminal {
        eprintln!("Encryption successful");
    }
    write_data(text.as_bytes(), output.unwrap_or("-"), true)
}
