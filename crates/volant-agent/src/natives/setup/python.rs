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

/// Run as `<interpreter> -c`. Each value is the reference's own: the `python` fact line for line
/// from `PythonFactCollector`, libselinux loaded as `module_utils.compat.selinux` loads it, the
/// locale set as `AnsibleModule._check_locale` sets it, and the environment read before anything
/// else can touch it.
///
/// It avoids the imports that cost most of a start (measured on the dev machine, Python 3.14:
/// `json` 11 ms, `platform` 11 ms, `ssl` 16 ms, `platform.architecture()` 26 ms more), each
/// replaced by what it computes here:
/// - `ssl` imports `_ssl` and takes the two names from it, so `_ssl` loading is the answer;
/// - `platform.python_version()` is the leading `[\w.+]+` of `sys.version`, padded to three
///   parts;
/// - `platform.architecture()` asks `file` about the interpreter's ELF class, which is its
///   pointer size;
/// - `locale.setlocale` hands a string straight to `_locale.setlocale`;
/// - the answer is written as ASCII JSON by hand.
const PROBE: &str = r#"
import sys
if sys.path and sys.path[0] in ('', '.'):
    del sys.path[0]
import os
env = dict(os.environ)
import _locale, struct
try:
    import _ssl
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
version = ''
for c in sys.version:
    if not ((c.isascii() and c.isalnum()) or c in '_.+'):
        break
    version += c
if version.count('.') == 1:
    version += '.0'
try:
    _locale.setlocale(_locale.LC_ALL, '')
    lc_time = _locale.setlocale(_locale.LC_TIME)
except _locale.Error:
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
def text(value):
    out = []
    for c in value:
        o = ord(c)
        if c in '"\\' or o < 32 or o > 126:
            if o > 0xffff:
                o -= 0x10000
                out.append('\\u%04x\\u%04x' % (0xd800 + (o >> 10), 0xdc00 + (o & 0x3ff)))
            else:
                out.append('\\u%04x' % o)
        else:
            out.append(c)
    return '"' + ''.join(out) + '"'
def dump(value):
    if value is None:
        return 'null'
    if value is True or value is False:
        return 'true' if value else 'false'
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str):
        return text(value)
    if isinstance(value, list):
        return '[' + ','.join(dump(item) for item in value) + ']'
    return '{' + ','.join(text(k) + ':' + dump(v) for k, v in value.items()) + '}'
sys.stdout.write(dump({
    'python': python,
    'python_version': version,
    'bits': str(struct.calcsize('P') * 8) + 'bit',
    'env': env,
    'lc_time': lc_time,
    'selinux': selinux,
}))
"#;

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

    /// The probe run by a real interpreter answers what the reference's own calls answer on the
    /// same interpreter: `platform.python_version()`, `platform.architecture()[0]`, the `ssl`
    /// import, `sys.executable`, `locale.setlocale`. And its hand-written JSON carries any text
    /// an environment can hold.
    ///
    /// What would make this red: a shortcut that stops agreeing with the call it replaces, or an
    /// environment value with a quote, a backslash, a newline or a character outside ASCII
    /// coming back different, or not at all.
    #[test]
    fn a_real_interpreter_answers_the_probe_as_the_reference_s_calls_do() {
        let tricky = "a\"b\\c\nd\té 😀 \u{7f}";
        // Safety: set before any thread of this test process reads the environment.
        unsafe { std::env::set_var("VOLANT_PROBE_TEXT", tricky) };
        let probe = probe("python3").expect("python3 answers the probe");
        assert_eq!(probe.env["VOLANT_PROBE_TEXT"], tricky);
        let reference = Command::new("python3")
            .args([
                "-c",
                "import json, locale, platform, sys\n\
                 try:\n    from ssl import create_default_context, SSLContext\n    ssl = True\n\
                 except ImportError:\n    ssl = False\n\
                 locale.setlocale(locale.LC_ALL, '')\n\
                 print(json.dumps([platform.python_version(), platform.architecture()[0],\n\
                 sys.executable, ssl, locale.setlocale(locale.LC_TIME)]))",
            ])
            .output()
            .unwrap();
        let reference: Value = serde_json::from_slice(&reference.stdout).unwrap();
        assert_eq!(
            json!([
                probe.python_version,
                probe.bits,
                probe.python["executable"],
                probe.python["has_sslcontext"],
                probe.lc_time,
            ]),
            reference
        );
        assert_eq!(probe.python["version"]["major"], 3);
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
