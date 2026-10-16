// SPDX-License-Identifier: GPL-3.0-or-later
//! Background jobs: a task run under `async`, the way ansible-core 2.19.12's `async_wrapper`
//! runs it, and read back by `async_status` from the same status file.
//!
//! Three processes, as there. The agent starts a watcher in a process group of its own and
//! answers the controller as soon as the watcher has written the job's first status; the watcher
//! starts a runner, in a group of its own too, that runs the task with the agent's own module
//! code and prints the result; the watcher writes that result over the status file, or kills the
//! runner's group once `async` seconds have passed and writes `Timeout exceeded` instead. The
//! watcher and the runner hold nothing of the agent's link, so a link that dies takes neither of
//! them with it.
//!
//! The status file is `<dir>/<jid>`, written whole into `<jid>.tmp` and renamed over it, so a
//! reader sees the previous status or the next one and never half of one. The job id is
//! `j<random>.<watcher pid>`, the reference's shape, which is what lets a job either engine
//! started be read by the other.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use volant_protocol::{AsyncJob, Task, TaskResult};

use crate::modules::command::kill_group;
use crate::modules::{self, Run};

/// How long the watcher gives the runner, once `async` seconds are up, to stop the module it
/// runs before the runner's whole group is killed. The runner stops it at the same moment, which
/// is what reaches a command the module started in a process group of its own.
const GRACE: Duration = Duration::from_secs(1);

/// How often the watcher looks at the runner.
const TICK: Duration = Duration::from_millis(50);

/// What the agent hands the watcher, and the watcher the runner, on stdin.
struct Spec {
    limit: u64,
    dir: PathBuf,
    task: Task,
}

impl Spec {
    fn to_json(&self) -> serde_json::Result<Value> {
        Ok(json!({
            "limit": self.limit,
            "dir": self.dir.to_string_lossy(),
            "task": serde_json::to_value(&self.task)?,
        }))
    }

    /// The spec on stdin, or why it is not one.
    fn read() -> Result<Self, String> {
        let value: Value =
            serde_json::from_reader(io::stdin().lock()).map_err(|err| err.to_string())?;
        let limit = value["limit"].as_u64().ok_or("no limit")?;
        let dir = value["dir"].as_str().ok_or("no directory")?.into();
        let task = serde_json::from_value(value["task"].clone()).map_err(|err| err.to_string())?;
        Ok(Self { limit, dir, task })
    }
}

/// `dir` with a leading `~` read as the home directory, as `os.path.expanduser` reads it for the
/// account the agent runs as. `~user` is left alone, which is the one spelling that differs.
pub fn expand_dir(dir: &str) -> PathBuf {
    expand_under(dir, std::env::var_os("HOME"))
}

fn expand_under(dir: &str, home: Option<std::ffi::OsString>) -> PathBuf {
    match (dir.strip_prefix('~'), home.filter(|home| !home.is_empty())) {
        (Some(""), Some(home)) => PathBuf::from(home),
        (Some(rest), Some(home)) if rest.starts_with('/') => {
            PathBuf::from(home).join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(dir),
    }
}

/// Starts `task` as a job and answers at once, with the result ansible-core's wrapper gives and
/// the `changed: true` its action plugin adds to it.
pub fn start(task: &Task, job: &AsyncJob) -> TaskResult {
    // A staged file lives in this connection's own directory, which the job outlives.
    if !task.files.is_empty() {
        return TaskResult::failed_with("async is not supported yet on a task that stages files");
    }
    let dir = expand_dir(&job.dir);
    if let Err(err) = private_dir(&dir) {
        return TaskResult::failed_with(format!(
            "could not create directory: {} - {err}",
            dir.display()
        ));
    }
    let spec = Spec {
        limit: job.limit,
        dir: dir.clone(),
        task: Task {
            job: None,
            ..task.clone()
        },
    };
    let jid = match spawn_detached("--async-job", &spec).and_then(first_line) {
        Ok(jid) => jid,
        Err(err) => return TaskResult::failed_with(format!("the async job did not start: {err}")),
    };
    let mut result = Map::new();
    result.insert("failed".into(), Value::Bool(false));
    result.insert("started".into(), Value::Bool(true));
    result.insert("finished".into(), Value::Bool(false));
    result.insert(
        "results_file".into(),
        Value::String(dir.join(&jid).to_string_lossy().into_owned()),
    );
    result.insert("ansible_job_id".into(), Value::String(jid));
    result.insert("changed".into(), Value::Bool(true));
    TaskResult(result)
}

/// The agent's own binary, started with `flag` in a process group of its own, `spec` on its
/// stdin, its stdout piped back and its stderr discarded: nothing of the caller's terminal or
/// link is left open in it.
fn spawn_detached(flag: &str, spec: &Spec) -> io::Result<Child> {
    // The running file rather than the path it was started by, which an upload may have renamed
    // a newer agent over since.
    let exe = if Path::new("/proc/self/exe").exists() {
        PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe()?
    };
    let mut command = Command::new(exe);
    command
        .arg(flag)
        // Where the payloads are, which the agent otherwise reads off its own path.
        .env("VOLANT_REMOTE_TMP", crate::blobs::remote_tmp())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A session of its own, as the reference's wrapper daemonizes: no terminal an `su` link
        // hangs up, and a process group of its own for the timeout to kill.
        // SAFETY: `setsid` is async-signal-safe and touches no memory of the parent.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let written = spec
        .to_json()
        .and_then(|value| serde_json::to_writer(&mut stdin, &value))
        .map_err(io::Error::other);
    drop(stdin);
    if let Err(err) = written {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
    Ok(child)
}

/// The job id the watcher prints once the first status is written, or the reason it printed
/// instead. The watcher is reaped by a thread of its own, so the agent waits for nothing more.
fn first_line(mut child: Child) -> io::Result<String> {
    let stdout = child.stdout.take().expect("stdout was piped");
    let mut line = String::new();
    let read = BufReader::new(stdout).read_line(&mut line);
    std::thread::spawn(move || child.wait());
    read?;
    let line = line.trim_end();
    match line.strip_prefix("jid ") {
        Some(jid) => Ok(jid.to_string()),
        None if line.is_empty() => Err(io::Error::other("the watcher ended without a job id")),
        None => Err(io::Error::other(
            line.trim_start_matches("error ").to_string(),
        )),
    }
}

/// The watcher, `volant-agent --async-job`: the process that outlives the agent's link.
pub fn watch() -> i32 {
    let spec = match Spec::read() {
        Ok(spec) => spec,
        Err(err) => {
            say(&format!("error reading the job: {err}"));
            return 1;
        }
    };
    let jid = format!("j{}.{}", random_digits(), std::process::id());
    let path = spec.dir.join(&jid);
    if let Err(err) = write_status(
        &path,
        &json!({"started": true, "finished": false, "ansible_job_id": jid}),
    ) {
        say(&format!("error writing {}: {err}", path.display()));
        return 1;
    }
    let runner = spawn_detached("--async-run", &spec);
    say(&format!("jid {jid}"));
    let result = match runner {
        Ok(runner) => supervise(runner, spec.limit, &jid),
        Err(err) => failure(&jid, &format!("the job's runner did not start: {err}")),
    };
    // Nobody is left to tell: the agent read the job id and went on. A watcher killed before
    // this line leaves the file at started and unfinished, which is the reference's answer too.
    let _ = write_status(&path, &result);
    0
}

/// Waits for the runner, and kills its group once `limit` seconds are up: the runner's result,
/// or the reference's `Timeout exceeded` naming the group that was killed.
fn supervise(mut runner: Child, limit: u64, jid: &str) -> Value {
    let pid = runner.id();
    let mut stdout = runner.stdout.take().expect("stdout was piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let read = stdout.read_to_string(&mut text);
        let _ = tx.send(read.map(|_| text));
    });
    let deadline = deadline(limit);
    let status = loop {
        match runner.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if !passed(deadline, GRACE) => std::thread::sleep(TICK),
            Ok(None) => break None,
            Err(err) => return failure(jid, &format!("waiting for the job: {err}")),
        }
    };
    let Some(status) = status else {
        kill_group(pid);
        let _ = runner.wait();
        return timed_out(pid, deadline);
    };
    let output = rx.recv_timeout(GRACE).unwrap_or_else(|_| Ok(String::new()));
    match output.map(|text| serde_json::from_str::<Value>(&text)) {
        Ok(Ok(result @ Value::Object(_))) => result,
        // Stopped by its own deadline: the module was killed, the group may hold what it left.
        _ if passed(deadline, Duration::ZERO) => {
            kill_group(pid);
            timed_out(pid, deadline)
        }
        _ => failure(jid, &format!("the job ended without a result ({status})")),
    }
}

/// The reference's answer for a job killed at its limit. Written no sooner than a second past
/// the limit, as the reference's watcher writes it a second after its kill: a poll that lands
/// on the limit sees the job still running under either engine.
fn timed_out(pid: u32, deadline: Option<Instant>) -> Value {
    if let Some(at) = deadline.and_then(|at| at.checked_add(GRACE)) {
        std::thread::sleep(at.saturating_duration_since(Instant::now()));
    }
    json!({"msg": "Timeout exceeded", "failed": true, "child_pid": pid})
}

/// The shape the reference's wrapper writes when the module could not report.
fn failure(jid: &str, msg: &str) -> Value {
    json!({"failed": true, "msg": msg, "ansible_job_id": jid})
}

/// The runner, `volant-agent --async-run`: the task, run by the agent's own module code, its
/// result printed as JSON. Nothing printed when the deadline stopped it.
pub fn run() -> i32 {
    let Ok(spec) = Spec::read() else {
        return 1;
    };
    let deadline = deadline(spec.limit);
    crate::python::batch_started();
    // `timeout` bounds the start of the job in the reference, not the job: the job has `async`.
    let task = Task {
        timeout: None,
        ..spec.task
    };
    let (run, _) = modules::run(&task, &|| passed(deadline, Duration::ZERO));
    match run {
        Run::Done(result) => match serde_json::to_string(&result) {
            Ok(text) => {
                say(&text);
                0
            }
            Err(_) => 1,
        },
        Run::Cancelled => 2,
    }
}

/// When a job of `limit` seconds is stopped, counted from now, or none for a limit past what the
/// clock can count to.
fn deadline(limit: u64) -> Option<Instant> {
    Instant::now().checked_add(kill_after(limit))
}

/// The moment the reference's watcher kills a job: it looks every five seconds, or every half
/// limit for a limit of five seconds or less, and kills at the first look past the limit. So a
/// limit of 7 runs for 10 seconds there, and here too.
fn kill_after(limit: u64) -> Duration {
    let limit = Duration::from_secs(limit);
    let mut step = Duration::from_secs(5);
    if !limit.is_zero() && step >= limit {
        step = limit / 2;
    }
    let mut at = step;
    while at < limit {
        at = at.saturating_add(step);
    }
    at
}

/// Whether `extra` has gone by since `deadline`.
fn passed(deadline: Option<Instant>, extra: Duration) -> bool {
    deadline
        .and_then(|at| at.checked_add(extra))
        .is_some_and(|at| Instant::now() >= at)
}

/// One line on stdout. A reader that went away is not an error here: `println!` would panic on
/// it, and the job has to go on whoever stopped listening.
fn say(line: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Writes `value` to `path` whole: into `<path>.tmp` first, then renamed over it.
fn write_status(path: &Path, value: &Value) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut text = String::new();
    python_json(value, &mut text);
    let mut file = private_file(&tmp)?;
    file.write_all(text.as_bytes())?;
    drop(file);
    std::fs::rename(&tmp, path)
}

/// The jobs directory, readable by its owner alone: a finished job's file holds the module's
/// whole output.
fn private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// A status file created for its owner alone, whatever the umask, before anything is in it.
fn private_file(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// `value` with `json.dumps`'s separators, `", "` and `": "`, so a status file reads as the
/// reference's wrapper writes it.
fn python_json(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                python_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push_str(": ");
                python_json(item, out);
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// Up to twelve random digits, as `secrets.randbelow(999999999999)` gives them.
fn random_digits() -> u64 {
    let mut bytes = [0u8; 8];
    let random = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map(|()| u64::from_le_bytes(bytes));
    let seed = random.unwrap_or_else(|_| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        now.as_secs().rotate_left(30)
            ^ u64::from(now.subsec_nanos())
            ^ u64::from(std::process::id())
    });
    seed % 999_999_999_999
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leading_tilde_is_the_home_directory_and_nothing_else_is_touched() {
        let home = || Some(std::ffi::OsString::from("/home/u"));
        assert_eq!(expand_under("~", home()), PathBuf::from("/home/u"));
        assert_eq!(
            expand_under("~/.ansible_async", home()),
            PathBuf::from("/home/u/.ansible_async")
        );
        assert_eq!(expand_under("~bob/x", home()), PathBuf::from("~bob/x"));
        assert_eq!(
            expand_under("/var/jobs", home()),
            PathBuf::from("/var/jobs")
        );
        assert_eq!(expand_under("~/x", None), PathBuf::from("~/x"));
    }

    #[rstest::rstest]
    #[case(1, 1)]
    #[case(2, 2)]
    #[case(5, 5)]
    #[case(7, 10)]
    #[case(10, 10)]
    #[case(11, 15)]
    fn a_job_is_killed_where_the_reference_kills_it(#[case] limit: u64, #[case] secs: u64) {
        assert_eq!(kill_after(limit), Duration::from_secs(secs));
    }

    #[test]
    fn a_status_is_replaced_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j1.2");
        write_status(&path, &json!({"started": true})).unwrap();
        write_status(&path, &json!({"rc": 0, "cmd": ["a", "b"]})).unwrap();
        let read = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read, r#"{"rc": 0, "cmd": ["a", "b"]}"#);
        assert!(!dir.path().join("j1.2.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let jobs = dir.path().join("jobs");
            private_dir(&jobs).unwrap();
            let mode = std::fs::metadata(&jobs).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }
}
