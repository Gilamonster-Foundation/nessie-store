#!/usr/bin/env bash
# nessie-store container entrypoint.
#
# Bootstraps a file-backed ZFS pool + a default config on first run, then execs
# the daemon. Requires --privileged (and the host's ZFS kernel module) so the
# container can create/import a pool.
set -euo pipefail

DATA_DIR="${NESSIE_DATA_DIR:-/data}"
POOL="${NESSIE_ZFS_POOL:-ontap-sim}"
VDEV="${DATA_DIR}/vdev.img"
VDEV_SIZE="${NESSIE_VDEV_SIZE:-1G}"
CONFIG="${DATA_DIR}/config.toml"

mkdir -p "$DATA_DIR"

# 1. Pool: create a sparse-vdev pool on first run; otherwise import it.
if ! zpool list -H "$POOL" >/dev/null 2>&1; then
    if [ ! -f "$VDEV" ]; then
        echo "entrypoint: creating sparse vdev $VDEV ($VDEV_SIZE)"
        truncate -s "$VDEV_SIZE" "$VDEV"
    fi
    echo "entrypoint: importing-or-creating pool $POOL"
    zpool import -d "$DATA_DIR" "$POOL" 2>/dev/null \
        || zpool create -f -m none "$POOL" "$VDEV"
fi

# 2. Config: write a default once, wired to this pool + the env password.
if [ ! -f "$CONFIG" ]; then
    echo "entrypoint: writing default config $CONFIG"
    nessie-store init --config "$CONFIG"
    # Point it at the data dir + pool + zfs backend, and turn on the embedded
    # userspace NFS server (no host kernel NFS in the container).
    sed -i \
        -e "s|^data_dir = .*|data_dir = \"${DATA_DIR}\"|" \
        -e "s|^backend = .*|backend = \"zfs\"|" \
        -e "s|^zfs_pool = .*|zfs_pool = \"${POOL}\"|" \
        -e "s|^nfs_enabled = .*|nfs_enabled = true|" \
        "$CONFIG"
fi

# NESSIE_ADMIN_PASSWORD is honored by the daemon at startup (never written to disk).
exec "$@"
