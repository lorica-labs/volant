// SPDX-License-Identifier: GPL-3.0-or-later
//! The `python` collector, and everything else only the module's interpreter can say: its
//! `platform` answers, the environment it starts with, its locale, and whether it can load
//! libselinux. One run of the interpreter per agent, kept while the interpreter file is the same.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::{Map, Value};

use super::Host;

/// Run as `<interpreter> -c`. Each value is computed the way the reference computes it: the
/// `python` fact line for line from `PythonFactCollector`, libselinux loaded as
/// `module_utils.compat.selinux` loads it, the locale set as `AnsibleModule._check_locale` sets
/// it. The environment is read before anything else can touch it.
const PROBE: &str = r"
import sys
if sys.path and sys.path[0] in ('', '.'):
    del sys.path[0]
import os
env = dict(os.environ)
import json, locale, platform
try:
    from ssl import create_default_context, SSLContext
    has_sslcontext = True
except ImportError:
    has_sslcontext = False
python = {
    'version': {
        'major': sys.version_info[0],
        'minor': sys.version_info[1],
        'micro': sys.version_info[2],
        'releaselevel': sys.version_info[3],
        'serial': sys.version_info[4],
    },
    'version_info': list(sys.version_info),
    'executable': sys.executable,
    'has_sslcontext': has_sslcontext,
}
try:
    python['type'] = sys.subversion[0]
except AttributeError:
    try:
        python['type'] = sys.implementation.name
    except AttributeError:
        python['type'] = None
try:
    locale.setlocale(locale.LC_ALL, '')
    lc_time = locale.setlocale(locale.LC_TIME)
except locale.Error:
    lc_time = None
selinux = None
try:
    from ctypes import CDLL
    lib = CDLL('libselinux.so.1', use_errno=True)
    for name in ('is_selinux_enabled', 'is_selinux_mls_enabled', 'lgetfilecon_raw',
                 'matchpathcon', 'security_policyvers', 'selinux_getenforcemode',
                 'security_getenforce', 'lsetfilecon', 'selinux_getpolicytype'):
        if not getattr(lib, name, None):
            raise ImportError(name)
    selinux = bool(lib.is_selinux_enabled())
except (ImportError, OSError):
    pass
print(json.dumps({
    'python': python,
    'python_version': platform.python_version(),
    'bits': platform.architecture()[0],
    'env': env,
    'lc_time': lc_time,
    'selinux': selinux,
}))
";

/// What the module's interpreter says about itself and its host.
#[derive(Clone, Debug)]
pub struct Probe {
    /// The `python` fact.
    pub python: Value,
    /// `platform.python_version()`.
    pub python_version: String,
    /// `platform.architecture()[0]`: `64bit`, `32bit`.
    pub bits: String,
    /// `os.environ` when the interpreter starts, which is the module's before the task's own
    /// variables are added.
    pub env: BTreeMap<String, String>,
    /// The `LC_TIME` locale after `setlocale(LC_ALL, '')`, `None` when that call fails.
    pub lc_time: Option<String>,
    /// `None` when libselinux cannot be loaded, else whether SELinux is enabled.
    pub selinux: Option<bool>,
}

type Key = (String, Option<(u64, SystemTime)>);

static CACHE: Mutex<Option<(Key, Probe)>> = Mutex::new(None);

/// The probe for `interpreter`, run once and kept while the interpreter's file keeps its size
/// and modification time: an upgrade of Python between two gathers runs it again.
pub fn probe(interpreter: &str) -> Result<Probe, String> {
    let key: Key = (
        interpreter.to_string(),
        std::fs::metadata(interpreter)
            .ok()
            .and_then(|meta| Some((meta.len(), meta.modified().ok()?))),
    );
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((cached, probe)) = cache.as_ref()
        && *cached == key
    {
        return Ok(probe.clone());
    }
    let out = Command::new(interpreter)
        .arg("-c")
        .arg(PROBE)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|err| format!("starting {interpreter}: {err}"))?;
    if !out.status.success() {
        return Err(format!("{interpreter} could not describe itself"));
    }
    let answer: Value = serde_json::from_slice(&out.stdout)
        .map_err(|err| format!("reading what {interpreter} said: {err}"))?;
    let text = |key: &str| answer[key].as_str().map(str::to_string);
    let probe = Probe {
        python: answer["python"].clone(),
        python_version: text("python_version").ok_or("no python version")?,
        bits: text("bits").ok_or("no userspace bits")?,
        env: serde_json::from_value(answer["env"].clone())
            .map_err(|err| format!("reading the interpreter's environment: {err}"))?,
        lc_time: text("lc_time"),
        selinux: answer["selinux"].as_bool(),
    };
    *cache = Some((key, probe.clone()));
    Ok(probe)
}

pub fn collect(host: &Host) -> Map<String, Value> {
    let mut facts = Map::new();
    facts.insert("python".into(), host.probe.python.clone());
    facts
}

#[cfg(test)]
pub mod tests {
    use serde_json::json;

    use super::super::tests::{FakeRoot, probe as ubuntu_probe};
    use super::*;

    /// An interpreter that answers the probe with `probe`, whatever it is asked.
    pub fn fake_interpreter(fake: &FakeRoot, probe: &Probe) -> String {
        let answer = json!({
            "python": probe.python,
            "python_version": probe.python_version,
            "bits": probe.bits,
            "env": probe.env,
            "lc_time": probe.lc_time,
            "selinux": probe.selinux,
        });
        fake.command("/usr/bin/fake-python", &answer.to_string(), 0);
        fake.0
            .join("usr/bin/fake-python")
            .to_string_lossy()
            .into_owned()
    }

    /// The probe run by a real interpreter reads what the reference's collectors read. Checked
    /// against `python3` on the machine running the test, whose answers are its own.
    ///
    /// What would make this red: the snippet failing under a real interpreter, the environment
    /// read after the locale was set, or `executable` read from anything but `sys.executable`.
    #[test]
    fn a_real_interpreter_answers_the_probe() {
        let probe = probe("python3").expect("python3 answers the probe");
        let executable = Command::new("python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
            .unwrap();
        assert_eq!(
            probe.python["executable"].as_str().unwrap(),
            String::from_utf8_lossy(&executable.stdout).trim()
        );
        assert!(probe.python["version"]["major"].as_u64() == Some(3));
        assert_eq!(probe.env.get("PATH"), std::env::var("PATH").ok().as_ref());
        assert!(
            matches!(probe.bits.as_str(), "64bit" | "32bit"),
            "{}",
            probe.bits
        );
    }

    /// The probe is run again when the interpreter file changes, and not otherwise.
    ///
    /// What would make this red: a cache keyed on the path alone, which keeps the old Python's
    /// version after an upgrade in the middle of a play.
    #[test]
    fn a_changed_interpreter_is_probed_again() {
        let fake = FakeRoot::new("probe-cache");
        let mut first = ubuntu_probe();
        let interpreter = fake_interpreter(&fake, &first);
        assert_eq!(probe(&interpreter).unwrap().python_version, "3.12.3");
        first.python_version = "3.12.40".into();
        fake_interpreter(&fake, &first);
        assert_eq!(probe(&interpreter).unwrap().python_version, "3.12.40");
    }
}
