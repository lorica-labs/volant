// SPDX-License-Identifier: GPL-3.0-or-later
//! The `python` collector, and everything else only the module's interpreter can say: its
//! `platform` answers, the environment it starts with, its locale, its clock, the `distro` it
//! would import, and whether it can load libselinux.
//!
//! One run per `setup` task, in the task's environment, as the reference starts one module per
//! task: nothing the interpreter says is kept for the next task, since a play can install a
//! `distro`, move the time zone or set `PYTHONPATH` between two gathers.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Map, Value};

use super::{Clock, Host, Stop};

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
import datetime, time
epoch_ts = time.time()
now = datetime.datetime.fromtimestamp(epoch_ts)
utcnow = datetime.datetime.fromtimestamp(epoch_ts, tz=datetime.timezone.utc)
date_time = {}
date_time['year'] = now.strftime('%Y')
date_time['month'] = now.strftime('%m')
date_time['weekday'] = now.strftime('%A')
date_time['weekday_number'] = now.strftime('%w')
date_time['weeknumber'] = now.strftime('%W')
date_time['day'] = now.strftime('%d')
date_time['hour'] = now.strftime('%H')
date_time['minute'] = now.strftime('%M')
date_time['second'] = now.strftime('%S')
date_time['epoch'] = now.strftime('%s')
if date_time['epoch'] == '' or date_time['epoch'][0] == '%':
    date_time['epoch'] = str(int(epoch_ts))
date_time['epoch_int'] = str(int(now.strftime('%s')))
if date_time['epoch_int'] == '' or date_time['epoch_int'][0] == '%':
    date_time['epoch_int'] = str(int(epoch_ts))
date_time['date'] = now.strftime('%Y-%m-%d')
date_time['time'] = now.strftime('%H:%M:%S')
date_time['iso8601_micro'] = utcnow.strftime('%Y-%m-%dT%H:%M:%S.%fZ')
date_time['iso8601'] = utcnow.strftime('%Y-%m-%dT%H:%M:%SZ')
date_time['iso8601_basic'] = now.strftime('%Y%m%dT%H%M%S%f')
date_time['iso8601_basic_short'] = now.strftime('%Y%m%dT%H%M%S')
date_time['tz'] = time.strftime('%Z')
date_time['tz_dst'] = time.tzname[1]
date_time['tz_offset'] = time.strftime('%z')
distro = None
try:
    import importlib.util
    spec = importlib.util.find_spec('distro')
except Exception:
    spec = None
    distro = ''
if spec is not None:
    distro = ''
    paths = [spec.origin or '']
    if paths[0].endswith('__init__.py'):
        paths.append(os.path.join(os.path.dirname(paths[0]), 'distro.py'))
    for path in paths:
        try:
            with open(path, encoding='utf-8') as source:
                for line in source:
                    value = line.partition('=')[2].strip()
                    if line.startswith('__version__') and value[:1] in ('"', "'"):
                        distro = value.strip('\'"')
                        break
        except (OSError, ValueError):
            pass
        if distro:
            break
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
    'date_time': date_time,
    'selinux': selinux,
    'distro': distro,
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
    /// `os.environ` when the interpreter starts under the task's environment: the module's.
    pub env: BTreeMap<String, String>,
    /// The `LC_TIME` locale after `setlocale(LC_ALL, '')`, `None` when that call fails.
    pub lc_time: Option<String>,
    /// The `date_time` fact, computed line for line as `DateTimeFactCollector` computes it,
    /// after the locale is set as the module sets it. The musl agent's own C library cannot
    /// stand in: it leaves `tzname[1]` empty for a zone without daylight time where glibc and
    /// Python repeat the standard name, and it never reloads `/etc/localtime` once read.
    pub date_time: Value,
    /// `None` when libselinux cannot be loaded, else whether SELinux is enabled.
    pub selinux: Option<bool>,
    /// The version of the `distro` package `import distro` finds, which
    /// `ansible.module_utils.distro` prefers to its bundled 1.9.0; `None` when there is none,
    /// empty when its version cannot be read.
    pub distro: Option<String>,
}

/// What `interpreter` says, started with the task's `environment` over the agent's own, as
/// ansible-core starts a module: `PYTHONPATH`, `PYTHONHOME`, `PYTHONUSERBASE` or `HOME` there
/// change what it imports.
pub fn probe(
    interpreter: &str,
    environment: &BTreeMap<String, String>,
    clock: Clock,
) -> Result<Probe, Stop> {
    let (code, out) = super::run(environment, clock, Path::new(interpreter), &["-c", PROBE])?
        .ok_or_else(|| format!("{interpreter} could not be started"))?;
    if code != 0 {
        return Err(format!("{interpreter} could not describe itself").into());
    }
    let answer: Value = serde_json::from_str(&out)
        .map_err(|err| format!("reading what {interpreter} said: {err}"))?;
    let text = |key: &str| answer[key].as_str().map(str::to_string);
    let probe = Probe {
        python: answer["python"].clone(),
        python_version: text("python_version").ok_or("no python version")?,
        bits: text("bits").ok_or("no userspace bits")?,
        env: serde_json::from_value(answer["env"].clone())
            .map_err(|err| format!("reading the interpreter's environment: {err}"))?,
        lc_time: text("lc_time"),
        date_time: Some(answer["date_time"].clone())
            .filter(Value::is_object)
            .ok_or("no date_time")?,
        selinux: answer["selinux"].as_bool(),
        distro: text("distro"),
    };
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

    use super::super::tests::FakeRoot;
    use super::*;

    /// An interpreter that answers the probe with `probe`, whatever it is asked.
    #[cfg(target_os = "linux")]
    pub fn fake_interpreter(fake: &FakeRoot, probe: &Probe) -> String {
        let answer = json!({
            "python": probe.python,
            "python_version": probe.python_version,
            "bits": probe.bits,
            "env": probe.env,
            "lc_time": probe.lc_time,
            "date_time": probe.date_time,
            "selinux": probe.selinux,
            "distro": probe.distro,
        });
        fake.command("/usr/bin/fake-python", &answer.to_string(), 0);
        fake.0
            .join("usr/bin/fake-python")
            .to_string_lossy()
            .into_owned()
    }

    /// The probe run by a real interpreter answers what the reference's own calls answer on the
    /// same interpreter: `platform.python_version()`, `platform.architecture()[0]`, the `ssl`
    /// import, `sys.executable`, `locale.setlocale`, the zone fields of `date_time`, and the
    /// `distro` that `import distro` finds. And its hand-written JSON carries any text an
    /// environment can hold.
    ///
    /// What would make this red: a shortcut that stops agreeing with the call it replaces, or an
    /// environment value with a quote, a backslash, a newline or a character outside ASCII
    /// coming back different, or not at all.
    #[test]
    fn a_real_interpreter_answers_the_probe_as_the_reference_s_calls_do() {
        let tricky = "a\"b\\c\nd\té 😀 \u{7f}";
        // Safety: set before any thread of this test process reads the environment.
        unsafe { std::env::set_var("VOLANT_PROBE_TEXT", tricky) };
        let probe = probe("python3", &BTreeMap::new(), super::super::unbounded())
            .expect("python3 answers the probe");
        assert_eq!(probe.env["VOLANT_PROBE_TEXT"], tricky);
        let reference = std::process::Command::new("python3")
            .args([
                "-c",
                "import json, locale, platform, sys, time\n\
                 try:\n    from ssl import create_default_context, SSLContext\n    ssl = True\n\
                 except ImportError:\n    ssl = False\n\
                 try:\n    import distro\n    distro = distro.__version__\n\
                 except ImportError:\n    distro = None\n\
                 locale.setlocale(locale.LC_ALL, '')\n\
                 print(json.dumps([platform.python_version(), platform.architecture()[0],\n\
                 sys.executable, ssl, locale.setlocale(locale.LC_TIME), time.strftime('%Z'),\n\
                 time.tzname[1], time.strftime('%z'), distro]))",
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
                probe.date_time["tz"],
                probe.date_time["tz_dst"],
                probe.date_time["tz_offset"],
                probe.distro,
            ]),
            reference
        );
        assert_eq!(probe.python["version"]["major"], 3);
    }

    /// A `distro` package found beside the interpreter is reported with its version, read off
    /// its source, whether it is a single module or a package.
    ///
    /// What would make this red: a system `distro` reported as absent, which lets the native
    /// answer with the bundled 1.9.0's version rules where 1.5 would give `11` for `11.11`.
    #[test]
    fn a_system_distro_is_reported_with_its_version() {
        let fake = FakeRoot::new("probe-distro");
        // The package layout Ubuntu ships: `__init__.py` re-exports the name it imported.
        fake.write(
            "/lib/distro/__init__.py",
            "from .distro import __version__\n__version__ = __version__\n",
        )
        .write(
            "/lib/distro/distro.py",
            "import os\n__version__ = \"1.5.0\"\n",
        )
        .write("/single/distro.py", "__version__ = '1.9.0'\n")
        // A lookup that raises: the module is in `sys.modules` with no spec.
        .write(
            "/broken/sitecustomize.py",
            "import sys, types\nm = types.ModuleType('distro')\nm.__spec__ = None\nsys.modules['distro'] = m\n",
        );
        let distro = |dir: &str| {
            let environment = BTreeMap::from([(
                "PYTHONPATH".to_string(),
                fake.0.join(dir).display().to_string(),
            )]);
            probe("python3", &environment, super::super::unbounded())
                .unwrap()
                .distro
        };
        assert_eq!(distro("lib").as_deref(), Some("1.5.0"));
        assert_eq!(distro("single").as_deref(), Some("1.9.0"));
        assert_eq!(
            distro("broken").as_deref(),
            Some(""),
            "a lookup that fails is a version nobody could read, not no distro"
        );
    }

    /// Nothing is kept from one probe to the next: a `distro` installed between two gathers,
    /// or a time zone moved, shows in the second.
    ///
    /// What would make this red: the probe cached per agent, which answers the second gather
    /// with the first one's `distro` and zone.
    #[test]
    fn each_probe_sees_what_changed_since_the_last() {
        let fake = FakeRoot::new("probe-fresh");
        fake.mkdir("/site");
        let clock = super::super::unbounded();
        let mut environment = BTreeMap::from([
            (
                "PYTHONPATH".to_string(),
                fake.0.join("site").display().to_string(),
            ),
            ("TZ".to_string(), "UTC".to_string()),
        ]);
        let first = probe("python3", &environment, clock).unwrap();
        // The machine's own `distro`, if it has one, until the one on `PYTHONPATH` appears.
        assert_ne!(first.distro.as_deref(), Some("1.5.0"));
        assert_eq!(first.date_time["tz"], "UTC");
        fake.write("/site/distro.py", "__version__ = \"1.5.0\"\n");
        environment.insert("TZ".into(), "Asia/Tokyo".into());
        let second = probe("python3", &environment, clock).unwrap();
        assert_eq!(second.distro.as_deref(), Some("1.5.0"));
        assert_eq!(second.date_time["tz"], "JST");
        assert_eq!(second.date_time["tz_offset"], "+0900");
    }
}
