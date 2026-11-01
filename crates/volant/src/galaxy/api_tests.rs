// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};

use super::api::download::galaxy_yml;
use super::api::servers::Servers;
use super::api::{Files, Server, VersionInfo, galaxy_error};
use super::settings::ServerSettings;

const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/reference/galaxy_server.py"
);
const RECORDED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/galaxy/collection-install/expected.json"
);

/// The fake Galaxy server of the golden kind, stopped when dropped.
struct Fake {
    child: Child,
    port: u16,
}

impl Fake {
    fn start() -> Option<Self> {
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
            BufReader::new(child.stdout.take().expect("piped"))
                .read_line(&mut line)
                .expect("a port");
            let port = line.trim().parse().expect("a port");
            return Some(Self { child, port });
        }
        eprintln!("skipped: no python3 to run the fake Galaxy server");
        None
    }

    /// The server as `-s` names it, with its response cache and token file under `home`.
    fn server(&self, home: &std::path::Path) -> Server {
        let settings = ServerSettings {
            name: "cmd_arg".into(),
            url: self.url(),
            token: None,
            validate_certs: true,
            timeout: 60,
        };
        let files = Files {
            cache_dir: Some(&home.join("galaxy_cache")),
            clear_cache: false,
            token_path: &home.join("galaxy_token"),
        };
        Server::new(&settings, &files, 0)
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    fn log(&self) -> Vec<String> {
        let body = ureq::get(format!("http://127.0.0.1:{}/_log", self.port))
            .call()
            .expect("the log")
            .body_mut()
            .read_to_string()
            .expect("text");
        serde_json::from_str(&body).expect("json")
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The versions of a collection over two pages, asked as ansible-galaxy asks: the same requests
/// in the same order as its recording, up to the version record.
#[test]
fn the_requests_are_those_ansible_galaxy_makes() {
    let Some(fake) = Fake::start() else {
        return;
    };
    let home = tempfile::tempdir().expect("a home");
    let mut server = fake.server(home.path());
    assert_eq!(
        server
            .collection_versions("demo", "base")
            .expect("versions"),
        ["1.0.0", "1.1.0", "2.0.0"]
    );
    server
        .version_info("demo", "base", "2.0.0")
        .expect("record");
    let recorded: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(RECORDED).expect("recorded")).expect("json");
    let want: Vec<String> = recorded["requests"]
        .as_array()
        .expect("requests")
        .iter()
        .map(|r| r.as_str().expect("text").to_string())
        .filter(|r| !r.contains("/artifacts/"))
        .collect();
    assert_eq!(fake.log(), want);
    assert_eq!(
        server.api_server(),
        format!("http://127.0.0.1:{}/api", fake.port)
    );
}

#[test]
fn a_version_record_gives_its_dependencies_and_download() {
    let Some(fake) = Fake::start() else {
        return;
    };
    let home = tempfile::tempdir().expect("a home");
    let mut server = fake.server(home.path());
    let info = server.version_info("demo", "app", "1.0.0").expect("record");
    assert_eq!(
        info.dependencies,
        [("demo.base".to_string(), ">=1.0.0,<2.0.0".to_string())]
    );
    assert_eq!(
        info.download_url,
        format!(
            "http://127.0.0.1:{}/api/v3/collections/demo/app/artifacts/demo-app-1.0.0.tar.gz",
            fake.port
        )
    );
    assert_eq!(info.href, "/api/v3/collections/demo/app/versions/1.0.0/");
    let dir = tempfile::tempdir().expect("a directory");
    let file = server
        .download(
            &info.download_url,
            &info.sha256,
            dir.path(),
            "demo.base:1.0.0",
        )
        .expect("downloaded");
    assert_eq!(file.file_name().expect("named"), "demo-app-1.0.0.tar.gz");
    assert!(
        file.parent()
            .expect("a holder")
            .file_name()
            .expect("named")
            .to_string_lossy()
            .starts_with("demo-app-1.0.0-")
    );
}

#[test]
fn a_hash_mismatch_leaves_nothing() {
    let Some(fake) = Fake::start() else {
        return;
    };
    let home = tempfile::tempdir().expect("a home");
    let mut server = fake.server(home.path());
    let info = server.version_info("demo", "app", "1.0.0").expect("record");
    let dir = tempfile::tempdir().expect("a directory");
    let failed = server.download(
        &info.download_url,
        &"0".repeat(64),
        dir.path(),
        "demo.base:1.0.0",
    );
    assert_eq!(
        failed.map_err(|f| f.message),
        Err("Mismatch artifact hash with downloaded file".to_string())
    );
    assert_eq!(std::fs::read_dir(dir.path()).expect("readable").count(), 0);
}

#[test]
fn an_unknown_collection_has_no_versions() {
    let Some(fake) = Fake::start() else {
        return;
    };
    let home = tempfile::tempdir().expect("a home");
    let mut server = fake.server(home.path());
    assert!(
        server
            .collection_versions("demo", "nope")
            .expect("answered")
            .is_empty()
    );
}

/// The file ansible-galaxy 2.19.12 wrote after installing `demo.base:1.1.0` from the fake server,
/// with the port replaced.
#[test]
fn the_galaxy_yml_is_written_as_ansible_galaxy_writes_it() {
    let Some(fake) = Fake::start() else {
        return;
    };
    let home = tempfile::tempdir().expect("a home");
    let mut server = fake.server(home.path());
    let info: VersionInfo = server
        .version_info("demo", "base", "1.1.0")
        .expect("record");
    let port = fake.port;
    assert_eq!(
        galaxy_yml(&info, &server),
        format!(
            "download_url: http://127.0.0.1:{port}/api/v3/collections/demo/base/artifacts/demo-base-1.1.0.tar.gz\nformat_version: 1.0.0\nname: base\nnamespace: demo\nserver: http://127.0.0.1:{port}/api\nsignatures: []\nversion: 1.1.0\nversion_url: /api/v3/collections/demo/base/versions/1.1.0/\n"
        )
    );
}

/// Measured on ansible-core 2.19.12 against a server answering 401 and 500 for the collection.
#[test]
fn an_http_error_reads_as_ansible_galaxy_writes_it() {
    let context = "Error when getting the collection info for demo.base from cmd_arg (http://127.0.0.1:1/api)";
    let url = "http://127.0.0.1:1/api/v3/collections/demo/base/";
    let status = |code| ureq::http::StatusCode::from_u16(code).expect("a status");
    assert_eq!(
        galaxy_error(
            context,
            url,
            status(401),
            r#"{"detail": "Authentication credentials were not provided."}"#
        ),
        format!("{context} (HTTP Code: 401, Message: Unauthorized Code: Unknown)")
    );
    assert_eq!(
        galaxy_error(context, url, status(500), "oops"),
        format!("{context} (HTTP Code: 500, Message: Internal Server Error Code: Unknown)")
    );
    assert_eq!(
        galaxy_error(
            context,
            url,
            status(404),
            r#"{"errors": [{"code": "not_found", "detail": "Not found."}]}"#
        ),
        format!("{context} (HTTP Code: 404, Message: Not found. Code: not_found)")
    );
}

fn unreachable(name: &str) -> ServerSettings {
    ServerSettings {
        name: name.into(),
        url: "http://127.0.0.1:9/api/".into(),
        token: None,
        validate_certs: true,
        timeout: 5,
    }
}

/// A server that cannot be reached is skipped, and the next one answers.
#[test]
fn an_unreachable_server_is_skipped() {
    let Some(fake) = Fake::start() else {
        return;
    };
    let home = tempfile::tempdir().expect("a home");
    let files = Files {
        cache_dir: None,
        clear_cache: false,
        token_path: &home.path().join("galaxy_token"),
    };
    let mut servers = Servers::new(vec![
        Server::new(&unreachable("dead"), &files, 0),
        fake.server(home.path()),
    ]);
    let versions = servers.versions("demo.base").expect("the second answers");
    assert_eq!(versions.len(), 3);
    assert!(versions.iter().all(|(_, at)| *at == 1));
    let mut alone = Servers::new(vec![Server::new(&unreachable("dead"), &files, 0)]);
    let failure = alone.versions("demo.base").expect_err("none answers");
    assert!(
        failure.summary().starts_with(
            "Unknown error when attempting to call Galaxy at 'http://127.0.0.1:9/api/': "
        ),
        "{}",
        failure.summary()
    );
    assert!(failure.shown().contains("\n\n<<< caused by >>>\n\n"));
}

/// The proxy of the environment is used, and `no_proxy` lifts it. Each test runs in a process
/// of its own under nextest, so the variables reach no other test.
#[test]
fn the_proxy_variables_are_honoured() {
    let Some(fake) = Fake::start() else {
        return;
    };
    let home = tempfile::tempdir().expect("a home");
    unsafe {
        std::env::set_var("http_proxy", "http://127.0.0.1:9");
        std::env::remove_var("no_proxy");
        std::env::remove_var("NO_PROXY");
    }
    let mut proxied = fake.server(home.path());
    assert!(proxied.collection_versions("demo", "base").is_err());
    unsafe { std::env::set_var("no_proxy", "127.0.0.1") };
    let mut direct = fake.server(home.path());
    assert_eq!(
        direct
            .collection_versions("demo", "base")
            .expect("direct")
            .len(),
        3
    );
}
