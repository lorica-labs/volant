# Shared by the scripts of this directory, which source it.
# Each script runs in a fresh copy of fixtures/, writes everything it prints to one file and
# compares that file with expected/<script>.out. RECORD=1 writes the expected file instead.
here=$(cd "$(dirname "$0")" && pwd)
name=$(basename "$0" .sh)
work=/tmp/command-names/$name
outdir=${OUT_DIR:-/tmp/command-names/out}
out=$outdir/$name.out
rm -rf "$work"
mkdir -p "$work" "$outdir"
cp -R "$here/fixtures/." "$work/"
cd "$work" || exit 1

# A command shown as typed, then its output and exit code.
run() {
  printf '$ %s\n' "$*"
  "$@" 2>&1
  printf 'rc=%s\n' "$?"
}

check() {
  if [ -n "${RECORD:-}" ]; then
    mkdir -p "$here/expected"
    cp "$out" "$here/expected/$name.out"
  else
    diff -u "$here/expected/$name.out" "$out"
  fi
}
