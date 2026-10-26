#!/bin/sh
. "$(dirname "$0")/lib.sh"
host=$(hostname)
# A command shown as typed, then what it wrote on each output and its exit code. The two are kept
# apart: ansible-pull writes its own lines when it exits, its children's as they come.
apart() {
  printf '$ %s\n' "$*"
  "$@" > stdout 2> stderr
  printf 'rc=%s\n' "$?"
  cat stdout
  printf -- '--- stderr\n'
  cat stderr
}
# The machine's name, the commit ids and the work directory are not the recording's. The first
# two lines, the start time and the command line, are left out: written when ansible-pull exits.
normal() {
  sed -e "s|$work|<work>|g" -e "s|$host|<host>|g" -e 's/[0-9a-f]\{40\}/<commit>/g' \
    -e '/^Starting Ansible Pull at /d' -e '/ansible-pull -U/{/^\$ /!d}'
}
commit() {
  git -C repo add . && git -C repo -c user.email=a@example.org -c user.name=n commit -qm "$1"
}
git init -q repo
printf -- '- hosts: localhost\n  gather_facts: false\n  tasks:\n    - debug: {msg: local}\n' > repo/local.yml
printf -- '- hosts: localhost\n  gather_facts: false\n  tasks:\n    - debug: {msg: by name v1}\n' > "repo/$host.yml"
commit one
{
  apart ansible-pull -U "$work/repo" -d "$work/dest"
  apart ansible-pull -U "$work/repo" -d "$work/dest" -o
  sed -i 's/v1/v2/' "repo/$host.yml" && commit two
  apart ansible-pull -U "$work/repo" -d "$work/dest" -o
  apart ansible-pull -U "$work/repo" -d "$work/dest" --purge local.yml
  run test -e dest
  apart ansible-pull -U "$work/absent" -d "$work/gone"
  apart ansible-pull -U "$work/absent" -d "$work/gone" -f
} | normal > "$out"
check
