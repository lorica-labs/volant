// SPDX-License-Identifier: GPL-3.0-or-later
//! Tag selection: which tasks `--tags` and `--skip-tags` leave in.

/// Which tasks `--tags` and `--skip-tags` leave in.
///
/// This is ansible-core's own tag algebra, read off `Taggable.evaluate_tags` on 2.19.12 and
/// measured case by case against it. Three things in it are easy to get wrong and each one was
/// measured: a task carrying `always` survives every `--tags`, and survives `--skip-tags all`
/// unless `always` is itself skipped; a task carrying `never` is left out of `--tags all` and of
/// `--tags tagged`, and comes back only when something names it; and a task carrying no tag at
/// all is not an empty set there but the one-element set `{untagged}`, which is why
/// `--tags untagged` selects it and `--skip-tags untagged` drops it.
///
/// One selection serves the run and the four listings, so `--tags x` cannot show one list and
/// run another.
#[derive(Debug, Clone)]
pub(crate) struct TagSelection {
    run: Vec<String>,
    skip: Vec<String>,
}

/// The selection a run nobody narrowed gets, built through [`TagSelection::new`] rather than from
/// two empty lists: an empty `run` list selects **nothing**, so a derived `Default` would give a
/// compiled play that drops every task it was handed.
impl Default for TagSelection {
    fn default() -> Self {
        TagSelection::new(Vec::new(), Vec::new())
    }
}

/// What the reference calls a task with no tags of its own.
const UNTAGGED: &str = "untagged";

impl TagSelection {
    /// An empty `--tags` is the pseudo-tag `all`, which the reference substitutes before a play
    /// ever sees the list. Doing it in the constructor rather than at each call site is what
    /// keeps `never` out of a run nobody narrowed: with an empty list the first half of
    /// [`TagSelection::selects`] does not run at all, and a `never` task would stay in.
    pub fn new(run: Vec<String>, skip: Vec<String>) -> Self {
        TagSelection {
            run: if run.is_empty() {
                vec!["all".to_string()]
            } else {
                run
            },
            skip,
        }
    }

    /// The tags asked for and the tags skipped, as `ansible_run_tags` and `ansible_skip_tags`
    /// show them: `['all']` and `[]` when nothing narrowed the run.
    pub fn lists(&self) -> (&[String], &[String]) {
        (&self.run, &self.skip)
    }

    pub fn selects(&self, tags: &[String]) -> bool {
        let own: &[String] = tags;
        let untagged = own.is_empty() || (own.len() == 1 && own[0] == UNTAGGED);
        // An empty tag list reads as `{untagged}`, exactly as it does in the reference.
        let carries = |name: &str| {
            if own.is_empty() {
                name == UNTAGGED
            } else {
                own.iter().any(|t| t == name)
            }
        };
        let names = |list: &[String]| list.iter().any(|t| carries(t));
        let listed = |list: &[String], name: &str| list.iter().any(|t| t == name);

        // No emptiness test on the run list: `new` put `all` in an empty one, so the selecting
        // half always applies. Guarding it would be a branch that cannot be taken and would read
        // as if the dangerous value were still reachable.
        let mut run = carries("always")
            || (listed(&self.run, "all") && !carries("never"))
            || names(&self.run)
            || (listed(&self.run, "tagged") && !untagged && !carries("never"));
        if run && !self.skip.is_empty() {
            if listed(&self.skip, "all") {
                // `--skip-tags all` is the one place `always` survives a skip, and it stops
                // surviving the moment `always` is named alongside it.
                run = carries("always") && !listed(&self.skip, "always");
            } else if names(&self.skip) || (listed(&self.skip, "tagged") && !untagged) {
                run = false;
            }
        }
        run
    }
}
