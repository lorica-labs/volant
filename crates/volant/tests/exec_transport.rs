// SPDX-License-Identifier: GPL-3.0-or-later
//! A link over an exec-with-stdio command, driven through a stand-in for `docker exec -i` that
//! runs each command on this machine, with `HOME` in a directory of the container's own.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use volant::agent::AgentSource;
use volant::transport::{BecomeMethod, ConnectError, Escalation, ExecTarget, Transport};
use volant_protocol::{BatchOutcome, FromAgent, Task, ToAgent};

/// Runs `exec -i <container> <program...>` here. `gone` is a container that does not exist,
/// `hang` one whose runtime never answers, and `nosh` one with no `/bin/sh`, whose `/tmp` is a
/// directory of its own. `cp - <container>:<dir>` unpacks a tar stream there, and `inspect` and
/// `image inspect` name this machine's architecture as docker does. The pid of the runtime and of what it started are
/// written beside the container's directory, and the program runs in the background with the
/// runtime's stdin, as a program in a container outlives the client that started it.
const RUNTIME: &str = r#"#!/bin/sh
case "$1" in
  inspect) echo "image-of-$4"; exit 0;;
  image) case "$(uname -m)" in x86_64) echo amd64;; aarch64) echo arm64;; *) uname -m;; esac; exit 0;;
  cp) echo "$3" >> "@ROOT@/cp.log"; c=${3%%:*}; dir=${3#*:}
      mkdir -p "@ROOT@/$c$dir" && exec tar -x -f - -C "@ROOT@/$c$dir";;
esac
[ "$1" = exec ] || { echo "fake runtime: only exec" >&2; exit 2; }
shift; [ "$1" = -i ] && shift
c=$1; shift
case "$c" in
  gone) echo "Error response from daemon: No such container: $c" >&2; exit 1;;
  hang) exec sleep 600;;
  nosh) case "$1" in
    /bin/sh) echo 'OCI runtime exec failed: exec failed: unable to start container process: exec: "/bin/sh": stat /bin/sh: no such file or directory' >&2; exit 127;;
    /tmp/volant-agent-*) p="@ROOT@/nosh$1"; shift; set -- "$p" "$@";;
  esac;;
esac
mkdir -p "@ROOT@/$c"
echo "runtime $$" >> "@ROOT@/$c.pids"
exec 3<&0
HOME="@ROOT@/$c" "$@" 0<&3 3<&- &
echo "program $!" >> "@ROOT@/$c.pids"
wait $!
"#;

struct Runtime {
    root: tempfile::TempDir,
    program: PathBuf,
    agents: AgentSource,
}

impl Runtime {
    fn new() -> Runtime {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("a scratch directory");
        let program = root.path().join("fake-docker");
        std::fs::write(
            &program,
            RUNTIME.replace("@ROOT@", root.path().to_str().unwrap()),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The agent this build made, under the name an upload looks for on this machine.
        let agents = root.path().join("agents");
        std::fs::create_dir(&agents).unwrap();
        let triple = volant::transport::triple_for(std::env::consts::ARCH).expect("a known arch");
        std::fs::copy(
            PathBuf::from(env!("CARGO_BIN_EXE_volant")).with_file_name("volant-agent"),
            agents.join(format!("volant-agent-{triple}")),
        )
        .expect("cargo build -p volant-agent");
        // nextest gives each test a process of its own, so no other thread reads the variable.
        let saved = std::env::var_os("VOLANT_AGENT_DIR");
        unsafe { std::env::set_var("VOLANT_AGENT_DIR", &agents) };
        let agents = AgentSource::discover();
        match saved {
            Some(value) => unsafe { std::env::set_var("VOLANT_AGENT_DIR", value) },
            None => unsafe { std::env::remove_var("VOLANT_AGENT_DIR") },
        }
        Runtime {
            root,
            program,
            agents,
        }
    }

    fn target(&self, container: &str) -> Transport {
        Transport::Exec(ExecTarget {
            name: container.into(),
            kind: "DOCKER".into(),
            prefix: vec![
                self.program.display().to_string(),
                "exec".into(),
                "-i".into(),
                container.into(),
            ],
            user: None,
            user_probe: None,
            unavailable: None,
            copy: Some(volant::transport::CopyInto {
                tar_into: vec![
                    self.program.display().to_string(),
                    "cp".into(),
                    "-".into(),
                    format!("{container}:{{dir}}"),
                ],
                arch: vec![
                    vec![
                        self.program.display().to_string(),
                        "inspect".into(),
                        "--format".into(),
                        "{{.Image}}".into(),
                        container.into(),
                    ],
                    vec![
                        self.program.display().to_string(),
                        "image".into(),
                        "inspect".into(),
                        "--format".into(),
                        "{{.Architecture}}".into(),
                        "{}".into(),
                    ],
                ],
            }),
            remote_tmp: "~/.ansible/tmp".into(),
            connect_timeout: Duration::from_secs(1),
        })
    }

    /// The pids the runtime wrote for `container`, by what they are.
    fn pids(&self, container: &str, what: &str) -> Vec<u32> {
        std::fs::read_to_string(self.root.path().join(format!("{container}.pids")))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(what)?.trim().parse().ok())
            .collect()
    }

    fn home(&self, container: &str) -> PathBuf {
        self.root.path().join(container)
    }
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn sudo() -> Option<Escalation> {
    let works = std::process::Command::new("sudo")
        .args(["-n", "true"])
        .status()
        .is_ok_and(|status| status.success());
    works.then(|| Escalation {
        method: BecomeMethod::Sudo,
        user: "root".into(),
        password: None,
        exe: Vec::new(),
        flags: None,
        prompt_l10n: Vec::new(),
    })
}

/// The probe finds nothing, the agent goes up through the runtime's stdin, the probe after it
/// checks it landed, and the link answers a batch, escalated or not.
#[tokio::test]
async fn a_whole_link_runs_over_the_exec_command() {
    let runtime = Runtime::new();
    let mut runs = vec![("app1", None)];
    match sudo() {
        Some(escalation) => runs.push(("app2", Some(escalation))),
        None => eprintln!("skipped the sudo link: sudo -n asks for a password here"),
    }
    for (container, escalation) in runs {
        let mut link = runtime
            .target(container)
            .connect(&runtime.agents, escalation.as_ref())
            .await
            .unwrap_or_else(|e| panic!("{container}: {e}"));
        link.handshake().await.unwrap();
        link.send(&ToAgent::RunBatch {
            id: 1,
            tasks: vec![Task {
                only_if: None,
                module: "command".into(),
                args: json!({"_raw_params": "id -un"})
                    .as_object()
                    .unwrap()
                    .clone(),
                ignore_errors: false,
                timeout: None,
                environment: BTreeMap::default(),
                payload: None,
                files: Vec::new(),
                force_python: false,
                job: None,
            }],
        })
        .await
        .unwrap();
        match link.recv().await.unwrap() {
            Some(FromAgent::TaskResult {
                index: 0, result, ..
            }) => {
                let answer = result.0["stdout"].as_str().unwrap_or_default();
                assert!(
                    escalation.is_none() || answer == "root",
                    "{container}: {answer}"
                );
            }
            other => panic!("{container}: expected a result, got {other:?}"),
        }
        assert_eq!(
            link.recv().await.unwrap(),
            Some(FromAgent::BatchDone {
                batch: 1,
                outcome: BatchOutcome::Completed
            })
        );
        link.shutdown().await;
    }
    let cached = std::fs::read_dir(runtime.home("app1").join(".ansible/tmp"))
        .unwrap()
        .count();
    assert_eq!(
        cached, 1,
        "one agent directory in the container's remote_tmp"
    );
}

#[tokio::test]
async fn a_container_that_does_not_exist_is_unreachable_in_the_runtime_s_words() {
    let runtime = Runtime::new();
    match runtime.target("gone").connect(&runtime.agents, None).await {
        Err(ConnectError::Unreachable(message)) => {
            assert!(message.contains("No such container: gone"), "{message}");
        }
        Err(other) => panic!("unreachable expected, got {other}"),
        Ok(_) => panic!("a link to a container that does not exist"),
    }
}

#[tokio::test]
async fn a_runtime_that_does_not_answer_is_given_up_after_the_connection_timeout() {
    let runtime = Runtime::new();
    let started = std::time::Instant::now();
    match runtime.target("hang").connect(&runtime.agents, None).await {
        Err(ConnectError::Unreachable(message)) => assert!(
            message.contains("did not answer within 1 second when"),
            "{message}"
        ),
        Err(other) => panic!("unreachable expected, got {other}"),
        Ok(_) => panic!("a link through a runtime that never answers"),
    }
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn su_is_refused_until_a_runtime_can_give_it_a_terminal() {
    let runtime = Runtime::new();
    let su = Escalation {
        method: BecomeMethod::Su,
        ..sudo().unwrap_or(Escalation {
            method: BecomeMethod::Su,
            user: "root".into(),
            password: None,
            exe: Vec::new(),
            flags: None,
            prompt_l10n: Vec::new(),
        })
    };
    match runtime
        .target("app1")
        .connect(&runtime.agents, Some(&su))
        .await
    {
        Err(ConnectError::Become(message)) => assert_eq!(
            message,
            "become_method 'su' is not supported yet on a docker connection"
        ),
        Err(other) => panic!("a become error expected, got {other}"),
        Ok(_) => panic!("su ran without a terminal"),
    }
}

/// A controller killed outright closes the runtime's stdin, and the agent behind it stops on
/// end of input, so nothing is left running in the container.
#[tokio::test]
async fn an_agent_whose_controller_died_stops_within_five_seconds() {
    let runtime = Runtime::new();
    let mut link = runtime
        .target("app3")
        .connect(&runtime.agents, None)
        .await
        .unwrap();
    link.handshake().await.unwrap();
    let agent = *runtime
        .pids("app3", "program")
        .last()
        .expect("the agent's pid");
    for pid in runtime.pids("app3", "runtime") {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
    drop(link);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while alive(agent) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(agent), "the agent {agent} is still running");
}

/// A target with no shell: the runtime's copy puts the agent in, the agent places itself, the
/// copy it ran from is gone, and a second link this run starts the placed agent without copying.
#[tokio::test]
async fn a_target_with_no_shell_gets_the_agent_by_copy() {
    let runtime = Runtime::new();
    for _ in 0..2 {
        let mut link = runtime
            .target("nosh")
            .connect(&runtime.agents, None)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        link.handshake().await.unwrap();
        link.shutdown().await;
    }
    let copies = std::fs::read_to_string(runtime.root.path().join("cp.log")).unwrap();
    assert_eq!(copies.lines().count(), 1, "{copies}");
    let landing = std::fs::read_dir(runtime.home("nosh").join("tmp"))
        .unwrap()
        .count();
    assert_eq!(landing, 0, "the copy the agent ran from is removed");
    let placed = runtime.home("nosh").join(".ansible/tmp");
    assert_eq!(std::fs::read_dir(placed).unwrap().count(), 1);
}

#[tokio::test]
async fn become_on_a_target_with_no_shell_says_why_it_cannot() {
    let runtime = Runtime::new();
    let sudo = Escalation {
        method: BecomeMethod::Sudo,
        user: "root".into(),
        password: None,
        exe: Vec::new(),
        flags: None,
        prompt_l10n: Vec::new(),
    };
    match runtime
        .target("nosh")
        .connect(&runtime.agents, Some(&sudo))
        .await
    {
        Err(ConnectError::Become(message)) => {
            assert_eq!(
                message,
                "become needs a shell in nosh, which has no /bin/sh"
            );
        }
        Err(other) => panic!("a become error expected, got {other}"),
        Ok(_) => panic!("become ran without a shell"),
    }
}
