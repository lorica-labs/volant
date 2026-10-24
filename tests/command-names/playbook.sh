#!/bin/sh
. "$(dirname "$0")/lib.sh"
{
  run ansible-playbook -i inventory.yml -f 1 site.yml
  run ansible-playbook -i inventory.yml -f 1 fails.yml
} > "$out"
check
