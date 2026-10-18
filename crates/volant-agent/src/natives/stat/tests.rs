// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::{Value, json};

use volant_protocol::TaskResult;

use super::super::common::golden::{Scratch, differences, within};
use super::*;

/// The index's volatile keys, plus the `lsattr` ones: the flags depend on the file system.
const VOLATILE: &[&str] = &[
    "stat.atime",
    "stat.mtime",
    "stat.ctime",
    "stat.inode",
    "stat.dev",
    "stat.version",
    "stat.attr_flags",
    "stat.attributes",
];

fn ask(args: &Value, context: &Context) -> NativeRun {
    run(args.as_object().unwrap(), context, &|| false)
}

/// Every recorded `stat` case, on the fixture the golden play builds.
#[test]
fn stat_answers_every_recorded_case_like_the_reference() {
    let scratch = Scratch::new("stat");
    scratch.fixture();
    let cases = [
        (
            "stat-file",
            include_str!("../../../../volant/tests/golden/module/stat-file.json"),
            json!({"path": "<golden-tmp>/f.txt"}),
        ),
        (
            "stat-dir",
            include_str!("../../../../volant/tests/golden/module/stat-dir.json"),
            json!({"path": "<golden-tmp>"}),
        ),
        (
            "stat-link-follow",
            include_str!("../../../../volant/tests/golden/module/stat-link-follow.json"),
            json!({"path": "<golden-tmp>/l", "follow": true}),
        ),
        (
            "stat-link-nofollow",
            include_str!("../../../../volant/tests/golden/module/stat-link-nofollow.json"),
            json!({"path": "<golden-tmp>/l"}),
        ),
        (
            "stat-missing",
            include_str!("../../../../volant/tests/golden/module/stat-missing.json"),
            json!({"path": "<golden-tmp>/nope"}),
        ),
        (
            "stat-plugin",
            include_str!("../../../../volant/tests/golden/module/stat-plugin.json"),
            json!({"path": "<golden-tmp>/f.txt", "follow": false, "get_checksum": true,
                   "checksum_algorithm": "sha1"}),
        ),
        (
            "stat-bare",
            include_str!("../../../../volant/tests/golden/module/stat-bare.json"),
            json!({"path": "<golden-tmp>/f.txt", "get_checksum": false, "get_mime": false,
                   "get_attributes": false}),
        ),
    ];
    let mut found = Vec::new();
    for (case, recording, args) in cases {
        let args: Value =
            serde_json::from_str(&args.to_string().replace("<golden-tmp>", &scratch.0)).unwrap();
        match ask(&args, &Context::default()) {
            NativeRun::Done(result) => found.extend(
                differences(recording, result.0, &scratch, VOLATILE)
                    .into_iter()
                    .map(|d| format!("{case}: {d}")),
            ),
            NativeRun::Fallback(why) => found.push(format!("{case}: handed back, {why}")),
            NativeRun::Cancelled => found.push(format!("{case}: cancelled")),
        }
    }
    assert!(found.is_empty(), "{found:#?}");
}

/// A long checksum stops at the timeout and the cancel; what stat cannot answer is handed back.
#[test]
fn a_long_checksum_and_a_hand_back_follow_the_native_s_limits() {
    {
        let scratch = Scratch::new("stat-sparse");
        let image = scratch.path("disk.img");
        fs::File::create(&image).unwrap().set_len(64 << 30).unwrap();
        let args = json!({"path": image, "get_mime": false, "get_attributes": false});
        let context = Context {
            timeout: Some(std::time::Duration::from_secs(1)),
            ..Context::default()
        };
        let timed = args.clone();
        let NativeRun::Done(result) = within(30, move || ask(&timed, &context)) else {
            panic!("no answer")
        };
        assert_eq!(result, TaskResult::timed_out(1));
        let cancelled = within(30, move || {
            run(args.as_object().unwrap(), &Context::default(), &|| true)
        });
        assert!(matches!(cancelled, NativeRun::Cancelled));
    }
    {
        let scratch = Scratch::new("stat-back");
        let file = scratch.fixture();
        let mut environment = Context::default();
        environment
            .environment
            .insert("PATH".into(), "/nowhere".into());
        for (args, context) in [
            (
                json!({"path": file, "checksum_algorithm": "md5"}),
                Context::default(),
            ),
            (json!({"path": file}), environment),
            (json!({"path": file, "follow": "yes"}), Context::default()),
            (json!({"path": file, "dest": file}), Context::default()),
            (json!({"path": "~/f.txt"}), Context::default()),
            (json!({"path": "$HOME/f.txt"}), Context::default()),
            (
                json!({"path": file, "get_checksum": false, "checksum_algorithm": "sha3"}),
                Context::default(),
            ),
            (
                json!({"path": file, "get_checksum": false, "checksum": "SHA256"}),
                Context::default(),
            ),
            (
                json!({"path": file, "get_checksum": false, "checksum_algorithm": 1}),
                Context::default(),
            ),
        ] {
            assert!(
                matches!(ask(&args, &context), NativeRun::Fallback(_)),
                "{args}"
            );
        }
        assert!(matches!(
            ask(
                &json!({"path": file, "checksum_algorithm": "md5", "get_checksum": false}),
                &Context::default()
            ),
            NativeRun::Done(_)
        ));
    }
}
