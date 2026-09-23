set shell := ["bash", "-uc"]
set windows-shell := ["bash", "-uc"]

# The measured line coverage minus one point: a threshold that cannot drift down in silence, and
# that one refactor removing a covered branch does not break.
COVERAGE_FLOOR := "91"

default:
    @just --list

# Install git hooks, sign-off, and the tools the other recipes need
setup:
    git config core.hooksPath .githooks
    cargo binstall -y cargo-nextest cargo-deny cargo-machete cargo-about cargo-dist release-plz typos-cli taplo-cli zizmor cargo-insta cargo-llvm-cov cargo-mutants

# Accept pending insta snapshots after reading them
insta-accept:
    cargo insta accept --workspace

# Regenerate the golden expectations with the reference ansible-core (see tests/golden/ANSIBLE_VERSION).
# Reproducible: running this on a correct tree leaves the three recordings byte for byte as they
# were, so an empty diff afterwards is a check and not a coincidence. That holds because the
# generator replaces the temporary directory it writes the play into with a fixed name; without
# that, two of the reference's messages quote the path they came from and every run differed.
golden:
    "$(uv tool dir)/ansible-core/bin/python" crates/volant/tests/golden/generate.py

# Everything CI runs, in the same order
check: fmt lint test

fmt:
    cargo fmt --all --check
    taplo fmt --check

lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items
    cargo deny check
    cargo machete
    typos
    actionlint .github/workflows/*.yml
    zizmor .github/workflows

test:
    cargo nextest run --workspace --no-fail-fast

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
    set -o pipefail; cargo llvm-cov nextest --workspace --summary-only --fail-under-lines {{COVERAGE_FLOOR}} | tee /tmp/volant-coverage.txt
    @awk '/^TOTAL/ {print "regions " $4 "   functions " $7 "   LINES " $10 "  <- the threshold is on this one"}' /tmp/volant-coverage.txt

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
# The copy adds and overwrites but never deletes, so a file you removed here still exists over
# there and can still be compiled; remove it by hand when that matters.
#
# VOLANT_REMOTE_DIR names the directory over there, defaulting to `volant`. Set it to work on
# several branches at once without them sharing a target directory or overwriting each other's
# sources: one worktree here, one directory there, one name.
remote +recipe:
    set -o pipefail; dir="${VOLANT_REMOTE_DIR:-volant}"; git ls-files -z --cached --others --exclude-standard | tar -C . --null --files-from=- -czf - | ssh "$VOLANT_DEV_HOST" "mkdir -p '$dir' && tar -xzf - -C '$dir'"
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
    VOLANT_EMBED_AGENTS_DIR="$agents" cargo build -p volant --release
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    cp target/release/volant "$work/"
    cd "$work"
    printf 'h1 ansible_connection=local\n' > inv.ini
    printf -- '- hosts: h1\n  gather_facts: false\n  tasks:\n    - command: /bin/true\n' > site.yml
    env -u VOLANT_AGENT_DIR XDG_CACHE_HOME="$work/cache" ./volant playbook -i inv.ini site.yml
    cmp cache/volant/agents/*/volant-agent "$agents/volant-agent"

# Tests that need an sshd on localhost and a key in VOLANT_SSH_TEST_KEY (see CONTRIBUTING)
ssh-test: agent-musl
    test -n "${VOLANT_SSH_TEST_KEY:-}" || { echo "VOLANT_SSH_TEST_KEY is not set"; exit 1; }
    "${VOLANT_PYTHON:-}" -c 'import ansible' 2>/dev/null || { echo "VOLANT_PYTHON must name a python with ansible-core"; exit 1; }
    VOLANT_AGENT_DIR="$PWD/target/agents" cargo nextest run --workspace --run-ignored ignored-only --no-tests=fail -E 'test(/^ssh_/)'
    # The python module, action plugin and collection module goldens, and the two collection
    # resolution tests of python.rs, here because this is the job that names the recorded
    # ansible-core in VOLANT_PYTHON, which turns each comparison's skip into a failure. The action
    # plugin one also needs `sudo -n` and systemd, and fails here without them; the collection ones
    # need the pinned collections this job installs. Without this line the two python.rs tests run
    # in no job at all: the `test` job's ansible-core-less pythons skip them.
    cargo nextest run --workspace --no-tests=fail -E 'test(=a_python_module_returns_the_reference_s_own_keys) | test(=an_action_plugin_returns_the_reference_s_own_keys) | test(=a_collection_module_returns_the_reference_s_own_keys) | test(=python::tests::a_collection_the_controller_cannot_run_is_answered_name_by_name) | test(=python::tests::the_controller_resolves_what_its_collections_hold)'

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
