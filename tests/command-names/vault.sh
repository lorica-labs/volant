#!/bin/sh
. "$(dirname "$0")/lib.sh"
{
  run ansible-vault encrypt --vault-password-file pw secret.yml
  head -n 1 secret.yml
  run ansible-vault view --vault-password-file pw secret.yml
  ansible-vault encrypt_string --vault-password-file pw -n key 'inline value' > inline.yml 2>/dev/null
  head -n 2 inline.yml
  run ansible-playbook -i inventory.yml --vault-password-file pw vault.yml
  run ansible-vault rekey --vault-password-file pw --new-vault-password-file pw2 secret.yml
  run ansible-vault decrypt --vault-password-file pw2 secret.yml
  cat secret.yml
} > "$out"
check
