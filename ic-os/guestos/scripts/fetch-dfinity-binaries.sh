#!/usr/bin/env bash

REPO_ROOT=$(git rev-parse --show-toplevel)

function error() {
    echo "$1"
    exit 1
}

if [[ -z "${GIT_REV:-}" ]]; then
    echo "Please provide the GIT_REV as env. variable or the command line with --git-rev=<value>"
    exit 1
fi

function get_dfinity_binaries() {

    if [[ -n "$NNS_REPLICA" ]]; then

        echo "Downloading replica binary from NNS machine"
        ic_version="0.8.0"
        REPLICA_PATH="/var/lib/dfinity-node/replica_binaries/$ic_version/replica"
        REPLICA_TARGET="rootfs/opt/ic/bin/replica"
        REPLICA_DIR=$(dirname "$REPLICA_TARGET")
        mkdir -p "$REPLICA_DIR"

        rsync --rsync-path="sudo rsync" "${NNS_REPLICA}:${REPLICA_PATH}" "$REPLICA_TARGET"
        chmod a+x "$REPLICA_TARGET"
        sha256sum "$REPLICA_TARGET"

        NODEMANAGER_PATH="/var/lib/dfinity-node/replica_binaries/$ic_version/nodemanager"
        NODEMANAGER_TARGET="rootfs/opt/ic/bin/nodemanager"
        NODEMANAGER_DIR=$(dirname "$NODEMANAGER_TARGET")
        mkdir -p "$NODEMANAGER_DIR"

        rsync --rsync-path="sudo rsync" "${NNS_REPLICA}:${NODEMANAGER_PATH}" "$NODEMANAGER_TARGET"
        chmod a+x "$NODEMANAGER_TARGET"
        sha256sum "$NODEMANAGER_TARGET"

    else
        echo "Downloading replica and nodemanager binaries"

        TARGET_DIR="$REPO_ROOT/ic-os/guestos/rootfs/opt/ic/bin"

        "${REPO_ROOT}"/gitlab-ci/src/artifacts/rclone_download.py \
            --git-rev "$GIT_REVISION" --remote-path=release --out="$TARGET_DIR" \
            --include "{replica,nodemanager}.gz"

        for f in replica nodemanager; do
            gunzip -f "$TARGET_DIR/$f"
            chmod +x "$TARGET_DIR/$f"
        done
    fi
}
