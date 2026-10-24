#!/bin/sh
. "$(dirname "$0")/lib.sh"
{
  run ansible all -i inventory.yml -m ping -f 1
  run ansible all -i inventory.yml -m command -a 'echo hi' -f 1
  run ansible all -i inventory.yml -a false -f 1
} > "$out"
check
