// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;

use super::args::PlaybookArgs;
use crate::config::Config;
use crate::inventory::Source;
use crate::render::Renderer;

/// What `--version` prints, and what `-vv` opens the run with in place of the reference's block
/// of machine paths: Volant's own version and the ansible-core release it matches.
pub(super) fn version_line() -> String {
    format!(
        "volant {} (ansible-core {} compatible)",
        crate::VERSION,
        crate::vars::ansible_version()["full"]
            .as_str()
            .unwrap_or_default()
    )
}

/// The lines the reference prints before it reads an inventory: the version at `-vv`, the
/// configuration file at `-v`, and at `-vvvv` the plugin set-up.
pub(super) fn announce(out: &mut Renderer) {
    if out.verbose_at(2) {
        out.plain(2, &version_line());
    }
    if out.verbose_at(1) {
        let absolute = |path: PathBuf| std::path::absolute(&path).unwrap_or(path);
        out.verbose(
            1,
            &match crate::config::locate() {
                Some(path) => format!("Using {} as config file", absolute(path).display()),
                None => "No config file found; using defaults".to_string(),
            },
        );
    }
    out.verbose(4, "setting up inventory plugins");
    // The reference's own line, trailing space included: it names the directory its collection
    // loader found `ansible.builtin` in, and the built-in plugins live in no directory here.
    out.verbose(4, "Loading collection ansible.builtin from ");
}

/// The lines before the first playbook: the stdout callback loaded, and the two the reference
/// sets aside. Its line names the plugin file, which has no counterpart here, so it names the
/// engine's own callback.
pub(super) fn announce_callbacks(out: &mut Renderer) {
    out.verbose(
        4,
        "Loading callback plugin default of type stdout, v2.0 from the built-in callback",
    );
    for callback in ["minimal", "oneline"] {
        out.verbose(
            2,
            &format!("Skipping callback '{callback}', as we already have a stdout callback."),
        );
    }
}

/// The sources a run reads: every `-i`, else the configuration's, else the reference's
/// `/etc/ansible/hosts`.
pub(super) fn inventory_sources(args: &PlaybookArgs, config: &Config) -> Vec<Source> {
    if !args.inventory.is_empty() {
        return args.inventory.iter().map(|s| Source::new(s)).collect();
    }
    if !config.inventory.is_empty() {
        return config
            .inventory
            .iter()
            .map(|p| Source::new(&p.to_string_lossy()))
            .collect();
    }
    vec![Source::default_source()]
}

/// The options `-vvvv` lists under each playbook's banner, in the reference's order. As in the
/// reference, an option shows when it holds a value: the ones with a default (`connection`,
/// `become_method`, `tags`, `inventory`, `forks`) always, the rest when the command line gave
/// them. The transfer arguments show though no transfer of this engine uses `scp` or `sftp`.
#[expect(
    clippy::too_many_lines,
    reason = "one line per option, in the reference's order"
)]
pub(super) fn options_dump(args: &PlaybookArgs, config: &Config, forks: usize) -> Vec<String> {
    // A Python tuple's repr: strings with Python's quoting, a lone item keeping its comma.
    let tuple = |items: Vec<String>| {
        let items: Vec<String> = items.iter().map(|i| py_str(i)).collect();
        match items.as_slice() {
            [one] => format!("({one},)"),
            _ => format!("({})", items.join(", ")),
        }
    };
    let tags: Vec<String> = args
        .tags
        .iter()
        .flat_map(|t| crate::config::tag_list(t))
        .collect();
    let skip_tags: Vec<String> = args
        .skip_tags
        .iter()
        .flat_map(|t| crate::config::tag_list(t))
        .collect();
    let inventory: Vec<String> = inventory_sources(args, config)
        .iter()
        .map(|s| s.text().to_string())
        .collect();
    let playbooks: Vec<String> = args
        .playbooks
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let mut lines = vec![
        format!("Positional arguments: {}", playbooks.join(" ")),
        format!("verbosity: {}", args.verbose),
    ];
    let mut given = |key: &str, value: Option<String>| {
        if let Some(value) = value {
            lines.push(format!("{key}: {value}"));
        }
    };
    let key = args
        .private_key
        .clone()
        .or_else(|| config.private_key_file.clone());
    given(
        "private_key_file",
        key.map(|p| std::path::absolute(&p).unwrap_or(p).display().to_string()),
    );
    given(
        "remote_user",
        args.user.clone().or_else(|| config.remote_user.clone()),
    );
    given(
        "connection",
        Some(args.connection.as_deref().unwrap_or("ssh").to_string()),
    );
    given("timeout", args.timeout.map(|t| t.to_string()));
    given("ssh_common_args", args.ssh_common_args.clone());
    given("sftp_extra_args", args.sftp_extra_args.clone());
    given("scp_extra_args", args.scp_extra_args.clone());
    given("ssh_extra_args", args.ssh_extra_args.clone());
    let flag = |on: bool| on.then(|| "True".to_string());
    given("force_handlers", flag(args.force_handlers));
    given("become", flag(args.r#become));
    given(
        "become_method",
        Some(
            args.become_method
                .as_deref()
                .unwrap_or(&config.become_method)
                .to_string(),
        ),
    );
    given("become_user", args.become_user.clone());
    given(
        "tags",
        Some(tuple(if tags.is_empty() {
            vec!["all".to_string()]
        } else {
            tags
        })),
    );
    given(
        "skip_tags",
        (!skip_tags.is_empty()).then(|| tuple(skip_tags)),
    );
    given("check", flag(args.check));
    given("diff", flag(args.diff));
    given("inventory", Some(tuple(inventory)));
    given("subset", args.limit.clone());
    given("flush_cache", flag(args.flush_cache));
    given(
        "extra_vars",
        (!args.extra_vars.is_empty()).then(|| tuple(args.extra_vars.clone())),
    );
    given(
        "vault_password_files",
        (!args.vault_password_file.is_empty()).then(|| {
            tuple(
                args.vault_password_file
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect(),
            )
        }),
    );
    given("forks", Some(forks.to_string()));
    let modules: Vec<String> = module_path_dirs(args)
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    given("module_path", (!modules.is_empty()).then(|| tuple(modules)));
    given("step", flag(args.step));
    given("start_at_task", args.start_at_task.clone());
    lines
}

/// The `-M` directories as the reference reads them: each argument split on `:`, empty parts
/// dropped, every path made absolute. A later `-M` goes in front of the earlier ones (the
/// option prepends), and the order inside one argument stays.
pub(super) fn module_path_dirs(args: &PlaybookArgs) -> Vec<PathBuf> {
    args.module_path
        .iter()
        .rev()
        .flat_map(|list| list.split(':'))
        .filter(|part| !part.is_empty())
        .map(|part| std::path::absolute(part).unwrap_or_else(|_| PathBuf::from(part)))
        .collect()
}

/// A Python `str`'s repr: single quotes unless the text holds one and no double quote.
fn py_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let body = text
        .replace('\\', r"\\")
        .replace(quote, &format!("\\{quote}"))
        .replace('\n', r"\n")
        .replace('\t', r"\t")
        .replace('\r', r"\r");
    format!("{quote}{body}{quote}")
}

/// The reference reads group_vars and host_vars as the first play starts, so a vaulted file it
/// cannot decrypt stops the run after that play's banner.
pub(super) fn banner_before_vars_refusal(
    out: &mut Renderer,
    err: &anyhow::Error,
    playbooks: &[crate::playbook::Playbook],
) {
    let refused = err.downcast_ref::<crate::stats::Refusal>();
    if refused.is_some_and(|r| r.code == 4)
        && let Some(play) = playbooks.iter().flat_map(|pb| &pb.plays).next()
    {
        out.play(&play.name);
    }
}
