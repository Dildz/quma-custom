#!/bin/sh
# quma container entrypoint: one-time bootstrap, then serve.
# `setup` runs only when quma isn't configured yet (no quartermaster.toml in the
# mount). After first boot the file persists in the volume, so every later start
# skips straight to `serve` — no per-boot container ops, no Forge calls.
#
# Requires (compose): docker.sock mounted, QUMA_ADMIN_PASSWORD from .env, and
# depends_on the SPT server so it exists before setup detects it — otherwise
# setup would create a fresh container from the wrong image/layout (setup.rs:593).
set -e

: "${QUMA_SPT_DIR:=/opt/server}"

if [ ! -f "$QUMA_SPT_DIR/quartermaster.toml" ]; then
  if [ -z "$QUMA_ADMIN_PASSWORD" ]; then
    echo "quma: first-boot setup needs QUMA_ADMIN_PASSWORD (set it in .env, min 8 chars)" >&2
    exit 1
  fi
  quma setup "$QUMA_SPT_DIR" --no-fika --no-modsync \
    --admin-password "$QUMA_ADMIN_PASSWORD" \
    --container-name "${QUMA_SERVER_CONTAINER:-fika-server-4.0}"
fi

exec quma serve
