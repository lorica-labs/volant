set shell := ["bash", "-uc"]
set windows-shell := ["bash", "-uc"]

# The measured line coverage minus one point: a threshold that cannot drift down in silence, and
# that one refactor removing a covered branch does not break.
COVERAGE_FLOOR := "91"

# The files coverage leaves out: the toolchain's sources, the integration tests, examples, benches
# and the build output. It replaces cargo-llvm-cov's default filter, which also drops every
# `tests.rs`, so the unit tests in their own files would stop counting.
COVERAGE_IGNORE := '(^|/)(rustc/[0-9a-f.]+|tests|examples|benches|target)/|/\.cargo/'


default:
    @just --list

# Install git hooks, sign-off, and the tools the other recipes need
setup:
    git config core.hooksPath .githooks
    cargo binstall -y cargo-nextest cargo-deny cargo-machete cargo-about cargo-dist release-plz typos-cli taplo-cli zizmor cargo-insta cargo-llvm-cov cargo-mutants

# Accept pending insta snapshots after reading them
insta-accept:
    cargo insta accept --workspace

# Regenerate the golden expectations with the reference ansible-core (see tests/golden/ANSIBLE_VERSION):
# every kind, or those named (template, inventory, scenario, module). Reproducible: running this on
# a correct tree leaves the template, inventory and scenario recordings byte for byte as they were,
# so an empty diff afterwards is a check and not a coincidence. The `module` kind needs sudo,
# systemd, apt and cron.
golden *kinds:
    "$(uv tool dir)/ansible-core/bin/python" crates/volant/tests/golden/generate.py {{kinds}}

# Regenerate the parity matrix's reference names (tests/parity/reference.json) with the
# reference ansible-core and the pinned `ansible` package's collections. Needs `uv`.
parity-reference:
    #!/usr/bin/env bash
    set -euo pipefail
    py="$(uv tool dir)/ansible-core/bin/python"
    pin="$(cat crates/volant/tests/parity/ANSIBLE_PACKAGE)"
    dir="target/parity/ansible-$pin"
    [ -d "$dir/ansible_collections" ] || uv pip install --python "$py" --target "$dir" --no-deps "ansible==$pin"
    "$py" crates/volant/tests/parity/generate.py "$dir"

# Regenerate the managed runtime's manifest (crates/volant/src/runtime/manifest.toml): the pinned
# python-build-standalone builds and, for each platform, the hash-pinned ansible-core wheels.
# Needs Python 3.12+ and network access.
runtime-lock:
    python3 -I crates/volant/src/runtime/lock.py

# Everything CI runs, in the same order
check: fmt lint test

# Before opening a pull request: formatting, clippy, the tests and typos. The rest of `lint`
# (the doc build, cargo deny, cargo machete, actionlint, zizmor) runs in CI on every pull request.
check-local: fmt clippy ratchet test
    typos

# The same as `check-local`, with the tests of the modules the branch changes instead of every test.
# CI runs every test.
check-touched: fmt clippy ratchet test-touched
    typos

# One line per test, `<binary-id> <name>`, with the module path up to the last `tests::` removed.
# A refactor that only moves code leaves this list unchanged.
test-names:
    cargo nextest list --workspace --message-format oneline --color never | sed -E 's/^([^ ]+) (.*::)?tests::/\1 /' | LC_ALL=C sort

# The tests of the modules changed against `base` (and in the working tree), as nextest filtersets.
# Over `just remote`, `base` is always origin/main.
test-touched base="origin/main":
    #!/usr/bin/env bash
    set -euo pipefail
    # Over `just remote` there is no .git: the changed files travel in .volant-changed instead.
    changed() {
        if git rev-parse --git-dir > /dev/null 2>&1; then
            git diff --name-only "{{base}}...HEAD" -- crates
            git diff --name-only HEAD -- crates
            git ls-files --others --exclude-standard -- crates
        else
            cat .volant-changed
        fi
    }
    terms=$(changed | tr -d '\r' | sort -u | awk -F/ '
        function module(p) {
            sub(/\/(mod|tests|test_support)\.rs$/, "", p); sub(/\.rs$/, "", p); gsub(/\//, "::", p); return p
        }
        $1 != "crates" || NF < 3 { next }
        {
            c = $2
            rest = substr($0, length("crates/" c "/") + 1)
            if (rest ~ /^src\/(lib|main|build)\.rs$/ || rest ~ /^src\/bin\// || rest == "Cargo.toml" || (rest ~ /^src\// && rest !~ /\.rs$/)) {
                print "package(" c ")"
            } else if (rest ~ /^src\/.*\.rs$/) {
                print "(package(" c ") & test(/^" module(substr(rest, 5)) "::/))"
            } else if (rest ~ /^tests\/[^\/]+\.rs$/) {
                name = substr(rest, 7); sub(/\.rs$/, "", name)
                print "(package(" c ") & binary(" name "))"
            } else if (rest ~ /^tests\//) {
                print "(package(" c ") & kind(test))"
            }
        }' | sort -u | paste -sd'|' - | sed 's/|/ | /g')
    if [ -z "$terms" ]; then
        echo "nothing to test"
        exit 0
    fi
    cargo nextest run --workspace --no-tests=warn -E "$terms"

fmt:
    cargo fmt --all --check
    taplo fmt --check

clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Source files may not grow past 500 lines, and the ones already past it may not grow at all:
# `file-budget.txt` holds `<path> <lines>`. `just ratchet write` lowers the numbers after a split
# and refuses to raise one. Test files are not counted.
ratchet mode="check":
    #!/usr/bin/env bash
    set -euo pipefail
    now=$(find crates/*/src -name '*.rs' ! -name tests.rs ! -name test_support.rs -exec wc -l {} + \
        | awk '$2 != "total" && $1 > 500 { print $2, $1 }' | LC_ALL=C sort)
    over=$(printf '%s\n' "$now" | awk '
        NR == FNR { budget[$1] = $2; next }
        $1 in budget { if ($2 > budget[$1]) printf "%s grew from %d to %d lines\n", $1, budget[$1], $2; next }
        { printf "%s has %d lines and is not in file-budget.txt (the limit is 500)\n", $1, $2 }
    ' file-budget.txt -)
    case "{{mode}}" in
        check | write) ;;
        *) echo "ratchet takes no argument or 'write', not '{{mode}}'" >&2; exit 2 ;;
    esac
    if [ -n "$over" ]; then
        echo "$over" >&2
        if [ "{{mode}}" = write ]; then
            echo "file-budget.txt not written: a number would go up" >&2
        else
            echo "split the file instead of growing it, then run 'just ratchet write'" >&2
        fi
        exit 1
    fi
    if [ "{{mode}}" = write ]; then
        printf '%s\n' "$now" > file-budget.txt
    fi

lint: clippy ratchet
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items
    cargo deny check
    cargo machete
    typos
    actionlint .github/workflows/*.yml
    zizmor .github/workflows

test:
    cargo nextest run --workspace --no-fail-fast

# The goldens that run the recorded ansible-core against this machine (sudo, systemd, apt).
# Run it when a native module, an action plugin or a golden recording changes; CI runs it in `ssh`.
test-reference:
    "${VOLANT_PYTHON:-$(uv tool dir)/ansible-core/bin/python}" -c 'import ansible' || { echo "no ansible-core"; exit 1; }
    VOLANT_PYTHON="${VOLANT_PYTHON:-$(uv tool dir)/ansible-core/bin/python}" cargo nextest run --workspace --ignore-default-filter --no-tests=fail -E 'binary(reference_modules) - test(=a_native_answer_is_held_to_the_path_its_index_names) | test(=every_runs_keyword_changes_something_observable)'

# One crate, one test pattern: the loop while you are iterating. `check` is the gate, not the loop.
test-one crate pattern:
    cargo nextest run -p {{crate}} -E 'test({{pattern}})'

# Line coverage over the workspace. The ssh tests are #[ignore]d and are not in this figure.
# llvm-cov's TOTAL row reads regions, then functions, then lines, and the first column is the one
# a reader takes for the last. The second command names the figure the threshold is about, so
# nobody has to count columns. `set -o pipefail` so a breach fails the recipe: this shell has no
# pipefail by default (see line 1), and without it the exit status is `tee`'s, which is always 0.
# Line coverage over the workspace, gated at COVERAGE_FLOOR.
coverage:
    set -o pipefail; cargo llvm-cov nextest --workspace --summary-only --fail-under-lines {{COVERAGE_FLOOR}} --no-default-ignore-filename-regex --ignore-filename-regex '{{COVERAGE_IGNORE}}' | tee /tmp/volant-coverage.txt
    @awk '/^TOTAL/ {print "regions " $4 "   functions " $7 "   LINES " $10 "  <- the threshold is on this one"}' /tmp/volant-coverage.txt

# The lcov file Codecov reads, from the profile data `just coverage` left behind and with its filter.
coverage-lcov:
    cargo llvm-cov report --lcov --output-path lcov.info --no-default-ignore-filename-regex --ignore-filename-regex '{{COVERAGE_IGNORE}}'

# Mutation score over the files this recipe is pointed at. Each mutant is a small edit to the
# source that the tests must notice; a mutant nothing catches is an assertion that could not have
# failed. `cargo build --workspace` first, and `--copy-target true`, carry the already-built
# target/ (with the sibling `volant-agent` binary the integration suite execs) into every
# mutant's scratch copy: without it, nothing inside cargo-mutants' own scratch build produces
# that binary and every mutant's baseline fails before a single mutation runs. `--test-tool
# nextest` matches every other recipe here and keeps tests process-isolated, which a handful of
# this suite's tests need.
#
# The constraint on `--jobs` is disk, not cores, and where the scratch copies land matters as
# much as how many there are. `--copy-target` gives each parallel job its own copy of target/,
# currently about 2.3 GB. cargo-mutants puts that scratch under TMPDIR, which defaults to /tmp;
# on the development machine /tmp is a 7.6 GB tmpfs shared by three lanes, and copying target/
# there overran it at both `--jobs 4` and `--jobs 2` ("Disk quota exceeded" mid-campaign,
# errors.md) - `--jobs 1` was the only value that ever fit on it. The plan's own prescribed
# scratch, `TMPDIR="$PWD/target/mutants-tmp"`, cannot work here either: it sits inside the tree
# `--copy-target` copies, so the copy recurses into its own destination until "File name too
# long" (errors.md, lane B's entry). TMPDIR below points at $HOME instead - outside the copied
# tree and on the real disk rather than the tmpfs - where three parallel copies at ~2.3 GB each
# is about 7 GB against the roughly 9.7 GB free there, which fits; four would not. Three lanes
# also share this machine's eight cores, so a mutants run must not claim the whole thing either.
# Slow on purpose: one build and one test run per mutant. Not in CI.
# Mutation score for the given file, one build and one test run per mutant. Not in CI.
mutants file="crates/volant/src/keywords.rs":
    mkdir -p "$HOME/.cache/volant-mutants"
    cargo build --workspace
    TMPDIR="$HOME/.cache/volant-mutants" cargo mutants --file {{file}} --timeout 120 --jobs 3 --no-shuffle --test-tool nextest --copy-target true

# Regenerate the modules reference page from the module registry
docs-modules:
    VOLANT_UPDATE_DOCS=1 cargo test -p volant-protocol the_documentation_table_matches_the_registry

# Regenerate the keywords reference page from the keyword tables
docs-keywords:
    VOLANT_UPDATE_DOCS=1 cargo test -p volant the_keyword_page_matches_the_tables

# Regenerate the parity page from the engine and tests/parity/reference.json
docs-parity:
    VOLANT_UPDATE_DOCS=1 cargo test -p volant --test parity the_parity_page_matches_the_engine

# Serve the documentation site with live reload (needs Node.js)
docs-dev:
    cd docs && npm ci && npx astro dev

# Build the documentation site and check every internal link (needs Node.js)
docs-build:
    cd docs && npm ci && npx astro build

# Run the CI workflow locally (needs Docker)
ci-local:
    gh act pull_request -W .github/workflows/ci.yml

# Copy the working tree to the machine named by VOLANT_DEV_HOST and run a recipe there. The file
# list comes from git, so tracked and new files travel while everything git ignores stays behind.
# The copy adds and overwrites, then deletes over there every file the list does not name (except
# under `target/`), so a removed scenario, fixture or test file never lingers and gets compiled.
#
# VOLANT_REMOTE_DIR names the directory over there, defaulting to `volant`. Set it to work on
# several branches at once without them sharing a target directory or overwriting each other's
# sources: one worktree here, one directory there, one name.
#
# `git ls-files` never carries `.git` itself across, so a recipe run over there through this one
# (`bench`, `bench-k3s`) has no `git describe` of its own to name the tree it measured. `.volant-
# describe` is written here, on the side that still has `.git`, from this same commit the tar is
# about to carry over, and travels as one more file in that same tar stream (added to the file list
# by hand, since it is gitignored and `--exclude-standard` would otherwise leave it behind). The list
# itself travels as `.volant-files`, which the deletion pass reads. `.volant-changed` lists the
# crate files the branch changes against origin/main, for `test-touched` over there.
remote +recipe:
    git describe --always --dirty > .volant-describe
    { git diff --name-only origin/main...HEAD -- crates; git diff --name-only HEAD -- crates; git ls-files --others --exclude-standard -- crates; } > .volant-changed
    git ls-files -z --cached --others --exclude-standard > .volant-files; printf '.volant-describe\0.volant-changed\0.volant-files\0' >> .volant-files
    set -o pipefail; dir="${VOLANT_REMOTE_DIR:-volant}"; prune='cd "$1" && find . -path ./target -prune -o -type f -print0 | sed -z "s|^\./||" | LC_ALL=C sort -z | LC_ALL=C comm -z -23 - <(LC_ALL=C sort -z .volant-files) | grep -zv -e "^\.volant-describe\$" -e "^\.volant-changed\$" -e "^\.volant-files\$" | xargs -0 -r rm -f --; find . -mindepth 1 -type d -empty -not -path ./target -not -path "./target/*" -delete'; tar -C . --null --files-from=.volant-files -czf - | ssh "$VOLANT_DEV_HOST" "mkdir -p '$dir' && tar -xzf - -C '$dir' && bash -c '$prune' _ '$dir'"
    dir="${VOLANT_REMOTE_DIR:-volant}"; ssh "$VOLANT_DEV_HOST" ". ~/.profile && cd '$dir' && just {{recipe}}"

# Build the agent as a static musl binary, the only form that can be uploaded to another host
agent-musl:
    cargo build -p volant-agent --release --target x86_64-unknown-linux-musl
    mkdir -p target/agents
    cp target/x86_64-unknown-linux-musl/release/volant-agent target/agents/volant-agent-x86_64-unknown-linux-musl

# Unpack a published archive into an empty directory and run one task from it. `just` runs
# recipes under `bash -uc`, without pipefail (see line 1), so this one is a script with the
# options it needs. `env -u VOLANT_AGENT_DIR` and the empty directory are the test: the
# controller has to use the agent it carries, with no other agent to fall back on.
smoke tag:
    #!/usr/bin/env bash
    set -euo pipefail
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    cd "$work"
    curl -fsSL -O "https://github.com/lorica-labs/volant/releases/download/{{tag}}/volant-x86_64-unknown-linux-musl.tar.xz"
    tar -xJf volant-x86_64-unknown-linux-musl.tar.xz --strip-components=1
    printf 'h1 ansible_connection=local\n' > inv.ini
    printf -- '- hosts: h1\n  gather_facts: false\n  tasks:\n    - command: /bin/true\n' > site.yml
    env -u VOLANT_AGENT_DIR ./volant playbook -i inv.ini site.yml

# What `smoke` checks on a release, from agents built here: embed them in a release controller
# through VOLANT_EMBED_AGENTS_DIR, copy the controller alone into an empty directory and run one
# task there with `VOLANT_AGENT_DIR` unset. The controller has to extract the agent it carries,
# and the recipe fails unless that agent then sits in the cache. The aarch64 agent needs zig to
# build here, so the x86_64 one stands in for it: this proves the mechanism, and the controller
# it leaves in target/release must not be pointed at an aarch64 host.
smoke-embedded: agent-musl
    #!/usr/bin/env bash
    set -euo pipefail
    agents="$PWD/target/embed-agents"
    mkdir -p "$agents"
    cargo build -p volant-agent --release
    cp target/release/volant-agent "$agents/volant-agent"
    cp target/agents/volant-agent-x86_64-unknown-linux-musl "$agents/"
    cp target/agents/volant-agent-x86_64-unknown-linux-musl "$agents/volant-agent-aarch64-unknown-linux-musl"
    # `volant_echo` is a native for the agent's own tests: a release agent must not carry it.
    if grep -alq volant_echo "$agents"/volant-agent*; then echo "a release agent carries the test-only native volant_echo"; exit 1; fi
    VOLANT_EMBED_AGENTS_DIR="$agents" cargo build -p volant --release
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    cp target/release/volant "$work/"
    cd "$work"
    printf 'h1 ansible_connection=local\n' > inv.ini
    printf -- '- hosts: h1\n  gather_facts: false\n  tasks:\n    - command: /bin/true\n' > site.yml
    env -u VOLANT_AGENT_DIR XDG_CACHE_HOME="$work/cache" ./volant playbook -i inv.ini site.yml
    cmp cache/volant/agents/*/volant-agent "$agents/volant-agent"

# Tests that need an sshd on localhost and a key in VOLANT_SSH_TEST_KEY (see CONTRIBUTING). The
# native module tests among them change the machine they run on: they go to the host
# VOLANT_TARGET_HOST names, through your own ssh configuration, and to localhost only on a GitHub
# Actions runner whose workflow sets VOLANT_NATIVE_TESTS_ON_LOCALHOST=1; otherwise they fail and
# say so.
ssh-test: agent-musl
    test -n "${VOLANT_SSH_TEST_KEY:-}" || { echo "VOLANT_SSH_TEST_KEY is not set"; exit 1; }
    "${VOLANT_PYTHON:-}" -c 'import ansible' 2>/dev/null || { echo "VOLANT_PYTHON must name a python with ansible-core"; exit 1; }
    VOLANT_AGENT_DIR="$PWD/target/agents" cargo nextest run --workspace --run-ignored ignored-only --no-tests=fail -E 'test(/^ssh_/)'
    # The module goldens (`reference_modules`) and the two collection resolution tests of
    # python.rs, here because this is the job that names the recorded ansible-core in
    # VOLANT_PYTHON, which turns each comparison's skip into a failure. The module play needs
    # `sudo -n`, systemd, apt, cron and the pinned collections this job installs. Without this
    # line the two python.rs tests run in no job at all: the `test` job's ansible-core-less
    # pythons skip them. The same holds for the CLI test of `retries` on an action plugin, which
    # needs ansible-core to run `copy`. The path check runs against `volant_echo`, a native only
    # an agent built with `test-natives` has, kept apart from the agent every other test runs.
    # Every scenario too: one that `needs` ansible-core skips in the `test` job and runs here.
    # And the tests that return early without VOLANT_PYTHON (`reference_python()` and `helper_or_skip`),
    # named one by one: selecting the whole `playbook_cli` binary would run its other tests twice.
    # A new test that needs ansible-core has to be added to this list, or it skips in every job.
    cargo build -p volant-agent --features test-natives
    mkdir -p target/test-natives
    cp target/debug/volant-agent target/test-natives/volant-agent
    VOLANT_TEST_NATIVES_AGENT_DIR="$PWD/target/test-natives" cargo nextest run --workspace --ignore-default-filter --no-tests=fail -E 'binary(reference_modules) | test(/^a_scenario_matches_the_reference::/) | test(=python::tests::a_collection_the_controller_cannot_run_is_answered_name_by_name) | test(=python::tests::the_controller_resolves_what_its_collections_hold) | test(=a_task_an_action_plugin_backs_retries_through_the_driver) | test(=every_runs_keyword_changes_something_observable) | test(=retries_are_rendered_per_item) | test(=a_lookup_reads_the_controller_after_the_tasks_in_front_of_it) | test(=a_task_behind_a_fact_writing_module_renders_the_new_facts) | test(=a_module_in_the_playbook_s_library_or_a_role_s_runs) | test(=a_module_written_in_the_playbook_s_library_rebuilds_a_kept_union) | test(=a_warning_repeated_by_two_items_prints_once) | test(=library_directories_join_the_search_in_the_reference_s_order) | test(=a_library_module_answers_its_short_name_and_never_ansible_builtin) | test(=a_task_an_action_plugin_backs_leaves_the_link_for_the_next_task) | test(=python::tests::dropping_the_builder_leaves_no_ansible_local_directory) | test(=python::tests::only_ansible_core_s_own_module_is_core) | test(=python::tests::the_helper_names_the_files_a_union_came_from)'
    just facts-compare

# Gather facts twice on one host, through the native setup and through the Python one, and
# compare them key by key. Without a host it is localhost over the ssh-test key; with one, the
# host is reached through your own ssh configuration (never in CI). The test's output is shown
# either way: it names the machine, and a native that hands back says why and compares nothing.
facts-compare host="": agent-musl
    "${VOLANT_PYTHON:-}" -c 'import ansible' 2>/dev/null || { echo "VOLANT_PYTHON must name a python with ansible-core"; exit 1; }
    VOLANT_FACTS_HOST='{{host}}' VOLANT_AGENT_DIR="$PWD/target/agents" cargo nextest run -p volant --run-ignored ignored-only --no-tests=fail --no-capture -E 'test(=native_facts_equal_python_facts_on_this_machine)'

# The commit the bench corpus is pinned to, so the thing being timed cannot change under the
# recipe. Refreshed by hand with `git ls-remote https://github.com/ansible-lockdown/UBUNTU22-CIS
# HEAD`; note the date beside it when it moves.
CORPUS_SHA := "fad97b54d843eaffc4b7790686cc88bbcbb2330e"   # ansible-lockdown/UBUNTU22-CIS, taken 2026-09-20

# Time the compilation of a real 900-task role next to ansible-playbook, which has to be on PATH.
# The role is cloned under target/ at a fixed commit, fetched again only when that commit is
# missing locally, so a developer offline with the corpus already checked out is not forced back
# online. Checked out with --force: the corpus lives under target/ and is disposable, so a stray
# edit left over from chasing a listing difference should not wedge the recipe on a git error
# instead of landing back on the pinned commit. Both sides only list the tasks; nothing runs on
# any host. The runs alternate and are compared every round, so a machine that warms up or
# throttles during the recipe, or a listing that only disagrees on an early round, cannot go
# unnoticed. This is a script, not a plain multi-line recipe, because a plain recipe hands each
# line to its own shell: the `cd target/corpus` would not reach the next line, and the alternating
# `for` loop cannot span lines without one. `set -e` needs to reach inside that loop too, so an
# engine that fails mid-round stops the recipe instead of leaving a stale file for the comparison.
# Times ansible-playbook and volant against the same pinned role, failing on a differing listing.
bench-compile:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v ansible-playbook > /dev/null || { echo "ansible-playbook is not on PATH"; exit 1; }
    /usr/bin/time -f '%e' true 2> /dev/null || { echo "GNU /usr/bin/time is not installed"; exit 1; }
    mkdir -p target/corpus/roles
    if [ ! -d target/corpus/roles/ubuntu22_cis ]; then
      git clone -q https://github.com/ansible-lockdown/UBUNTU22-CIS target/corpus/roles/ubuntu22_cis
    fi
    git -C target/corpus/roles/ubuntu22_cis cat-file -e {{CORPUS_SHA}}^{commit} 2>/dev/null \
      || git -C target/corpus/roles/ubuntu22_cis fetch -q origin {{CORPUS_SHA}}
    git -C target/corpus/roles/ubuntu22_cis checkout -q --force {{CORPUS_SHA}}
    printf -- '- hosts: localhost\n  gather_facts: false\n  roles: [ubuntu22_cis]\n' > target/corpus/site.yml
    printf -- 'localhost ansible_connection=local\n' > target/corpus/inv.ini
    cargo build --release -p volant
    cd target/corpus
    export ANSIBLE_ROLES_PATH=roles
    for i in 1 2 3; do
      /usr/bin/time -f 'ansible-playbook %e s  %M KB' ansible-playbook -i inv.ini --list-tasks site.yml > ansible.txt
      /usr/bin/time -f 'volant           %e s  %M KB' ../release/volant playbook -i inv.ini --list-tasks site.yml > volant.txt
      if ! diff volant.txt ansible.txt > /dev/null; then
        echo "listings differ: diff target/corpus/volant.txt target/corpus/ansible.txt"
        exit 1
      fi
    done
    echo "listings identical"

# Run the end-to-end playbook against the machine named by VOLANT_TARGET_HOST (never in CI)
e2e-target: agent-musl
    test -n "${VOLANT_TARGET_HOST:-}" || { echo "VOLANT_TARGET_HOST is not set"; exit 1; }
    cargo build -p volant
    printf '[targets]\n%s ansible_host=%s\n' "$VOLANT_TARGET_HOST" "$VOLANT_TARGET_HOST" > target/e2e-inventory.ini
    VOLANT_AGENT_DIR="$PWD/target/agents" ./target/debug/volant playbook -i target/e2e-inventory.ini crates/volant/tests/fixtures/ssh/e2e.yml

# Run the proof playbook against the machine named by VOLANT_TARGET_HOST (never in CI), under
# either engine, and print the wall clock of the run itself without the build in front of it.
#
# `ANSIBLE_PIPELINING` is ansible-core's own variable and is read only by the reference: volant
# keeps one agent alive per host for the whole run, so it has nothing to pipeline and no setting
# for it. The label the timing prints carries whatever the reference was given, so a number can
# never be recorded against the wrong configuration.
proof engine="volant": agent-musl
    #!/usr/bin/env bash
    set -euo pipefail
    test -n "${VOLANT_TARGET_HOST:-}" || { echo "VOLANT_TARGET_HOST is not set"; exit 1; }
    printf '[targets]\n%s ansible_host=%s\n' "$VOLANT_TARGET_HOST" "$VOLANT_TARGET_HOST" > target/proof-inventory.ini
    play=crates/volant/tests/fixtures/proof/site.yml
    case "{{engine}}" in
      volant)
        # Release, not debug: the number this prints is compared against a released ansible-core,
        # and timing an unoptimised build against it would not be a comparison. Measured on
        # 2026-09-21, the two builds run this play within half a second of each other - the run is
        # not controller-bound - so this is fairness rather than a speed-up.
        cargo build --release -p volant
        VOLANT_PYTHON="${VOLANT_PYTHON:-$(uv tool dir)/ansible-core/bin/python}" \
        VOLANT_AGENT_DIR="$PWD/target/agents" \
          /usr/bin/time -f 'proof volant %e s' \
          ./target/release/volant playbook -i target/proof-inventory.ini "$play"
        ;;
      reference)
        command -v ansible-playbook > /dev/null || { echo "ansible-playbook is not on PATH"; exit 1; }
        /usr/bin/time -f "proof reference (pipelining ${ANSIBLE_PIPELINING:-False}) %e s" \
          ansible-playbook -i target/proof-inventory.ini "$play"
        ;;
      *)
        echo "engine must be 'volant' or 'reference'"; exit 1
        ;;
    esac

# Undo everything `proof` writes, so the first run of a pair meets a host that was never
# baselined. The paths and the packages are the ones in the role's `defaults/main.yml`. Only ever
# pointed at a disposable test target.
proof-reset:
    test -n "${VOLANT_TARGET_HOST:-}" || { echo "VOLANT_TARGET_HOST is not set"; exit 1; }
    ssh "$VOLANT_TARGET_HOST" 'sudo rm -rf /opt/volant-proof && sudo apt-get -qq -y purge tree ncdu > /dev/null && sudo apt-get -qq -y autoremove > /dev/null && echo reset'

# Run the four pinned Galaxy roles (see tests/fixtures/proof-roles/requirements.yml) against the
# machine named by VOLANT_TARGET_HOST, and optionally VOLANT_SECOND_HOST for a second line in the
# inventory, under either engine (never in CI), and print the wall clock of the run itself
# without the build or the role install in front of it.
#
# `ANSIBLE_PIPELINING` is ansible-core's own variable and is read only by the reference, for the
# same reason as `proof`: volant keeps one agent alive per host for the whole run, so it has
# nothing to pipeline. The roles come from Ansible Galaxy, installed once into `target/proof-
# roles/` and never copied into this repository, so what runs is the published archive; a role
# already installed at the pinned version (read from its own `.galaxy_install_info`) is left
# alone rather than reinstalled on every call, and `--force` makes a stale or partial one actually
# get replaced rather than silently skipped, which `ansible-galaxy role install` does to any
# directory that already exists.
proof-roles engine="volant" *args:
    #!/usr/bin/env bash
    set -euo pipefail
    test -n "${VOLANT_TARGET_HOST:-}" || { echo "VOLANT_TARGET_HOST is not set"; exit 1; }
    {
      printf '[targets]\n'
      printf '%s ansible_host=%s ansible_python_interpreter=/usr/bin/python3\n' "$VOLANT_TARGET_HOST" "$VOLANT_TARGET_HOST"
      if [ -n "${VOLANT_SECOND_HOST:-}" ]; then
        printf '%s ansible_host=%s ansible_python_interpreter=/usr/bin/python3\n' "$VOLANT_SECOND_HOST" "$VOLANT_SECOND_HOST"
      fi
    } > target/proof-roles-inventory.ini
    requirements=crates/volant/tests/fixtures/proof-roles/requirements.yml
    play=crates/volant/tests/fixtures/proof-roles/site.yml
    roles_dir="$PWD/target/proof-roles"
    installed=true
    while IFS= read -r role; do
      version="$(grep -A1 "name: $role" "$requirements" | sed -n 's/.*version: //p')"
      info="$roles_dir/$role/meta/.galaxy_install_info"
      [ -f "$info" ] && grep -q "^version: $version\$" "$info" || { installed=false; break; }
    done < <(grep 'name:' "$requirements" | sed 's/.*name: *//')
    [ "$installed" = true ] || "$(uv tool dir)/ansible-core/bin/ansible-galaxy" role install --force -r "$requirements" -p "$roles_dir"
    export ANSIBLE_ROLES_PATH="$roles_dir"
    case "{{engine}}" in
      volant)
        just agent-musl
        cargo build --release -p volant
        VOLANT_PYTHON="${VOLANT_PYTHON:-$(uv tool dir)/ansible-core/bin/python}" \
        VOLANT_AGENT_DIR="$PWD/target/agents" \
          /usr/bin/time -f 'proof-roles volant %e s' \
          ./target/release/volant playbook -i target/proof-roles-inventory.ini "$play" {{args}}
        ;;
      reference)
        /usr/bin/time -f "proof-roles reference (pipelining ${ANSIBLE_PIPELINING:-False}) %e s" \
          "$(uv tool dir)/ansible-core/bin/ansible-playbook" -i target/proof-roles-inventory.ini "$play" {{args}}
        ;;
      *)
        echo "engine must be 'volant' or 'reference'"; exit 1
        ;;
    esac

# Undo everything `proof-roles` writes, as far as it is safe to. Never touches sshd, /etc/ssh,
# netbird, the account or sudo: this host is reachable only over ssh, and locking any of that out
# would strand it beyond recovery. `unattended-upgrades` is left installed too, since Ubuntu ships
# it by default and purging it is not part of undoing what the roles configured.
proof-roles-reset:
    #!/usr/bin/env bash
    set -euo pipefail
    test -n "${VOLANT_TARGET_HOST:-}" || { echo "VOLANT_TARGET_HOST is not set"; exit 1; }
    ssh "$VOLANT_TARGET_HOST" '
      set -euo pipefail
      sudo apt-get -qq -y purge nginx nginx-common fail2ban python3-pip > /dev/null
      sudo apt-get -qq -y autoremove > /dev/null
      sudo rm -rf /etc/nginx /etc/fail2ban/jail.local /etc/apt/apt.conf.d/10periodic /etc/apt/apt.conf.d/50unattended-upgrades
      echo reset
    '

# Time the four pinned Galaxy roles under both engines and print the ratio the public speed claim
# cites. Builds once, installs the roles the same way `proof-roles` does, then runs one uncounted
# warm-up pass per engine (filling Volant's payload cache and the host's own caches) followed by
# `runs` timed passes of Volant and `runs` timed passes of the reference
# (`ANSIBLE_PIPELINING=True`), each preceded by a wait for any leftover build or test on this
# machine so it never inflates a measured time. Every timed pass's PLAY RECAP is compared against
# the first one, engine included: a difference there means the two sides did not do the same work,
# and the number would be meaningless.
#
# `flock` on a lock file outside the tree (so `git worktree remove` never takes it with it) is
# held from the first warm-up pass to the last timed pass: several lanes can share this machine's
# target and second host, and one bench timing a pass while another is mid-run once cost half a
# day of measurements to a collision neither noticed until after the fact. The wait is unbounded
# and never kills the other side; one line prints while this call is waiting for it.
bench runs="3":
    #!/usr/bin/env bash
    set -euo pipefail
    test -n "${VOLANT_TARGET_HOST:-}" || { echo "VOLANT_TARGET_HOST is not set"; exit 1; }
    hosts=1
    [ -z "${VOLANT_SECOND_HOST:-}" ] || hosts=2

    just agent-musl
    cargo build --release -p volant

    requirements=crates/volant/tests/fixtures/proof-roles/requirements.yml
    play=crates/volant/tests/fixtures/proof-roles/site.yml
    roles_dir="$PWD/target/proof-roles"
    installed=true
    while IFS= read -r role; do
      version="$(grep -A1 "name: $role" "$requirements" | sed -n 's/.*version: //p')"
      info="$roles_dir/$role/meta/.galaxy_install_info"
      [ -f "$info" ] && grep -q "^version: $version\$" "$info" || { installed=false; break; }
    done < <(grep 'name:' "$requirements" | sed 's/.*name: *//')
    [ "$installed" = true ] || "$(uv tool dir)/ansible-core/bin/ansible-galaxy" role install --force -r "$requirements" -p "$roles_dir"
    export ANSIBLE_ROLES_PATH="$roles_dir"

    {
      printf '[targets]\n'
      printf '%s ansible_host=%s ansible_python_interpreter=/usr/bin/python3\n' "$VOLANT_TARGET_HOST" "$VOLANT_TARGET_HOST"
      if [ -n "${VOLANT_SECOND_HOST:-}" ]; then
        printf '%s ansible_host=%s ansible_python_interpreter=/usr/bin/python3\n' "$VOLANT_SECOND_HOST" "$VOLANT_SECOND_HOST"
      fi
    } > target/proof-roles-inventory.ini

    idle() {
      while pgrep -x cargo > /dev/null || pgrep -x rustc > /dev/null || pgrep -x cargo-nextest > /dev/null || pgrep -f bin/ansible-playbook > /dev/null; do
        sleep 5
      done
      awk '{print "load " $1}' /proc/loadavg
    }
    recap() {
      sed -n '/PLAY RECAP/,$p' "$1" | tail -n +2 | tr -s ' '
    }
    check_recap() {
      local expected="$1" ref="$2"; shift 2
      for log in "$ref" "$@"; do
        local n
        n="$(recap "$log" | grep -c .)"
        [ "$n" -eq "$expected" ] || {
          echo "PLAY RECAP for $log has $n host line(s), expected $expected:"
          recap "$log"
          exit 1
        }
      done
      for log in "$@"; do
        diff <(recap "$ref") <(recap "$log") > /dev/null || {
          echo "PLAY RECAP differs between passes:"
          echo "-- $ref --"; recap "$ref"
          echo "-- $log --"; recap "$log"
          exit 1
        }
      done
    }
    median() {
      printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{n=NR; if(n%2==1) m=a[(n+1)/2]; else m=(a[n/2]+a[n/2+1])/2; printf "%.3f", m}'
    }
    describe() {
      local d
      d="$(git describe --always --dirty 2>/dev/null)" && { printf '%s' "$d"; return 0; }
      if [ -f .volant-describe ]; then
        printf '%s' "$(cat .volant-describe)"
        return 0
      fi
      echo "no git metadata and no .volant-describe (sync with 'just remote' first): cannot name the measured tree" >&2
      return 1
    }

    ansible_playbook="$(uv tool dir)/ansible-core/bin/ansible-playbook"
    ansible_core_version="$("$ansible_playbook" --version | head -1)"
    volant_version="$(describe)" || exit 1
    stamp="$(date -u +%Y-%m-%dT%H%MZ)"
    mkdir -p target/bench

    lock="${XDG_RUNTIME_DIR:-/tmp}/volant-bench.lock"
    exec 200>"$lock"
    if ! flock -n 200; then
      echo "waiting for another bench to release the lock"
      flock 200
    fi

    idle
    VOLANT_PYTHON="${VOLANT_PYTHON:-$(uv tool dir)/ansible-core/bin/python}" \
    VOLANT_AGENT_DIR="$PWD/target/agents" \
      ./target/release/volant playbook -i target/proof-roles-inventory.ini "$play" > "target/bench/$stamp-roles-volant-warmup.log"
    idle
    ANSIBLE_PIPELINING=True "$ansible_playbook" -i target/proof-roles-inventory.ini "$play" > "target/bench/$stamp-roles-reference-warmup.log"

    volant_logs=()
    volant_times=()
    for i in $(seq 1 {{runs}}); do
      idle
      log="target/bench/$stamp-roles-volant-$i.log"
      start="$(date +%s.%N)"
      VOLANT_PYTHON="${VOLANT_PYTHON:-$(uv tool dir)/ansible-core/bin/python}" \
      VOLANT_AGENT_DIR="$PWD/target/agents" \
      VOLANT_PROFILE_JSON="target/bench/$stamp-roles-volant-$i.jsonl" \
        ./target/release/volant playbook -i target/proof-roles-inventory.ini "$play" > "$log"
      end="$(date +%s.%N)"
      volant_times+=("$(awk -v a="$start" -v b="$end" 'BEGIN{printf "%.3f", b-a}')")
      volant_logs+=("$log")
    done

    reference_logs=()
    reference_times=()
    for i in $(seq 1 {{runs}}); do
      idle
      log="target/bench/$stamp-roles-reference-$i.log"
      start="$(date +%s.%N)"
      ANSIBLE_PIPELINING=True "$ansible_playbook" -i target/proof-roles-inventory.ini "$play" > "$log"
      end="$(date +%s.%N)"
      reference_times+=("$(awk -v a="$start" -v b="$end" 'BEGIN{printf "%.3f", b-a}')")
      reference_logs+=("$log")
    done

    check_recap "$hosts" "${volant_logs[0]}" "${volant_logs[@]:1}" "${reference_logs[@]}"

    flock -u 200

    volant_median="$(median "${volant_times[@]}")"
    reference_median="$(median "${reference_times[@]}")"
    ratio="$(awk -v r="$reference_median" -v v="$volant_median" 'BEGIN{printf "%.2f", r/v}')"

    out="target/bench/$stamp-roles.txt"
    {
      printf 'bench roles hosts=%s date=%s volant=%s ansible-core=%s pipelining=True\n' "$hosts" "$stamp" "$volant_version" "$ansible_core_version"
      printf 'volant    runs %s  median %s s\n' "${volant_times[*]}" "$volant_median"
      printf 'reference runs %s  median %s s\n' "${reference_times[*]}" "$reference_median"
      printf 'ratio %s\n' "$ratio"
    } | tee "$out"

# The commit the k3s proof's playbooks are pinned to, so what runs never drifts out from under
# the recipe. The kubectl build the proof downloads and the k3s version the inventory pins are
# kept together here too, so a version bump touches one place.
K3S_ANSIBLE_SHA := "1a600b60d37e0f8a6e2e79b0e474147b5b108ae5"   # k3s-io/k3s-ansible
K3S_VERSION := "v1.31.12+k3s1"
KUBECTL_VERSION := "v1.31.12"

# The pinned k3s-ansible commit, cloned under target/ and never advanced or copied into this
# repository. Fetched again only when the pinned commit is missing locally, on the same pattern
# as `bench-compile`'s corpus: a developer offline with it already checked out is not forced back
# online, and `--force` lands back on the pinned commit past any stray edit.
_k3s-clone:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p target
    if [ ! -d target/k3s-ansible/.git ]; then
      git clone -q https://github.com/k3s-io/k3s-ansible target/k3s-ansible
    fi
    git -C target/k3s-ansible cat-file -e {{K3S_ANSIBLE_SHA}}^{commit} 2>/dev/null \
      || git -C target/k3s-ansible fetch -q origin {{K3S_ANSIBLE_SHA}}
    git -C target/k3s-ansible checkout -q --force {{K3S_ANSIBLE_SHA}}

# The three collections the pinned commit's roles call, pinned again in this repository's own
# fixture (Volant never installs a collection on its own) and installed once into
# target/collections. `ansible-galaxy collection install` decides "already installed" against
# whatever `ANSIBLE_COLLECTIONS_PATH` names, not against `-p` alone: measured without it, a
# collection already sitting in this account's default collections cache made the command skip
# `-p target/collections` entirely and leave it empty, so it has to be set here too, scoped to
# this one call, for the check and the install to agree on the same directory.
_k3s-collections:
    ANSIBLE_COLLECTIONS_PATH="$PWD/target/collections" "$(uv tool dir)/ansible-core/bin/ansible-galaxy" collection install -r crates/volant/tests/fixtures/proof-k3s/requirements.yml -p target/collections

# The official kubectl, pinned and checksummed against its own published `.sha256`, put under
# target/bin so the recipes that need it can put that directory first on PATH. The k3s_server
# role's `fetch` task only runs when kubectl is found, so without this the proof cannot reach
# measurement 2 of its own criteria (`kubectl get nodes`). A binary already at this version is
# left alone.
_k3s-kubectl:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p target/bin
    if ! target/bin/kubectl version --client 2>/dev/null | grep -q "{{KUBECTL_VERSION}}"; then
      curl -fsSL -o target/bin/kubectl "https://dl.k8s.io/release/{{KUBECTL_VERSION}}/bin/linux/amd64/kubectl"
      curl -fsSL -o target/bin/kubectl.sha256 "https://dl.k8s.io/release/{{KUBECTL_VERSION}}/bin/linux/amd64/kubectl.sha256"
      echo "$(cat target/bin/kubectl.sha256)  target/bin/kubectl" | sha256sum -c -
      chmod +x target/bin/kubectl
    fi

# The INI inventory k3s-ansible's own roles read, written on every call: Volant does not read the
# reference's YAML inventory, so this writes the equivalent groups by hand. `ansible_user` and
# `api_endpoint` are read from the server host itself. `token` is kept across calls when an
# inventory already exists: a new token on every pass changes the server's config and restarts
# k3s, which would stop a second pass from reproducing the first pass's recap; `proof-k3s-reset`
# removes the file once the hosts are actually clean, so a fresh series still gets a fresh token.
# The `sed` reading it back carries its own `|| true`: with no inventory file yet (the very first
# call, or right after a reset), `sed` on a missing path fails and `head` still exits 0, and under
# `pipefail` that failure alone would exit the recipe silently before it ever gets to generate one.
# None of the three is ever printed or committed. `umask 077` plus the `chmod` below keep the
# file `0600` from the moment it is created, because `token` is a cluster secret. `k3s_version`
# is pinned to the kubectl build above, so `kubectl get nodes` and the cluster it is pointed at
# always name the same version.
_k3s-inventory:
    #!/usr/bin/env bash
    set -euo pipefail
    umask 077
    test -n "${VOLANT_TARGET_HOST:-}" || { echo "VOLANT_TARGET_HOST is not set"; exit 1; }
    test -n "${VOLANT_SECOND_HOST:-}" || { echo "VOLANT_SECOND_HOST is not set"; exit 1; }
    ansible_user="$(ssh -o ConnectTimeout=10 -o BatchMode=yes "$VOLANT_TARGET_HOST" whoami)"
    api_endpoint="$(ssh -o ConnectTimeout=10 -o BatchMode=yes "$VOLANT_TARGET_HOST" ip -4 -o route get 1.1.1.1 | awk '{for (i=1;i<=NF;i++) if ($i=="src") print $(i+1)}')"
    token="$(sed -n 's/^token=//p' target/k3s-inventory.ini 2>/dev/null | head -1 || true)"
    [ -n "$token" ] || token="$(openssl rand -hex 16)"
    {
      printf '[server]\n%s ansible_host=%s ansible_python_interpreter=/usr/bin/python3\n' "$VOLANT_TARGET_HOST" "$VOLANT_TARGET_HOST"
      printf '[agent]\n%s ansible_host=%s ansible_python_interpreter=/usr/bin/python3\n' "$VOLANT_SECOND_HOST" "$VOLANT_SECOND_HOST"
      printf '[k3s_cluster:children]\nserver\nagent\n'
      printf '[k3s_cluster:vars]\n'
      printf 'ansible_user=%s\n' "$ansible_user"
      printf 'k3s_version=%s\n' "{{K3S_VERSION}}"
      printf 'token=%s\n' "$token"
      printf 'api_endpoint=%s\n' "$api_endpoint"
    } > target/k3s-inventory.ini
    chmod 600 target/k3s-inventory.ini

# Arm the dead-man switch on both hosts before a run touches k3s: thirty minutes from now, each
# host runs whichever of the reference's own uninstall scripts applies there. Checked first, not
# just reset: `systemd-run --unit=NAME` names both a `.timer` and the `.service` it triggers, and
# refuses to arm again over one still running, so an already-armed switch aborts here with a
# clear message instead of a raw systemd error two lines down. The check captures `is-active`'s
# own text instead of testing its exit code under `2>/dev/null`: a failed ssh (exit 255) has to
# abort the recipe here, same as any other ssh failure would, rather than read as "not armed" and
# arm blind over a host it never actually reached. If arming the second host fails, the first
# host's timer is stopped again before this recipe exits, so a partial arm never leaves one host
# ticking toward an uninstall with nothing left to disarm it.
_k3s-deadman-arm:
    #!/usr/bin/env bash
    set -euo pipefail
    for host in "$VOLANT_TARGET_HOST" "$VOLANT_SECOND_HOST"; do
      state="$(ssh -o ConnectTimeout=10 -o BatchMode=yes "$host" 'systemctl is-active volant-k3s-deadman.timer || true')"
      [ "$state" != active ] || { echo "dead-man switch already armed on $host" >&2; exit 1; }
    done
    armed=()
    for host in "$VOLANT_TARGET_HOST" "$VOLANT_SECOND_HOST"; do
      if ssh -o ConnectTimeout=10 -o BatchMode=yes "$host" '
        sudo systemctl reset-failed volant-k3s-deadman.timer volant-k3s-deadman.service > /dev/null 2>&1 || true
        sudo systemd-run --unit=volant-k3s-deadman --on-active=30min /bin/sh -c \
          "[ -x /usr/local/bin/k3s-uninstall.sh ] && /usr/local/bin/k3s-uninstall.sh; [ -x /usr/local/bin/k3s-agent-uninstall.sh ] && /usr/local/bin/k3s-agent-uninstall.sh; true"
      '; then
        armed+=("$host")
      else
        for done_host in "${armed[@]}"; do
          ssh -o ConnectTimeout=10 -o BatchMode=yes "$done_host" 'sudo systemctl stop volant-k3s-deadman.timer' 2>/dev/null || true
        done
        echo "failed to arm the dead-man switch on $host" >&2
        exit 1
      fi
    done

# Cancel the dead-man switch, but only once both hosts answer twice, two minutes apart: the play
# ending is not the same moment as the cluster settling, and disarming a second after `site.yml`
# returns would race kube-proxy and flannel still installing their own iptables rules. A host
# that fails either round is left armed, and the recipe fails instead of disarming blind.
# `systemd-run --unit=NAME` names the `.timer` doing the scheduling, not just the `.service` it
# triggers, so that is what has to stop; a trailing `|| true` here would hide a stop that failed
# and leave the timer ticking toward an uninstall.
_k3s-deadman-disarm:
    #!/usr/bin/env bash
    set -euo pipefail
    reachable() {
      local host="$1" attempt
      for attempt in 1 2 3; do
        ssh -o ConnectTimeout=10 -o BatchMode=yes "$host" true 2>/dev/null && return 0
        sleep 5
      done
      return 1
    }
    both_reachable() {
      reachable "$VOLANT_TARGET_HOST" && reachable "$VOLANT_SECOND_HOST"
    }
    settled=false
    if both_reachable; then
      sleep 120
      both_reachable && settled=true
    fi
    if [ "$settled" = true ]; then
      for host in "$VOLANT_TARGET_HOST" "$VOLANT_SECOND_HOST"; do
        ssh -o ConnectTimeout=10 -o BatchMode=yes "$host" '
          sudo systemctl stop volant-k3s-deadman.timer
          ! systemctl is-active --quiet volant-k3s-deadman.timer
        '
      done
    else
      echo "a host did not answer: leaving the dead-man switch armed" >&2
      exit 1
    fi

# Run the official k3s-ansible playbook (the pinned commit above) against the two hosts named by
# VOLANT_TARGET_HOST (the k3s server) and VOLANT_SECOND_HOST (the k3s agent), under either engine
# (never in CI), and print the wall clock of the run itself without the build, the clone, the
# collection install or the kubectl download in front of it.
#
# `-e kubeconfig=...` points the server role's kubeconfig away from its own default
# (`~/.kube/config.new`), so a run from this machine never merges the cluster into this
# machine's own `~/.kube/config`. `ANSIBLE_COLLECTIONS_PATH` and `ANSIBLE_ROLES_PATH` are
# exported for both engines, since Volant reads both exactly as the reference does and `site.yml`
# names its roles bare, resolved only through one of these or an `ansible.cfg` in the current
# directory. Neither engine `cd`s into target/k3s-ansible to pick that file's own `ansible.cfg`
# up on its own: it also sets `pipelining = True`, and running from there would enable it even
# for a pass this recipe means to label `pipelining=False`. `ANSIBLE_PIPELINING` itself is read
# only by the reference, for the same reason as `proof-roles`. `umask 077` covers the whole
# recipe, both engines: the `fetch`ed `kubeconfig` this run writes under target/ carries the
# cluster's admin credentials, and the process umask is what either engine's own file-writing
# tasks inherit. The dead-man switch is armed on both hosts after the build (so a build failure
# never arms it for nothing) and disarmed on every exit path (`trap ... EXIT`), not just a clean
# one.
proof-k3s engine="volant" *args:
    #!/usr/bin/env bash
    set -euo pipefail
    umask 077
    test -n "${VOLANT_TARGET_HOST:-}" || { echo "VOLANT_TARGET_HOST is not set"; exit 1; }
    test -n "${VOLANT_SECOND_HOST:-}" || { echo "VOLANT_SECOND_HOST is not set"; exit 1; }
    case "{{engine}}" in
      volant|reference) ;;
      *) echo "engine must be 'volant' or 'reference'"; exit 1 ;;
    esac
    just _k3s-clone
    just _k3s-collections
    just _k3s-kubectl
    just _k3s-inventory
    export ANSIBLE_COLLECTIONS_PATH="$PWD/target/collections"
    export ANSIBLE_ROLES_PATH="$PWD/target/k3s-ansible/roles"
    export PATH="$PWD/target/bin:$PATH"
    play=target/k3s-ansible/playbooks/site.yml
    kubeconfig="$PWD/target/k3s-kubeconfig"
    if [ "{{engine}}" = "volant" ]; then
      just agent-musl
      # `k3s_server` runs tasks on the controller (`delegate_to: 127.0.0.1`), and those need the
      # local agent under its plain name next to the cross-built one: a development build of
      # `volant` carries no agent inside it.
      cargo build --release -p volant -p volant-agent
      cp target/release/volant-agent target/agents/volant-agent
    fi
    just _k3s-deadman-arm
    trap 'just _k3s-deadman-disarm' EXIT
    case "{{engine}}" in
      volant)
        VOLANT_PYTHON="${VOLANT_PYTHON:-$(uv tool dir)/ansible-core/bin/python}" \
        VOLANT_AGENT_DIR="$PWD/target/agents" \
          /usr/bin/time -f 'proof-k3s volant %e s' \
          ./target/release/volant playbook -i target/k3s-inventory.ini "$play" -e "kubeconfig=$kubeconfig" {{args}}
        ;;
      reference)
        /usr/bin/time -f "proof-k3s reference (pipelining ${ANSIBLE_PIPELINING:-False}) %e s" \
          "$(uv tool dir)/ansible-core/bin/ansible-playbook" -i target/k3s-inventory.ini "$play" -e "kubeconfig=$kubeconfig" {{args}}
        ;;
    esac

# Undo what `proof-k3s` leaves on both hosts: the reference's own `playbooks/reset.yml`, then a
# check that k3s actually left no trace. The recap `ansible-playbook` prints is not proof of a
# clean host by itself; the checks below are. The inventory is removed once both hosts check out
# clean, so the next `proof-k3s` call starts a fresh series with a fresh token (see
# `_k3s-inventory`).
proof-k3s-reset:
    #!/usr/bin/env bash
    set -euo pipefail
    test -n "${VOLANT_TARGET_HOST:-}" || { echo "VOLANT_TARGET_HOST is not set"; exit 1; }
    test -n "${VOLANT_SECOND_HOST:-}" || { echo "VOLANT_SECOND_HOST is not set"; exit 1; }
    just _k3s-clone
    just _k3s-inventory
    "$(uv tool dir)/ansible-core/bin/ansible-playbook" -i target/k3s-inventory.ini target/k3s-ansible/playbooks/reset.yml
    for host in "$VOLANT_TARGET_HOST" "$VOLANT_SECOND_HOST"; do
      ssh -o ConnectTimeout=10 -o BatchMode=yes "$host" '
        set -euo pipefail
        test ! -e /usr/local/bin/k3s || { echo "k3s still present"; exit 1; }
        ! systemctl is-active --quiet volant-k3s-deadman.timer || { echo "dead-man switch still armed"; exit 1; }
        rules="$(sudo iptables -S)"
        if grep -Eiq "kube|flannel|cni" <<<"$rules"; then
          echo "leftover k3s iptables rules"
          exit 1
        fi
      '
    done
    rm -f target/k3s-inventory.ini
    echo "k3s-reset ok"

# Time the official k3s-ansible playbook under both engines and print the ratio the public speed
# claim for k3s cites. Sets the cluster up the same way `proof-k3s` does, then a `fresh` pass of
# the reference without pipelining (the only pass that installs k3s, so its recap is its own and
# never compared), one uncounted warm-up pass per engine, and `runs` timed passes of each engine
# against the now-established cluster, exactly as `bench` does for the roles: idle wait, the
# pass's own PLAY RECAP checked against the first timed pass, and `VOLANT_PROFILE_JSON` on every
# timed Volant pass.
#
# `umask 077` covers the recipe because the fetched kubeconfig carries the cluster's admin
# credentials, same reasoning as `proof-k3s`. The dead-man switch is armed once the build is done
# and disarmed on every exit path, and `proof-k3s-reset` always runs after it, successful run or
# not, so a failed bench never leaves k3s on either host. The bench lock (see `bench`) is held from
# the `fresh` pass - the first pass that actually touches the two hosts - through that reset, since
# `bench-k3s` and `bench` share the same two hosts and a `fresh` k3s install racing another lane's
# timed roles pass would be exactly the collision the lock exists to prevent.
bench-k3s runs="3":
    #!/usr/bin/env bash
    set -euo pipefail
    umask 077
    test -n "${VOLANT_TARGET_HOST:-}" || { echo "VOLANT_TARGET_HOST is not set"; exit 1; }
    test -n "${VOLANT_SECOND_HOST:-}" || { echo "VOLANT_SECOND_HOST is not set"; exit 1; }

    just _k3s-clone
    just _k3s-collections
    just _k3s-kubectl
    just _k3s-inventory
    export ANSIBLE_COLLECTIONS_PATH="$PWD/target/collections"
    export ANSIBLE_ROLES_PATH="$PWD/target/k3s-ansible/roles"
    export PATH="$PWD/target/bin:$PATH"
    play=target/k3s-ansible/playbooks/site.yml
    kubeconfig="$PWD/target/k3s-kubeconfig"

    just agent-musl
    # The k3s play delegates a task to the control node itself (127.0.0.1): that connection needs
    # an agent built for this machine's own architecture, not the musl one `agent-musl` builds for
    # the two remote hosts. Without it the delegated task fails "no executable agent binary
    # 'volant-agent'" - measured on a worktree whose target/ had never built this binary before
    # (proof-k3s can look fine without this step only because some other build in the same tree
    # happened to leave target/release/volant-agent behind first).
    cargo build -p volant-agent --release
    cp target/release/volant-agent target/agents/volant-agent
    cargo build --release -p volant

    idle() {
      while pgrep -x cargo > /dev/null || pgrep -x rustc > /dev/null || pgrep -x cargo-nextest > /dev/null || pgrep -f bin/ansible-playbook > /dev/null; do
        sleep 5
      done
      awk '{print "load " $1}' /proc/loadavg
    }
    recap() {
      sed -n '/PLAY RECAP/,$p' "$1" | tail -n +2 | tr -s ' '
    }
    check_recap() {
      local expected="$1" ref="$2"; shift 2
      for log in "$ref" "$@"; do
        local n
        n="$(recap "$log" | grep -c .)"
        [ "$n" -eq "$expected" ] || {
          echo "PLAY RECAP for $log has $n host line(s), expected $expected:"
          recap "$log"
          exit 1
        }
      done
      for log in "$@"; do
        diff <(recap "$ref") <(recap "$log") > /dev/null || {
          echo "PLAY RECAP differs between passes:"
          echo "-- $ref --"; recap "$ref"
          echo "-- $log --"; recap "$log"
          exit 1
        }
      done
    }
    median() {
      printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{n=NR; if(n%2==1) m=a[(n+1)/2]; else m=(a[n/2]+a[n/2+1])/2; printf "%.3f", m}'
    }
    describe() {
      local d
      d="$(git describe --always --dirty 2>/dev/null)" && { printf '%s' "$d"; return 0; }
      if [ -f .volant-describe ]; then
        printf '%s' "$(cat .volant-describe)"
        return 0
      fi
      echo "no git metadata and no .volant-describe (sync with 'just remote' first): cannot name the measured tree" >&2
      return 1
    }

    ansible_playbook="$(uv tool dir)/ansible-core/bin/ansible-playbook"
    ansible_core_version="$("$ansible_playbook" --version | head -1)"
    volant_version="$(describe)" || exit 1
    stamp="$(date -u +%Y-%m-%dT%H%MZ)"
    mkdir -p target/bench

    # The trap is registered before anything is armed, not after: `_k3s-deadman-arm` leaves both
    # hosts armed on success, and the very next step can block for an unbounded time (a sibling
    # lane's own bench holding the lock). Arming, then waiting, then registering the trap left a
    # window where a Ctrl-C during that wait killed the shell with the timer armed and no disarm or
    # reset ever run - the exact gap `proof-k3s` avoids by never putting anything blocking between
    # its own arm and its own trap. `armed` and `locked` gate what the trap actually does: it fires
    # harmlessly before either is true (nothing was armed or locked yet to undo), and does the
    # equivalent of `proof-k3s`'s own trap plus the reset once both are set.
    armed=false
    locked=false
    trap '
      if [ "$armed" = true ]; then just _k3s-deadman-disarm; just proof-k3s-reset; fi
      if [ "$locked" = true ]; then flock -u 200; fi
    ' EXIT

    just _k3s-deadman-arm
    armed=true
    lock="${XDG_RUNTIME_DIR:-/tmp}/volant-bench.lock"
    exec 200>"$lock"
    if ! flock -n 200; then
      echo "waiting for another bench to release the lock"
      flock 200
    fi
    locked=true

    "$ansible_playbook" -i target/k3s-inventory.ini "$play" -e "kubeconfig=$kubeconfig" > "target/bench/$stamp-k3s-fresh.log"

    idle
    VOLANT_PYTHON="${VOLANT_PYTHON:-$(uv tool dir)/ansible-core/bin/python}" \
    VOLANT_AGENT_DIR="$PWD/target/agents" \
      ./target/release/volant playbook -i target/k3s-inventory.ini "$play" -e "kubeconfig=$kubeconfig" > "target/bench/$stamp-k3s-volant-warmup.log"
    idle
    ANSIBLE_PIPELINING=True "$ansible_playbook" -i target/k3s-inventory.ini "$play" -e "kubeconfig=$kubeconfig" > "target/bench/$stamp-k3s-reference-warmup.log"

    volant_logs=()
    volant_times=()
    for i in $(seq 1 {{runs}}); do
      idle
      log="target/bench/$stamp-k3s-volant-$i.log"
      start="$(date +%s.%N)"
      VOLANT_PYTHON="${VOLANT_PYTHON:-$(uv tool dir)/ansible-core/bin/python}" \
      VOLANT_AGENT_DIR="$PWD/target/agents" \
      VOLANT_PROFILE_JSON="target/bench/$stamp-k3s-volant-$i.jsonl" \
        ./target/release/volant playbook -i target/k3s-inventory.ini "$play" -e "kubeconfig=$kubeconfig" > "$log"
      end="$(date +%s.%N)"
      volant_times+=("$(awk -v a="$start" -v b="$end" 'BEGIN{printf "%.3f", b-a}')")
      volant_logs+=("$log")
    done

    reference_logs=()
    reference_times=()
    for i in $(seq 1 {{runs}}); do
      idle
      log="target/bench/$stamp-k3s-reference-$i.log"
      start="$(date +%s.%N)"
      ANSIBLE_PIPELINING=True "$ansible_playbook" -i target/k3s-inventory.ini "$play" -e "kubeconfig=$kubeconfig" > "$log"
      end="$(date +%s.%N)"
      reference_times+=("$(awk -v a="$start" -v b="$end" 'BEGIN{printf "%.3f", b-a}')")
      reference_logs+=("$log")
    done

    check_recap 2 "${volant_logs[0]}" "${volant_logs[@]:1}" "${reference_logs[@]}"

    volant_median="$(median "${volant_times[@]}")"
    reference_median="$(median "${reference_times[@]}")"
    ratio="$(awk -v r="$reference_median" -v v="$volant_median" 'BEGIN{printf "%.2f", r/v}')"

    out="target/bench/$stamp-k3s.txt"
    {
      printf 'bench k3s hosts=2 date=%s volant=%s ansible-core=%s pipelining=True\n' "$stamp" "$volant_version" "$ansible_core_version"
      printf 'volant    runs %s  median %s s\n' "${volant_times[*]}" "$volant_median"
      printf 'reference runs %s  median %s s\n' "${reference_times[*]}" "$reference_median"
      printf 'ratio %s\n' "$ratio"
    } | tee "$out"
