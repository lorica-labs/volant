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
    test "$(ansible-playbook --version | head -1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')" = "$(cat crates/volant/tests/golden/ANSIBLE_VERSION)"
    @echo "golden generator arrives with the templating task"

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

# Copy the working tree to the machine named by VOLANT_DEV_HOST and run a recipe there
remote +recipe:
    tar -C . --exclude=./target --exclude=./.git --exclude=./docs/superpowers --exclude=./docs/book -czf - . | ssh "$VOLANT_DEV_HOST" 'mkdir -p volant && tar -xzf - -C volant'
    ssh "$VOLANT_DEV_HOST" '. ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && cd volant && just {{recipe}}'
