// SPDX-License-Identifier: GPL-3.0-or-later
//! The `galaxy` golden kind: ansible-galaxy 2.19.12 run against the fake Galaxy server, recorded
//! under `golden/galaxy/<case>/expected.json`.

#[path = "common/galaxy.rs"]
mod galaxy;
#[path = "common/process.rs"]
mod process;

use std::time::Duration;

use galaxy::GalaxyServer;

/// The server still answers every request a recording made, and logs them as the recording did.
#[test]
fn the_fake_server_answers_every_recorded_request() {
    for (name, case) in galaxy::cases() {
        let Some(server) = GalaxyServer::start() else {
            return;
        };
        for request in &case.requests {
            let path = request.strip_prefix("GET ").expect("a GET");
            let (status, _) = server.get(path);
            // `/` is not an API root, which ansible-galaxy tries first; a collection the server
            // does not have is a 404 too.
            assert!(
                status == 200 || status == 404,
                "{name}: {request}: {status}"
            );
        }
        assert_eq!(server.log(), case.requests, "{name}");
    }
}

/// The cases whose subcommand `volant galaxy` still refuses.
const NOT_NATIVE: [&str; 1] = ["role-install"];

/// `volant galaxy` against each case: the same exit code, output, requests and installed files.
#[test]
fn a_galaxy_case_matches_the_reference() {
    let mut differences = Vec::new();
    for (name, case) in galaxy::cases() {
        if NOT_NATIVE.contains(&name.as_str()) {
            continue;
        }
        let Some(server) = GalaxyServer::start() else {
            return;
        };
        let held = tempfile::tempdir().expect("a scratch directory");
        // Canonical: the macOS temporary directory is a link, and the paths Volant prints are
        // resolved ones.
        let tmp = held.path().canonicalize().expect("a real path");
        let home = tmp.as_path().join("home");
        std::fs::create_dir(&home).expect("a home");
        let mut inputs = galaxy::copy_inputs(&name, tmp.as_path());
        inputs.extend(galaxy::make_repos(&case.repos, tmp.as_path()));
        let volant = |args: &[String]| {
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_volant"));
            command
                .arg("galaxy")
                .args(galaxy::argv(args, &server, tmp.as_path()))
                .current_dir(tmp.as_path())
                .env("HOME", &home);
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("ANSIBLE_") {
                    command.env_remove(key);
                }
            }
            process::run_within(&mut command, Duration::from_secs(120), None)
        };
        for args in &case.setup {
            let out = volant(args);
            assert!(out.status.success(), "{name}: setup {args:?}: {out:?}");
        }
        let seen = server.log().len();
        let out = volant(&case.args);
        let text =
            |b: &[u8]| galaxy::normalise(&String::from_utf8_lossy(b), tmp.as_path(), server.port);
        let mut differ = |what: &str, ours: String, theirs: String| {
            if ours != theirs {
                differences.push(format!(
                    "{name}: {what}
  volant: {ours}
  ansible-galaxy: {theirs}"
                ));
            }
        };
        differ(
            "exit code",
            format!("{:?}", out.status.code()),
            format!("{:?}", Some(case.rc)),
        );
        differ("stdout", text(&out.stdout), case.stdout.clone());
        differ("stderr", text(&out.stderr), case.stderr.clone());
        differ(
            "requests",
            format!("{:?}", &server.log()[seen..]),
            format!("{:?}", case.requests),
        );
        differ(
            "files",
            format!("{:?}", galaxy::tree(tmp.as_path(), server.port, &inputs)),
            format!("{:?}", case.tree),
        );
    }
    assert!(
        differences.is_empty(),
        "{}",
        differences.join(
            "
"
        )
    );
}
