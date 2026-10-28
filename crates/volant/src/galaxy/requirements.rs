// SPDX-License-Identifier: GPL-3.0-or-later
//! A requirements file: `collections:` and `roles:`, or the old bare list of roles, read as
//! ansible-galaxy 2.19.12 reads it (`GalaxyCLI._parse_requirements_file`,
//! `RoleRequirement.role_yaml_parse`, `Requirement.from_requirement_dict`).

use std::path::Path;

use serde_json::Value;

use super::Failure;

/// What a requirements file asks for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Requirements {
    pub collections: Vec<Collection>,
    pub roles: Vec<Role>,
}

/// Where a collection comes from, the `type` of its entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Galaxy,
    File,
    Dir,
    Git,
    Url,
    Subdirs,
}

/// One collection entry. A `version` is kept as text, `*` when the entry has none: the resolver
/// reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    pub name: Option<String>,
    pub version: String,
    pub source: Option<String>,
    pub kind: Kind,
    pub signatures: Vec<String>,
}

/// One role entry, after the reference's rewriting of `src`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Role {
    pub name: Option<String>,
    pub src: Option<String>,
    pub scm: Option<String>,
    pub version: Option<String>,
}

/// Reads `path`, which exists. `old_format` accepts the bare list of roles, as `role install`
/// does and `collection install` does not.
pub fn read(path: &Path, old_format: bool, verbose: u8) -> Result<Requirements, Failure> {
    let shown = path.display();
    let parsed = load(path).map_err(|e| {
        Failure::error(format!(
            "Failed to parse the requirements yml at '{shown}' with the following error:\n{e}"
        ))
    })?;
    let mut out = Requirements::default();
    match parsed {
        Value::Null => {
            return Err(Failure::error(format!(
                "No requirements found in file '{shown}'"
            )));
        }
        Value::Array(entries) => {
            if !old_format {
                return Err(Failure::error(
                    "Expecting requirements file to be a dict with the key 'collections' that contains a list of collections to install",
                ));
            }
            for entry in &entries {
                out.roles.extend(role_entry(entry, path, verbose)?);
            }
        }
        Value::Object(map) => {
            let extra: Vec<&str> = map
                .keys()
                .map(String::as_str)
                .filter(|k| *k != "roles" && *k != "collections")
                .collect();
            if !extra.is_empty() {
                return Err(Failure::error(format!(
                    "Expecting only 'roles' and/or 'collections' as base keys in the requirements file. Found: {}",
                    extra.join(", ")
                )));
            }
            for entry in items(map.get("roles")) {
                out.roles.extend(role_entry(&entry, path, verbose)?);
            }
            for entry in items(map.get("collections")) {
                out.collections.push(collection(&entry)?);
            }
        }
        other => {
            return Err(Failure::error(format!(
                "Expecting requirements yaml to be a list or dictionary but got {}",
                python_type(&other)
            )));
        }
    }
    Ok(out)
}

fn load(path: &Path) -> anyhow::Result<Value> {
    let text = std::fs::read_to_string(path)?;
    let docs = crate::yaml::load(&text, &path.display().to_string())?;
    docs.first()
        .map_or(Ok(Value::Null), |doc| crate::yaml::to_json(doc))
}

/// The entries under a key, as Python iterates them: a list item by item, a string character by
/// character, nothing for a missing or empty value.
fn items(value: Option<&Value>) -> Vec<Value> {
    match value {
        Some(Value::Array(list)) => list.clone(),
        Some(Value::String(s)) => s.chars().map(|c| Value::String(c.to_string())).collect(),
        Some(Value::Object(map)) => map.keys().map(|k| Value::String(k.clone())).collect(),
        _ => Vec::new(),
    }
}

/// The roles one entry of a requirements file stands for: itself, or every role of the file its
/// `include` names. The include path is read from the working directory, as the reference does.
fn role_entry(entry: &Value, file: &Path, verbose: u8) -> Result<Vec<Role>, Failure> {
    if let Some(include) = entry.get("include") {
        let named = text(include);
        let included = Path::new(&named);
        if !included.is_file() {
            return Err(Failure::error(format!(
                "Failed to find include requirements file '{named}' in '{}'",
                file.display()
            )));
        }
        let unable = |e: String| {
            Failure::error(format!(
                "Unable to load data from include requirements file: {} {e}",
                file.display()
            ))
        };
        let loaded = load(included).map_err(|e| unable(e.to_string()))?;
        let Value::Array(entries) = loaded else {
            return Err(unable(format!(
                "'{}' object is not iterable",
                python_type(&loaded)
            )));
        };
        return entries
            .iter()
            .map(|e| role(e).map_err(|f| unable(f.message)))
            .collect();
    }
    let parsed = role(entry)?;
    if verbose >= 3 {
        println!("found role {} in yaml file", role_repr(entry, &parsed));
    }
    if parsed.name.is_none() && parsed.src.is_none() {
        return Err(Failure::error("Must specify name or src for role"));
    }
    Ok(vec![parsed])
}

/// The dictionary `role_yaml_parse` returns, as Python prints it: a mapping's own keys in their
/// order, then the ones it adds, `name`, `version` and `scm`.
fn role_repr(entry: &Value, parsed: &Role) -> String {
    const KEYS: [&str; 4] = ["name", "src", "scm", "version"];
    let mut order: Vec<&str> = match entry {
        Value::Object(map) if !map.contains_key("role") => map
            .keys()
            .filter_map(|k| KEYS.iter().find(|v| *v == k).copied())
            .collect(),
        _ => KEYS.to_vec(),
    };
    for key in ["name", "version", "scm"] {
        if !order.contains(&key) {
            order.push(key);
        }
    }
    let value = |key: &str| match key {
        "name" => &parsed.name,
        "src" => &parsed.src,
        "scm" => &parsed.scm,
        _ => &parsed.version,
    };
    let items: Vec<String> = order
        .iter()
        .map(|key| match value(key) {
            Some(v) => format!(
                "'{key}': '{}'",
                v.replace('\\', "\\\\").replace('\'', "\\'")
            ),
            None => format!("'{key}': None"),
        })
        .collect();
    format!("{{{}}}", items.join(", "))
}

/// A role written as `src[,version[,name]]` or as a mapping (`RoleRequirement.role_yaml_parse`).
pub fn role(entry: &Value) -> Result<Role, Failure> {
    if let Value::String(line) = entry {
        let parts: Vec<&str> = line.trim().splitn(3, ',').collect();
        let (src, version, name) = match (line.matches(',').count(), parts.as_slice()) {
            (0, _) => (line.as_str(), None, None),
            (1, [src, version]) => (*src, Some((*version).to_string()), None),
            (2, [src, version, name]) => (
                *src,
                Some((*version).to_string()),
                Some((*name).to_string()),
            ),
            _ => {
                return Err(Failure::error(format!(
                    "Invalid role line ({line}). Proper format is 'role_name[,version[,name]]'"
                )));
            }
        };
        let name = name.unwrap_or_else(|| role_name_of(src));
        let (scm, src) = match src.split_once('+') {
            Some((scm, rest)) => (Some(scm.to_string()), rest.to_string()),
            None => (None, src.to_string()),
        };
        return Ok(Role {
            name: Some(name),
            src: Some(src),
            scm,
            version,
        });
    }
    let field = |key: &str| entry.get(key).map(text);
    if let Some(old) = field("role") {
        if old.contains(',') {
            return Err(Failure::error(format!(
                "Invalid old style role requirement: {old}"
            )));
        }
        return Ok(Role {
            name: Some(old),
            src: field("src"),
            scm: field("scm"),
            version: field("version"),
        });
    }
    let mut out = Role {
        name: field("name"),
        src: field("src"),
        scm: field("scm"),
        version: Some(field("version").unwrap_or_default()),
    };
    if let Some(mut src) = out.src.take() {
        if src.contains("github.com")
            && src.contains("http")
            && !src.contains('+')
            && !src.ends_with(".tar.gz")
        {
            src = format!("git+{src}");
        }
        if let Some((scm, rest)) = src.split_once('+') {
            out.scm = Some(scm.to_string());
            src = rest.to_string();
        }
        if out.name.is_none() {
            out.name = Some(role_name_of(&src));
        }
        out.src = Some(src);
    }
    Ok(out)
}

/// The role name a source stands for: the source itself, or the last part of a URL without
/// `.git`, `.tar.gz` and a `,version` (`RoleRequirement.repo_url_to_role_name`).
fn role_name_of(src: &str) -> String {
    if !src.contains("://") && !src.contains('@') {
        return src.to_string();
    }
    let mut tail = src.rsplit('/').next().unwrap_or(src);
    tail = tail.strip_suffix(".git").unwrap_or(tail);
    tail = tail.strip_suffix(".tar.gz").unwrap_or(tail);
    tail.split(',').next().unwrap_or(tail).to_string()
}

/// A collection entry: a name, or a mapping with `name`, `version`, `source`, `type` and
/// `signatures`. The type, when not given, is found from the name as the reference finds it,
/// looking at the file system for a file, a collection directory or a namespace directory.
pub fn collection(entry: &Value) -> Result<Collection, Failure> {
    let map = match entry {
        Value::Object(map) => map.clone(),
        Value::String(_) | Value::Null => {
            let mut map = serde_json::Map::new();
            map.insert("name".into(), entry.clone());
            map
        }
        other => {
            return Err(Failure::unexpected(&format!(
                "unsupported type <class '{}'>",
                python_type(other)
            )));
        }
    };
    let mut name = map.get("name").filter(|v| !v.is_null()).map(text);
    let version = map.get("version").map_or_else(|| "*".to_string(), text);
    let mut source = map.get("source").filter(|v| !v.is_null()).map(text);
    let signatures = match map.get("signatures") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(list)) => list.iter().map(text).collect(),
        Some(one) => vec![text(one)],
    };
    let kind = match map.get("type").filter(|v| !v.is_null()).map(text) {
        Some(given) => match given.as_str() {
            "galaxy" => Kind::Galaxy,
            "file" => Kind::File,
            "dir" => Kind::Dir,
            "git" => Kind::Git,
            "url" => Kind::Url,
            "subdirs" => Kind::Subdirs,
            _ => {
                return Err(Failure::error(
                    "The collection requirement entry key 'type' must be one of file, galaxy, git, dir, subdirs, or url.",
                ));
            }
        },
        None => match name.as_deref() {
            Some(n) if is_collection_name(n) => Kind::Galaxy,
            Some(n) if concrete(n) => {
                source = name.take();
                pointed(source.as_deref().unwrap_or_default())
            }
            _ => return Err(not_resolvable(name.as_deref(), source.as_deref())),
        },
    };
    if kind == Kind::Galaxy && name.is_none() {
        return Err(Failure::error(
            "Collections requirement entry should contain the key 'name' if it's requested from a Galaxy-like index server.",
        ));
    }
    if kind != Kind::Galaxy && source.is_none() {
        source = name.take();
    }
    if kind == Kind::Dir
        && let Some(dir) = &mut source
    {
        while dir.len() > 1 && dir.ends_with(std::path::MAIN_SEPARATOR) {
            dir.pop();
        }
    }
    Ok(Collection {
        name,
        version,
        source,
        kind,
        signatures,
    })
}

/// A collection named on the command line: an FQCN or a pointer as it is, `name:version`, or a
/// name with a PEP 440 specifier such as `a.b>=1.0` (`Requirement.from_string`).
pub fn from_string(input: &str) -> Result<Collection, Failure> {
    let mut map = serde_json::Map::new();
    if concrete(input) || is_collection_name(input) {
        map.insert("name".into(), input.into());
    } else if let Some((name, version)) = input.split_once(':') {
        map.insert("name".into(), name.into());
        if !version.is_empty() {
            map.insert("version".into(), version.into());
        }
    } else if let Some(at) = input.find(['<', '>', '=', '!', '~']) {
        map.insert("name".into(), input[..at].trim().into());
        map.insert("version".into(), input[at..].replace(' ', "").into());
    } else {
        map.insert("name".into(), input.into());
    }
    collection(&Value::Object(map))
}

fn not_resolvable(name: Option<&str>, source: Option<&str>) -> Failure {
    let directory = |p: &str| {
        format!(
            "\n\nTip: Make sure you are pointing to the right subdirectory — `{p}` looks like a directory but it is neither a collection, nor a namespace dir."
        )
    };
    let tip = match (source, name) {
        (Some(s), _) if Path::new(s).is_dir() => directory(s),
        (_, Some(n)) if Path::new(n).is_dir() => directory(n),
        (_, Some(n)) if !n.is_empty() => format!("\n\nCould not find {n}."),
        _ => String::new(),
    };
    Failure::error(format!(
        "Neither the collection requirement entry key 'name', nor 'source' point to a concrete resolvable collection artifact. Also 'name' is not an FQCN. A valid collection name must be in the format <namespace>.<collection>. Please make sure that the namespace and the collection name contain characters from [a-zA-Z0-9_] only.{tip}"
    ))
}

/// `namespace.name`, each part a Python identifier and not a keyword
/// (`AnsibleCollectionRef.is_valid_collection_name`).
pub fn is_collection_name(name: &str) -> bool {
    const KEYWORDS: [&str; 35] = [
        "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
        "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global",
        "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return",
        "try", "while", "with", "yield",
    ];
    let identifier = |part: &str| {
        let mut chars = part.chars();
        chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
            && chars.all(|c| c == '_' || c.is_alphanumeric())
            && !KEYWORDS.contains(&part)
    };
    name.matches('.').count() == 1 && name.split('.').all(identifier)
}

fn is_git(s: &str) -> bool {
    s.starts_with("git+") || s.starts_with("git@")
}

fn is_http(s: &str) -> bool {
    s.split_once("://").is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    })
}

fn is_collection_dir(path: &Path) -> bool {
    path.join("MANIFEST.json").is_file() || path.join("galaxy.yml").is_file()
}

fn is_namespace_dir(path: &Path) -> bool {
    std::fs::read_dir(path)
        .is_ok_and(|entries| entries.flatten().any(|e| is_collection_dir(&e.path())))
}

/// Whether `s` points at an artifact: a git or HTTP URL, a file, or a collection or namespace
/// directory.
fn concrete(s: &str) -> bool {
    let path = Path::new(s);
    is_git(s) || is_http(s) || path.is_file() || is_collection_dir(path) || is_namespace_dir(path)
}

/// The type of the artifact `s` points at.
fn pointed(s: &str) -> Kind {
    let path = Path::new(s);
    if is_git(s) {
        Kind::Git
    } else if is_http(s) {
        Kind::Url
    } else if path.is_file() {
        Kind::File
    } else if is_collection_dir(path) {
        Kind::Dir
    } else {
        Kind::Subdirs
    }
}

/// A scalar as Python's `str()` prints it.
fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if !n.is_i64() && !n.is_u64() && f.fract() == 0.0 && f.is_finite() => {
                format!("{f:.1}")
            }
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

/// The name of the Python type a YAML value loads as.
fn python_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}
