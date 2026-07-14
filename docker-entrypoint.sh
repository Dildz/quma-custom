#!/bin/sh
# quma container entrypoint: align to the docker socket, drop privileges, then
# one-time bootstrap + serve.
#
# quma talks to the Docker API to (re)start the SPT server container, so the run
# user must be in the *host's* docker-socket group — whose gid varies per host
# (994 here, 999/998 elsewhere). Rather than make the operator look that up, we
# start as root, read the mounted socket's gid, add the run user to it, then gosu
# down to PUID:PGID. Host-agnostic, no QUMA_DOCKER_GID knob.
#
# Requires (compose): docker.sock mounted, QUMA_ADMIN_PASSWORD from .env, and
# depends_on the SPT server so it exists before setup detects it — otherwise
# setup would create a fresh container from the wrong image/layout (setup.rs:593).
set -e

: "${QUMA_SPT_DIR:=/opt/server}"
: "${PUID:=1000}"
: "${PGID:=1000}"

# Privileged phase: only runs on a root start. Re-execs this script as PUID:PGID.
if [ "$(id -u)" = "0" ]; then
  # Reuse any existing group/user for the target ids, else create them.
  getent group "$PGID"  >/dev/null || groupadd -g "$PGID" quma
  getent passwd "$PUID" >/dev/null || useradd  -u "$PUID" -g "$PGID" -M -s /bin/sh quma
  run_user="$(getent passwd "$PUID" | cut -d: -f1)"

  # Put the run user in the mounted socket's group (host-specific gid).
  if [ -S /var/run/docker.sock ]; then
    sock_gid="$(stat -c %g /var/run/docker.sock)"
    getent group "$sock_gid" >/dev/null || groupadd -g "$sock_gid" dockersock
    usermod -aG "$(getent group "$sock_gid" | cut -d: -f1)" "$run_user"
  fi

  # quma writes its toml/db/mods here; make sure the run user owns the mount root.
  chown "$PUID:$PGID" "$QUMA_SPT_DIR" 2>/dev/null || true

  exec gosu "$run_user" "$0" "$@"
fi

# Unprivileged phase (dropped above, or compose set `user:` directly).
# Gate on the db as well as the toml: the admin user lives in the db, so a surviving
# toml alone would skip setup and leave `serve` dead on "No admin user exists".
# setup is idempotent — it reuses an existing toml and skips an existing admin.
if [ ! -f "$QUMA_SPT_DIR/quartermaster.toml" ] || [ ! -f "$QUMA_SPT_DIR/quartermaster.db" ]; then
  if [ -z "$QUMA_ADMIN_PASSWORD" ]; then
    echo "quma: first-boot setup needs QUMA_ADMIN_PASSWORD (set it in .env, min 8 chars)" >&2
    exit 1
  fi
  quma setup "$QUMA_SPT_DIR" --no-fika --no-modsync \
    --admin-password "$QUMA_ADMIN_PASSWORD" \
    --container-name "${QUMA_SERVER_CONTAINER:-fika-server-4.0}"
fi

exec quma serve
