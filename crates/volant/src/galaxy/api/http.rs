// SPDX-License-Identifier: GPL-3.0-or-later
//! The URLs and the HTTP error messages of ansible-galaxy 2.19.12 (`_urljoin`, `GalaxyError`,
//! `get_cache_id`).

use serde_json::Value;

use crate::galaxy::Failure;

/// `GalaxyError`'s message for an HTTP error answer.
pub(crate) fn galaxy_error(
    context: &str,
    url: &str,
    status: ureq::http::StatusCode,
    body: &str,
) -> String {
    let code = status.as_u16();
    let reason = status.canonical_reason().unwrap_or_default();
    let info: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let parts: Vec<&str> = url.split('/').collect();
    if parts.contains(&"v2") {
        let message = info["message"].as_str().unwrap_or(reason);
        let galaxy_code = info["code"].as_str().unwrap_or("Unknown");
        format!("{context} (HTTP Code: {code}, Message: {message} Code: {galaxy_code})")
    } else if parts.contains(&"v3") {
        let errors = match info["errors"].as_array() {
            Some(list) if !list.is_empty() => list.clone(),
            _ => vec![Value::Null],
        };
        let lines: Vec<String> = errors
            .iter()
            .map(|e| {
                let message = e["detail"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .or_else(|| e["title"].as_str().filter(|s| !s.is_empty()))
                    .unwrap_or(reason);
                let galaxy_code = e["code"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Unknown");
                format!("(HTTP Code: {code}, Message: {message} Code: {galaxy_code})")
            })
            .collect();
        format!("{context} {}", lines.join(", "))
    } else {
        let message = info["default"].as_str().unwrap_or(reason);
        format!("{context} (HTTP Code: {code}, Message: {message})")
    }
}

/// `_urljoin`: each part without its slashes, joined by one; a last part of `/` leaves the
/// trailing slash, and one of `/?q` the query after it.
pub(super) fn join(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| p.trim_matches('/'))
        .collect::<Vec<_>>()
        .join("/")
}

/// `urljoin(api_server, link)` for a link that is a full URL or a path from the host's root.
pub(super) fn absolute(api_server: &str, link: &str) -> Result<String, Failure> {
    if link.contains("://") {
        return Ok(link.to_string());
    }
    if !link.starts_with('/') {
        return Err(Failure::error(format!(
            "Invalid non absolute pagination link: {link}"
        )));
    }
    Ok(format!("{}{link}", origin(api_server)))
}

/// `scheme://host[:port]` of a URL.
pub(super) fn origin(url: &str) -> &str {
    url.find("://")
        .and_then(|at| url[at + 3..].find('/').map(|slash| &url[..at + 3 + slash]))
        .unwrap_or(url)
}

/// The path of a URL, without its query.
pub(super) fn path_of(url: &str) -> &str {
    let rest = &url[origin(url).len()..];
    rest.split('?').next().unwrap_or(rest)
}

/// `get_cache_id`: `host:port`, the port empty when the URL has none.
pub(super) fn cache_id(url: &str) -> String {
    let host_port = origin(url).split("://").nth(1).unwrap_or_default();
    let host_port = host_port.rsplit('@').next().unwrap_or(host_port);
    match host_port.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => format!("{host}:{port}"),
        _ => format!("{host_port}:"),
    }
}
