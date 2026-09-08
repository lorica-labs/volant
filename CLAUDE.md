# Volant repository conventions

This is a public open source repository. Everything visible in it follows the rules below.

## Non-negotiable

- Never mention an AI assistant anywhere: commits, pull requests, issues, code, docs. No `Co-Authored-By` trailer. This file and `.claude/skills/` are the only exceptions.
- No tool-specific comments in code. Deliberate simplifications are documented in `docs/superpowers/architecture.md`, which is git-ignored.
- `docs/superpowers/` holds specs, plans, `progress.md`, `errors.md`, `architecture.md`, `infra.md`. Never commit it. Update `progress.md` after each task and `errors.md` when something breaks.
- Never write infrastructure hostnames, addresses or usernames in the repository. Use the SSH aliases from `docs/superpowers/infra.md`.
- Public text (README, docs, error messages) is English and passes the humanizer skill before commit.

## Git

- Branch per change (`feat/`, `fix/`, `docs/`, `ci/`, `chore/`), pull request per branch, `gh pr merge --squash --delete-branch`. Never push to `main`, never merge locally.
- Conventional Commits, imperative, subject under 72 characters, no mention of phases, tasks, milestones or plans. The `prepare-commit-msg` hook adds the sign-off trailer and git signs the commit with SSH; `just setup` installs the hooks.
- A pull request body carries **no** `Signed-off-by` trailer: `gh pr create --fill` puts one there, delete it afterward. Check with `gh pr view --json body --jq .body | grep -c Signed-off-by`, which must print `0`. The squash still takes the trailer from the branch commit, so deleting it from the body still leaves one in the merged commit; leaving it in doubles it. The `signoff` CI check reads commits, not the body, so it cannot catch a duplicate.
- A workflow is run locally with `gh act` before it is committed.

## Code

- `// SPDX-License-Identifier: GPL-3.0-or-later` first line of every `.rs` file.
- `just check` before every pull request. New dependencies are justified in the pull request.
- No `scripts/` directory: `justfile` recipes, and a shell script only when a one-line recipe cannot do it.

## Skills

- `release`: cut a release through release-plz and cargo-dist.
