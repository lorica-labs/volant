#!/bin/sh
. "$(dirname "$0")/lib.sh"
{
  # The module's file is somewhere in the install that holds ansible.
  run ansible-doc ping | sed 's|(/.*/ping.py)|(ping.py)|'
  run env ANSIBLE_FORKS=7 ansible-config dump --only-changed
} > "$out"
check
