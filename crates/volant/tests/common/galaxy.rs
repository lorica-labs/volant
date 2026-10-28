// SPDX-License-Identifier: GPL-3.0-or-later
//! The fake Galaxy server of `golden/reference/galaxy_server.py`, started for a test, and the
//! recordings of the `galaxy` golden kind it answers.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use sha2::{Digest as _, Sha256};

pub const CASES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/galaxy");
const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/reference/galaxy_server.py"
);
const TMP: &str = "<golden-tmp>";
const SERVER: &str = "<golden-server>";
const PORT: &str = "<golden-port>";

/// The server, running on 127.0.0.1 until dropped.
pub struct GalaxyServer {
    child: Child,
    pub port: u16,
}

impl GalaxyServer {
    /// The server, or `None` after saying why on stderr when no Python 3 is on `PATH`.
    pub fn start() -> Option<Self> {
        for python in ["python3", "python"] {
            let Ok(mut child) = Command::new(python)
                .arg(SCRIPT)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
            else {
                continue;
            };
            let mut line = String::new();
            BufReader::new(child.stdout.take().expect("stdout is piped"))
                .read_line(&mut line)
                .expect("the server prints its port");
            let port = line.trim().parse().unwrap_or_else(|_| {
                let _ = child.kill();
                panic!("{python} {SCRIPT} printed {line:?}, not a port")
            });
            return Some(Self { child, port });
        }
        eprintln!("skipped: no python3 or python on PATH to run the fake Galaxy server");
        None
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// The status and body of a `GET` of `path`.
    pub fn get(&self, path: &str) -> (u16, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).expect("the server listens");
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            self.port
        )
        .expect("the request is sent");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("the answer is read");
        let end = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("the answer has a header");
        let status = std::str::from_utf8(&raw[9..12])
            .ok()
            .and_then(|s| s.parse().ok())
            .expect("the status line has a code");
        (status, raw[end + 4..].to_vec())
    }

    /// The requests the server has seen, as `METHOD /path?query`, in order.
    pub fn log(&self) -> Vec<String> {
        serde_json::from_slice(&self.get("/_log").1).expect("the log is JSON")
    }
}

impl Drop for GalaxyServer {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A fixture repository: one commit per source directory, the tags on the first.
#[derive(serde::Deserialize)]
pub struct Repo {
    pub path: String,
    pub sources: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Makes the case's repositories as `golden/reference/galaxy.py` makes them, with the same author
/// and dates so their commits have the same hashes, and returns their top directories.
pub fn make_repos(repos: &[Repo], tmp: &Path) -> Vec<String> {
    let mut made = Vec::new();
    for repo in repos {
        let path = tmp.join(&repo.path);
        std::fs::create_dir_all(&path).expect("a repository directory");
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(&path)
                .envs([
                    ("GIT_AUTHOR_NAME", "Volant"),
                    ("GIT_AUTHOR_EMAIL", "volant@example.invalid"),
                    ("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+00:00"),
                    ("GIT_COMMITTER_NAME", "Volant"),
                    ("GIT_COMMITTER_EMAIL", "volant@example.invalid"),
                    ("GIT_COMMITTER_DATE", "2026-01-01T00:00:00+00:00"),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        for (number, source) in repo.sources.iter().enumerate() {
            for entry in std::fs::read_dir(&path).expect("readable").flatten() {
                if entry.file_name() != ".git" {
                    let p = entry.path();
                    if p.is_dir() {
                        std::fs::remove_dir_all(&p).expect("removed");
                    } else {
                        std::fs::remove_file(&p).expect("removed");
                    }
                }
            }
            copy_tree(&tmp.join(source), &path);
            // The index would trust a file of the same size and time as the one it replaces.
            git(&["rm", "-r", "-q", "--cached", "--ignore-unmatch", "."]);
            git(&["add", "-A"]);
            git(&["commit", "-q", "-m", &format!("fixture {number}")]);
            if number == 0 {
                for tag in &repo.tags {
                    git(&["tag", tag]);
                }
            }
        }
        made.push(repo.path.split('/').next().unwrap_or_default().to_string());
    }
    made
}

fn copy_tree(from: &Path, to: &Path) {
    if from.is_dir() {
        std::fs::create_dir_all(to).expect("a directory");
        for entry in std::fs::read_dir(from).expect("readable").flatten() {
            copy_tree(&entry.path(), &to.join(entry.file_name()));
        }
    } else {
        std::fs::copy(from, to).expect("copied");
    }
}

/// One case's `expected.json`.
#[derive(serde::Deserialize)]
pub struct Case {
    pub args: Vec<String>,
    /// Runs made before `args`, not compared.
    #[serde(default)]
    pub setup: Vec<Vec<String>>,
    /// Git repositories made before the runs.
    #[serde(default)]
    pub repos: Vec<Repo>,
    pub rc: i32,
    pub stdout: String,
    pub stderr: String,
    pub requests: Vec<String>,
    pub tree: std::collections::BTreeMap<String, String>,
}

/// Every recorded case, by name.
pub fn cases() -> Vec<(String, Case)> {
    let mut names: Vec<_> = std::fs::read_dir(CASES)
        .expect("the galaxy cases are there")
        .filter_map(|e| {
            let path = e.ok()?.path();
            path.join("expected.json").is_file().then_some(path)
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|dir| {
            let text = std::fs::read_to_string(dir.join("expected.json")).expect("readable");
            let name = dir
                .file_name()
                .expect("named")
                .to_string_lossy()
                .into_owned();
            (
                name,
                serde_json::from_str(&text).expect("expected.json parses"),
            )
        })
        .collect()
}

/// `args` with the server's URL and `tmp` in place of their placeholders.
pub fn argv(args: &[String], server: &GalaxyServer, tmp: &Path) -> Vec<String> {
    let tmp = tmp.to_string_lossy();
    args.iter()
        .map(|a| a.replace(SERVER, &server.url()).replace(TMP, &tmp))
        .collect()
}

/// `text` as the recorder writes it: see `golden/reference/galaxy.py`.
pub fn normalise(text: &str, tmp: &Path, port: u16) -> String {
    let galaxy_tmp = regex::Regex::new(
        r"(?m)\S*/\.ansible/tmp/ansible-local-[^/\s]+/tmp[^/\s]+/(\S+?)-[a-z0-9_]{8}(\s|$)",
    )
    .expect("valid");
    let install_date = regex::Regex::new(r"(?m)^install_date: '[^'\n]*'$").expect("valid");
    let text = text
        .replace(&*tmp.to_string_lossy(), TMP)
        .replace(&format!("127.0.0.1:{port}"), &format!("127.0.0.1:{PORT}"));
    let text = galaxy_tmp.replace_all(&text, "<golden-galaxy-tmp>/$1-<golden-random>$2");
    let text = install_date.replace_all(&text, "install_date: '<golden-date>'");
    let git = regex::Regex::new(r"Command '([\[(])'[^']*/git', ").expect("valid");
    git.replace_all(&text, "Command '${1}'git', ").into_owned()
}

/// Copies the files a case's run reads, every file of its directory but `case.yml` and
/// `expected.json`, into `tmp`, and returns their names.
pub fn copy_inputs(name: &str, tmp: &Path) -> Vec<String> {
    fn copy(from: &Path, to: &Path) {
        if from.is_dir() {
            std::fs::create_dir_all(to).expect("a directory");
            for entry in std::fs::read_dir(from).expect("readable").flatten() {
                copy(&entry.path(), &to.join(entry.file_name()));
            }
        } else {
            std::fs::copy(from, to).expect("copied");
        }
    }
    let dir = Path::new(CASES).join(name);
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("readable")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "case.yml" && n != "expected.json")
        .collect();
    names.sort();
    for input in &names {
        copy(&dir.join(input), &tmp.join(input));
    }
    names
}

/// Every file under `tmp` outside its `home` and `skip`, by path, with the SHA-256 of its
/// normalised text, or of its members for a `.tar.gz` (see [`members`]).
pub fn tree(tmp: &Path, port: u16, skip: &[String]) -> std::collections::BTreeMap<String, String> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("readable").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(tmp).expect("readable").flatten() {
        let path = entry.path();
        if skip.iter().any(|s| entry.file_name() == s.as_str()) {
            continue;
        }
        if path.is_dir() && entry.file_name() != "home" {
            walk(&path, &mut files);
        } else if path.is_file() {
            files.push(path);
        }
    }
    files
        .into_iter()
        .map(|path| {
            let mut data = std::fs::read(&path).expect("readable");
            if path.to_string_lossy().ends_with(".tar.gz") {
                data = members(&data);
            }
            let data = match String::from_utf8(data) {
                Ok(text) => normalise(&text, tmp, port).into_bytes(),
                Err(e) => e.into_bytes(),
            };
            let name = path
                .strip_prefix(tmp)
                .expect("under tmp")
                .to_string_lossy()
                .replace('\\', "/");
            (name, format!("{:x}", Sha256::digest(&data)))
        })
        .collect()
}

/// A tarball as `golden/reference/galaxy.py` records it: its members sorted by name, each with
/// its kind and content hash, a line each. Times, owners and compression do not count.
fn members(data: &[u8]) -> Vec<u8> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(data));
    let mut lines: Vec<(String, String)> = Vec::new();
    for entry in archive.entries().expect("a tarball") {
        let mut entry = entry.expect("a member");
        let name = entry
            .path()
            .expect("a name")
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        let line = match entry.header().entry_type() {
            tar::EntryType::Directory => format!("{name} dir"),
            tar::EntryType::Symlink => format!(
                "{name} link {}",
                entry
                    .link_name()
                    .ok()
                    .flatten()
                    .map(|l| l.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            _ => {
                let mut content = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut content).expect("readable");
                format!("{name} file {:x}", Sha256::digest(&content))
            }
        };
        lines.push((name, line));
    }
    lines.sort();
    lines
        .into_iter()
        .map(|(_, l)| l)
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}
