set shell := ["bash", "-uc"]
set windows-shell := ["bash", "-uc"]

default:
    @just --list

# Install git hooks, sign-off, and the tools the other recipes need
setup:
    git config core.hooksPath .githooks
    cargo binstall -y cargo-nextest cargo-deny cargo-machete cargo-about cargo-dist release-plz typos-cli taplo-cli zizmor mdbook

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
    actionlint
    zizmor .github/workflows

test:
    cargo nextest run --workspace

# Run the CI workflow locally (needs Docker)
ci-local:
    gh act pull_request -W .github/workflows/ci.yml
