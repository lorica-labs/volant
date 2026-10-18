// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::python::ModuleFacts;
use crate::testing::scratch;

/// A union of one module whose zip is `bytes`, and one answer of each kind for the
/// collections, so the manifest is read back whole.
fn entry(root: &Path, bytes: &[u8]) -> Entry {
    let core = root.join("site/ansible/module_utils/basic.py");
    let files = root.join("coll/ansible_collections/ns/c/FILES.json");
    for (path, text) in [(&core, "basic"), (&files, "{\"files\": [0]}")] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let modules = BTreeMap::from([(
        "ns.c.m".to_string(),
        ModuleFacts {
            module_fqn: "ansible_collections.ns.c.plugins.modules.m".into(),
            profile: "legacy".into(),
            rlimit_nofile: 0,
            extensions: Map::new(),
            core: false,
        },
    )]);
    let resolved = BTreeMap::from([
        (
            "ns.c.m".to_string(),
            Resolved::Module {
                fqcn: "ns.c.m".into(),
                collection: Some(("ns.c".into(), "1.0.0".into())),
            },
        ),
        (
            "ns.c.act".to_string(),
            Resolved::ActionPlugin {
                fqcn: "ns.c.act".into(),
                with_module: true,
            },
        ),
        (
            "ns.c.gone".to_string(),
            Resolved::Unusable {
                reason: "removed".into(),
            },
        ),
        (
            "no.such.m".to_string(),
            Resolved::Missing {
                collection: Some("no.such".into()),
            },
        ),
        (
            "ns.c.none".to_string(),
            Resolved::Missing { collection: None },
        ),
    ]);
    Entry {
        union: Union {
            hash: blake3::hash(bytes).to_hex().to_string(),
            zip_b64: volant_protocol::encoding::b64_encode(bytes),
            modules,
            refused: BTreeMap::from([("ns.c.gone".to_string(), "removed".to_string())]),
            natives: crate::python::Natives::default(),
        },
        sources: [core, files]
            .iter()
            .map(|path| Source::now(path).unwrap())
            .collect(),
        resolved,
    }
}

fn interpreter() -> InterpreterId {
    InterpreterId {
        path: "/venv/bin/python".into(),
        real: "/usr/bin/python3.12".into(),
        len: 1,
        mtime_ns: 2,
    }
}

fn some_key() -> CacheKey {
    key_from(
        None,
        &interpreter(),
        &BTreeSet::from(["ns.c.m".to_string()]),
        &["ns.c.m".to_string()],
        &[],
        &[],
        Path::new("/work"),
    )
}

/// Moves `path`'s mtime a second back without touching its bytes or its length.
fn age(path: &Path) {
    let file = fs::File::options().write(true).open(path).unwrap();
    let mtime = file.metadata().unwrap().modified().unwrap();
    file.set_modified(mtime - Duration::from_secs(1)).unwrap();
}

/// A second run hits the cache; a source rewritten in place or an upgraded collection misses it.
#[test]
fn a_cache_hits_until_a_source_or_collection_changes() {
    {
        let root = scratch();
        let dir = root.path().join("unions");
        let stored = entry(root.path(), b"PK\x03\x04 zip");
        store(&dir, &some_key(), &stored).unwrap();
        let loaded = load(&dir, &some_key()).expect("nothing changed");
        assert_eq!(loaded, stored);
        assert_eq!(loaded.union.hash, stored.union.hash);
    }
    {
        let root = scratch();
        let dir = root.path().join("unions");
        let stored = entry(root.path(), b"PK\x03\x04 zip");
        store(&dir, &some_key(), &stored).unwrap();
        let core = &stored.sources[0].path;
        fs::write(core, "BASIC").unwrap();
        age(core);
        assert_eq!(Source::now(core).unwrap().len, stored.sources[0].len);
        assert_eq!(load(&dir, &some_key()), None);
    }
    {
        let root = scratch();
        let dir = root.path().join("unions");
        let stored = entry(root.path(), b"PK\x03\x04 zip");
        store(&dir, &some_key(), &stored).unwrap();
        let files = &stored.sources[1].path;
        fs::write(files, "{\"files\": [1]}").unwrap();
        age(files);
        assert_eq!(load(&dir, &some_key()), None);
    }
}

/// A rebuilt controller and every non-file input have a key of their own.
#[test]
fn a_controller_or_environment_change_is_another_key() {
    {
        let build = |mtime_ns| Source {
            path: "/bin/volant".into(),
            len: 10,
            mtime_ns,
        };
        let with = |controller: &Source| {
            key_from(
                Some(controller),
                &interpreter(),
                &BTreeSet::new(),
                &[],
                &[],
                &[],
                Path::new("/work"),
            )
        };
        assert_eq!(with(&build(1)), with(&build(1)));
        assert_ne!(with(&build(1)), with(&build(2)), "a rebuild");
        let longer = Source {
            len: 11,
            ..build(1)
        };
        assert_ne!(with(&build(1)), with(&longer), "another size");
    }
    {
        let modules = BTreeSet::from(["ping".to_string()]);
        let with = |env: &[(&str, &str)], cfg: &[(PathBuf, Vec<u8>)], cwd: &str| {
            let env: Vec<(OsString, OsString)> = env
                .iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect();
            key_from(
                None,
                &interpreter(),
                &modules,
                &[],
                &env,
                cfg,
                Path::new(cwd),
            )
        };
        let base = with(&[("TERM", "xterm")], &[], "/work");
        assert_eq!(base, with(&[("TERM", "dumb")], &[], "/work"));
        for other in [
            with(
                &[("ANSIBLE_MODULE_COMPRESSION", "ZIP_STORED")],
                &[],
                "/work",
            ),
            with(&[("ANSIBLE_COLLECTIONS_PATH", "/c")], &[], "/work"),
            with(&[("PYTHONPATH", "/p")], &[], "/work"),
            with(
                &[],
                &[("ansible.cfg".into(), b"[defaults]\n".to_vec())],
                "/work",
            ),
            with(&[], &[], "/elsewhere"),
            key_from(
                None,
                &InterpreterId {
                    mtime_ns: 3,
                    ..interpreter()
                },
                &modules,
                &[],
                &[],
                &[],
                Path::new("/work"),
            ),
            // A Nix profile's `python3`: the same path and size, another store path behind it.
            key_from(
                None,
                &InterpreterId {
                    real: "/nix/store/other-python3/bin/python3".into(),
                    ..interpreter()
                },
                &modules,
                &[],
                &[],
                &[],
                Path::new("/work"),
            ),
            key_from(
                None,
                &interpreter(),
                &BTreeSet::from(["ping".to_string(), "stat".to_string()]),
                &[],
                &[],
                &[],
                Path::new("/work"),
            ),
        ] {
            assert_ne!(base, other);
        }
    }
}

/// Writes `text` to `path` and sets its mtime `ago` seconds back, old enough to vouch for.
fn written(path: &Path, text: &str, ago: u64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(ago))
        .unwrap();
}

/// The key over `dirs` and nothing else, the plugin files read as `key` reads them.
fn plugin_key(dirs: &[PathBuf]) -> CacheKey {
    key_from(
        None,
        &interpreter(),
        &BTreeSet::from(["stat".to_string()]),
        &[],
        &[],
        &plugin_files(dirs, SystemTime::now() + Duration::from_secs(3)).expect("nothing recent"),
        Path::new("/work"),
    )
}

/// A library module is another key, a too-recent plugin file gives none, and an edit behind a link is seen.
#[test]
fn plugin_files_decide_the_key() {
    {
        let root = scratch();
        let (play, role) = (root.path().join("play"), root.path().join("role"));
        fs::create_dir_all(&play).unwrap();
        fs::create_dir_all(&role).unwrap();
        let both = [play.clone(), role.clone()];
        let mut seen = vec![plugin_key(&both)];
        assert_ne!(
            plugin_key(&[role.clone(), play.clone()]),
            seen[0],
            "the order"
        );
        let stat = play.join("library").join("stat.py");
        written(&stat, "one", 100);
        seen.push(plugin_key(&both));
        written(&stat, "two", 50);
        seen.push(plugin_key(&both));
        written(&role.join("module_utils").join("net").join("a.py"), "", 100);
        seen.push(plugin_key(&both));
        for (i, one) in seen.iter().enumerate() {
            for other in &seen[i + 1..] {
                assert_ne!(one, other);
            }
        }
        assert_eq!(plugin_key(&both), seen[3], "nothing changed since");
    }
    {
        let root = scratch();
        fs::create_dir_all(root.path().join("library")).unwrap();
        fs::write(root.path().join("library").join("m.py"), "x").unwrap();
        let dirs = [root.path().to_path_buf()];
        assert!(plugin_files(&dirs, SystemTime::now()).is_none());
        assert!(plugin_files(&dirs, SystemTime::now() + Duration::from_secs(3)).is_some());
    }
    #[cfg(unix)]
    {
        let root = scratch();
        let (role, shared) = (root.path().join("role"), root.path().join("shared"));
        written(&shared.join("m.py"), "one", 100);
        let utils = role.join("module_utils");
        fs::create_dir_all(&utils).unwrap();
        std::os::unix::fs::symlink(&shared, utils.join("shared")).unwrap();
        std::os::unix::fs::symlink(&utils, utils.join("again")).unwrap();
        let before = plugin_key(std::slice::from_ref(&role));
        written(&shared.join("m.py"), "three", 100);
        assert_ne!(plugin_key(std::slice::from_ref(&role)), before);
    }
}

/// A corrupt, older-format or re-hashed entry is rebuilt, never loaded.
#[test]
fn an_entry_that_cannot_be_trusted_is_a_miss() {
    {
        let root = scratch();
        let dir = root.path().join("unions");
        let key = some_key();
        let stored = entry(root.path(), b"PK\x03\x04 zip");
        store(&dir, &key, &stored).unwrap();
        let zip = dir.join(format!("{}.zip", key.0));
        let bytes = fs::read(&zip).unwrap();
        fs::write(&zip, &bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(load(&dir, &key), None, "a truncated zip");

        store(&dir, &key, &stored).unwrap();
        let manifest = dir.join(format!("{}.json", key.0));
        fs::write(&manifest, "{\"format\": 1, \"hash\": ").unwrap();
        assert_eq!(load(&dir, &key), None, "a manifest that is not JSON");

        store(&dir, &key, &stored).unwrap();
        let text = fs::read_to_string(&manifest).unwrap();
        fs::write(
            &manifest,
            text.replace(&format!("\"format\":{FORMAT}"), "\"format\":99"),
        )
        .unwrap();
        assert_eq!(load(&dir, &key), None, "another format");

        fs::remove_file(&manifest).unwrap();
        assert_eq!(load(&dir, &key), None, "no entry");
    }
    {
        let root = scratch();
        let dir = root.path().join("unions");
        let key = some_key();
        let mut stored = entry(root.path(), b"PK zip");
        stored
            .union
            .modules
            .values_mut()
            .for_each(|f| f.core = true);
        store(&dir, &key, &stored).unwrap();
        assert!(load(&dir, &key).is_some(), "the current format reads back");
        let manifest = dir.join(format!("{}.json", key.0));
        let text = fs::read_to_string(&manifest).unwrap();
        let old = text
            .replace(&format!("\"format\":{FORMAT}"), "\"format\":1")
            .replace(",\"core\":true", "")
            .replace("\"core\":true,", "");
        assert!(!old.contains("core"), "{old}");
        fs::write(&manifest, old).unwrap();
        assert_eq!(load(&dir, &key), None);
    }
    {
        let root = scratch();
        let dir = root.path().join("unions");
        let key = some_key();
        let stored = entry(root.path(), &long_zip());
        store(&dir, &key, &stored).unwrap();
        let manifest = dir.join(format!("{}.json", key.0));
        let text = fs::read_to_string(&manifest).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let hash = format!("\"deflated\":\"{}\",", value["deflated"].as_str().unwrap());
        let (three, two) = ("\"format\":3", "\"format\":2");
        assert!(text.contains(three) && text.contains(&hash), "{text}");
        for (why, old) in [
            ("format 2 with a hash", text.replace(three, two)),
            ("format 2", text.replace(three, two).replace(&hash, "")),
            ("format 3 without a hash", text.replace(&hash, "")),
        ] {
            fs::write(&manifest, old).unwrap();
            assert_eq!(load(&dir, &key), None, "{why}");
        }
        fs::write(&manifest, text).unwrap();
        assert_eq!(load(&dir, &key), Some(stored));
    }
    {
        let root = scratch();
        let dir = root.path().join("unions");
        let key = some_key();
        store(&dir, &key, &entry(root.path(), b"PK\x03\x04 zip")).unwrap();
        let zip = dir.join(format!("{}.zip", key.0));
        let mut bytes = fs::read(&zip).unwrap();
        bytes[4] ^= 1;
        fs::write(&zip, bytes).unwrap();
        assert_eq!(load(&dir, &key), None);
    }
}

/// A zip long enough to be sent deflated.
const LONG_ZIP: &[u8] = b"PK\x03\x04 a stored zip, as the helper builds it; ";

fn long_zip() -> Vec<u8> {
    LONG_ZIP.repeat(200)
}

/// The bytes a frame carries, decoded as the agent decodes them.
fn unpacked(frame: &BlobFrame) -> Vec<u8> {
    match frame.encoding {
        volant_protocol::BlobEncoding::Raw => frame.bytes.clone(),
        volant_protocol::BlobEncoding::Deflate => {
            volant_protocol::encoding::inflate(&frame.bytes, frame.len as usize).unwrap()
        }
    }
}

/// The frame kept for a union is found by the zip's hash and comes from the stored deflated
/// form without decoding the union again.
#[tokio::test]
async fn the_frame_kept_for_a_union_is_found_by_its_content_alone() {
    let root = scratch();
    let dir = root.path().join("unions");
    let zip = long_zip();
    let stored = entry(root.path(), &zip);
    store(&dir, &some_key(), &stored).unwrap();
    let kept = crate::agent::frame(&stored.union.hash, "not base64 at all")
        .await
        .expect("kept by the store");
    assert_eq!(kept.encoding, volant_protocol::BlobEncoding::Deflate);
    assert_eq!(unpacked(&kept), zip);

    let other = [b"another zip ".as_slice(), &zip].concat();
    let other_hash = blake3::hash(&other).to_hex().to_string();
    let fresh = crate::agent::frame(&other_hash, &volant_protocol::encoding::b64_encode(&other))
        .await
        .unwrap();
    assert_eq!(unpacked(&fresh), other);
    let again = crate::agent::frame(&stored.union.hash, "").await.unwrap();
    assert_eq!(unpacked(&again), zip);
}

/// A deflated form that is missing, or is not the one the manifest names, is made again from
/// the zip and written back, and the frame holds the zip: the entry is still a hit.
#[tokio::test]
async fn a_missing_or_corrupt_deflated_form_is_made_again_never_sent() {
    let root = scratch();
    let dir = root.path().join("unions");
    let key = some_key();
    let zip = long_zip();
    let stored = entry(root.path(), &zip);
    store(&dir, &key, &stored).unwrap();
    let path = dir.join(format!("{}.deflate", key.0));
    let good = fs::read(&path).unwrap();
    let wrong = volant_protocol::encoding::deflate(&[b"not this union ".as_slice(), &zip].concat());
    for (why, damage) in [
        ("missing", None),
        ("another stream", Some(wrong)),
        ("truncated", Some(good[..good.len() / 2].to_vec())),
    ] {
        match damage {
            None => fs::remove_file(&path).unwrap(),
            Some(bytes) => fs::write(&path, bytes).unwrap(),
        }
        crate::agent::forget_frames();
        assert_eq!(load(&dir, &key), Some(stored.clone()), "{why}");
        let kept = crate::agent::frame(&stored.union.hash, "not base64 at all")
            .await
            .expect(why);
        assert_eq!(unpacked(&kept), zip, "{why}");
        assert_eq!(fs::read(&path).unwrap(), good, "{why}: written back");
    }
}

/// The cache is private to this user; a linked or shared directory is refused.
#[test]
fn the_cache_directory_is_private_to_this_user() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch();
        let dir = root.path().join("unions");
        let key = some_key();
        store(&dir, &key, &entry(root.path(), b"PK\x03\x04 zip")).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(format!("{}.zip", key.0))), 0o600);
        assert_eq!(mode(&dir.join(format!("{}.json", key.0))), 0o600);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(load(&dir, &key), None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch();
        let target = root.path().join("dot-ssh");
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let volant = root.path().join("volant");
        fs::create_dir_all(&volant).unwrap();
        let dir = volant.join("unions");
        std::os::unix::fs::symlink(&target, &dir).unwrap();
        let key = some_key();
        assert!(store(&dir, &key, &entry(root.path(), b"PK")).is_err());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        assert_eq!(load(&dir, &key), None);

        fs::remove_file(&dir).unwrap();
        store(&dir, &key, &entry(root.path(), b"PK")).unwrap();
        fs::set_permissions(&volant, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(load(&dir, &key), None);
        fs::set_permissions(&volant, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(
            load(&dir, &key).is_some(),
            "a group-writable volant/ is the 002 umask"
        );
    }
}

/// An entry left alone for a month is swept; nothing else is.
#[test]
fn the_sweep_removes_old_entries_and_only_its_own_files() {
    {
        let root = scratch();
        let dir = root.path().join("unions");
        let key = some_key();
        store(&dir, &key, &entry(root.path(), b"PK\x03\x04 zip")).unwrap();
        sweep(&dir, SystemTime::now() + STALE - Duration::from_secs(60));
        assert!(load(&dir, &key).is_some(), "a recent entry is kept");
        sweep(&dir, SystemTime::now() + STALE + Duration::from_secs(60));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }
    {
        let root = scratch();
        let dir = root.path().join("unions");
        let key = some_key();
        store(&dir, &key, &entry(root.path(), b"PK")).unwrap();
        let hex = "a".repeat(64);
        for name in [
            "id_ed25519".to_string(),
            format!("{hex}.zip.bak"),
            format!("{hex}.json.tmp.x.1"),
            format!("{}.zip", "A".repeat(64)),
        ] {
            fs::write(dir.join(name), "not the cache's").unwrap();
        }
        fs::write(dir.join(format!("{hex}.json.tmp.12.3")), "a dead run's").unwrap();
        sweep(&dir, SystemTime::now() + STALE + Duration::from_secs(60));
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|f| f.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                format!("{}.zip", "A".repeat(64)),
                format!("{hex}.json.tmp.x.1"),
                format!("{hex}.zip.bak"),
                "id_ed25519".to_string(),
            ]
        );
    }
}

/// The interpreter is found without running it, a shim is never a key, and a link names where it leads.
#[test]
fn the_interpreter_is_identified_without_running_it() {
    {
        let root = scratch();
        let python = root.path().join("venv/bin/python");
        fs::create_dir_all(python.parent().unwrap()).unwrap();
        fs::write(&python, "not a python").unwrap();
        let venv = root.path().join("venv").display().to_string();
        let found = interpreter_without_running(None, Some(&venv)).unwrap();
        assert_eq!(found.path, python);
        assert_eq!(found.len, 12);
        let explicit = python.display().to_string();
        assert_eq!(
            interpreter_without_running(Some(&explicit), None).unwrap(),
            found
        );
        assert_eq!(
            interpreter_without_running(Some("/nowhere/python"), None),
            None
        );
    }
    {
        let root = scratch();
        let shim = root.path().join("shims/python3");
        fs::create_dir_all(shim.parent().unwrap()).unwrap();
        fs::write(
            &shim,
            "#!/usr/bin/env bash\nexec pyenv exec python3 \"$@\"\n",
        )
        .unwrap();
        let explicit = shim.display().to_string();
        assert_eq!(interpreter_without_running(Some(&explicit), None), None);
    }
    #[cfg(unix)]
    {
        let root = scratch();
        let real = root.path().join("store/python3");
        let link = root.path().join("profile/python3");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::write(&real, "ELF").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let found = interpreter_without_running(Some(&link.display().to_string()), None).unwrap();
        assert_eq!(found.path, link);
        assert_eq!(found.real, fs::canonicalize(&real).unwrap());
    }
}

/// An ansible.cfg directory is read for the key, and its text is expanded first.
#[test]
fn the_ansible_config_is_read_and_expanded_for_the_key() {
    {
        let root = scratch();
        let cfg = root.path().join("ansible.cfg");
        let key_now = || {
            let found = configs(Some(root.path().as_os_str()), None);
            key_from(
                None,
                &interpreter(),
                &BTreeSet::new(),
                &[],
                &[],
                &found,
                Path::new("/work"),
            )
        };
        fs::write(&cfg, "[defaults]\ncollections_path = ./colls\n").unwrap();
        let before = key_now();
        fs::write(&cfg, "[defaults]\ncollections_path = ./colls-next\n").unwrap();
        assert_ne!(key_now(), before);
        // A file named directly, and one that is not there, are both what they are.
        assert_eq!(configs(Some(cfg.as_os_str()), None)[0].0, cfg);
        let home = root.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(".ansible.cfg"), "[defaults]\n").unwrap();
        let fell_through = configs(
            Some(root.path().join("absent.cfg").as_os_str()),
            Some(&home),
        );
        assert!(
            fell_through
                .iter()
                .any(|(path, _)| *path == home.join(".ansible.cfg")),
            "{fell_through:?}"
        );
    }
    {
        let var = |name: &str| (name == "PROJ").then(|| "/srv/proj".to_string());
        let home = Some(Path::new("/home/u"));
        assert_eq!(
            expanded("~/a/ansible.cfg", home, var),
            "/home/u/a/ansible.cfg"
        );
        assert_eq!(
            expanded("$PROJ/ansible.cfg", home, var),
            "/srv/proj/ansible.cfg"
        );
        assert_eq!(expanded("${PROJ}/x/~", home, var), "/srv/proj/x/~");
        assert_eq!(expanded("$NOPE/${NOPE}/$", home, var), "$NOPE/${NOPE}/$");
        assert_eq!(expanded("~other/a", home, var), "~other/a");

        let root = scratch();
        let cfg = root.path().join("proj/ansible.cfg");
        fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        fs::write(
            &cfg,
            "[defaults]
    ",
        )
        .unwrap();
        let found = configs(Some("~/proj/ansible.cfg".as_ref()), Some(root.path()));
        assert!(found.iter().any(|(path, _)| *path == cfg), "{found:?}");
    }
}
