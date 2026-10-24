#!/bin/sh
. "$(dirname "$0")/lib.sh"
{
  run ansible-inventory -i hosts.ini --list
  run ansible-inventory -i hosts.ini --host w1
  run ansible-inventory -i hosts.ini --graph
  run ansible-inventory -i hosts.ini --list --yaml
  run ansible-inventory -i hosts.ini --list --export
} > "$out"
check
