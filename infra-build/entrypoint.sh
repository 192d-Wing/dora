#!/bin/bash

set -e


# Prefer `docker run --init` / `podman --init`, which obsoletes dumb-init. Only
# fall back to dumb-init if it is actually installed -- the hardened UBI base
# image does not ship it.
if [ $$ -eq 1 ] && command -v dumb-init >/dev/null 2>&1; then
    run="exec dumb-init --"
else
    run="exec"
fi

# Single argument to command line is interface name
if [ $# -eq 1 -a -n "$1" ]; then
    # skip wait-for-interface behavior if found in path
    # (`command -v` is portable; the minimal UBI base has no `which`)
    if ! command -v "$1" >/dev/null 2>&1; then
        # loop until interface is found, or we give up
        NEXT_WAIT_TIME=1
        until [ -e "/sys/class/net/$1" ] || [ $NEXT_WAIT_TIME -eq 4 ]; do
            sleep $(( NEXT_WAIT_TIME++ ))
            echo "Waiting for interface '$1' to become available... ${NEXT_WAIT_TIME}"
        done
        if [ -e "/sys/class/net/$1" ]; then
            IFACE="$1"
        fi
    fi
fi

# No arguments mean all interfaces
if [ -z "$1" ]; then
    IFACE=" "
fi

if [ -n "$IFACE" ]; then
    # Run dora for specified interface or all interfaces

    data_dir="/var/lib/dora"
    if [ ! -d "$data_dir" ]; then
        echo "Please ensure '$data_dir' folder is available."
        echo 'If you just want to keep your configuration in "data/", add -v "$(pwd)/data:/var/lib/dora" to the docker run command line.'
        exit 1
    fi

    dora_conf="$data_dir/config.yaml"
    if [ ! -r "$dora_conf" ]; then
        echo "Please ensure '$dora_conf' exists and is readable."
        exit 1
    fi

    uid=$(stat -c%u "$data_dir")
    gid=$(stat -c%g "$data_dir")
    groupmod -og $gid dhcpd
    usermod -ou $uid dhcpd

    [ -e "$data_dir/em.db" ] || touch "$data_dir/em.db"
    chown dhcpd:dhcpd "$data_dir/em.db"
    if [ -e "$data_dir/em.db~" ]; then
        chown dhcpd:dhcpd "$data_dir/em.db~"
    fi

    # Warn when we are not on the host network: outside --net=host the
    # container's $HOSTNAME defaults to a prefix of its container id. Done in
    # pure shell so the hardened base needs no perl. cgroup lookup is best-effort
    # (empty under cgroup v2) and must not abort the script under `set -e`.
    container_id=$(grep -m1 docker /proc/self/cgroup 2>/dev/null | sed -n 's#.*/##p') || true
    if [ -n "$container_id" ] && [ -n "$HOSTNAME" ]; then
        case "$container_id" in
            "$HOSTNAME"*)
                echo "You must add the 'docker run' option '--net=host' if you want to provide DHCP service to the host network."
                ;;
        esac
    fi

    exec /usr/local/bin/dora
else
    # Run another binary
    if [ $$ -eq 1 ] && command -v dumb-init >/dev/null 2>&1; then
        exec dumb-init -- "$@"
    else
        exec "$@"
    fi
fi
