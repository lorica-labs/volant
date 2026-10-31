// SPDX-License-Identifier: GPL-3.0-or-later
//! The keyword reference page, generated from the tables.

use super::*;

/// What one table says about `name`, as the published page spells it, with the class the site
/// draws it by.
fn status(table: &[Keyword], name: &str) -> &'static str {
    match table.iter().find(|k| k.name == name).map(|k| k.support) {
        Some(Runs) => r#"<span class="kw kw-runs">runs</span>"#,
        Some(Partial) => r#"<span class="kw kw-partial">partial</span>"#,
        None => r#"<span class="kw kw-no">not accepted</span>"#,
    }
}

/// The rows that run with a limit, and the limit.
fn limits(out: &mut String) {
    let mut noted: Vec<(&str, &str)> = [TASK_KEYWORDS, PLAY_KEYWORDS, BLOCK_KEYWORDS]
        .iter()
        .flat_map(|table| table.iter())
        .filter_map(|k| k.note.map(|n| (k.name, n)))
        .collect();
    noted.sort_unstable();
    noted.dedup();
    out.push_str("\n## Limits of what runs\n\n| Keyword | Limit |\n|---|---|\n");
    for (name, note) in noted {
        let _ = writeln!(out, "| `{name}` | {note} |");
    }
}

/// The keywords that run with a gap, and the sentence naming it.
fn partial_rows() -> Vec<(&'static str, &'static str)> {
    let mut partial: Vec<(&str, &str)> = [
        TASK_KEYWORDS,
        PLAY_KEYWORDS,
        LOOP_CONTROL_KEYWORDS,
        BLOCK_KEYWORDS,
        HANDLER_KEYWORDS,
    ]
    .iter()
    .flat_map(|table| table.iter())
    .filter_map(|k| k.missing.map(|m| (k.name, m)))
    .collect();
    partial.sort_unstable();
    partial.dedup();
    partial
}

/// The page published in the documentation, generated from the tables so it cannot say
/// something the loader and the pre-flight do not do. The legend and the main grid are HTML, so the
/// site can draw each status as a shape and filter the grid; everything reads without scripts.
///
/// The status is per place a keyword can be written rather than per keyword, because the two
/// differ: `ignore_errors` runs on a block and on a task and is not supported on a play, and a
/// single column would have to pick one of the two and be wrong about the other.
pub fn documentation() -> String {
    render(&partial_rows())
}

/// The page, given the keywords that run with a gap and the sentence naming each gap.
pub(super) fn render(partial: &[(&str, &str)]) -> String {
    let mut out = String::from(
        "---\ntitle: Keywords\ndescription: Every play, block and task keyword ansible-core 2.19 \
         knows, and what Volant does with each one.\n---\n\n<!-- Generated from \
         crates/volant/src/keywords.rs by `just docs-keywords`. Do not edit by hand. -->\n\n\
         Volant loads a playbook against the whole ansible-core 2.19 grammar, so it parses here \
         as it parses under Ansible. What this release does not support yet is named before the \
         run starts, never accepted and then ignored. Each keyword has a status for each place it \
         can be written.\n\n\
         <ul class=\"kw-legend not-content\">\n\
         <li><span class=\"kw kw-runs\">runs</span><span>Volant honors it, or names the one value \
         it does not support yet.</span></li>\n",
    );
    if !partial.is_empty() {
        out.push_str(
            "<li><span class=\"kw kw-partial\">partial</span><span>Accepted and honored, but not \
             everything Ansible does with it. The gap is listed below the grid.</span></li>\n",
        );
    }
    out.push_str(
        "<li><span class=\"kw kw-no\">not accepted</span><span>Cannot be written there. Ansible \
         rejects it too, when it reads the playbook.</span></li>\n\
         </ul>\n\n\
         A keyword inside a file that only a dynamic `include_tasks` or `include_role` names is \
         checked when a host reaches that statement: if it is not supported yet, the statement \
         fails for that host, a `rescue` around it can catch the failure, and nothing in the file \
         runs. See [Includes and imports](/docs/playbooks/roles/#dynamic-includes).\n\n\
         A keyword that takes a boolean or a number may be written as a template: `become`, \
         `no_log`, `run_once`, `any_errors_fatal`, `ignore_unreachable`, `delegate_facts`, \
         `timeout`, `async` and `poll`. It is rendered for each host when its task runs, once the \
         task's `when` has held, and a value the keyword cannot take fails the task with the \
         reference's sentence. `gather_facts`, `force_handlers`, `strategy` and `order` of a play \
         are rendered when the play starts, with the play's variables and the extra vars. A \
         literal that is not a boolean is refused when the playbook loads. A `timeout` that is \
         negative or not a number fails its task when it runs, as in Ansible, even when written \
         as a literal.\n\n\
         This page is generated from the tables the loader and the pre-flight read, so it always \
         describes the release it ships with. A status says what Volant does with the keyword, \
         not that every value works: the [known gaps](/docs/start/compatibility/#known-gaps) list the \
         values that fail.\n\n\
         ## All keywords\n\n\
         <div class=\"kw-grid-wrap not-content\">\n<table class=\"kw-grid\">\n<thead><tr>\
         <th>Keyword</th><th>Play</th><th>Block</th><th>Task</th></tr></thead>\n<tbody>\n",
    );
    let mut names: Vec<&str> = [TASK_KEYWORDS, PLAY_KEYWORDS, BLOCK_KEYWORDS]
        .iter()
        .flat_map(|table| table.iter())
        .map(|k| k.name)
        .collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let _ = writeln!(
            out,
            "<tr><td><code>{name}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>",
            status(PLAY_KEYWORDS, name),
            status(BLOCK_KEYWORDS, name),
            status(TASK_KEYWORDS, name)
        );
    }
    out.push_str("</tbody>\n</table>\n</div>\n");
    if !partial.is_empty() {
        out.push_str(
            "\n## What partial leaves out\n\nEach keyword below behaves the same way in every \
             place the grid marks it `partial`.\n\n| Keyword | What is missing |\n|---|---|\n",
        );
        for (name, missing) in partial {
            let _ = writeln!(out, "| `{name}` | {missing} |");
        }
    }
    limits(&mut out);

    out.push_str(
        "\n## Handlers\n\nA handler accepts every task keyword above, plus one of its own.\n\n\
         | Keyword | Status |\n|---|---|\n",
    );
    for k in HANDLER_KEYWORDS {
        let _ = writeln!(
            out,
            "| `{}` | {} |",
            k.name,
            status(HANDLER_KEYWORDS, k.name)
        );
    }
    out.push_str(
        "\n## Under loop_control\n\nA sub-key ansible-core does not know is rejected when the \
         playbook loads.\n\n| Sub-key | Status |\n|---|---|\n",
    );
    for k in LOOP_CONTROL_KEYWORDS {
        let _ = writeln!(
            out,
            "| `{}` | {} |",
            k.name,
            status(LOOP_CONTROL_KEYWORDS, k.name)
        );
    }
    out
}
