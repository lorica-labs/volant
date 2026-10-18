// SPDX-License-Identifier: GPL-3.0-or-later
use super::session::{Next, session_for_tests};
use super::*;
use rstest::rstest;

#[rstest]
#[case::always(Some(Debugger::Always), false, Kind::Passed, true)]
#[case::never_beats_the_strategy(Some(Debugger::Never), true, Kind::Failed, false)]
#[case::on_failed(Some(Debugger::OnFailed), false, Kind::Failed, true)]
#[case::on_failed_not_skipped(Some(Debugger::OnFailed), false, Kind::Skipped, false)]
#[case::on_failed_not_unreachable(Some(Debugger::OnFailed), true, Kind::Unreachable, false)]
#[case::on_skipped(Some(Debugger::OnSkipped), false, Kind::Skipped, true)]
#[case::on_unreachable(Some(Debugger::OnUnreachable), false, Kind::Unreachable, true)]
#[case::on_unreachable_not_failed(Some(Debugger::OnUnreachable), false, Kind::Failed, false)]
#[case::strategy_default_failed(None, true, Kind::Failed, true)]
#[case::strategy_default_unreachable(None, true, Kind::Unreachable, true)]
#[case::strategy_default_skipped(None, true, Kind::Skipped, false)]
#[case::strategy_default_passed(None, true, Kind::Passed, false)]
#[case::silent_without_either(None, false, Kind::Failed, false)]
fn a_result_opens_the_prompt_as_the_keyword_says(
    #[case] setting: Option<Debugger>,
    #[case] by_default: bool,
    #[case] kind: Kind,
    #[case] opens_it: bool,
) {
    assert_eq!(opens(setting.as_ref(), by_default, kind), opens_it);
}

#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Feeds `lines` to a prompt and returns what it printed and the last answer.
fn typed(session: &mut Session, lines: &[&str]) -> (String, Next) {
    let captured = Captured::default();
    let mut out = Renderer::with_writer(Box::new(captured.clone()), false, 80, 0);
    let mut next = Next::Stay;
    for line in lines {
        next = session.onecmd(line, &mut out);
    }
    drop(out);
    let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    (text, next)
}

const HELP: &str = "\nDocumented commands (type help <topic>):\n\
    ========================================\n\
    EOF  c  continue  h  help  p  pprint  q  quit  r  redo  u  update_task\n\n";

/// What each line typed at the prompt prints, and whether the prompt stays open.
#[rstest]
#[case::task("p task", "TASK: t_ok\n", Next::Stay)]
#[case::task_name("p task.name", "'t_ok'\n", Next::Stay)]
#[case::action("pprint task.action", "'command'\n", Next::Stay)]
#[case::host("p host", "h1\n", Next::Stay)]
#[case::var("p task_vars['x']", "1\n", Next::Stay)]
#[case::keys("p task_vars.keys()", "dict_keys(['x', 'hostvars'])\n", Next::Stay)]
#[case::result("p result", "<TaskResult h1 t_ok>\n", Next::Stay)]
#[case::result_name("p result.task_name", "'t_ok'\n", Next::Stay)]
#[case::result_attr(
    "p result._result",
    "***AttributeError:AttributeError(\"'_RawTaskResult' object has no attribute '_result'\")\n",
    Next::Stay
)]
#[case::missing_key("p task_vars['nope']", "***KeyError:KeyError('nope')\n", Next::Stay)]
#[case::name_error(
    "foo",
    "***NameError:NameError(\"name 'foo' is not defined\")\n",
    Next::Stay
)]
#[case::bare_expression("task_vars['x']", "1\n", Next::Stay)]
#[case::assignment("task.args['_raw_params'] = '/bin/true'", "", Next::Stay)]
#[case::help_one(
    "h r",
    "Schedule task for re-execution. The re-execution may not be the next result\n",
    Next::Stay
)]
#[case::help_update(
    "help update_task",
    "Recreate the task from ``task._ds``, and template with updated ``task_vars``\n",
    Next::Stay
)]
#[case::help_unknown("h nope", "*** No help on nope\n", Next::Stay)]
#[case::help("h", HELP, Next::Stay)]
#[case::continue_short("c", "", Next::Continue)]
#[case::continue_long("continue", "", Next::Continue)]
#[case::redo_short("r", "", Next::Redo)]
#[case::redo_long("redo", "", Next::Redo)]
#[case::quit("q", "", Next::Quit)]
#[case::end_of_input("EOF", "", Next::Quit)]
fn a_line_at_the_prompt_answers_as_the_reference_does(
    #[case] line: &str,
    #[case] printed: &str,
    #[case] next: Next,
) {
    let mut s = session_for_tests("h1", "t_ok");
    s.vars.map.insert("x".to_string(), serde_json::json!(1));
    assert_eq!(typed(&mut s, &[line]), (printed.to_string(), next));
}

/// An empty line repeats the last command; an assignment shows in the next `p`, and `u`
/// renders the task again from the edited `task_vars`.
#[test]
fn the_prompt_keeps_what_was_typed() {
    let mut s = session_for_tests("h1", "t_ok");
    s.raw_args
        .insert("_raw_params".to_string(), serde_json::json!("{{ cmd }}"));
    let lines = [
        "p host",
        "",
        "task_vars['cmd'] = '/bin/true'",
        "u",
        "p task.args",
        "task.args['_raw_params'] = 'x'",
        "p task.args['_raw_params']",
    ];
    assert_eq!(
        typed(&mut s, &lines),
        (
            "h1\nh1\n{'_raw_params': '/bin/true'}\n'x'\n".to_string(),
            Next::Stay
        )
    );
}

/// A mapping too wide for one line prints one sorted key per line, as `pprint` does.
#[test]
fn a_wide_mapping_prints_one_key_per_line() {
    let value = serde_json::json!({"b": "x".repeat(40), "a": "y".repeat(40)});
    assert_eq!(
        expr::pformat(&value),
        format!("{{'a': '{}',\n 'b': '{}'}}", "y".repeat(40), "x".repeat(40))
    );
}
