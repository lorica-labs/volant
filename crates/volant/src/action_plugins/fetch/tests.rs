// SPDX-License-Identifier: GPL-3.0-or-later
use crate::testing::scratch;
use std::collections::BTreeSet;

use serde_json::json;
use volant_protocol::encoding::b64_encode;

use super::*;
use crate::config::HostNamedPaths;
use crate::vars::HostVars;

fn start_with(
    args: Value,
    untrusted: &BTreeSet<String>,
    host: &str,
    escalated: bool,
) -> Box<dyn Plugin> {
    let mut item_vars = HostVars::default();
    item_vars.insert("inventory_hostname".into(), json!(host));
    start_vars(args, untrusted, &item_vars, escalated)
}

fn start_vars(
    args: Value,
    untrusted: &BTreeSet<String>,
    item_vars: &HostVars,
    escalated: bool,
) -> Box<dyn Plugin> {
    start_within(
        args,
        untrusted,
        item_vars,
        escalated,
        HostNamedPaths::Allow,
        None,
    )
}

/// [`start_vars`] under a `host_named_paths` of `mode` and a `host_named_dest_dirs` of `dirs`.
fn start_within(
    args: Value,
    untrusted: &BTreeSet<String>,
    item_vars: &HostVars,
    escalated: bool,
    mode: HostNamedPaths,
    dirs: Option<&[PathBuf]>,
) -> Box<dyn Plugin> {
    let Value::Object(args) = args else {
        unreachable!()
    };
    let running = HostVars::default();
    let dir = std::env::temp_dir();
    let templar = crate::template::Templar::new(dir.clone());
    let origin = crate::compile::Origin::default();
    let mut warnings = Vec::new();
    start(Context {
        link: &crate::action_plugins::LinkFacts::default(),
        args: &args,
        args_untrusted: untrusted,
        running_vars: &running,
        delegated: false,
        escalated,
        local: false,
        item_vars,
        templar: &templar,
        origin: &origin,
        playbook_dir: &dir,
        host_named_paths: mode,
        host_named_dest_dirs: dirs,
        warnings: &mut warnings,

        environment: None,
        timeout: None,
        check_mode: false,
        diff: false,
        async_val: 0,
    })
}

fn start_plain(args: Value) -> Box<dyn Plugin> {
    start_with(args, &BTreeSet::new(), "probe-hostname", false)
}

fn result(value: Value) -> TaskResult {
    let Value::Object(map) = value else {
        unreachable!()
    };
    TaskResult(map)
}

/// The plugin run to its end against a host holding `data` at `reported`, the path the host
/// names back: each sub-task it asks for, and the result it ends with.
fn drive(plugin: &mut dyn Plugin, data: &[u8], reported: &str) -> (Vec<Sub>, Value) {
    let mut subs = Vec::new();
    let mut last = None;
    loop {
        match plugin.next(last.take()) {
            Step::Done(result) => return (subs, Value::Object(result.0)),
            Step::Run(sub) => {
                last = Some(result(match sub.module {
                    "stat" => json!({"changed": false, "stat": {"exists": true,
                        "isdir": false, "path": reported, "checksum": sha1_hex(data)}}),
                    "slurp" => json!({"changed": false, "content": b64_encode(data),
                        "source": reported, "encoding": "base64"}),
                    other => panic!("unexpected sub-task {other}"),
                }));
                subs.push(sub);
            }
            other => panic!("a step this plugin never takes: {other:?}"),
        }
    }
}

/// The temporary files left in `dir`.
fn temps(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".volant-tmp"))
        .collect()
}

fn modules(subs: &[Sub]) -> Vec<&str> {
    subs.iter().map(|s| s.module).collect()
}

/// A `dest` a managed host named is written as the reference writes it; with the protection on,
/// it is refused before the host is asked anything and nothing is created under it.
#[rstest::rstest]
#[case::default(HostNamedPaths::Allow, None, true)]
#[case::empty_list(HostNamedPaths::Allow, Some(Vec::new()), false)]
#[case::contained(HostNamedPaths::Contained, None, false)]
#[case::refuse(HostNamedPaths::Refuse, None, false)]
fn a_dest_a_host_named_is_written_unless_the_protection_is_on(
    #[case] mode: HostNamedPaths,
    #[case] dirs: Option<Vec<PathBuf>>,
    #[case] written: bool,
    #[values(false, true)] escalated: bool,
) {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let dest = dir.join("chosen");
    let untrusted: BTreeSet<String> = ["dest".to_string()].into();
    let mut item_vars = HostVars::default();
    item_vars.insert("inventory_hostname".into(), json!("probe-hostname"));
    let mut plugin = start_within(
        json!({"src": "/etc/one.txt", "dest": dest.display().to_string()}),
        &untrusted,
        &item_vars,
        escalated,
        mode,
        dirs.as_deref(),
    );
    let (subs, result) = drive(plugin.as_mut(), b"x", "/etc/one.txt");
    if written {
        assert_eq!(result["changed"], json!(true), "{result}");
        let file = dest.join("probe-hostname/etc/one.txt");
        assert_eq!(std::fs::read(file).unwrap(), b"x");
        return;
    }
    assert!(subs.is_empty(), "the host was asked: {:?}", modules(&subs));
    assert_eq!(result["failed"], json!(true));
    assert_eq!(
        result["msg"],
        json!(
            "the 'dest' of this task was named by a managed host, and a controller path a host chose is never written unless host_named_dest_dirs names a directory above it"
        )
    );
    assert!(!dest.exists(), "{} was created", dest.display());
}

/// A fetch of `/etc/one.txt` whose `dest` a host named, under `host_named_dest_dirs = dirs`.
#[cfg(unix)]
fn fetch_named(dest: &str, dirs: &[PathBuf]) -> (Vec<Sub>, Value) {
    let untrusted: BTreeSet<String> = ["dest".to_string()].into();
    let mut item_vars = HostVars::default();
    item_vars.insert("inventory_hostname".into(), json!("probe-hostname"));
    let mut plugin = start_within(
        json!({"src": "/etc/one.txt", "dest": dest}),
        &untrusted,
        &item_vars,
        false,
        HostNamedPaths::Allow,
        Some(dirs),
    );
    drive(plugin.as_mut(), b"x", "/etc/one.txt")
}

/// A `dest` a host named is written when it lies strictly inside a directory of the setting,
/// and the directory may not exist yet below it.
#[cfg(unix)]
#[test]
fn a_dest_a_host_named_is_written_inside_the_directories() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir_guard = scratch();
    let dir = dir_guard.path().canonicalize().unwrap();
    let (other, allowed) = (dir.join("other"), dir.join("allowed"));
    std::fs::create_dir_all(&allowed).unwrap();
    // Private whatever the umask: a group-writable directory of the list holds nothing.
    std::fs::set_permissions(&allowed, std::fs::Permissions::from_mode(0o755)).unwrap();
    let dirs = [other, allowed.clone()];
    let dest = format!("{}/new/dir/", allowed.display());
    let (subs, result) = fetch_named(&dest, &dirs);
    assert_eq!(modules(&subs), ["stat", "slurp"], "{result}");
    assert_eq!(result["changed"], json!(true), "{result}");
    let written = allowed.join("new/dir/probe-hostname/etc/one.txt");
    assert_eq!(std::fs::read(written).unwrap(), b"x");
}

/// Each spelling that leaves the directory is refused before the host is asked anything, names
/// the setting, and creates nothing.
#[cfg(unix)]
#[rstest::rstest]
#[case::relative("got/", "'got/' is not an absolute path")]
#[case::tilde("~/got/", "'~/got/' is not an absolute path")]
#[case::dotdot("{allowed}/../outside/got/", "has a '..' component")]
#[case::outside("{outside}/got/", "is not inside any of the directories")]
#[case::root("{allowed}/", "is not inside any of the directories")]
#[case::sibling("{allowed}-evil/got/", "is not inside any of the directories")]
#[case::link("{allowed}/link/got/", "which is not inside any of the directories")]
#[case::dangling("{allowed}/dangling/got/", "cannot be resolved")]
fn a_dest_a_host_named_outside_the_directories_is_refused(#[case] dest: &str, #[case] says: &str) {
    use std::os::unix::fs::symlink;
    let dir_guard = scratch();
    let dir = dir_guard.path().canonicalize().unwrap();
    let (allowed, outside) = (dir.join("allowed"), dir.join("outside"));
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    symlink(&outside, allowed.join("link")).unwrap();
    symlink(dir.join("nowhere"), allowed.join("dangling")).unwrap();
    let dest = dest
        .replace("{allowed}", &allowed.display().to_string())
        .replace("{outside}", &outside.display().to_string());
    let (subs, result) = fetch_named(&dest, std::slice::from_ref(&allowed));
    assert!(subs.is_empty(), "the host was asked: {:?}", modules(&subs));
    assert_eq!(result["failed"], json!(true), "{result}");
    let msg = result["msg"].as_str().unwrap();
    assert!(msg.contains(says), "{msg}");
    assert!(
        msg.ends_with(&format!("(host_named_dest_dirs = {})", allowed.display())),
        "{msg}"
    );
    for path in [&outside, &dir.join("got"), &dir.join("allowed-evil")] {
        let found = std::fs::read_dir(path).map_or(0, Iterator::count);
        assert_eq!(found, 0, "{} was written", path.display());
    }
}

/// A directory of the setting that other users can write holds nothing, unless it is sticky.
#[cfg(unix)]
#[rstest::rstest]
#[case::world_writable(0o777, false)]
#[case::group_writable(0o770, false)]
#[case::sticky(0o1777, true)]
#[case::private(0o700, true)]
fn a_directory_other_users_write_holds_a_dest_only_when_it_is_sticky(
    #[case] mode: u32,
    #[case] written: bool,
) {
    use std::os::unix::fs::PermissionsExt as _;
    let dir_guard = scratch();
    let allowed = dir_guard.path().canonicalize().unwrap().join("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::set_permissions(&allowed, std::fs::Permissions::from_mode(mode)).unwrap();
    let dest = format!("{}/got/", allowed.display());
    let (_, result) = fetch_named(&dest, std::slice::from_ref(&allowed));
    assert_eq!(result["changed"] == json!(true), written, "{result}");
    if !written {
        assert!(
            result["msg"]
                .as_str()
                .unwrap()
                .contains("which other users can write to")
        );
        assert!(!allowed.join("got").exists());
    }
}

/// Below a directory of the setting, a link that stays inside it and a directory every user can
/// write are refused, since another user could turn either elsewhere; nothing is written.
#[cfg(unix)]
#[rstest::rstest]
#[case::inner_link("{allowed}/inner/got/", "goes through the symbolic link")]
#[case::open_dir("{allowed}/open/got/", "with mode 777")]
fn a_dest_a_host_named_goes_through_no_path_another_user_controls(
    #[case] dest: &str,
    #[case] says: &str,
) {
    use std::os::unix::fs::PermissionsExt as _;
    let dir_guard = scratch();
    let allowed = dir_guard.path().canonicalize().unwrap().join("allowed");
    let (real, open) = (allowed.join("real"), allowed.join("open"));
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(&open).unwrap();
    std::fs::set_permissions(&allowed, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    std::os::unix::fs::symlink(&real, allowed.join("inner")).unwrap();
    let dest = dest.replace("{allowed}", &allowed.display().to_string());
    let (_, result) = fetch_named(&dest, std::slice::from_ref(&allowed));
    assert_eq!(result["failed"], json!(true), "{result}");
    assert!(result["msg"].as_str().unwrap().contains(says), "{result}");
    for path in [&real, &open] {
        assert_eq!(std::fs::read_dir(path).unwrap().count(), 0);
    }
}

/// A link planted after the plugin was built is seen before the write: by the containment
/// check with the protection on, by the walk below `dest` without it.
#[cfg(unix)]
#[rstest::rstest]
#[case::protected(true, "slot", "resolves to")]
#[case::default(false, "slot/probe-hostname", "goes through the symbolic link")]
fn a_link_planted_after_the_first_check_is_seen_before_the_write(
    #[case] protected: bool,
    #[case] planted: &str,
    #[case] says: &str,
) {
    use std::os::unix::fs::PermissionsExt as _;
    let dir_guard = scratch();
    let dir = dir_guard.path().canonicalize().unwrap();
    let (allowed, outside) = (dir.join("allowed"), dir.join("outside"));
    std::fs::create_dir_all(allowed.join("slot/probe-hostname")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::set_permissions(&allowed, std::fs::Permissions::from_mode(0o755)).unwrap();
    let untrusted: BTreeSet<String> = ["dest".to_string()].into();
    let mut item_vars = HostVars::default();
    item_vars.insert("inventory_hostname".into(), json!("probe-hostname"));
    let dirs = [allowed.clone()];
    let dest = format!("{}/slot/", allowed.display());
    let mut plugin = start_within(
        json!({"src": "/etc/one.txt", "dest": dest}),
        &untrusted,
        &item_vars,
        false,
        HostNamedPaths::Allow,
        protected.then_some(&dirs[..]),
    );
    std::fs::remove_dir_all(allowed.join(planted)).unwrap();
    std::os::unix::fs::symlink(&outside, allowed.join(planted)).unwrap();
    let (_, result) = drive(plugin.as_mut(), b"x", "/etc/one.txt");
    assert_eq!(result["failed"], json!(true), "{result}");
    assert!(result["msg"].as_str().unwrap().contains(says), "{result}");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

/// The relative `src` that climbs out of `dest` is refused, and nothing is written where it
/// points.
#[test]
fn a_src_that_climbs_out_of_dest_is_refused_and_nothing_is_written() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let marker = dir.join("esc.txt");
    let dest = dir.join("fetched");
    let climb = format!(
        "{}{}",
        "../".repeat(dest.components().count() + 8),
        marker.display().to_string().trim_start_matches('/')
    );
    for escalated in [false, true] {
        let mut plugin = start_with(
            json!({"src": climb, "dest": dest.display().to_string()}),
            &BTreeSet::new(),
            "probe-hostname",
            escalated,
        );
        let (_, result) = drive(plugin.as_mut(), b"escaped", &climb);
        assert_eq!(result["failed"], json!(true), "{result}");
        assert_eq!(
            result["msg"],
            json!(format!(
                "the fetched path {} is outside '{}'",
                marker.display(),
                dest.display()
            ))
        );
        assert!(!marker.exists(), "{} was written", marker.display());
        assert!(!dest.exists(), "{} was created", dest.display());
    }
}

/// A path the host names back that climbs out of its own directory is refused the same way,
/// even when the playbook's `src` did not climb.
#[test]
fn a_path_the_host_names_back_cannot_climb_out_either() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let dest = dir.join("fetched");
    // Out of `probe-hostname/`, into a sibling host's directory: still under `dest`, and
    // still refused.
    for (escalated, reported) in [(false, "../other/one.txt"), (true, "../../esc.txt")] {
        let mut plugin = start_with(
            json!({"src": "one.txt", "dest": dest.display().to_string()}),
            &BTreeSet::new(),
            "probe-hostname",
            escalated,
        );
        let (_, result) = drive(plugin.as_mut(), b"x", reported);
        assert_eq!(result["failed"], json!(true), "{result}");
        let msg = result["msg"].as_str().unwrap();
        assert!(msg.starts_with("the fetched path "), "{msg}");
        assert!(
            msg.ends_with(&format!(" is outside '{}'", dest.display())),
            "{msg}"
        );
    }
    assert!(!dest.join("other").exists());
    assert!(!dir.join("esc.txt").exists());
}

/// An `inventory_hostname` that is not one path component is refused before anything, and
/// `flat`, which does not use it, is not.
#[test]
fn a_host_name_that_is_not_one_path_component_is_refused() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let dest = dir.join("fetched");
    for host in ["a/b", "..", ".", ""] {
        let mut plugin = start_with(
            json!({"src": "/etc/one.txt", "dest": dest.display().to_string()}),
            &BTreeSet::new(),
            host,
            false,
        );
        let (subs, result) = drive(plugin.as_mut(), b"x", "/etc/one.txt");
        assert!(subs.is_empty(), "{host}: {:?}", modules(&subs));
        assert_eq!(
            result["msg"],
            json!(format!(
                "the inventory_hostname '{host}' is not one path component, and fetch files what it fetches under it"
            ))
        );
    }
    assert!(!dest.exists());
    let flat = dir.join("flat.txt");
    let mut plugin = start_with(
        json!({"src": "/etc/one.txt", "dest": flat.display().to_string(), "flat": true}),
        &BTreeSet::new(),
        "a/b",
        false,
    );
    let (_, result) = drive(plugin.as_mut(), b"x", "/etc/one.txt");
    assert_eq!(result["changed"], json!(true), "{result}");
    assert_eq!(std::fs::read(&flat).unwrap(), b"x");
}

/// A symbolic link planted below `dest` never redirects the write: each case is refused and the
/// link's target is left as it was.
#[cfg(unix)]
#[test]
fn a_symlink_inside_dest_never_redirects_the_write() {
    use std::os::unix::fs::symlink;
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let outside = dir.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("target.txt"), "kept").unwrap();

    let dest = dir.join("by-host");
    std::fs::create_dir_all(&dest).unwrap();
    symlink(&outside, dest.join("probe-hostname")).unwrap();
    let deeper = dir.join("deeper");
    std::fs::create_dir_all(deeper.join("probe-hostname")).unwrap();
    symlink(&outside, deeper.join("probe-hostname/etc")).unwrap();
    let flat = dir.join("flat");
    std::fs::create_dir_all(&flat).unwrap();
    symlink(outside.join("target.txt"), flat.join("one.txt")).unwrap();

    for (args, link) in [
        (
            json!({"src": "/etc/one.txt", "dest": dest.display().to_string()}),
            dest.join("probe-hostname"),
        ),
        (
            json!({"src": "/etc/one.txt", "dest": deeper.display().to_string()}),
            deeper.join("probe-hostname/etc"),
        ),
        (
            json!({"src": "/etc/one.txt", "dest": format!("{}/", flat.display()), "flat": true}),
            flat.join("one.txt"),
        ),
        (
            json!({"src": "/etc/one.txt", "dest": flat.join("one.txt").display().to_string(), "flat": true}),
            flat.join("one.txt"),
        ),
    ] {
        // A `dest` a host named is walked the same way when no protection is on.
        let named: BTreeSet<String> = ["dest".to_string()].into();
        for (escalated, untrusted) in [
            (false, BTreeSet::new()),
            (true, BTreeSet::new()),
            (false, named),
        ] {
            let mut plugin = start_with(args.clone(), &untrusted, "probe-hostname", escalated);
            let (_, result) = drive(plugin.as_mut(), b"planted", "/etc/one.txt");
            assert_eq!(result["failed"], json!(true), "{args}: {result}");
            let msg = result["msg"].as_str().unwrap();
            assert!(
                msg.ends_with(&format!(
                    " goes through the symbolic link {}, and fetch never follows one",
                    link.display()
                )),
                "{msg}"
            );
        }
    }
    assert!(
        !outside.join("etc").exists(),
        "a directory was made through the link"
    );
    assert_eq!(std::fs::read(outside.join("target.txt")).unwrap(), b"kept");
}

/// A fetched file is written under `dest/<inventory_hostname>/<src>`; with the file already
/// there, `stat` alone runs and `changed` is false.
#[test]
fn a_file_is_written_under_its_host_and_left_alone_once_there() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let dest = dir.join("fetched");
    let data = b"{{ 1 + 1 }}\n";
    let sum = sha1_hex(data);
    let args = json!({"src": "/tmp/v/d/one.txt", "dest": dest.display().to_string()});
    let file = dest.join("probe-hostname/tmp/v/d/one.txt");

    let mut plugin = start_plain(args.clone());
    let (subs, result) = drive(plugin.as_mut(), data, "/tmp/v/d/one.txt");
    assert_eq!(modules(&subs), ["stat", "slurp"]);
    assert_eq!(
        Value::Object(subs[0].args.clone()),
        json!({"path": "/tmp/v/d/one.txt", "follow": true, "get_checksum": true,
               "get_mime": false, "get_attributes": false, "checksum_algorithm": "sha1"})
    );
    assert_eq!(
        Value::Object(subs[1].args.clone()),
        json!({"src": "/tmp/v/d/one.txt"})
    );
    assert_eq!(
        result,
        json!({"changed": true, "checksum": sum, "dest": file.display().to_string(),
               "md5sum": null, "remote_checksum": sum, "remote_md5sum": null})
    );
    assert_eq!(std::fs::read(&file).unwrap(), data);
    assert_eq!(temps(file.parent().unwrap()), Vec::<PathBuf>::new());

    let mut plugin = start_plain(args);
    let (subs, result) = drive(plugin.as_mut(), data, "/tmp/v/d/one.txt");
    assert_eq!(modules(&subs), ["stat"]);
    assert_eq!(
        result,
        json!({"changed": false, "checksum": sum, "dest": file.display().to_string(),
               "file": "/tmp/v/d/one.txt", "md5sum": null, "remote_checksum": null})
    );
}

/// Under `become`, `slurp` alone, and the sum taken off its bytes.
#[test]
fn under_become_the_file_is_slurped_without_a_stat() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let dest = dir.join("fetched");
    let args = json!({"src": "/etc/k.yaml", "dest": dest.display().to_string()});
    let mut plugin = start_with(args.clone(), &BTreeSet::new(), "probe-hostname", true);
    let (subs, result) = drive(plugin.as_mut(), b"secret", "/etc/k.yaml");
    assert_eq!(modules(&subs), ["slurp"]);
    assert_eq!(result["changed"], json!(true), "{result}");
    assert_eq!(result["remote_checksum"], json!(sha1_hex(b"secret")));
    let mut plugin = start_with(args, &BTreeSet::new(), "probe-hostname", true);
    let (_, result) = drive(plugin.as_mut(), b"secret", "/etc/k.yaml");
    assert_eq!(result["changed"], json!(false), "{result}");
}

/// `flat` writes `dest` itself, or the source's name inside a `dest` that ends with `/`; a
/// `dest` that is an existing directory without the `/` is refused in the reference's words.
#[test]
fn flat_writes_dest_or_the_name_inside_it() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let into = format!("{}/", dir.join("into").display());
    let mut plugin = start_plain(json!({"src": "/tmp/v/d/one.txt", "dest": into, "flat": true}));
    let (_, result) = drive(plugin.as_mut(), b"one", "/tmp/v/d/one.txt");
    assert_eq!(
        result["dest"],
        json!(dir.join("into/one.txt").display().to_string())
    );
    assert_eq!(std::fs::read(dir.join("into/one.txt")).unwrap(), b"one");

    let mut plugin = start_plain(
        json!({"src": "/tmp/v/d/one.txt", "dest": dir.join("into").display().to_string(),
               "flat": "yes"}),
    );
    let (_, result) = drive(plugin.as_mut(), b"two", "/tmp/v/d/one.txt");
    assert_eq!(
        result["msg"],
        json!(
            "dest is an existing directory, use a trailing slash if you want to fetch src into that directory"
        )
    );
    assert!(dir.join("into").is_dir());

    // With the `/`, a name inside it that is itself a directory, in the reference's words.
    std::fs::create_dir_all(dir.join("into/sub.d")).unwrap();
    let mut plugin = start_plain(json!({"src": "/etc/sub.d", "dest": into, "flat": true}));
    let (_, result) = drive(plugin.as_mut(), b"three", "/etc/sub.d");
    assert_eq!(
        result["msg"],
        json!(format!(
            "calculated dest '{}' is an existing directory, use another path that does not point to an existing directory",
            dir.join("into/sub.d").display()
        ))
    );
    assert!(dir.join("into/sub.d").is_dir());
}

/// A source that is not there follows `fail_on_missing`, and a directory is refused.
#[test]
fn a_missing_source_follows_fail_on_missing_and_a_directory_fails() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let dest = dir.join("fetched").display().to_string();
    let not_found = json!({"failed": true, "msg": "file not found: /nope"});
    for (fail_on_missing, failed) in [(false, None), (true, Some(json!(true)))] {
        let mut plugin =
            start_plain(json!({"src": "/nope", "dest": dest, "fail_on_missing": fail_on_missing}));
        assert!(matches!(
            plugin.next(None),
            Step::Run(Sub { module: "stat", .. })
        ));
        let Step::Run(sub) = plugin.next(Some(result(json!({"stat": {"exists": false}})))) else {
            panic!("no slurp")
        };
        assert_eq!(sub.module, "slurp");
        let Step::Done(done) = plugin.next(Some(result(not_found.clone()))) else {
            panic!("not done")
        };
        assert_eq!(done.0.get("failed").cloned(), failed);
        assert_eq!(
            done.0["msg"],
            json!("the remote file does not exist, not transferring, ignored")
        );
    }
    assert!(!dir.join("fetched").exists());

    // A directory, from `stat`: `ok` and ignored, or failed with `changed: false`.
    for (fail_on_missing, want) in [
        (
            false,
            json!({"msg": format!("{IS_A_DIRECTORY}, not transferring, ignored")}),
        ),
        (
            true,
            json!({"changed": false, "failed": true, "msg": IS_A_DIRECTORY}),
        ),
    ] {
        let mut plugin =
            start_plain(json!({"src": "/etc", "dest": dest, "fail_on_missing": fail_on_missing}));
        plugin.next(None);
        let Step::Done(done) = plugin.next(Some(result(
            json!({"stat": {"exists": true, "isdir": true}}),
        ))) else {
            panic!("not done")
        };
        assert_eq!(Value::Object(done.0), want);
    }

    // Under `become` it is `slurp` that says so, and an answer the agent refused as too big
    // fails in its own words whatever `fail_on_missing` says: the file is there, and nothing
    // was written.
    let too_big = "the module wrote more than 4194304 bytes, which is more than a result can carry (exit status 0)";
    for fail_on_missing in [true, false] {
        for (msg, want, failed) in [
            (
                "source is a directory and must be a file",
                IS_A_DIRECTORY,
                fail_on_missing,
            ),
            (too_big, too_big, true),
        ] {
            let mut plugin = start_with(
                json!({"src": "/etc", "dest": dest, "fail_on_missing": fail_on_missing}),
                &BTreeSet::new(),
                "probe-hostname",
                true,
            );
            plugin.next(None);
            let Step::Done(done) = plugin.next(Some(result(json!({"failed": true, "msg": msg}))))
            else {
                panic!("not done")
            };
            assert_eq!(done.failed(), failed, "{msg}: {:?}", done.0);
            assert_eq!(done.0["msg"], json!(want));
        }
    }
    assert!(!dir.join("fetched").exists());
}

/// A file whose bytes do not match the sum `stat` reported is not written, and the result names
/// both sums; with `validate_checksum: false` it is written.
#[test]
fn a_checksum_mismatch_writes_nothing() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let dest = dir.join("fetched");
    let file = dest.join("probe-hostname/one.txt");
    for validate in [true, false] {
        let mut plugin = start_plain(json!({"src": "/one.txt",
            "dest": dest.display().to_string(), "validate_checksum": validate}));
        plugin.next(None);
        let stated = json!({"stat": {"exists": true, "isdir": false, "path": "/one.txt",
            "checksum": sha1_hex(b"before")}});
        plugin.next(Some(result(stated)));
        let Step::Done(done) = plugin.next(Some(result(json!({"content": b64_encode(b"after"),
            "source": "/one.txt", "encoding": "base64"}))))
        else {
            panic!("not done")
        };
        assert_eq!(done.0["checksum"], json!(sha1_hex(b"after")));
        assert_eq!(done.0["remote_checksum"], json!(sha1_hex(b"before")));
        if validate {
            assert_eq!(done.0["msg"], json!("checksum mismatch"));
            assert_eq!(done.0["failed"], json!(true));
            assert!(!file.exists(), "the mismatched file was written");
        } else {
            assert_eq!(done.0["changed"], json!(true));
            assert_eq!(std::fs::read(&file).unwrap(), b"after");
        }
    }
}

/// A write that fails leaves no temporary file behind.
#[test]
fn a_failed_write_leaves_no_temporary_file() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let dest = dir.join("fetched");
    let file = dest.join("probe-hostname/one.txt");
    std::fs::create_dir_all(&file).unwrap();
    let mut plugin = start_plain(json!({"src": "/one.txt", "dest": dest.display().to_string()}));
    let (_, result) = drive(plugin.as_mut(), b"x", "/one.txt");
    assert_eq!(result["failed"], json!(true), "{result}");
    assert!(
        result["msg"]
            .as_str()
            .unwrap()
            .starts_with("Failed to fetch the file: "),
        "{result}"
    );
    assert_eq!(temps(&dest.join("probe-hostname")), Vec::<PathBuf>::new());
}

/// Two hosts fetching `flat` to the same path at once each write through a temporary file of
/// their own: every result is its own bytes' and consistent, and no temporary file is left.
#[test]
fn two_hosts_fetching_to_one_path_do_not_mix_their_bytes() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let target = dir.join("kubeconfig");
    for round in 0..20 {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let writers: Vec<_> = b"ab"
            .iter()
            .map(|&byte| {
                let barrier = barrier.clone();
                let target = target.clone();
                std::thread::spawn(move || {
                    let data = vec![byte; 256 * 1024 + round];
                    let mut plugin = start_with(
                        json!({"src": "/k.yaml", "dest": target.display().to_string(),
                               "flat": true}),
                        &BTreeSet::new(),
                        "probe-hostname",
                        true,
                    );
                    assert!(matches!(plugin.next(None), Step::Run(_)));
                    barrier.wait();
                    let Step::Done(done) = plugin.next(Some(result(json!({
                        "content": b64_encode(&data), "source": "/k.yaml",
                        "encoding": "base64"}))))
                    else {
                        panic!("not done")
                    };
                    (sha1_hex(&data), Value::Object(done.0))
                })
            })
            .collect();
        let sums: Vec<String> = writers
            .into_iter()
            .map(|w| {
                let (sum, done) = w.join().unwrap();
                assert_eq!(done["failed"], Value::Null, "{done}");
                assert_eq!(done["checksum"], json!(sum), "{done}");
                sum
            })
            .collect();
        let on_disk = sha1_hex(&std::fs::read(&target).unwrap());
        assert!(sums.contains(&on_disk), "round {round}: torn file");
        assert_eq!(temps(&dir), Vec::<PathBuf>::new(), "round {round}");
        std::fs::remove_file(&target).unwrap();
    }
}

/// A file fetched again keeps its mode, as the reference's write in place does.
#[cfg(unix)]
#[test]
fn a_file_fetched_again_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let target = dir.join("kubeconfig");
    std::fs::write(&target, "old").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut plugin =
        start_plain(json!({"src": "/k.yaml", "dest": target.display().to_string(), "flat": true}));
    let (_, done) = drive(plugin.as_mut(), b"new", "/k.yaml");
    assert_eq!(done["changed"], json!(true), "{done}");
    assert_eq!(std::fs::read(&target).unwrap(), b"new");
    let mode = std::fs::metadata(&target).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o600);
}

/// A loop variable `inventory_hostname` a managed host set names the directory of the fetch
/// without `flat`, as the reference does; with the protection on, it is refused.
#[rstest::rstest]
#[case::default(HostNamedPaths::Allow, None, true)]
#[case::empty_list(HostNamedPaths::Allow, Some(Vec::new()), false)]
#[case::refuse(HostNamedPaths::Refuse, None, false)]
fn a_host_directory_a_host_named_is_refused_only_when_the_protection_is_on(
    #[case] mode: HostNamedPaths,
    #[case] dirs: Option<Vec<PathBuf>>,
    #[case] written: bool,
) {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    let dest = dir.join("fetched");
    let mut vars = HostVars::default();
    vars.insert_untrusted("inventory_hostname".into(), json!("other-host"));
    let mut plugin = start_within(
        json!({"src": "/etc/one.txt", "dest": dest.display().to_string()}),
        &BTreeSet::new(),
        &vars,
        false,
        mode,
        dirs.as_deref(),
    );
    let (subs, done) = drive(plugin.as_mut(), b"x", "/etc/one.txt");
    if written {
        assert_eq!(done["changed"], json!(true), "{done}");
        assert_eq!(
            std::fs::read(dest.join("other-host/etc/one.txt")).unwrap(),
            b"x"
        );
        return;
    }
    assert!(subs.is_empty(), "{:?}", modules(&subs));
    assert_eq!(
        done["msg"],
        json!(
            "the 'inventory_hostname' of this item was named by a managed host, and a controller path a host chose is never written"
        )
    );
    assert!(!dest.exists());
}

/// What is refused before any sub-task, in the reference's words.
#[test]
fn missing_or_mistyped_arguments_are_refused() {
    for (args, msg) in [
        (json!({"dest": "/tmp/x"}), "src and dest are required"),
        (json!({"src": "/x"}), "src and dest are required"),
        (
            json!({"src": 1, "dest": "/tmp/x"}),
            "Invalid type supplied for source option, it must be a string",
        ),
        (
            json!({"src": "/x", "dest": ["/tmp"]}),
            "Invalid type supplied for dest option, it must be a string",
        ),
    ] {
        let mut plugin = start_plain(args.clone());
        let Step::Done(done) = plugin.next(None) else {
            panic!("a sub-task was sent for {args}")
        };
        assert_eq!(done.0["msg"], json!(msg), "{args}");
    }
}

/// `.` goes, `..` takes the component before it and stops at the root, and nothing is looked
/// up.
#[test]
fn a_path_is_normalised_without_the_file_system() {
    assert_eq!(normalise(Path::new("/a/./b//../c")), Path::new("/a/c"));
    assert_eq!(normalise(Path::new("/a/../../../x")), Path::new("/x"));
    assert_eq!(normalise(Path::new("a/../../x")), Path::new("../x"));
    let pb = Path::new("/pb");
    assert_eq!(absolute("out", pb), Path::new("/pb/out"));
    assert_eq!(absolute("/abs", pb), Path::new("/abs"));
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    assert_eq!(absolute("~/.kube/config", pb), home.join(".kube/config"));
    assert_eq!(absolute("~", pb), home);
}
