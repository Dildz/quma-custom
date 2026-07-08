# LOCAL-SETUP.md — running this fork against the SPT-Fika-Docker stack

This is a **detached local fork** of `quartermaster`, adapted for the Docker +
docker-compose stack on this VPS (image `ghcr.io/dildz/spt-fika-server`, Dildz/Corter
**ModSync** — *not* NarcoNet). Upstream remote and publish workflows have been removed.

The golden rule: **never point `quma` at the live 4.0.x stack.** Run it against a
throwaway test container on an isolated port, live stack down.

---

## Why almost no code change was needed

`src/container.rs` uses **bollard** (the Docker Engine API client), and connects to
`/var/run/docker.sock` **first**, Podman socket only as fallback. With `dockerd`
running it drives your containers natively. So this fork is really a **web mod-manager +
dashboard + HTTPS proxy** for a container it *controls by name* — no Podman required.

What was changed for this box:
- Removed the `origin` remote + `release.yml` / `publish-crate.yml` workflows (detach).
- `SPT_SERVER_IMAGE` default → `ghcr.io/dildz/spt-fika-server:latest` (`src/container.rs`).
- Docs (`CLAUDE.md`) corrected from "Podman" to Docker reality.

What was **not** changed (deliberately):
- **NarcoNet → ModSync** (`src/modsync.rs`). quma's "modsync" writes a **NarcoNet**
  `config.yaml`. You run Dildz/Corter ModSync, which is a different mod with a different
  config. That feature is **off unless you add a `[modsync]` block to the config** — so
  just leave it unset and quma won't touch anything. Porting it to ModSync's schema is a
  separate job; not needed to try the tool.
- The container **create/bootstrap** path (mount `/opt/server`, zhliau layout). Your image
  uses a game-root mount, so a from-scratch `quma setup` bootstrap would build a broken
  container. **Don't bootstrap — wrap your existing compose container** (below).

---

## Prerequisites

```bash
# Rust toolchain (2021 edition)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # if not installed

# quma must reach the Docker socket
sudo usermod -aG docker "$USER"    # then re-login, or run quma as a user in the docker group
```

Build:

```bash
cd /home/ubuntu/github-repos/quartermaster
cargo build --release     # ./target/release/quma
```

---

## Step 1 — stand up an isolated TEST container (live stack must be down)

Your live stack owns the game ports; per your stack rules you can't run a second server
while it's up. Bring live down first, then start a test container on a spare port
(you've used 6979 before), bind-mounting a **copy** of a server dir:

```bash
# a scratch game-root the test container + quma will share on the host
cp -a /path/to/a/server-copy /home/ubuntu/quma-test/spt      # host game-root

docker run -d --name spt-quma-test \
  -u 1000:1000 \
  -p 6979:6969 \
  -v /home/ubuntu/quma-test/spt:/opt/server \
  ghcr.io/dildz/spt-fika-server:latest
```

> **Ownership matters.** quma writes mod files directly on the host at `spt_dir`, and the
> container runs as uid 1000. Keep everything under `/home/ubuntu/quma-test/spt` owned by
> uid 1000 or you'll get root-owned files the server can't read (the exact footgun from
> your image notes). `chown -R 1000:1000 /home/ubuntu/quma-test/spt` if unsure.

Adjust the mount target (`/opt/server`) to whatever game-root path your image actually
mounts — check with `docker inspect <live-container> -f '{{json .Mounts}}'`.

## Step 2 — wrap it with quma (detect, don't create)

```bash
./target/release/quma --spt-dir /home/ubuntu/quma-test/spt setup /home/ubuntu/quma-test/spt
```

`setup` follows the **wrap-existing** path: it reads the SPT version, **detects** the
container whose mount matches `spt_dir` (finds `spt-quma-test`), writes
`quartermaster.toml` + `quartermaster.db` into `spt_dir`, and creates an `admin` user.

When it prompts:
- **Install Fika from Forge?** → **No.** Your image already provides Fika.
- **Install NarcoNet/ModSync from Forge?** → **No.** You use Dildz ModSync; NarcoNet is
  a different mod and its auto-sync doesn't apply here.

If detection ever fails and it offers to *create* a container, **decline / Ctrl-C** — see
the "don't bootstrap" note above.

## Step 3 — run the dashboard

```bash
QUMA_SPT_DIR=/home/ubuntu/quma-test/spt \
  ./target/release/quma serve          # web UI on 0.0.0.0:9190
```

Then hit `http://<vps>:9190`, log in as `admin`, and exercise:
`quma list`, `quma check`, `quma status`, `quma server start|stop|restart|logs`,
mod install/update from Forge, backups. All of it acts on `spt-quma-test` only.

---

## Minimal config reference (`<spt_dir>/quartermaster.toml`)

`setup` writes this for you; shown here so you know what each knob does. Every field is
also overridable via `QUMA_*` env vars.

```toml
spt_dir          = "/home/ubuntu/quma-test/spt"
server_container = "spt-quma-test"   # the container quma controls (start/stop/logs)
server_host      = "0.0.0.0"
server_port      = 6969              # SPT's port INSIDE the container
web_bind         = "0.0.0.0"
web_port         = 9190
auto_start_server = false            # don't let quma auto-start on boot while testing
# [modsync]  -> leave UNSET: prevents NarcoNet config.yaml generation
```

## Teardown

```bash
docker rm -f spt-quma-test
rm -rf /home/ubuntu/quma-test
# bring the live 4.0.x stack back up
```

---

## If you later want it in front of the LIVE stack

Two things to sort first:
1. **ModSync port.** quma's HTTPS/WSS proxy expects to sit between clients and SPT. Your
   clients currently hit the server directly; re-pointing them through quma is a client-side
   change, not just a server one. Test the proxy against `spt-quma-test` before trusting it live.
2. **NarcoNet vs ModSync.** If you want quma's client mod-sync, `src/modsync.rs` must be
   ported from NarcoNet's `config.yaml` schema to Dildz/Corter ModSync's. Until then, keep
   `[modsync]` unset and manage ModSync as you do today.
