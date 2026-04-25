#!/bin/bash
set -e

CONNECTOR="${TAPFS_CONNECTOR:-jsonplaceholder}"
MOUNT="${TAPFS_MOUNT:-/mnt/tap}"
DATA="${TAPFS_DATA:-/var/lib/tapfs}"

case "${1:-mount}" in
    mount)
        echo "tapfs: mounting connector=$CONNECTOR at $MOUNT"
        mkdir -p "$MOUNT" "$DATA"
        exec tap mount "$CONNECTOR" \
            --mount-point "$MOUNT" \
            --data-dir "$DATA" \
            --debug
        ;;
    shell)
        echo "tapfs: starting shell"
        echo "  Mount:  tap mount <connector> -m $MOUNT"
        echo "  Built-in connectors: $(tap connectors 2>/dev/null | grep '  ' | tr -d ' ' | tr '\n' ' ')"
        exec /bin/bash
        ;;
    *)
        exec "$@"
        ;;
esac
