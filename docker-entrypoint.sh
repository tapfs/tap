#!/bin/bash
set -e

SPEC="${TAPFS_SPEC:-/etc/tapfs/connectors/jsonplaceholder.yaml}"
MOUNT="${TAPFS_MOUNT:-/mnt/tap}"
DATA="${TAPFS_DATA:-/var/lib/tapfs}"

case "${1:-mount}" in
    mount)
        echo "tapfs: mounting with spec=$SPEC at $MOUNT"
        mkdir -p "$MOUNT" "$DATA"
        exec tap mount jsonplaceholder \
            --spec "$SPEC" \
            --mount-point "$MOUNT" \
            --data-dir "$DATA" \
            --debug
        ;;
    shell)
        echo "tapfs: starting shell (mount manually with: tap mount ...)"
        exec /bin/bash
        ;;
    *)
        exec "$@"
        ;;
esac
