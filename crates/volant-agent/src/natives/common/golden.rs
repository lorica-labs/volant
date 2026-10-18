// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::os::unix::fs::PermissionsExt;

use serde_json::{Map, Value};

/// A directory under `/var/tmp` (the recordings' file system is not `tmpfs`), removed when
/// dropped.
pub struct Scratch(pub String);

impl Scratch {
    pub fn new(name: &str) -> Self {
        let dir = format!("/var/tmp/volant-native-{name}-{}", std::process::id());
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        Self(dir)
    }

    /// `f.txt` holding `hello\n` in 0644 and `l`, a link to it, as the golden play sets
    /// them up. Returns the file's path.
    pub fn fixture(&self) -> String {
        let file = self.path("f.txt");
        fs::write(&file, "hello\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink("f.txt", self.path("l")).unwrap();
        file
    }

    pub fn path(&self, name: &str) -> String {
        format!("{}/{name}", self.0)
    }
}

/// Held by every test that changes this process's environment, for as long as the change
/// lasts, so that no two of them overlap under a test runner that shares the process.
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The `PATH` a [`fake_getent`] replaced, put back when this is dropped, with
/// [`ENV_LOCK`] held until then.
#[must_use = "PATH goes back when this is dropped"]
pub struct FakePath {
    path: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for FakePath {
    fn drop(&mut self) {
        // SAFETY: under ENV_LOCK, like the change it undoes.
        unsafe {
            match &self.path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

/// A `getent` running `body` (`sh`), put first on this test process's `PATH` until the
/// returned guard is dropped.
pub fn fake_getent(scratch: &Scratch, body: &str) -> FakePath {
    let lock = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let bin = scratch.path("bin");
    fs::create_dir(&bin).unwrap();
    let getent = format!("{bin}/getent");
    fs::write(&getent, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&getent, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::var_os("PATH");
    let mut fake = std::ffi::OsString::from(format!("{bin}:"));
    fake.push(path.clone().unwrap_or_default());
    // SAFETY: under ENV_LOCK, which every test that changes the environment holds.
    unsafe { std::env::set_var("PATH", fake) };
    FakePath { path, _lock: lock }
}

/// `f`'s answer, run on a thread, or a failure after `secs`: a native that ignores its clock
/// would otherwise hold the test until nextest ends it, ten minutes on.
pub fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (sent, got) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sent.send(f());
    });
    got.recv_timeout(std::time::Duration::from_secs(secs))
        .expect("the native did not stop at the task's timeout or cancel")
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Compares a native's answer with a recording, key by key, and returns every difference.
///
/// The recording loses what the controller adds (`action`, `_ansible_no_log`, `exception`)
/// and the golden test's own `_after`. The answer gets the `changed` the dispatcher fills in,
/// the scratch directory written `<golden-tmp>`, and the running account written as the
/// recording's placeholders. A `volatile` key (`stat.atime`) is only required on both sides.
pub fn differences(
    recording: &str,
    answer: Map<String, Value>,
    scratch: &Scratch,
    volatile: &[&str],
) -> Vec<String> {
    let mut want: Map<String, Value> = serde_json::from_str(recording).unwrap();
    for key in ["action", "_ansible_no_log", "exception", "_after"] {
        want.remove(key);
    }
    let mut ours = crate::modules::module_result(answer).0;
    mask(&mut ours, scratch);
    let mut found = Vec::new();
    compare(
        "",
        &Value::Object(want),
        &Value::Object(ours),
        volatile,
        &mut found,
    );
    found
}

fn mask(map: &mut Map<String, Value>, scratch: &Scratch) {
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let clock = super::super::runtime::unbounded();
    let user = super::lookup_user(&uid.to_string(), clock)
        .unwrap()
        .unwrap()
        .name;
    let group = super::lookup_group(&gid.to_string(), clock)
        .unwrap()
        .unwrap()
        .name;
    for (key, value) in map.iter_mut() {
        let placeholder = match (key.as_str(), &*value) {
            ("uid", v) if *v == uid => Some("<uid>"),
            ("gid", v) if *v == gid => Some("<gid>"),
            ("owner" | "pw_name", v) if *v == user.as_str() => Some("<user>"),
            ("group" | "gr_name", v) if *v == group.as_str() => Some("<group>"),
            _ => None,
        };
        if let Some(placeholder) = placeholder {
            *value = placeholder.into();
            continue;
        }
        mask_value(value, scratch);
    }
}

/// The scratch directory written `<golden-tmp>` in a value, inside lists (`lineinfile`'s
/// `diff`) as well as maps.
fn mask_value(value: &mut Value, scratch: &Scratch) {
    match value {
        Value::String(text) => *text = text.replace(&scratch.0, "<golden-tmp>"),
        Value::Object(inner) => mask(inner, scratch),
        Value::Array(items) => {
            for item in items {
                mask_value(item, scratch);
            }
        }
        _ => {}
    }
}

fn compare(at: &str, want: &Value, ours: &Value, volatile: &[&str], found: &mut Vec<String>) {
    if let (Value::Object(want), Value::Object(ours)) = (want, ours) {
        for key in want
            .keys()
            .chain(ours.keys().filter(|k| !want.contains_key(*k)))
        {
            let path = if at.is_empty() {
                key.clone()
            } else {
                format!("{at}.{key}")
            };
            match (want.get(key), ours.get(key)) {
                (Some(w), Some(o)) if !volatile.contains(&path.as_str()) => {
                    compare(&path, w, o, volatile, found);
                }
                (Some(_), Some(_)) => {}
                (w, o) => found.push(format!("{path}: reference {w:?}, ours {o:?}")),
            }
        }
    } else if want != ours {
        found.push(format!("{at}: reference {want}, ours {ours}"));
    }
}
