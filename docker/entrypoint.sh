#!/bin/sh
set -eu

has_ipv4=false
for address in $(hostname -i 2>/dev/null); do
    case "$address" in
        127.*|*:*) continue ;;
    esac
    socat "TCP4-LISTEN:7268,bind=$address,fork,reuseaddr" TCP4:127.0.0.1:7268 &
    has_ipv4=true
done

if [ "$has_ipv4" = false ]; then
    echo 'scour: no container IPv4 address; run the image on a Docker bridge network' >&2
    exit 1
fi

exec scour serve "$@"
