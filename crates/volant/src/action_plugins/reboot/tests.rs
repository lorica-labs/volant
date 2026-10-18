// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

/// A deadline past what an `Instant` holds is held far away instead of panicking the
/// controller: `reboot_timeout: 18446744073709551615` is a `u64` the options accept.
#[test]
fn a_deadline_that_overflows_is_held_far_away() {
    let now = Instant::now();
    assert!(after(now, Duration::MAX) > now + Duration::from_secs(1 << 31));
    assert_eq!(
        after(now, Duration::from_secs(5)),
        now + Duration::from_secs(5)
    );
}

fn map(value: Value) -> Map<String, Value> {
    object(value)
}

/// The reference's defaults, for a `reboot: {}`: ten minutes, `whoami`, the five paths and the
/// message the measured `shutdown` line carried.
#[test]
fn the_defaults_are_the_reference_s() {
    let options = read_options(&Map::new()).expect("no arguments is fine");
    assert_eq!(options.reboot_timeout, 600);
    assert_eq!(options.pre_reboot_delay, 0);
    assert_eq!(options.post_reboot_delay, 0);
    assert_eq!(options.connect_timeout, None);
    assert_eq!(options.test_command, "whoami");
    assert_eq!(options.boot_time_command, None);
    assert_eq!(options.search_paths, DEFAULT_SEARCH_PATHS);
    assert_eq!(options.reboot_command, None);
    assert_eq!(options.msg, "Reboot initiated by Ansible");
}

/// An argument the reference does not accept fails the task naming it, and the delays read
/// as `int()` reads them, a negative one as zero.
#[test]
fn the_arguments_are_read_as_the_reference_reads_them() {
    let Err(msg) = read_options(&map(json!({"reboot_wait": 5, "delay": 1}))) else {
        panic!("two unknown arguments");
    };
    assert_eq!(msg, "Invalid options for reboot: delay,reboot_wait");
    let options = read_options(&map(json!({
        "pre_reboot_delay": "120", "post_reboot_delay": -4, "reboot_timeout": 30.9,
        "connect_timeout": 5, "search_paths": "/opt/sbin", "msg": "kernel update"
    })))
    .expect("all valid");
    assert_eq!(options.pre_reboot_delay, 120);
    assert_eq!(options.post_reboot_delay, 0);
    assert_eq!(options.reboot_timeout, 30);
    assert_eq!(options.connect_timeout, Some(Duration::from_secs(5)));
    assert_eq!(options.search_paths, ["/opt/sbin"]);
    let Err(msg) = read_options(&map(json!({"pre_reboot_delay": "soon"}))) else {
        panic!("not an integer");
    };
    assert_eq!(msg, "invalid literal for int() with base 10: 'soon'");
}

/// The shutdown line follows the reference's tables per distribution and argument, including a
/// `reboot_command` split at its first space.
#[test]
fn the_shutdown_line_follows_the_reference_s_tables() {
    let line = |args: Value, distribution: (&str, &str)| {
        let args = map(args);
        let plugin = Reboot {
            args: &args,
            local: false,
            state: State::Start,
            options: Some(read_options(&args).expect("valid")),
            distribution: (distribution.0.into(), distribution.1.into()),
            previous: String::new(),
            started: None,
            deadline: None,
        };
        (plugin.shutdown_bin(), plugin.shutdown_args())
    };
    assert_eq!(
        line(json!({"pre_reboot_delay": 150}), ("ubuntu", "debian")),
        (
            "shutdown".into(),
            "-r 2 \"Reboot initiated by Ansible\"".into()
        )
    );
    assert_eq!(
        line(json!({}), ("alpine", "alpine")),
        ("reboot".into(), String::new())
    );
    assert_eq!(
        line(json!({"msg": "bye"}), ("void", "void")),
        ("shutdown".into(), "-r +0 \"bye\"".into())
    );
    assert_eq!(
        line(
            json!({"reboot_command": "/usr/bin/systemctl reboot -i"}),
            ("ubuntu", "debian")
        ),
        ("/usr/bin/systemctl".into(), "reboot -i".into())
    );
}
