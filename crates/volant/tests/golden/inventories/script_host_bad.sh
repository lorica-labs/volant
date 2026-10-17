#!/bin/sh
case "$1" in
--list) echo '{"g":{"hosts":["h1"]}}' ;;
--host) echo "{bad" ;;
esac
