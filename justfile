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
    cargo binstall -y cargo-nextest cargo-deny cargo-machete cargo-about cargo-dist release-plz typos-cli taplo-cli zizmor mdbook cargo-insta cargo-llvm-cov cargo-mutants

# Accept pending insta snapshots after reading them
insta-accept:
    cargo insta accept --workspace

# Regenerate the golden expectations with the reference ansible-core (see tests/golden/ANSIBLE_VERSION)
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
# this suite's tests need. `--jobs 1`, not 4: measured on the dev host, `--copy-target` copies a
# ~2 GB target/ per parallel job, and /tmp there is a 7.6 GB tmpfs shared with everything else
# running - both 4 and 2 parallel copies exceeded it mid-campaign ("Disk quota exceeded"); one
# job at a time is the value that ran two full campaigns without it. Slow on purpose: one build
# and one test run per mutant. Not in CI.
# Mutation score for the given file, one build and one test run per mutant. Not in CI.
mutants file="crates/volant/src/keywords.rs":
    cargo build --workspace
    cargo mutants --file {{file}} --timeout 120 --jobs 1 --no-shuffle --test-tool nextest --copy-target true

# Regenerate docs/src/modules.md from the module registry
docs-modules:
    VOLANT_UPDATE_DOCS=1 cargo test -p volant-protocol the_documentation_table_matches_the_registry

# Regenerate docs/src/keywords.md from the keyword tables
docs-keywords:
    VOLANT_UPDATE_DOCS=1 cargo test -p volant the_keyword_page_matches_the_tables

# Run the CI workflow locally (needs Docker)
ci-local:
    gh act pull_request -W .github/workflows/ci.yml

# Copy the working tree to the machine named by VOLANT_DEV_HOST and run a recipe there. The file
# list comes from git, so tracked and new files travel while everything git ignores stays behind.
# The copy adds and overwrites but never deletes, so a file you removed here still exists over
# there and can still be compiled; remove it by hand when that matters.
remote +recipe:
    set -o pipefail; git ls-files -z --cached --others --exclude-standard | tar -C . --null --files-from=- -czf - | ssh "$VOLANT_DEV_HOST" 'mkdir -p volant && tar -xzf - -C volant'
    ssh "$VOLANT_DEV_HOST" '. ~/.profile && cd volant && just {{recipe}}'

# Build the agent as a static musl binary, the only form that can be uploaded to another host
agent-musl:
    cargo build -p volant-agent --release --target x86_64-unknown-linux-musl
    mkdir -p target/agents
    cp target/x86_64-unknown-linux-musl/release/volant-agent target/agents/volant-agent-x86_64-unknown-linux-musl

# Unpack a published archive into an empty directory and run one task from it. `just` runs
# recipes under `bash -uc`, without pipefail (see line 1), so this one is a script with the
# options it needs. `env -u VOLANT_AGENT_DIR` and the empty directory are the test: the
# controller has to find its agent in what was downloaded, beside itself.
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

# Tests that need an sshd on localhost and a key in VOLANT_SSH_TEST_KEY (see CONTRIBUTING)
ssh-test: agent-musl
    test -n "${VOLANT_SSH_TEST_KEY:-}" || { echo "VOLANT_SSH_TEST_KEY is not set"; exit 1; }
    VOLANT_AGENT_DIR="$PWD/target/agents" cargo nextest run --workspace --run-ignored ignored-only --no-tests=fail -E 'test(/^ssh_/)'

# The commit the bench corpus is pinned to, so the thing being timed cannot change under the
# recipe. Refreshed by hand with `git ls-remote https://github.com/ansible-lockdown/UBUNTU22-CIS
# HEAD`; note the date beside it when it moves.
CORPUS_SHA := "fad97b54d843eaffc4b7790686cc88bbcbb2330e"   # ansible-lockdown/UBUNTU22-CIS, taken 2026-09-20

# Time the compilation of a real 900-task role next to ansible-playbook, which has to be on PATH.
# The role is cloned under target/ at a fixed commit. Both sides only list the tasks; nothing
# runs on any host. The runs alternate, so a machine that warms up or throttles during the
# recipe cannot favour whichever engine went first. `just` runs a recipe under `bash -uc`,
# without pipefail (see line 1), and a plain multi-line recipe does not stop at the first
# failing line, so this is a script with the options it needs.
bench-compile:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v ansible-playbook > /dev/null || { echo "ansible-playbook is not on PATH"; exit 1; }
    /usr/bin/time -f '%e' true 2> /dev/null || { echo "GNU /usr/bin/time is not installed"; exit 1; }
    mkdir -p target/corpus/roles
    if [ ! -d target/corpus/roles/ubuntu22_cis ]; then
      git clone -q https://github.com/ansible-lockdown/UBUNTU22-CIS target/corpus/roles/ubuntu22_cis
    fi
    git -C target/corpus/roles/ubuntu22_cis fetch -q origin {{CORPUS_SHA}}
    git -C target/corpus/roles/ubuntu22_cis checkout -q {{CORPUS_SHA}}
    printf -- '- hosts: localhost\n  gather_facts: false\n  roles: [ubuntu22_cis]\n' > target/corpus/site.yml
    printf -- 'localhost ansible_connection=local\n' > target/corpus/inv.ini
    cargo build --release -p volant
    cd target/corpus
    export ANSIBLE_ROLES_PATH=roles
    for i in 1 2 3; do
      /usr/bin/time -f 'ansible-playbook %e s  %M KB' ansible-playbook -i inv.ini --list-tasks site.yml > ansible.txt
      /usr/bin/time -f 'volant           %e s  %M KB' ../release/volant playbook -i inv.ini --list-tasks site.yml > volant.txt
    done
    if ! diff volant.txt ansible.txt > /dev/null; then
      echo "listings differ: diff target/corpus/volant.txt target/corpus/ansible.txt"
      exit 1
    fi
    echo "listings identical"

# Run the end-to-end playbook against the machine named by VOLANT_TARGET_HOST (never in CI)
e2e-target: agent-musl
    test -n "${VOLANT_TARGET_HOST:-}" || { echo "VOLANT_TARGET_HOST is not set"; exit 1; }
    cargo build -p volant
    printf '[targets]\n%s ansible_host=%s\n' "$VOLANT_TARGET_HOST" "$VOLANT_TARGET_HOST" > target/e2e-inventory.ini
    VOLANT_AGENT_DIR="$PWD/target/agents" ./target/debug/volant playbook -i target/e2e-inventory.ini crates/volant/tests/fixtures/ssh/e2e.yml
