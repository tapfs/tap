#!/bin/bash
set -e

CONNECTOR="${TAPFS_CONNECTOR:-rest}"
SPEC="${TAPFS_SPEC:-/etc/tapfs/connectors/${CONNECTOR}.yaml}"
MOUNT="${TAPFS_MOUNT:-/mnt/tap}"
DATA="${TAPFS_DATA:-/var/lib/tapfs}"

case "${1:-mount}" in
    mount)
        echo "tapfs: mounting connector=$CONNECTOR spec=$SPEC at $MOUNT"
        mkdir -p "$MOUNT" "$DATA"
        exec tap mount "$CONNECTOR" \
            --spec "$SPEC" \
            --mount-point "$MOUNT" \
            --data-dir "$DATA" \
            --debug
        ;;
    shell)
        echo "tapfs: starting shell"
        echo "  Mount manually:  tap mount <connector> -s /etc/tapfs/connectors/<name>.yaml -m $MOUNT"
        echo "  Available specs: $(ls /etc/tapfs/connectors/*.yaml 2>/dev/null | xargs -n1 basename | tr '\n' ' ')"
        exec /bin/bash
        ;;
    *)
        exec "$@"
        ;;
esac
