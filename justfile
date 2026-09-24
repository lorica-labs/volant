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

# Before opening a pull request: formatting, clippy, the tests and typos. The rest of `lint`
# (the doc build, cargo deny, cargo machete, actionlint, zizmor) runs in CI on every pull request.
check-local: fmt clippy test
    typos

fmt:
    cargo fmt --all --check
    taplo fmt --check

clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

lint: clippy
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
#
# `git ls-files` never carries `.git` itself across, so a recipe run over there through this one
# (`bench`, `bench-k3s`) has no `git describe` of its own to name the tree it measured. `.volant-
# describe` is written here, on the side that still has `.git`, from this same commit the tar is
# about to carry over, and travels as one more file in that same tar stream (added to the file list
# by hand, since it is gitignored and `--exclude-standard` would otherwise leave it behind).
remote +recipe:
    git describe --always --dirty > .volant-describe
    set -o pipefail; dir="${VOLANT_REMOTE_DIR:-volant}"; { git ls-files -z --cached --others --exclude-standard; printf '.volant-describe\0'; } | tar -C . --null --files-from=- -czf - | ssh "$VOLANT_DEV_HOST" "mkdir -p '$dir' && tar -xzf - -C '$dir'"
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
    # in no job at all: the `test` job's ansible-core-less pythons skip them. The same holds for
    # the CLI test of `retries` on an action plugin, which needs ansible-core to run `copy`.
    cargo nextest run --workspace --no-tests=fail -E 'test(=a_python_module_returns_the_reference_s_own_keys) | test(=an_action_plugin_returns_the_reference_s_own_keys) | test(=a_collection_module_returns_the_reference_s_own_keys) | test(=python::tests::a_collection_the_controller_cannot_run_is_answered_name_by_name) | test(=python::tests::the_controller_resolves_what_its_collections_hold) | test(=a_task_an_action_plugin_backs_retries_through_the_driver)'

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
