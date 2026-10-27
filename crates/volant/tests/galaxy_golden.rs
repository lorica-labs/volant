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
            // ansible-galaxy first tries the server URL itself as the API root, which is not one.
            let want = if path == "/" { 404 } else { 200 };
            assert_eq!(status, want, "{name}: {request}");
        }
        assert_eq!(server.log(), case.requests, "{name}");
    }
}

/// The cases whose subcommand `volant galaxy` still refuses.
const NOT_NATIVE: [&str; 2] = ["collection-install", "role-install"];

/// `volant galaxy` against each case: the same exit code, output, requests and installed files.
#[test]
fn a_galaxy_case_matches_the_reference() {
    for (name, case) in galaxy::cases() {
        if NOT_NATIVE.contains(&name.as_str()) {
            continue;
        }
        let Some(server) = GalaxyServer::start() else {
            return;
        };
        let tmp = tempfile::tempdir().expect("a scratch directory");
        let home = tmp.path().join("home");
        std::fs::create_dir(&home).expect("a home");
        let inputs = galaxy::copy_inputs(&name, tmp.path());
        let volant = |args: &[String]| {
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_volant"));
            command
                .arg("galaxy")
                .args(galaxy::argv(args, &server, tmp.path()))
                .current_dir(tmp.path())
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
            |b: &[u8]| galaxy::normalise(&String::from_utf8_lossy(b), tmp.path(), server.port);
        assert_eq!(out.status.code(), Some(case.rc), "{name}: exit code");
        assert_eq!(text(&out.stdout), case.stdout, "{name}: stdout");
        assert_eq!(text(&out.stderr), case.stderr, "{name}: stderr");
        assert_eq!(server.log()[seen..], case.requests, "{name}: requests");
        assert_eq!(
            galaxy::tree(tmp.path(), server.port, &inputs),
            case.tree,
            "{name}: files"
        );
    }
}
