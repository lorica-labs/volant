// SPDX-License-Identifier: GPL-3.0-or-later
//! Text filters: paths, `split`, base64, `comment`, `quote`, UUIDs and the IP address wrapper.

use std::fmt::Write;
use std::net::Ipv6Addr;
use std::str::FromStr;

use minijinja::value::{Kwargs, ValueKind};
use minijinja::{Error, Value};

use volant_protocol::encoding::{b64_decode, b64_encode, sha1_hex};

use super::{invalid, json};

pub(super) fn basename(value: Value) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("basename requires a string"))?;
    Ok(Value::from(text.rsplit('/').next().unwrap_or("")))
}

pub(super) fn dirname(value: Value) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("dirname requires a string"))?;
    Ok(Value::from(match text.rfind('/') {
        Some(0) => "/",
        Some(i) => &text[..i],
        None => "",
    }))
}

/// Python's `str.split`: no separator splits on runs of whitespace and drops empty fields.
pub(super) fn split(
    value: Value,
    sep: Option<String>,
    maxsplit: Option<i64>,
) -> Result<Value, Error> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("split requires a string"))?;
    let limit = maxsplit.filter(|n| *n >= 0).map(|n| n as usize + 1);
    let parts: Vec<&str> = match (sep, limit) {
        (None, None) => text.split_whitespace().collect(),
        (None, Some(n)) => text
            .trim_start()
            .splitn(n, char::is_whitespace)
            .filter(|s| !s.is_empty())
            .collect(),
        (Some(sep), None) => text.split(sep.as_str()).collect(),
        (Some(sep), Some(n)) => text.splitn(n, sep.as_str()).collect(),
    };
    Ok(Value::from(
        parts.iter().map(|s| Value::from(*s)).collect::<Vec<_>>(),
    ))
}

/// A value as Python's `to_text` would hand it to a filter: a string as it is, anything else
/// printed.
pub(super) fn text_of(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_string)
}

/// `encoding` and `urlsafe`, the two options `b64decode` and `b64encode` share. Only UTF-8 is
/// an encoding here: any other is refused by name rather than read as UTF-8.
pub(super) fn b64_options(
    filter: &str,
    encoding: Option<String>,
    kwargs: &Kwargs,
) -> Result<bool, Error> {
    let encoding = kwargs
        .get::<Option<String>>("encoding")?
        .or(encoding)
        .unwrap_or_else(|| "utf-8".to_string());
    let urlsafe = kwargs.get::<Option<bool>>("urlsafe")?.unwrap_or(false);
    kwargs.assert_all_used()?;
    if !encoding.eq_ignore_ascii_case("utf-8") && !encoding.eq_ignore_ascii_case("utf8") {
        return Err(invalid(format!(
            "{filter}: the encoding '{encoding}' is not supported, only utf-8 is"
        )));
    }
    Ok(urlsafe)
}

/// `base64.b64decode`, then the bytes read as UTF-8 text; `urlsafe` reads `-` and `_` for `+`
/// and `/`. Called without `validate`, Python's decoder skips whitespace, so this does too.
pub(super) fn b64decode(
    value: Value,
    encoding: Option<String>,
    kwargs: Kwargs,
) -> Result<String, Error> {
    let urlsafe = b64_options("b64decode", encoding, &kwargs)?;
    let mut text = text_of(&value);
    text.retain(|c| !c.is_ascii_whitespace());
    if urlsafe {
        text = text.replace('-', "+").replace('_', "/");
    }
    let bytes = b64_decode(&text).map_err(|e| invalid(format!("b64decode: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| invalid(format!("b64decode: the decoded bytes are not UTF-8: {e}")))
}

pub(super) fn b64encode(
    value: Value,
    encoding: Option<String>,
    kwargs: Kwargs,
) -> Result<String, Error> {
    let urlsafe = b64_options("b64encode", encoding, &kwargs)?;
    let out = b64_encode(text_of(&value).as_bytes());
    Ok(if urlsafe {
        out.replace('+', "-").replace('/', "_")
    } else {
        out
    })
}

/// The port of `comment` in ansible-core's `plugins/filter/core.py`, parameter for parameter:
/// each style gives a decoration and, for the block styles, a beginning and an end; any of
/// them, and the prefix and postfix lines built from the decoration, can be set by keyword.
pub(super) fn comment(
    text: String,
    style: Option<String>,
    kwargs: Kwargs,
) -> Result<String, Error> {
    let style = kwargs
        .get::<Option<String>>("style")?
        .or(style)
        .unwrap_or_else(|| "plain".to_string());
    let (beginning, decoration, end) = match style.as_str() {
        "plain" => ("", "# ", ""),
        "erlang" => ("", "% ", ""),
        "c" => ("", "// ", ""),
        "cblock" => ("/*", " * ", " */"),
        "xml" => ("<!--", " - ", "-->"),
        _ => return Err(invalid(format!("Invalid style '{style}'."))),
    };
    let get = |key: &str, default: &str| -> Result<String, Error> {
        Ok(kwargs
            .get::<Option<String>>(key)?
            .unwrap_or_else(|| default.to_string()))
    };
    let decoration = get("decoration", decoration)?;
    let prepostfix = decoration.trim_end().to_string();
    let newline = get("newline", "\n")?;
    let beginning = get("beginning", beginning)?;
    let end = get("end", end)?;
    let prefix = get("prefix", &prepostfix)?;
    let postfix = get("postfix", &prepostfix)?;
    let count = |key: &str| -> Result<usize, Error> {
        Ok(kwargs.get::<Option<i64>>(key)?.unwrap_or(1).max(0) as usize)
    };
    let prefix_count = count("prefix_count")?;
    let postfix_count = count("postfix_count")?;
    kwargs.assert_all_used()?;

    let mut out = String::new();
    if !beginning.is_empty() {
        out.push_str(&beginning);
        out.push_str(&newline);
    }
    if !prefix.is_empty() {
        let line = if prefix == newline {
            newline.clone()
        } else {
            format!("{prefix}{newline}")
        };
        out.push_str(&line.repeat(prefix_count));
    }
    // Each line gets the decoration, and a line that is nothing but the decoration loses the
    // decoration's trailing space.
    let body = format!(
        "{decoration}{}",
        text.replace(&newline, &format!("{newline}{decoration}"))
    );
    out.push_str(&body.replace(
        &format!("{decoration}{newline}"),
        &format!("{}{newline}", decoration.trim_end()),
    ));
    for _ in 0..postfix_count {
        out.push_str(&newline);
        out.push_str(&postfix);
    }
    if !end.is_empty() {
        out.push_str(&newline);
        out.push_str(&end);
    }
    Ok(out)
}

/// `to_uuid`'s default namespace, ansible-core's own.
pub(super) const UUID_NAMESPACE_ANSIBLE: &str = "361E6D51-FAEC-444A-9079-341386DA8E2E";

pub(super) fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex digits"))
        .collect()
}

/// `uuid.uuid5(namespace, text)`: the SHA-1 of the namespace's sixteen bytes and the text, its
/// first sixteen bytes, version 5 and the RFC 4122 variant. The namespace is read the way
/// `uuid.UUID` reads one: braces, `urn:` and `uuid:` and hyphens dropped, 32 hex digits left.
pub(super) fn to_uuid(
    value: Value,
    namespace: Option<String>,
    kwargs: Kwargs,
) -> Result<String, Error> {
    let namespace = kwargs
        .get::<Option<String>>("namespace")?
        .or(namespace)
        .unwrap_or_else(|| UUID_NAMESPACE_ANSIBLE.to_string());
    kwargs.assert_all_used()?;
    let hex = namespace.replace("urn:", "").replace("uuid:", "");
    let hex = hex.trim_matches(['{', '}']).replace('-', "");
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid(format!(
            "Invalid value '{namespace}' for 'namespace': badly formed hexadecimal UUID string"
        )));
    }
    let mut data = hex_bytes(&hex);
    data.extend_from_slice(text_of(&value).as_bytes());
    let mut id = hex_bytes(&sha1_hex(&data));
    id.truncate(16);
    id[6] = (id[6] & 0x0f) | 0x50;
    id[8] = (id[8] & 0x3f) | 0x80;
    let h = id.iter().fold(String::new(), |mut h, b| {
        let _ = write!(h, "{b:02x}");
        h
    });
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    ))
}

/// `shlex.quote`, with `None` read as the empty string: a word made only of `[A-Za-z0-9_]` and
/// `@%+=:,./-` is left alone, anything else is single-quoted with each `'` written `'"'"'`.
pub(super) fn quote(value: Value) -> String {
    let s = if value.is_none() {
        String::new()
    } else {
        text_of(&value)
    };
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        return s;
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// `ansible.utils.ipwrap`, ported from the collection's own filter (`plugins/filter/ipwrap.py`,
/// read on the dev machine): every IPv6 address, with or without a valid prefix (0 to 128), is
/// bracketed; a string that is not one - a hostname, an IPv4 address or subnet, an out-of-range
/// or non-numeric prefix, the empty string - is left as it is, and a list is wrapped element by
/// element. Measured on ansible-core 2.19.12 (A9):
/// `['192.0.2.1', '2001:db8::1', 'example.org', '192.0.2.0/24', '2001:db8::/64', '']` becomes
/// `["192.0.2.1", "[2001:db8::1]", "example.org", "192.0.2.0/24", "[2001:db8::]/64", ""]`, and a
/// bare integer fails with the reference's own wording, `format`'s second placeholder included:
/// `The filter plugin 'ansible.utils.ipwrap' failed: Unrecognized type <<class 'int'>> for
/// ipwrap filter <value>`.
pub(super) fn ipwrap(value: Value) -> Result<Value, Error> {
    match value.kind() {
        ValueKind::String | ValueKind::Bool => Ok(ipwrap_scalar(&value)),
        ValueKind::Seq => Ok(Value::from(
            value
                .try_iter()?
                .map(|item| ipwrap_scalar(&item))
                .collect::<Vec<_>>(),
        )),
        _ => Err(invalid(format!(
            "The filter plugin 'ansible.utils.ipwrap' failed: Unrecognized type <<class '{}'>> for ipwrap filter <value>",
            crate::template::type_name(&json(&value))
        ))),
    }
}

/// One value through the filter. The address is parsed on the part before `/`, which is what
/// `std::net::Ipv6Addr::from_str` gets: on a hit, that part is bracketed and the `/prefix` kept
/// outside the brackets. On a miss the value comes back unchanged - the reference's own `except
/// Exception: return value` - and that covers more than "not an IPv6 address at all": `netaddr`
/// builds an `IPNetwork` out of the address and the prefix together, so a prefix past 128 (an
/// IPv6 address has no more bits) or one that is not a plain integer also raises there and is
/// left untouched here, checked against the real filter on the dev machine:
/// `'2001:db8::/129' | ansible.utils.ipwrap` and `'2001:db8::/abc' | ansible.utils.ipwrap` both
/// answer their own input, not a bracketed one.
pub(super) fn ipwrap_scalar(value: &Value) -> Value {
    let Some(text) = value.as_str() else {
        return value.clone();
    };
    let (address, prefix) = match text.split_once('/') {
        Some((address, prefix)) => (address, Some(prefix)),
        None => (text, None),
    };
    if Ipv6Addr::from_str(address).is_err() {
        return value.clone();
    }
    let Some(prefix) = prefix else {
        return Value::from(format!("[{address}]"));
    };
    match prefix.parse::<u8>() {
        Ok(bits) if bits <= 128 => Value::from(format!("[{address}]/{prefix}")),
        _ => value.clone(),
    }
}
