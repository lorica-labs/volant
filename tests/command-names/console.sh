#!/bin/sh
. "$(dirname "$0")/lib.sh"
# A session on a pipe: the prompt before each line, nothing echoed. LOGNAME names the user the
# prompt shows.
printf 'list\ncd web\nping\nshell echo hi\n!echo there\nforks 2\nbecome yes\nhelp forks\nexit\n' > session
{
  printf '$ ansible-console -i console.yml -f 1 < session\n'
  LOGNAME=u ansible-console -i console.yml -f 1 < session > stdout 2> stderr
  printf 'rc=%s\n' "$?"
  cat stdout
  printf -- '--- stderr\n'
  cat stderr
} > "$out"
check
