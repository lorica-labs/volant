// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use rstest::rstest;

#[rstest]
#[case::always(Some(Debugger::Always), false, Kind::Passed, true)]
#[case::never_beats_the_strategy(Some(Debugger::Never), true, Kind::Failed, false)]
#[case::on_failed(Some(Debugger::OnFailed), false, Kind::Failed, true)]
#[case::on_failed_not_skipped(Some(Debugger::OnFailed), false, Kind::Skipped, false)]
#[case::on_skipped(Some(Debugger::OnSkipped), false, Kind::Skipped, true)]
#[case::on_unreachable(Some(Debugger::OnUnreachable), false, Kind::Unreachable, true)]
#[case::on_unreachable_not_failed(Some(Debugger::OnUnreachable), false, Kind::Failed, false)]
#[case::strategy_default_failed(None, true, Kind::Failed, true)]
#[case::strategy_default_passed(None, true, Kind::Passed, false)]
#[case::silent_without_either(None, false, Kind::Failed, false)]
fn a_result_opens_the_prompt_as_the_keyword_says(
    #[case] setting: Option<Debugger>,
    #[case] strategy_debug: bool,
    #[case] kind: Kind,
    #[case] opens_it: bool,
) {
    assert_eq!(opens(setting, strategy_debug, kind), opens_it);
}
