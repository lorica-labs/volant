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

/// The options `-vvvv` lists under each playbook's banner. The reference lists every option it
/// holds a value for; this lists the ones every run has, so a run that sets others shows fewer.
pub(super) fn options_dump(args: &PlaybookArgs, config: &Config, forks: usize) -> Vec<String> {
    let quoted = |items: Vec<String>| match items.as_slice() {
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
        .map(|s| format!("'{}'", s.text()))
        .collect();
    // A Python tuple's repr: a lone item keeps its trailing comma.
    let inventory = match inventory.as_slice() {
        [one] => format!("({one},)"),
        all => format!("({})", all.join(", ")),
    };
    let playbooks: Vec<String> = args
        .playbooks
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    vec![
        format!("Positional arguments: {}", playbooks.join(" ")),
        format!("verbosity: {}", args.verbose),
        format!(
            "connection: {}",
            args.connection.as_deref().unwrap_or("ssh")
        ),
        format!(
            "become_method: {}",
            args.become_method
                .as_deref()
                .unwrap_or(&config.become_method)
        ),
        format!(
            "tags: {}",
            quoted(if tags.is_empty() {
                vec!["all".to_string()]
            } else {
                tags
            })
        ),
        format!("inventory: {inventory}"),
        format!("forks: {forks}"),
    ]
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
