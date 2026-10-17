#!/bin/sh
case "$1" in
--list) echo '{"g":{"hosts":["h1","h2"],"vars":{"gv":"g"}}}' ;;
--host) echo "{\"hv\":\"$2\"}" ;;
esac
