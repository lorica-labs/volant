// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::testing::scratch;

fn ran(path: ExecPath, micros: u64) -> Ran {
    Ran {
        path,
        reason: None,
        micros,
        fork_micros: None,
        import_micros: None,
        module_micros: None,
    }
}

fn record(host: &str, module: &str, ran: Option<Ran>) -> TaskRecord {
    TaskRecord {
        host: host.into(),
        index: 1,
        task: "t".into(),
        module: module.into(),
        ran,
        prepare_micros: 7,
    }
}

fn filled() -> Profile {
    let profile = Profile::default();
    profile.phase(None, Phase::Compile, 1200);
    profile.phase(Some("h1"), Phase::Connect, 300);
    profile.phase(Some("h2"), Phase::Connect, 200);
    profile.phase(Some("h1"), Phase::Connect, 100);
    profile.natives("h1", &["stat".to_string()]);
    profile.natives("h2", &[]);
    profile.task(record("h1", "stat", Some(ran(ExecPath::Native, 40))));
    profile.task(record("h2", "stat", Some(ran(ExecPath::Native, 60))));
    profile.task(record("h1", "stat", Some(ran(ExecPath::Native, 50))));
    let python = Ran {
        fork_micros: Some(900),
        import_micros: Some(8000),
        module_micros: Some(3000),
        ..ran(ExecPath::Python, 12000)
    };
    profile.task(record("h1", "setup", Some(python)));
    let fallback = |reason: &str| Ran {
        reason: Some(reason.into()),
        ..ran(ExecPath::Fallback, 5000)
    };
    profile.task(record("h1", "copy", Some(fallback("validate"))));
    profile.task(record("h2", "copy", Some(fallback("validate"))));
    profile.task(record("h2", "copy", Some(fallback("remote_src"))));
    profile.task(record("h2", "ping", None));
    profile
}

/// The profile table has a line per phase and per module and path, plus Python's parts and the
/// hand-back reasons.
#[test]
fn the_table_has_a_line_per_phase_and_per_module_and_path() {
    let table = filled().render();
    let expected = "\
PROFILE
phase              total_us  per host
compile                1200
connect                 600  h1=400 h2=200
module                   path           n  median_us       sum_us   fork_us import_us module_us  reasons
copy                     fallback       3       5000        15000         -         -         -  remote_src x1, validate x2
ping                     unreported     1          -            0         -         -         -
setup                    python         1      12000        12000       900      8000      3000
stat                     native         3         50          150         -         -         -
tasks by path: fallback=3 native=3 python=1 unreported=1
counters
";
    assert!(table.starts_with(expected), "\n{table}");
}

/// Times a host reported are added without overflowing: the host is not trusted to send figures
/// that fit.
#[test]
fn a_host_s_figures_cannot_overflow_the_sum() {
    let profile = Profile::default();
    profile.task(record("h1", "stat", Some(ran(ExecPath::Native, u64::MAX))));
    profile.task(record("h1", "stat", Some(ran(ExecPath::Native, u64::MAX))));
    let table = profile.render();
    assert!(table.contains(&format!(" {} ", u64::MAX)), "{table}");
}

/// The first line names each host's natives, which is how the golden comparison knows which
/// natives were on; then the phases; then one line per task, `ran` spread into it.
#[test]
fn the_json_starts_with_the_natives_and_has_a_line_per_task() {
    let dir_guard = scratch();
    let dir = dir_guard.path().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("profile.jsonl");
    filled().write_json(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0], json!({"natives": {"h1": ["stat"], "h2": []}}));
    assert_eq!(
        lines[1],
        json!({"phase": "compile", "host": null, "micros": 1200})
    );
    assert_eq!(
        lines[2],
        json!({"phase": "connect", "host": "h1", "micros": 400})
    );
    assert_eq!(lines.len(), 1 + 3 + 8 + 1);
    assert_eq!(lines[12]["counters"]["render_when"], json!(0), "{text}");
    assert_eq!(
        lines[12]["repeats"],
        json!({"total": 0, "top": []}),
        "{text}"
    );
    assert_eq!(
        lines[4],
        json!({"host": "h1", "index": 1, "task": "t", "module": "stat", "path": "native",
               "micros": 40, "prepare_micros": 7})
    );
    assert_eq!(
        lines[8],
        json!({"host": "h1", "index": 1, "task": "t", "module": "copy", "path": "fallback",
               "reason": "validate", "micros": 5000, "prepare_micros": 7})
    );
}

/// The counters come by name in declaration order, and the table carries them.
#[test]
fn the_counters_are_named_in_declaration_order() {
    count(Counter::RenderWhen, 2);
    count(Counter::WireInBytes, 9);
    let all = counters();
    assert_eq!(all.len(), 14);
    assert_eq!(all[0], ("render_when", 2));
    assert_eq!(all[13], ("wire_in_bytes", 9));
    let table = Profile::default().render();
    let row = format!("\n{:<24} {:>14}\n", "render_when", 2);
    assert!(table.contains(&row), "{table}");
}

/// A text rendered twice for one step and one host is one repeat; the same text for another host
/// or another step is not.
#[test]
fn a_text_rendered_again_for_one_step_and_host_is_a_repeat() {
    track_repeats(true);
    for (task, host) in [(3, "h1"), (3, "h1"), (3, "h2"), (4, "h1")] {
        note_render(task, host, "{{ x }}");
    }
    note_render(5, "h1", "{{ y\n }}");
    note_render(5, "h1", "{{ y\n }}");
    bridge_call("to_yaml", 40);
    bridge_call("to_yaml", 2);
    assert_eq!(
        repeats(20),
        (2, vec![("{{ x }}".into(), 1), ("{{ y\n }}".into(), 1)])
    );
    assert_eq!(repeats(1).1.len(), 1);
    let table = Profile::default().render();
    let block = table.split_once("bridge plugin").unwrap().1;
    let row = format!("\n{:<24} {:>14} {:>14}\n", "to_yaml", 2, 42);
    assert!(block.contains(&row), "{table}");
    assert!(block.contains("\nrepeated renders: 2\n"), "{table}");
    assert!(block.contains("\n       1  {{ y\\n }}\n"), "{table}");
}

/// Without tracking nothing is kept but the counters themselves.
#[test]
fn nothing_but_the_counters_is_kept_without_tracking() {
    note_render(1, "h1", "{{ x }}");
    note_render(1, "h1", "{{ x }}");
    bridge_call("to_yaml", 40);
    assert_eq!(repeats(20), (0, Vec::new()));
    assert!(bridge_table().is_empty());
    assert_eq!(counters()[10], ("bridge_calls", 1));
}
