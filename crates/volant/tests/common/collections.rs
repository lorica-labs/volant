// SPDX-License-Identifier: GPL-3.0-or-later
//! The collection version rule `golden.rs` and `ssh_e2e.rs` both apply before they run a
//! collection module: what `golden/COLLECTIONS` pins, read back from a controller python.

/// The versions `COLLECTIONS` pins: the three real collections, plus `netaddr`, which is not a
/// collection but is needed in the same controller environment for `ansible.utils.ipwrap` and is
/// pinned the same way.
fn pinned_collections() -> Vec<(String, String)> {
    include_str!("../golden/COLLECTIONS")
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, version)| (name.to_string(), version.to_string()))
        .collect()
}

/// What `generate.py`'s own `_installed_collections()` runs, in the interpreter's own words: a
/// collection's version comes from its `MANIFEST.json`, found on one of `C.COLLECTIONS_PATHS`;
/// `netaddr` is a plain import next to it. Kept in the interpreter this refuses to run without
/// (`reference_python()`) rather than reimplemented in Rust, because a manifest's shape and a
/// collection's path list are ansible-core's own to read.
///
/// The tuple of names is built from `pinned_collections()` rather than written out a second time:
/// `COLLECTIONS` gaining a fourth pin (or losing one) must not also require editing this string
/// by hand to match.
fn collection_version_script() -> String {
    let tuple = pinned_collections()
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| name != "netaddr")
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"
import json, os
from ansible import constants as C
versions = {{}}
for fqcn in ({tuple},):
    namespace, name = fqcn.split(".", 1)
    for root in C.COLLECTIONS_PATHS:
        manifest = os.path.join(os.path.expanduser(root), "ansible_collections", namespace, name, "MANIFEST.json")
        if os.path.exists(manifest):
            with open(manifest, encoding="utf-8") as f:
                versions[fqcn] = json.load(f)["collection_info"]["version"]
            break
try:
    import netaddr
    versions["netaddr"] = netaddr.__version__
except ImportError:
    pass
print(json.dumps(versions))
"#
    )
}

/// What in `COLLECTIONS` this `python` does not actually have installed, one line per pin, on the
/// model of `controller_python`'s own version check: a collection found alone at another version
/// names both; one nowhere on the path reads `not installed`.
pub fn collection_mismatches(python: &std::path::Path) -> Vec<String> {
    let out = std::process::Command::new(python)
        .args(["-c", &collection_version_script()])
        .output()
        .expect("python runs");
    let installed: std::collections::BTreeMap<String, String> =
        serde_json::from_slice(&out.stdout).unwrap_or_default();
    pinned_collections()
        .into_iter()
        .filter_map(|(name, version)| {
            let got = installed.get(&name).cloned();
            (got.as_deref() != Some(version.as_str())).then(|| {
                format!(
                    "{name} is {}, not {version}",
                    got.unwrap_or_else(|| "not installed".into())
                )
            })
        })
        .collect()
}
