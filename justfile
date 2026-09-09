set shell := ["bash", "-uc"]
set windows-shell := ["bash", "-uc"]

default:
    @just --list

# Install git hooks, sign-off, and the tools the other recipes need
setup:
    git config core.hooksPath .githooks
    cargo binstall -y cargo-nextest cargo-deny cargo-machete cargo-about cargo-dist release-plz typos-cli taplo-cli zizmor mdbook cargo-insta

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
    cargo nextest run --workspace

# Regenerate docs/src/modules.md from the module registry
docs-modules:
    VOLANT_UPDATE_DOCS=1 cargo test -p volant-protocol the_documentation_table_matches_the_registry

# Run the CI workflow locally (needs Docker)
ci-local:
    gh act pull_request -W .github/workflows/ci.yml

# Copy the working tree to the machine named by VOLANT_DEV_HOST and run a recipe there. The file
# list comes from git, so tracked and new files travel while everything git ignores stays behind.
remote +recipe:
    git ls-files -z --cached --others --exclude-standard | tar -C . --null --files-from=- -czf - | ssh "$VOLANT_DEV_HOST" 'mkdir -p volant && tar -xzf - -C volant'
    ssh "$VOLANT_DEV_HOST" '. ~/.profile && cd volant && just {{recipe}}'

# Build the agent as a static musl binary, the only form that can be uploaded to another host
agent-musl:
    cargo build -p volant-agent --release --target x86_64-unknown-linux-musl
    mkdir -p target/agents
    cp target/x86_64-unknown-linux-musl/release/volant-agent target/agents/volant-agent-x86_64-unknown-linux-musl

# Tests that need an sshd on localhost and a key in VOLANT_SSH_TEST_KEY (see CONTRIBUTING)
ssh-test: agent-musl
    test -n "${VOLANT_SSH_TEST_KEY:-}" || { echo "VOLANT_SSH_TEST_KEY is not set"; exit 1; }
    VOLANT_AGENT_DIR="$PWD/target/agents" cargo nextest run --workspace --run-ignored ignored-only --no-tests=fail -E 'test(/^ssh_/)'

# Time the compilation of a real 900-task role next to ansible-playbook, which has to be on PATH.
# The role is cloned under target/. Both sides only list the tasks; nothing runs on any host.
bench-compile:
    mkdir -p target/corpus/roles
    test -d target/corpus/roles/ubuntu22_cis || git clone -q --depth 1 https://github.com/ansible-lockdown/UBUNTU22-CIS target/corpus/roles/ubuntu22_cis
    printf -- '- hosts: localhost\n  gather_facts: false\n  roles: [ubuntu22_cis]\n' > target/corpus/site.yml
    printf -- 'localhost ansible_connection=local\n' > target/corpus/inv.ini
    cd target/corpus && for i in 1 2 3; do /usr/bin/time -f 'ansible-playbook %e s  %M KB' env ANSIBLE_ROLES_PATH=roles ansible-playbook -i inv.ini --list-tasks site.yml > ansible.txt; done
    cargo build --release -p volant
    cd target/corpus && for i in 1 2 3; do /usr/bin/time -f 'volant           %e s  %M KB' env ANSIBLE_ROLES_PATH=roles ../release/volant playbook -i inv.ini --list-tasks site.yml > volant.txt; done
    cd target/corpus && diff volant.txt ansible.txt > /dev/null && echo "listings identical" || echo "listings differ: diff target/corpus/volant.txt target/corpus/ansible.txt"

# Run the end-to-end playbook against the machine named by VOLANT_TARGET_HOST (never in CI)
e2e-target: agent-musl
    test -n "${VOLANT_TARGET_HOST:-}" || { echo "VOLANT_TARGET_HOST is not set"; exit 1; }
    cargo build -p volant
    printf '[targets]\n%s ansible_host=%s\n' "$VOLANT_TARGET_HOST" "$VOLANT_TARGET_HOST" > target/e2e-inventory.ini
    VOLANT_AGENT_DIR="$PWD/target/agents" ./target/debug/volant playbook -i target/e2e-inventory.ini crates/volant/tests/fixtures/ssh/e2e.yml
