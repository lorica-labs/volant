// SPDX-License-Identifier: GPL-3.0-or-later
use super::format::{WIDTH, encrypt_with_salt, envelope, parts, strip_crlf};
use super::load::{Source, spec, typed_secret};
use super::*;
use rstest::rstest;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/vault")
            .join(name),
    )
    .expect("fixture")
}

/// The vault text of a `name: !vault |` fixture, its indentation removed.
fn yaml_value(name: &str) -> Vec<u8> {
    let text = String::from_utf8(fixture(name)).unwrap();
    let lines: Vec<&str> = text.lines().skip(1).map(str::trim).collect();
    format!("{}\n", lines.join("\n")).into_bytes()
}

fn secrets(names: &[&str]) -> Secrets {
    Secrets::new(
        names
            .iter()
            .map(|n| fixture(n).trim_ascii().to_vec())
            .collect(),
    )
}

#[rstest]
#[case::file_11("f11.txt", &["pw1"], b"hello")]
#[case::file_11_pw2_first("fd.txt", &["pw2", "pw1"], b"hello")]
#[case::file_12_label_ignored("f2.txt", &["pw1", "pw2"], b"hello")]
fn files_decrypt_byte_for_byte(#[case] name: &str, #[case] pw: &[&str], #[case] want: &[u8]) {
    assert_eq!(decrypt(&fixture(name), &secrets(pw)).unwrap(), want);
}

#[rstest]
#[case::var_11("v11.yml", b"secret11")]
#[case::var_12("v12.yml", b"secret12")]
fn yaml_values_decrypt(#[case] name: &str, #[case] want: &[u8]) {
    assert_eq!(
        decrypt(&yaml_value(name), &secrets(&["pw1", "pw2"])).unwrap(),
        want
    );
}

#[rstest]
#[case::wrong_password("err-f.yml", &["pw2"], "Decryption failed (no vault secrets were found that could decrypt)")]
#[case::no_password("err-f.yml", &[], "Attempting to decrypt but no vault secrets found")]
#[case::corrupt("err-fcorrupt.yml", &["pw1"], "Vault format unhexlify error: Non-hexadecimal digit found")]
#[case::bad_version("err-fh.yml", &["pw1"], "Vault vaulttext format error: not enough values to unpack (expected 3, got 1)")]
#[case::not_hex("err-fz.yml", &["pw1"], "Vault format unhexlify error: Non-hexadecimal digit found")]
#[case::plain("err-h.txt", &["pw1"], "Input is not vault encrypted data.")]
fn bad_data_is_refused_in_the_references_words(
    #[case] name: &str,
    #[case] pw: &[&str],
    #[case] want: &str,
) {
    let err = decrypt(&fixture(name), &secrets(pw)).unwrap_err();
    assert_eq!(err.0, want);
}

#[rstest]
#[case::v11("f11.txt", None, "pw1")]
#[case::default_id_is_11("f11.txt", Some("default"), "pw1")]
#[case::v12("f2.txt", Some("b"), "pw2")]
fn encrypting_with_the_fixtures_salt_gives_the_fixture(
    #[case] name: &str,
    #[case] id: Option<&str>,
    #[case] pw: &str,
) {
    let data = fixture(name);
    let salt = parts(&envelope(&data).unwrap().body).unwrap().salt;
    let salt: [u8; 32] = salt.try_into().unwrap();
    let got = encrypt_with_salt(b"hello", fixture(pw).trim_ascii(), id, &salt);
    assert_eq!(got.as_bytes(), &data[..]);
}

#[test]
fn a_fresh_encryption_decrypts_and_wraps_at_80() {
    let text = encrypt(b"abc", b"pw", Some("dev")).unwrap();
    assert!(text.lines().skip(1).all(|l| l.len() <= WIDTH));
    assert!(text.ends_with('\n'));
    let got = decrypt(text.as_bytes(), &Secrets::new(vec![b"pw".to_vec()])).unwrap();
    assert_eq!(got, b"abc");
}

#[test]
fn secrets_do_not_show_in_debug() {
    assert_eq!(
        format!("{:?}", Secrets::new(vec![b"hunter2".to_vec()])),
        "Secrets(1 redacted)"
    );
}

#[rstest]
#[case::bare("pw1", "default")]
#[case::labelled("dev@pw1", "dev")]
fn a_spec_names_its_label(#[case] text: &str, #[case] label: &str) {
    assert_eq!(spec(text).0, label);
}

#[test]
fn prompt_is_the_only_source_that_is_not_a_file() {
    assert_eq!(spec("dev@prompt").1, Source::Prompt);
    assert_eq!(spec("prompt").1, Source::Prompt);
    assert_eq!(spec("a@b").1, Source::File("b".into()));
}

#[rstest]
#[case::plain(b"pw1", Some(&b"pw1"[..]))]
#[case::spaced(b"  pw1 	", Some(&b"pw1"[..]))]
#[case::empty(b"", None)]
#[case::blank(b" 	 ", None)]
fn a_typed_password_is_stripped_and_not_empty(#[case] typed: &[u8], #[case] want: Option<&[u8]>) {
    assert_eq!(typed_secret(typed).as_deref(), want);
}

#[test]
fn a_later_install_replaces_the_secrets() {
    install(Secrets::new(vec![b"a".to_vec()]));
    install(Secrets::new(vec![b"b".to_vec()]));
    assert_eq!(run_secrets().first(), Some(&b"b"[..]));
}

#[test]
fn script_output_loses_cr_and_lf_at_both_ends() {
    assert_eq!(strip_crlf(b"\r\n pw \r\n"), b" pw ");
    assert_eq!(strip_crlf(b"\n\r"), b"");
}

#[cfg(unix)]
mod files {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    fn write(dir: &Path, name: &str, body: &[u8], mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn request(path: &Path, id: &str) -> Request {
        Request {
            ids: vec![format!("{id}@{}", path.display())],
            ..Request::default()
        }
    }

    #[rstest]
    #[case::crlf(b"pw1\r\n", 0o600, b"pw1")]
    #[case::newline(b"pw1\n", 0o600, b"pw1")]
    #[case::script(b"#!/bin/sh\nprintf 'pw1\\r\\n'\n", 0o700, b"pw1")]
    fn a_password_file_or_script_gives_its_text(
        #[case] body: &[u8],
        #[case] mode: u32,
        #[case] want: &[u8],
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "pw", body, mode);
        let got = load(&request(&path, "dev")).unwrap();
        assert_eq!(got.first(), Some(want));
    }

    #[test]
    fn an_unreadable_file_is_named_in_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "pw", b"x", 0o000);
        if std::fs::read(&path).is_ok() {
            return; // running as root: nothing is unreadable
        }
        let err = load(&request(&path, "d")).unwrap_err();
        assert_eq!(
            err.message,
            format!("Could not read vault password file '{}'.", path.display())
        );
    }

    #[test]
    fn a_script_gets_no_arguments_and_a_failure_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let ok = write(
            dir.path(),
            "ok.sh",
            b"#!/bin/sh\n[ $# -eq 0 ] && echo pw1\n",
            0o700,
        );
        assert_eq!(
            load(&request(&ok, "dev")).unwrap().first(),
            Some(&b"pw1"[..])
        );
        let bad = write(dir.path(), "bad.sh", b"#!/bin/sh\nexit 3\n", 0o700);
        let err = load(&request(&bad, "x")).unwrap_err();
        let shown = bad.display();
        assert_eq!(
            err.message,
            format!("Vault password script {shown} returned non-zero (3): None")
        );
        assert_eq!(
            err.warning,
            format!("Error in vault password file loading (x): {}", err.message)
        );
    }

    #[test]
    fn missing_directory_and_empty_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let shown = dir.path().display();
        let missing = load(&request(&dir.path().join("nope"), "d")).unwrap_err();
        assert_eq!(
            missing.message,
            format!("The vault password file {shown}/nope was not found")
        );
        assert!(
            missing
                .warning
                .starts_with("Error getting vault password file (d): ")
        );
        let folder = load(&request(dir.path(), "d")).unwrap_err();
        assert_eq!(
            folder.message,
            format!("The vault password file provided '{shown}' can not be a directory")
        );
        let empty = write(dir.path(), "empty", b"\n", 0o600);
        let err = load(&request(&empty, "d")).unwrap_err();
        assert_eq!(
            err.message,
            format!("Invalid vault password was provided from file ({shown}/empty)")
        );
    }

    #[test]
    fn every_source_is_collected_config_first() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "a", b"one\n", 0o600);
        let b = write(dir.path(), "b", b"two\n", 0o600);
        let c = write(dir.path(), "c", b"three\n", 0o600);
        let got = load(&Request {
            password_files: vec![b],
            ids: vec![format!("x@{}", a.display())],
            config_file: Some(c),
            ..Request::default()
        })
        .unwrap();
        assert_eq!(got.0, [b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]);
    }
}
