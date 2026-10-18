// SPDX-License-Identifier: GPL-3.0-or-later
use super::fetch::{extract, fetch, fetch_with, sha256_of};
use super::manifest::Manifest;
use super::*;
use crate::testing::scratch;

use rstest::rstest;
use std::fs;

#[rstest]
#[case::glibc_linux("linux", "x86_64", false, Ok("x86_64-unknown-linux-gnu"))]
#[case::musl_linux("linux", "aarch64", true, Ok("aarch64-unknown-linux-musl"))]
#[case::macos_ignores_libc("macos", "aarch64", true, Ok("aarch64-apple-darwin"))]
#[case::other_arch("linux", "riscv64", false, Err("not supported on linux riscv64"))]
#[case::other_os("windows", "x86_64", false, Err("not supported on windows x86_64"))]
fn the_triple_follows_the_host(
    #[case] os: &str,
    #[case] arch: &str,
    #[case] musl: bool,
    #[case] expected: Result<&str, &str>,
) {
    match (triple(os, arch, || musl), expected) {
        (Ok(triple), Ok(expected)) => assert_eq!(triple, expected),
        (Err(e), Err(expected)) => assert!(e.contains(expected) && e.contains(INSTEAD), "{e}"),
        (got, expected) => panic!("got {got:?}, expected {expected:?}"),
    }
}

#[rstest]
#[case::glibc("ldd (Ubuntu GLIBC 2.39-0ubuntu8) 2.39", true, false)]
#[case::other_glibc_wording("ldd (GNU libc) 2.40", true, false)]
#[case::musl("musl libc (x86_64)\nVersion 1.2.5", false, true)]
#[case::no_ldd_musl_loader("", true, true)]
#[case::no_ldd_no_loader("", false, false)]
fn the_libc_is_read_from_ldd_then_the_loader(
    #[case] ldd: &str,
    #[case] loader: bool,
    #[case] musl: bool,
) {
    let ldd = Some(ldd).filter(|text| !text.is_empty());
    assert_eq!(is_musl(ldd, loader), musl);
}

/// The built-in manifest pins the six triples, each with ansible-core and the host's among them.
#[test]
fn the_pinned_manifest_covers_every_triple() {
    let manifest = Manifest::pinned().unwrap();
    for triple in [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ] {
        let platform = manifest.platform(triple).unwrap();
        let archive = format!(
            "cpython-{}%2B{}-{triple}-",
            manifest.python, manifest.python_release
        );
        assert!(
            platform.python.url.contains(&archive),
            "{}",
            platform.python.url
        );
        assert!(platform.wheels.len() > 1, "{triple} pins no dependency");
    }
    assert_eq!(manifest.ansible_core, "2.19.12");
    manifest.platform(&host_triple().unwrap()).unwrap();
}

fn manifest(edit: (&str, &str)) -> String {
    let text = r#"
python = "3.13.16"
python-release = "20261013"
ansible-core = "2.19.12"

[platform.x86_64-unknown-linux-gnu.python]
url = "https://example.invalid/python.tar.gz"
sha256 = "PYTHON_HASH"

[[platform.x86_64-unknown-linux-gnu.wheel]]
name = "ansible-core"
version = "2.19.12"
url = "https://example.invalid/ansible_core.whl"
sha256 = "WHEEL_HASH"
"#;
    text.replacen(edit.0, edit.1, 1)
        .replace("PYTHON_HASH", &"a".repeat(64))
        .replace("WHEEL_HASH", &"b".repeat(64))
}

#[rstest]
#[case::not_toml(("[platform", "[[platform"), "TOML parse error")]
#[case::no_version(("ansible-core = ", "core = "), "ansible-core is missing")]
#[case::no_python(("[platform.", "[other."), "python is missing or not a table")]
#[case::no_wheel(("[[platform.x86_64-unknown-linux-gnu.wheel]]", "[[other]]"), "wheel is missing")]
#[case::plain_http(("https://example.invalid/python", "http://example.invalid/python"), "python.url is not an https URL")]
#[case::no_filename(("ansible_core.whl", ""), "wheel[0].url: https://example.invalid/ does not end with a file name")]
#[case::dot_dot_filename(("ansible_core.whl", ".."), "does not end with a file name")]
#[case::query_filename(("python.tar.gz", "python.tar.gz?x=1"), "does not end with a file name")]
#[case::short_hash(("PYTHON_HASH", "abc"), "python.sha256 is not 64 lowercase hex digits")]
#[case::upper_hash(("WHEEL_HASH", "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"), "wheel[0].sha256 is not 64 lowercase")]
#[case::no_ansible_core(("name = \"ansible-core\"", "name = \"jinja2\""), "has no ansible-core 2.19.12")]
fn a_broken_manifest_is_refused(#[case] edit: (&str, &str), #[case] expected: &str) {
    let text = manifest(edit);
    assert_ne!(text, manifest(("", "")), "the edit changed nothing");
    let e = Manifest::parse(&text).unwrap_err();
    assert!(
        e.starts_with("the runtime manifest is invalid: ") && e.contains(expected),
        "{e}"
    );
}

#[test]
fn a_triple_with_no_build_is_refused() {
    let manifest = Manifest::parse(&manifest(("", ""))).unwrap();
    let wheel = &manifest
        .platform("x86_64-unknown-linux-gnu")
        .unwrap()
        .wheels[0];
    assert_eq!(wheel.file.filename().unwrap(), "ansible_core.whl");
    let e = manifest.platform("aarch64-apple-darwin").unwrap_err();
    assert!(
        e.contains("aarch64-apple-darwin") && e.contains(INSTEAD),
        "{e}"
    );
    assert_eq!(
        manifest
            .platform("x86_64-unknown-linux-gnu")
            .unwrap()
            .wheels[0]
            .file
            .sha256,
        "b".repeat(64)
    );
}

/// The downloader writes `served`; only a matching hash reaches `dest`, and nothing else is left.
#[rstest]
#[case::matching(b"pinned bytes".as_slice(), None)]
#[case::altered(b"other bytes".as_slice(), Some("does not match its pinned hash"))]
fn only_a_matching_download_reaches_dest(#[case] served: &[u8], #[case] refusal: Option<&str>) {
    let dir = scratch();
    let pinned = dir.path().join("pinned");
    fs::write(&pinned, b"pinned bytes").unwrap();
    let sha256 = sha256_of(&pinned).unwrap();
    fs::remove_file(&pinned).unwrap();
    let dest = dir.path().join("python.tar.gz");

    let result = fetch_with(
        |_, out| fs::write(out, served).map_err(|e| e.to_string()),
        "https://example.invalid/x",
        &sha256,
        &dest,
    );

    let left: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    match refusal {
        None => {
            result.unwrap();
            assert_eq!(fs::read(&dest).unwrap(), served);
            assert_eq!(left, ["python.tar.gz"]);
        }
        Some(refusal) => {
            let e = result.unwrap_err();
            let actual = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(served));
            assert!(
                e.contains(refusal) && e.contains(&sha256) && e.contains(&actual),
                "{e}"
            );
            assert!(left.is_empty(), "left behind: {left:?}");
        }
    }
}

/// A failed download leaves nothing, and a URL that is not https never reaches a downloader.
#[test]
fn a_failed_download_leaves_nothing() {
    let dir = scratch();
    let dest = dir.path().join("python.tar.gz");
    let e = fetch_with(
        |_, _| Err("no network".to_owned()),
        "https://example.invalid/x",
        &"a".repeat(64),
        &dest,
    )
    .unwrap_err();
    assert_eq!(e, "no network");
    let e = fetch("file:///etc/hostname", &"a".repeat(64), &dest).unwrap_err();
    assert!(e.contains("only https downloads are supported"), "{e}");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[rstest]
#[case::install_only("python/bin/python3", false, None)]
#[case::other_layout("bin/python3", false, Some("holds no python/bin/python3"))]
#[case::stale_interpreter("bin/python3", true, Some("is not empty"))]
fn extract_finds_the_interpreter(
    #[case] member: &str,
    #[case] stale: bool,
    #[case] refusal: Option<&str>,
) {
    let dir = scratch();
    let runtime = dir.path().join("runtime");
    if stale {
        fs::create_dir_all(runtime.join("python/bin")).unwrap();
        fs::write(runtime.join("python/bin/python3"), "").unwrap();
    }
    let source = dir.path().join("source");
    fs::create_dir_all(source.join(member).parent().unwrap()).unwrap();
    fs::write(source.join(member), "#!/bin/sh\n").unwrap();
    let tarball = dir.path().join("python.tar.gz");
    let packed = Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(&source)
        .arg(member.split('/').next().unwrap())
        .status()
        .unwrap();
    assert!(packed.success());

    let result = extract(&tarball, &runtime);
    match refusal {
        None => assert_eq!(result.unwrap(), runtime.join("python/bin/python3")),
        Some(refusal) => {
            let e = result.unwrap_err();
            assert!(e.contains(refusal), "{e}");
        }
    }
}
