// SPDX-License-Identifier: GPL-3.0-or-later
//! The package_facts tests.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::dpkg::{Installed, installed};
use super::imp::{LISTS, Request, answer, request, run};
use super::scan::{canonical, scan, scan_all};
use super::sources::apt_bool;
use crate::modules::Context;
use crate::natives::NativeRun;
use crate::natives::runtime::{Clock, Root, Stop, unbounded};

/// The reason guard's cases (`natives::tests`): apt sources and a `PATH` whose every
/// field the native hands back on holds a secret-looking value.
#[cfg(target_os = "linux")]
#[expect(clippy::too_many_lines)]
pub(in crate::natives) fn secret_probes() -> Vec<crate::natives::tests::Probe> {
    let example = "/etc/apt/sources.list.d/example.list";
    let line = "deb [signed-by=/usr/share/keyrings/example.gpg] \
                https://repo.example.org/debian stable main";
    let edited = |from: &str, to: &str| {
        let tree = Tree::new(HOST);
        tree.edit(example, from, to);
        tree
    };
    let written = |path: &str, content: &str| {
        let tree = Tree::new(HOST);
        tree.write(path, content);
        tree
    };
    let cases = vec![
        (
            "package_facts scheme",
            "K3S0SECRET1A",
            edited("https://repo.", "K3S0SECRET1A:repo."),
            "/usr/bin:/bin",
        ),
        (
            "package_facts user info",
            "K3S0SECRET1B",
            edited("https://repo.", "https://user:K3S0SECRET1B@repo."),
            "/usr/bin:/bin",
        ),
        (
            "package_facts suite",
            "K3S0SECRET1C",
            edited("stable main", "sta~K3S0SECRET1C main"),
            "/usr/bin:/bin",
        ),
        (
            "package_facts component",
            "K3S0SECRET1D",
            edited("stable main", "stable ma!nK3S0SECRET1D"),
            "/usr/bin:/bin",
        ),
        (
            "package_facts incomplete line",
            "K3S0SECRET1E",
            written(
                "/etc/apt/sources.list",
                "deb http://K3S0SECRET1E.example.org/debian\n",
            ),
            "/usr/bin:/bin",
        ),
        (
            "package_facts line type",
            "K3S0SECRET1F",
            written(
                "/etc/apt/sources.list",
                "deb-K3S0SECRET1F http://archive.example.org/debian stable main\n",
            ),
            "/usr/bin:/bin",
        ),
        (
            "package_facts option",
            "K3S0SECRET1G",
            edited("deb [signed-by=", "deb [target=K3S0SECRET1G signed-by="),
            "/usr/bin:/bin",
        ),
        (
            "package_facts option +",
            "K3S0SECRET1H",
            edited("deb [signed-by=", "deb [arch+=K3S0SECRET1H signed-by="),
            "/usr/bin:/bin",
        ),
        (
            "package_facts no component",
            "k3s0secret1i",
            edited(
                "https://repo.example.org/debian stable main",
                "https://k3s0secret1i.example.org/debian stable",
            ),
            "/usr/bin:/bin",
        ),
        (
            "package_facts two Signed-By",
            "k3s0secret1j",
            written(
                example,
                &format!(
                    "{}\n{}\n",
                    line.replace("stable", "k3s0secret1j"),
                    line.replace("stable", "k3s0secret1j")
                        .replace("example.gpg", "other.gpg"),
                ),
            ),
            "/usr/bin:/bin",
        ),
        (
            "package_facts Enabled",
            "K3S0SECRET1K",
            {
                let tree = Tree::new(HOST);
                let path = "/etc/apt/sources.list.d/ubuntu.sources";
                let full = tree.0.join(path.trim_start_matches('/'));
                let text = std::fs::read_to_string(&full).unwrap();
                std::fs::write(full, format!("{text}Enabled: K3S0SECRET1K\n")).unwrap();
                tree
            },
            "/usr/bin:/bin",
        ),
        (
            "package_facts deb822 type",
            "K3S0SECRET1L",
            {
                let tree = Tree::new(HOST);
                tree.edit(
                    "/etc/apt/sources.list.d/ubuntu.sources",
                    "Types: deb\n",
                    "Types: K3S0SECRET1L\n",
                );
                tree
            },
            "/usr/bin:/bin",
        ),
        (
            "package_facts PATH",
            "K3S0SECRET1M",
            Tree::new(HOST),
            "K3S0SECRET1M:/usr/bin",
        ),
    ];
    cases
        .into_iter()
        .map(|(case, secret, tree, path)| {
            let reason = match tree.answer_with(json!({}), &[("PATH", path)]) {
                Err(Stop::HandBack(reason)) => Some(reason),
                _ => None,
            };
            (case, secret, reason)
        })
        .collect()
}

/// A host as files: `=== <path>` starts each file, the lines up to the next header are its
/// content. `/usr/bin/python3` stands for the interpreter the reference respawns under.
const HOST: &str = r"=== /usr/bin/python3
=== /etc/apt/sources.list.d/ubuntu.sources
Types: deb
URIs: http://archive.ubuntu.com/ubuntu/
Suites: noble noble-updates
Components: main universe
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

Types: deb-src
URIs: http://archive.ubuntu.com/ubuntu/
Suites: noble
Components: main
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
=== /etc/apt/sources.list.d/example.list
# a third-party repository
deb [signed-by=/usr/share/keyrings/example.gpg] https://repo.example.org/debian stable main
=== /etc/apt/sources.list.d/example.list.save
deb https://old.example.org/debian stable main
=== /var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble_InRelease
-----BEGIN PGP SIGNED MESSAGE-----
Hash: SHA512

Origin: Ubuntu
Label: Ubuntu
Suite: noble
Version: 24.04
Codename: noble
Architectures: amd64 arm64 armhf i386 ppc64el riscv64 s390x
Components: main restricted universe multiverse
Description: Ubuntu Noble 24.04
-----BEGIN PGP SIGNATURE-----

iQIzBAEBCgAdFiEEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
=AAAA
-----END PGP SIGNATURE-----
=== /var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble-updates_Release
Origin: Ubuntu
Label: Ubuntu
Suite: noble-updates
Codename: noble
Components: main restricted universe multiverse
Description: Ubuntu Noble Updates
=== /var/lib/apt/lists/repo.example.org_debian_dists_stable_InRelease
-----BEGIN PGP SIGNED MESSAGE-----
Hash: SHA256

Origin: Example
Label: Example
Suite: stable
Codename: stable
Components: main
-----BEGIN PGP SIGNATURE-----

iQIzBAEBCgAdFiEEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
=AAAA
-----END PGP SIGNATURE-----
=== /var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble_main_binary-amd64_Packages
Package: base-files
Architecture: amd64
Version: 13ubuntu10
Multi-Arch: foreign
Priority: required
Essential: yes
Section: admin
Installed-Size: 404
Size: 72952
Description: Debian base system miscellaneous files

Package: bash
Architecture: amd64
Version: 5.2.21-2ubuntu4
Multi-Arch: foreign
Priority: required
Essential: yes
Section: shells
Installed-Size: 1848
Pre-Depends: libc6 (>= 2.36), libtinfo6 (>= 6)
Depends: base-files (>= 2.1.12), debianutils (>= 5.6-0.1)
Replaces: bash-completion (<< 20060301-0), bash-doc (<= 2.05-1)
Size: 796066
Description: GNU Bourne Again SHell

Package: dpkg
Architecture: amd64
Version: 1.22.6ubuntu6
Multi-Arch: foreign
Priority: required
Essential: yes
Section: admin
Installed-Size: 6476
Pre-Depends: libbz2-1.0, libc6 (>= 2.38), liblzma5 (>= 5.4.0), libzstd1 (>= 1.5.5)
Depends: tar (>= 1.28-1)
Size: 1275398
Description: Debian package management system

Package: held
Architecture: amd64
Version: 1.4-1
Section: utils
Installed-Size: 20
Size: 4000
Description: a package on hold

Package: python3-apt
Architecture: amd64
Version: 2.7.7ubuntu1
Priority: important
Section: python
Installed-Size: 736
Depends: python3 (<< 3.13), python3 (>= 3.12~), python3:any, libapt-pkg6.0t64 (>= 2.7.11)
Size: 168406
Description: Python 3 interface to libapt-pkg

Package: tzdata
Architecture: all
Version: 2024a-2ubuntu1
Multi-Arch: foreign
Priority: important
Section: localization
Installed-Size: 1400
Depends: debconf (>= 0.5) | debconf-2.0
Size: 273024
Description: time zone and daylight-saving time data
=== /var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble_universe_binary-amd64_Packages
Package: hello
Architecture: amd64
Version: 2.10-3build2
Priority: optional
Section: universe/devel
Installed-Size: 112
Depends: libc6 (>= 2.34)
Size: 30534
Description: example package based on GNU hello

Package: tool
Architecture: amd64
Version: 2.0-1
Section: universe/utils
Installed-Size: 50
Size: 9000
Description: a tool the archive carries in another version
=== /var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble-updates_main_binary-amd64_Packages
Package: base-files
Architecture: amd64
Version: 13ubuntu10.2
Multi-Arch: foreign
Priority: required
Essential: yes
Section: admin
Installed-Size: 405
Size: 73000
Description: Debian base system miscellaneous files

Package: bash
Architecture: amd64
Version: 5.2.21-2ubuntu4
Multi-Arch: foreign
Priority: required
Essential: yes
Section: shells
Installed-Size: 1848
Pre-Depends: libc6 (>= 2.36), libtinfo6 (>= 6)
Depends: base-files (>= 2.1.12), debianutils (>= 5.6-0.1)
Replaces: bash-completion (<< 20060301-0), bash-doc (<= 2.05-1)
Size: 796066
Description: GNU Bourne Again SHell
=== /var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble-updates_universe_binary-amd64_Packages
=== /var/lib/apt/lists/repo.example.org_debian_dists_stable_main_binary-amd64_Packages
Package: tool
Version: 1.0-1
Architecture: amd64
Section: utils
Installed-Size: 48
Size: 8800
Description: a tool from a third-party repository
=== /var/lib/dpkg/status
Package: base-files
Essential: yes
Status: install ok installed
Priority: required
Section: admin
Installed-Size: 404
Maintainer: Ubuntu Developers <ubuntu-devel-discuss@lists.ubuntu.com>
Architecture: amd64
Multi-Arch: foreign
Version: 13ubuntu10.1
Description: Debian base system miscellaneous files

Package: bash
Essential: yes
Status: install ok installed
Priority: required
Section: shells
Installed-Size: 1848
Architecture: amd64
Multi-Arch: foreign
Version: 5.2.21-2ubuntu4
Replaces: bash-completion (<< 20060301-0), bash-doc (<= 2.05-1)
Depends: base-files (>= 2.1.12), debianutils (>= 5.6-0.1)
Pre-Depends: libc6 (>= 2.36), libtinfo6 (>= 6)
Conffiles:
 /etc/bash.bashrc 89269e1298235f1b12b4c16e4065ad0d
 /etc/skel/.bashrc 0f1a3c5b0ad6d1e3a4bc86bb9d71ba6b
Description: GNU Bourne Again SHell

Package: dpkg
Essential: yes
Status: install ok installed
Priority: required
Section: admin
Installed-Size: 6476
Architecture: amd64
Multi-Arch: foreign
Version: 1.22.6ubuntu6
Depends: tar (>= 1.28-1)
Pre-Depends: libbz2-1.0, libc6 (>= 2.38), liblzma5 (>= 5.4.0), libzstd1 (>= 1.5.5)
Description: Debian package management system

Package: held
Status: hold ok installed
Section: utils
Installed-Size: 20
Architecture: amd64
Version: 1.4-1
Description: a package on hold

Package: hello
Status: install ok installed
Priority: optional
Section: devel
Installed-Size: 112
Architecture: amd64
Version: 2.10-3build2
Depends: libc6 (>= 2.34)
Description: example package based on GNU hello

Package: oldpkg
Status: deinstall ok config-files
Priority: optional
Section: misc
Installed-Size: 10
Architecture: amd64
Version: 0.9-1
Conffiles:
 /etc/oldpkg.conf 00000000000000000000000000000000
Description: a removed package whose configuration stays

Package: python3-apt
Status: install ok installed
Priority: important
Section: python
Installed-Size: 736
Architecture: amd64
Version: 2.7.7ubuntu1
Depends: python3 (<< 3.13), python3 (>= 3.12~), python3:any, libapt-pkg6.0t64 (>= 2.7.11)
Description: Python 3 interface to libapt-pkg

Package: tool
Status: install ok installed
Section: utils
Installed-Size: 48
Architecture: amd64
Version: 1.0-1
Description: a tool from a third-party repository

Package: tzdata
Status: install ok installed
Priority: important
Section: localization
Installed-Size: 1400
Architecture: all
Multi-Arch: foreign
Version: 2024a-2ubuntu1
Depends: debconf (>= 0.5) | debconf-2.0
Description: time zone and daylight-saving time data
";

/// What python-apt 2.7 and 3.1 (`apt.Cache(rootdir=...)`, read the way `package_facts`
/// reads it) say of `HOST`. Measured on both: `hello`'s section is the archive's, not
/// the status's; `tool` takes the origin of the list that carries its installed
/// version, not the one that carries its name; `base-files` is in no list at its
/// installed version and gets the empty origin; `held` (on hold) is installed;
/// `oldpkg` (configuration only) is not.
const REFERENCE: &str = r#"{
    "base-files": [{"arch": "amd64", "category": "admin", "name": "base-files", "origin": "", "source": "apt", "version": "13ubuntu10.1"}],
    "bash": [{"arch": "amd64", "category": "shells", "name": "bash", "origin": "Ubuntu", "source": "apt", "version": "5.2.21-2ubuntu4"}],
    "dpkg": [{"arch": "amd64", "category": "admin", "name": "dpkg", "origin": "Ubuntu", "source": "apt", "version": "1.22.6ubuntu6"}],
    "held": [{"arch": "amd64", "category": "utils", "name": "held", "origin": "Ubuntu", "source": "apt", "version": "1.4-1"}],
    "hello": [{"arch": "amd64", "category": "universe/devel", "name": "hello", "origin": "Ubuntu", "source": "apt", "version": "2.10-3build2"}],
    "python3-apt": [{"arch": "amd64", "category": "python", "name": "python3-apt", "origin": "Ubuntu", "source": "apt", "version": "2.7.7ubuntu1"}],
    "tool": [{"arch": "amd64", "category": "utils", "name": "tool", "origin": "Example", "source": "apt", "version": "1.0-1"}],
    "tzdata": [{"arch": "all", "category": "localization", "name": "tzdata", "origin": "Ubuntu", "source": "apt", "version": "2024a-2ubuntu1"}]
}"#;

/// `HOST` written under a directory of its own, removed at the end.
struct Tree(PathBuf);

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Tree {
    fn new(host: &str) -> Tree {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = PathBuf::from(format!(
            "/tmp/volant-package-facts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("var/lib/apt/lists/partial")).unwrap();
        std::fs::create_dir_all(dir.join("var/lib/dpkg/updates")).unwrap();
        let tree = Tree(dir);
        let mut path: Option<String> = None;
        let mut content = String::new();
        for line in host.split_inclusive('\n').chain(std::iter::once("=== \n")) {
            let Some(next) = line.strip_prefix("=== ") else {
                content.push_str(line);
                continue;
            };
            if let Some(path) = path.take() {
                tree.write(&path, &content);
            }
            content.clear();
            path = Some(next.trim().to_string()).filter(|next| !next.is_empty());
        }
        tree
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.0.join(path.trim_start_matches('/'));
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, content).unwrap();
        if path.starts_with("/usr/bin/") {
            std::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn edit(&self, path: &str, from: &str, to: &str) {
        let full = self.0.join(path.trim_start_matches('/'));
        let text = std::fs::read_to_string(&full).unwrap();
        assert!(text.contains(from), "{path} has no {from:?}");
        std::fs::write(&full, text.replacen(from, to, 1)).unwrap();
    }

    fn answer(&self, args: Value) -> Result<Map<String, Value>, Stop> {
        self.answer_with(args, &[])
    }

    fn answer_with(&self, args: Value, env: &[(&str, &str)]) -> Result<Map<String, Value>, Stop> {
        let mut module_env = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".into())]);
        module_env.extend(
            env.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        );
        answer(
            args.as_object().unwrap(),
            &Root::at(&self.0),
            &module_env,
            unbounded(),
        )
    }

    fn hands_back(&self, args: Value) -> String {
        match self.answer(args) {
            Err(Stop::HandBack(reason)) => reason,
            other => panic!("answered where the native should hand back: {other:?}"),
        }
    }
}

/// A host is answered as python-apt answers it: records, `enabled`, apt before apk, dpkg version order.
#[test]
#[expect(clippy::too_many_lines)]
fn a_host_is_answered_as_the_reference_answers_it() {
    {
        let tree = Tree::new(HOST);
        let result = tree.answer(json!({})).unwrap();
        let reference: Value = serde_json::from_str(REFERENCE).unwrap();
        assert_eq!(result["ansible_facts"]["packages"], reference);
        assert_eq!(
            result["invocation"],
            json!({"module_args": {"manager": ["auto"], "strategy": "first"}})
        );
        let result = tree
            .answer(json!({"manager": "APT", "strategy": "first"}))
            .unwrap();
        assert_eq!(result["ansible_facts"]["packages"], reference);
        assert_eq!(
            result["invocation"],
            json!({"module_args": {"manager": ["APT"], "strategy": "first"}}),
            "the invocation keeps the manager as written, converted to a list"
        );
    }
    {
        for (text, expected) in [
            ("0", Some(false)),
            ("00", Some(false)),
            ("-0", Some(false)),
            ("0x0", Some(false)),
            ("1", Some(true)),
            ("+1", Some(true)),
            ("0X1", Some(true)),
            ("-1", None),
            ("2", None),
            ("08", None),
            ("0x", None),
            ("++1", None),
            ("0x+1", None),
            ("No", Some(false)),
            ("FALSE", Some(false)),
            ("without", Some(false)),
            ("off", Some(false)),
            ("disable", Some(false)),
            ("yes", Some(true)),
            ("True", Some(true)),
            ("with", Some(true)),
            ("ON", Some(true)),
            ("enable", Some(true)),
            ("maybe", None),
            ("", None),
        ] {
            assert_eq!(apt_bool(text), expected, "{text:?}");
        }
        let reference: Value = serde_json::from_str(REFERENCE).unwrap();
        let as_deb822 = |tree: &Tree, enabled: &str| {
            tree.write("/etc/apt/sources.list.d/example.list", "");
            tree.write(
                "/etc/apt/sources.list.d/example.sources",
                &format!(
                    "Types: deb\nURIs: https://repo.example.org/debian\nSuites: stable\n\
                 Components: main\nSigned-By: /usr/share/keyrings/example.gpg\n\
                 Enabled: {enabled}\n"
                ),
            );
        };
        for disabled in ["false", "0", "off", "Disable", "NO"] {
            let tree = Tree::new(HOST);
            as_deb822(&tree, disabled);
            let given = tree.hands_back(json!({}));
            assert!(
                given.contains("is a list no configured source names"),
                "{disabled}: {given}"
            );
        }
        for enabled in ["yes", "1", "on"] {
            let tree = Tree::new(HOST);
            as_deb822(&tree, enabled);
            let result = tree.answer(json!({})).unwrap();
            assert_eq!(result["ansible_facts"]["packages"], reference, "{enabled}");
        }
        let tree = Tree::new(HOST);
        as_deb822(&tree, "maybe");
        assert!(tree.hands_back(json!({})).contains("is not a boolean"));
        let tree = Tree::new(HOST);
        tree.edit(
            "/etc/apt/sources.list.d/example.list",
            "deb [signed-by=",
            "deb [enabled=no signed-by=",
        );
        let result = tree.answer(json!({})).unwrap();
        assert_eq!(result["ansible_facts"]["packages"], reference);
    }
    {
        let tree = Tree::new(HOST);
        tree.write("/usr/bin/apk", "");
        assert!(tree.answer(json!({"manager": ["auto", "apt"]})).is_ok());
    }
    {
        for (left, right) in [
            ("1.0-1", "0:1.0-1"),
            ("1.00-1", "1.0-1"),
            ("1.0", "1.0-0"),
            ("1.0", "1."),
            ("2:007", "2:7"),
        ] {
            assert_eq!(canonical(left), canonical(right), "{left} {right}");
        }
        for (left, right) in [
            ("1.0", "1.0.0"),
            ("1.0-1", "1.0-1ubuntu1"),
            ("1:1.0", "1.0"),
            ("1.0~rc1", "1.0"),
        ] {
            assert_ne!(canonical(left), canonical(right), "{left} {right}");
        }
    }
}

/// A half-configured package, a host outside the subset and any other record or source hand back.
#[test]
#[expect(clippy::too_many_lines)]
fn what_the_native_does_not_read_hands_back() {
    {
        for state in [
            "install ok half-configured",
            "install ok unpacked",
            "install ok half-installed",
            "install ok triggers-pending",
            "install reinstreq installed",
        ] {
            let tree = Tree::new(HOST);
            tree.edit(
                "/var/lib/dpkg/status",
                "Package: hello\nStatus: install ok installed",
                &format!("Package: hello\nStatus: {state}"),
            );
            let reason = tree.hands_back(json!({}));
            assert!(
                reason.starts_with("hello is ") && reason.ends_with("as installed"),
                "{state}: {reason}"
            );
        }
    }
    {
        type Case = (&'static str, Box<dyn Fn(&Tree)>);
        let cases: Vec<Case> = vec![
            (
                "is a list no configured source names",
                Box::new(|tree| {
                    tree.write(
                        &format!(
                            "{LISTS}/old.example.org_debian_dists_stable_main_binary-amd64_Packages"
                        ),
                        "",
                    );
                }),
            ),
            (
                "is a compressed list",
                Box::new(|tree| {
                    tree.write(
                    &format!("{LISTS}/repo.example.org_debian_dists_stable_main_binary-amd64_Packages.lz4"),
                    "",
                );
                }),
            ),
            (
                "apk is found",
                Box::new(|tree| tree.write("/usr/bin/apk", "")),
            ),
            (
                "sets dir::state",
                Box::new(|tree| {
                    tree.write(
                        "/etc/apt/apt.conf.d/99lists",
                        "Dir::State::Lists \"/srv/lists\";\n",
                    );
                }),
            ),
            (
                "sets architecture\"",
                Box::new(|tree| {
                    tree.write(
                        "/etc/apt/apt.conf.d/99arch",
                        "APT {\n  Architecture \"i386\";\n};\n",
                    );
                }),
            ),
            (
                "for the foreign architecture i386",
                Box::new(|tree| {
                    tree.edit(
                        "/var/lib/dpkg/status",
                        "Architecture: amd64\nVersion: 1.4-1",
                        "Architecture: i386\nVersion: 1.4-1",
                    );
                }),
            ),
            (
                "two different Signed-By or Trusted",
                Box::new(|tree| {
                    tree.edit(
                    "/etc/apt/sources.list.d/ubuntu.sources",
                    "Components: main\nSigned-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg",
                    "Components: main",
                );
                }),
            ),
            (
                "python3-apt is not installed",
                Box::new(|tree| {
                    tree.edit(
                        "/var/lib/dpkg/status",
                        "Package: python3-apt\nStatus: install ok installed",
                        "Package: python3-apt\nStatus: deinstall ok config-files",
                    );
                }),
            ),
            (
                "with fields apt hashes that differ from the status",
                Box::new(|tree| {
                    tree.edit(
                        "/var/lib/dpkg/status",
                        "Depends: libc6 (>= 2.34)",
                        "Depends: libc6 (>= 2.35)",
                    );
                }),
            ),
            (
                "which apt compares equal",
                Box::new(|tree| {
                    tree.edit(
                    &format!("{LISTS}/repo.example.org_debian_dists_stable_main_binary-amd64_Packages"),
                    "Version: 1.0-1",
                    "Version: 0:1.00-1",
                );
                }),
            ),
            (
                "in sources that disagree about it",
                Box::new(|tree| {
                    tree.edit(
                    &format!("{LISTS}/archive.ubuntu.com_ubuntu_dists_noble_universe_binary-amd64_Packages"),
                    "Version: 2.0-1\nSection: universe/utils\nInstalled-Size: 50\nSize: 9000",
                    "Version: 1.0-1\nSection: utils\nInstalled-Size: 48\nSize: 8800",
                );
                }),
            ),
            (
                "a deb822 source paragraph has no URIs",
                Box::new(|tree| {
                    tree.edit(
                        "/etc/apt/sources.list.d/ubuntu.sources",
                        "URIs: http://archive.ubuntu.com/ubuntu/\nSuites: noble noble-updates",
                        "Suites: noble noble-updates",
                    );
                }),
            ),
        ];
        for (reason, break_host) in cases {
            let tree = Tree::new(HOST);
            tree.answer(json!({}))
                .expect("the fixture host is answered");
            break_host(&tree);
            let given = tree.hands_back(json!({}));
            assert!(given.contains(reason), "expected {reason:?}, got {given:?}");
        }
        let tree = Tree::new(HOST);
        tree.write("/var/lib/dpkg/updates/0001", "");
        assert!(tree.hands_back(json!({})).contains("dpkg has updates"));
        let tree = Tree::new(HOST);
        let given = match tree.answer_with(json!({}), &[("APT_CONFIG", "/srv/apt.conf")]) {
            Err(Stop::HandBack(reason)) => reason,
            other => panic!("APT_CONFIG answered: {other:?}"),
        };
        assert!(given.contains("APT_CONFIG"), "{given}");
    }
    {
        let append = |tree: &Tree, path: &str, text: &str| {
            let full = tree.0.join(path.trim_start_matches('/'));
            let mut content = std::fs::read_to_string(&full).unwrap_or_default();
            content.push_str(text);
            std::fs::write(full, content).unwrap();
        };
        let status = "/var/lib/dpkg/status";
        let example = "/etc/apt/sources.list.d/example.list";
        let ubuntu = "/etc/apt/sources.list.d/ubuntu.sources";
        let repo =
            format!("{LISTS}/repo.example.org_debian_dists_stable_main_binary-amd64_Packages");
        type Case<'a> = (&'static str, Box<dyn Fn(&Tree) + 'a>);
        let cases: Vec<Case> = vec![
            (
                "Multi-Arch: weird is not one apt knows",
                Box::new(|tree| {
                    tree.edit(
                        status,
                        "Section: devel\n",
                        "Section: devel\nMulti-Arch: weird\n",
                    );
                }),
            ),
            (
                "hello has the status",
                Box::new(|tree| {
                    tree.edit(
                        status,
                        "Package: hello\nStatus: install ok installed",
                        "Package: hello\nStatus: installed",
                    );
                }),
            ),
            (
                "hello is installed without a version or an architecture",
                Box::new(|tree| {
                    tree.edit(status, "Version: 2.10-3build2", "Xersion: 2.10-3build2");
                }),
            ),
            (
                "hello is installed twice",
                Box::new(|tree| {
                    append(
                        tree,
                        status,
                        "\nPackage: hello\nStatus: install ok installed\nVersion: 9\nArchitecture: amd64\nSection: devel\n",
                    );
                }),
            ),
            (
                "tool is listed without a version or an architecture",
                Box::new(|tree| tree.edit(&repo, "Version: 1.0-1", "Xersion: 1.0-1")),
            ),
            (
                "has a name apt may skip",
                Box::new(|tree| tree.write("/etc/apt/sources.list.d/my repo.list", "")),
            ),
            (
                "a component is not a plain name",
                Box::new(|tree| tree.edit(example, "stable main", "stable ma!n")),
            ),
            (
                "names no component",
                Box::new(|tree| tree.edit(example, "stable main", "stable")),
            ),
            (
                "a suite is not a plain name",
                Box::new(|tree| tree.edit(example, "stable main", "sta~ble main")),
            ),
            (
                "a source URI is not a plain one",
                Box::new(|tree| tree.edit(example, "https://repo.", "https://user@repo.")),
            ),
            (
                "a source line is incomplete",
                Box::new(|tree| {
                    tree.write(
                        "/etc/apt/sources.list",
                        "deb http://archive.example.org/debian\n",
                    );
                }),
            ),
            (
                "a source type is not deb",
                Box::new(|tree| {
                    tree.write(
                        "/etc/apt/sources.list",
                        "deb-foo http://archive.example.org/debian stable main\n",
                    );
                }),
            ),
            (
                "the source option target changes its lists",
                Box::new(|tree| {
                    tree.edit(
                        example,
                        "deb [signed-by=",
                        "deb [target=Packages signed-by=",
                    );
                }),
            ),
            (
                "the source option arch+ changes its lists",
                Box::new(|tree| {
                    tree.edit(example, "deb [signed-by=", "deb [arch+=i386 signed-by=");
                }),
            ),
            (
                "the source field Snapshot changes its lists",
                Box::new(|tree| append(tree, ubuntu, "Snapshot: yes\n")),
            ),
            (
                "a source type is not deb",
                Box::new(|tree| tree.edit(ubuntu, "Types: deb\n", "Types: rpm\n")),
            ),
        ];
        for (reason, break_host) in cases {
            let tree = Tree::new(HOST);
            break_host(&tree);
            let given = tree.hands_back(json!({}));
            assert!(given.contains(reason), "expected {reason:?}, got {given:?}");
        }
        let tree = Tree::new(HOST);
        let given = match tree.answer_with(json!({}), &[("PATH", "bin:/usr/bin")]) {
            Err(Stop::HandBack(reason)) => reason,
            other => panic!("a relative PATH answered: {other:?}"),
        };
        assert!(given.contains("PATH holds a relative directory"), "{given}");
        // A `deb-src` twin with the same `Signed-By` and a `sources.list` that exists are
        // read, and change nothing.
        let tree = Tree::new(HOST);
        append(
            &tree,
            example,
            "deb-src [signed-by=/usr/share/keyrings/example.gpg] https://repo.example.org/debian stable main\n",
        );
        tree.write("/etc/apt/sources.list", "# moved to sources.list.d\n");
        let reference: Value = serde_json::from_str(REFERENCE).unwrap();
        let result = tree.answer(json!({})).unwrap();
        assert_eq!(result["ansible_facts"]["packages"], reference);
    }
}

/// The entry point hands back what the answer hands back and reads the arguments like the module.
#[test]
fn the_entry_point_hands_back_and_reads_the_arguments() {
    {
        let context = Context {
            timeout: None,
            environment: BTreeMap::new(),
            interpreter: None,
            remote_tmp: "/tmp".into(),
            check_mode: false,
        };
        let args = json!({"strategy": "all"});
        match run(args.as_object().unwrap(), &context, &|| false) {
            NativeRun::Fallback(reason) => assert_eq!(reason, "strategy is not first"),
            _ => panic!("strategy all was answered"),
        }
    }
    {
        let ok = |args: Value| request(args.as_object().unwrap());
        assert_eq!(
            ok(json!({"manager": "auto,apt"})).unwrap(),
            Request {
                manager: vec!["auto".into(), "apt".into()],
                apt_named: true
            }
        );
        assert_eq!(
            ok(json!({})).unwrap(),
            Request {
                manager: vec!["auto".into()],
                apt_named: false
            }
        );
        for args in [
            json!({"manager": ["auto", "auto"]}),
            json!({"manager": "auto, apt"}),
            json!({"manager": ["rpm"]}),
            json!({"manager": []}),
            json!({"manager": 5}),
            json!({"manager": [5]}),
            json!({"manager": null}),
            json!({"strategy": "all"}),
            json!({"strategy": null}),
            json!({"strategy": "First"}),
            json!({"other": 1}),
        ] {
            assert!(ok(args.clone()).is_err(), "{args} was accepted");
        }
    }
}

/// A cancel or the deadline stops the readers, and a reader told to stop reads no further.
#[test]
fn the_readers_stop_at_the_cancel_or_the_deadline() {
    {
        let files = vec![(PathBuf::from("/nonexistent"), String::new())];
        let installed = HashMap::new();
        let slow = |_: &PathBuf, _: &str, _: &HashMap<&str, Installed<'_>>, halt: &AtomicBool| {
            while !halt.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(Vec::new())
        };
        let yes = || true;
        let clock = Clock {
            deadline: None,
            cancelled: &yes,
        };
        assert!(matches!(
            scan_all(&files, &installed, clock, slow),
            Err(Stop::Cancelled)
        ));
        let no = || false;
        let clock = Clock {
            deadline: Some(Instant::now() + Duration::from_millis(100)),
            cancelled: &no,
        };
        assert!(matches!(
            scan_all(&files, &installed, clock, slow),
            Err(Stop::TimedOut)
        ));
    }
    {
        let tree = Tree::new(HOST);
        let status = std::fs::read_to_string(tree.0.join("var/lib/dpkg/status")).unwrap();
        let (_, installed) = installed(&status).unwrap();
        let path = tree.0.join(
            "var/lib/apt/lists/archive.ubuntu.com_ubuntu_dists_noble_main_binary-amd64_Packages",
        );
        let halt = AtomicBool::new(true);
        assert!(scan(&path, "Ubuntu", &installed, &halt).unwrap().is_empty());
        halt.store(false, Ordering::Relaxed);
        assert_eq!(scan(&path, "Ubuntu", &installed, &halt).unwrap().len(), 6);
    }
}
