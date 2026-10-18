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
pub(super) fn options_dump(args: &PlaybookArgs, config: &Config, forks: usize) -> Vec<String> {
    let tuple = |items: Vec<String>| match items.as_slice() {
        [one] => format!("('{one}',)"),
        _ => format!(
            "({})",
            items
                .iter()
                .map(|i| format!("'{i}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let tags: Vec<String> = args
        .tags
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
    given(
        "private_key_file",
        args.private_key.as_ref().map(|p| {
            std::path::absolute(p)
                .unwrap_or_else(|_| p.clone())
                .display()
                .to_string()
        }),
    );
    given("remote_user", args.user.clone());
    given(
        "connection",
        Some(args.connection.as_deref().unwrap_or("ssh").to_string()),
    );
    given("timeout", args.timeout.map(|t| t.to_string()));
    given("ssh_common_args", args.ssh_common_args.clone());
    given("sftp_extra_args", args.sftp_extra_args.clone());
    given("scp_extra_args", args.scp_extra_args.clone());
    given("ssh_extra_args", args.ssh_extra_args.clone());
    given(
        "become_method",
        Some(
            args.become_method
                .as_deref()
                .unwrap_or(&config.become_method)
                .to_string(),
        ),
    );
    given(
        "tags",
        Some(tuple(if tags.is_empty() {
            vec!["all".to_string()]
        } else {
            tags
        })),
    );
    given("inventory", Some(tuple(inventory)));
    given(
        "extra_vars",
        (!args.extra_vars.is_empty()).then(|| tuple(args.extra_vars.clone())),
    );
    given("forks", Some(forks.to_string()));
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
