# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Quartermaster (`quma`) is a Rust CLI + web UI tool for managing server-side mods on an SPT/Fika dedicated server. It installs, updates, and removes mods from [SPT Forge](https://forge.sp-tarkov.com), with a web dashboard for server hosts and connected players. Linux-only for v1; the SPT server runs in a container.

> **Detached local fork.** This is a private, upstream-detached copy adapted for a Docker + compose stack running the `ghcr.io/dildz/spt-fika-server` image with Dildz/Corter ModSync (not NarcoNet). The container layer talks to the Docker Engine API via bollard and uses the Docker socket first, so "Podman" below reads as "Docker" here. See `LOCAL-SETUP.md` for how to run it against this box's stack without touching the live server. See **Upstream Divergence** below before pulling anything from upstream.

**Binary name**: `quma`

## Build & Development Commands

```bash
just build          # cargo build
just check          # cargo check
just test           # cargo test
just clippy         # cargo clippy -- -D warnings
just fmt            # cargo fmt
just lint           # just fmt + just clippy + just check-logging + just cpd
just run <ARGS>     # cargo run -- <ARGS>
just serve          # cargo run -- serve (starts web UI on 0.0.0.0:9190)
just audit          # cargo audit
just changelog      # git-cliff changelog generation
just changelog-preview  # preview unreleased changes only
just release-dry-run    # dist build (dry run)
```

**Additional recipes:**

```bash
just dev-install-tools  # install dev tools (cargo-watch for auto-reload)
just check-logging      # validate logging conventions via scripts/check-logging.sh
just cpd                # jscpd copy-paste detection
just install-hooks      # set up git hooks for local CI linting
```

**Local dev environment** (SPT dev environment at `.dev-server/`, bootstrapped via `quma setup`):

```bash
just dev-init       # bootstrap SPT dev environment at .dev-server/ via quma setup
just dev-serve      # build & run web UI against .dev-server/
just dev-cli <ARGS> # run any quma command against .dev-server/
just dev-watch      # auto-rebuild & restart dev server on file changes (needs cargo-watch)
just dev-seed       # seed dev database with test data (wipes & repopulates)
just dev-reset-db   # wipe .dev-server/ database (keeps config & structure)
just dev-clean      # remove .dev-server/ and container entirely
just dev-info       # show dev environment settings (port, container, worktree)
```

**Worktree-safe parallel dev environments**: The `dev-*` recipes auto-detect git worktrees and derive unique port/container names so multiple agents can work in parallel without conflicts:
- **Main repo**: port 9190, container `spt-server-dev`
- **Worktrees**: deterministic port 9191-9289, container `spt-server-<worktree-name>`
- Override with `QUMA_DEV_PORT` / `QUMA_DEV_CONTAINER` env vars if needed
- `.dev-server/`, `target/`, and database files are relative paths — already isolated per worktree
- Run `just dev-info` to check the current settings

Run a single test: `cargo test <test_name>` or `cargo test -p quartermaster <test_name>`

**Environment for testing**: Set `QUMA_SPT_DIR=~/spt-server` to point at the local SPT install. The database lives at `<spt_dir>/quartermaster.db`, config at `<spt_dir>/quartermaster.toml`.

## Architecture

Single Rust binary — the CLI and actix-web server share the same codebase. The web server is just the `serve` subcommand.

### Core Layers

- **`src/cli/`** — One file per CLI subcommand (clap derive). Each command's `run()` function is the entry point. `common.rs` holds `CliContext` (spt_dir, config, db, forge client) and shared helpers like `resolve_mod()` for resolving user input to Forge mod IDs.
- **`src/web/`** — actix-web server. `mod.rs` defines all routes and middleware wiring. `state.rs` defines `AppState` (shared via `web::Data`). Handlers live in `web/handlers/` (one file per page group: admin, auth, backup, clients, common, dashboard, join, logs, metrics, mods, modsync, profiles, queue, raids, requests, server, settings, setup, svm, tasks). `common.rs` has shared helpers (`ForgeSearchResult`, `forge_search`). Authentication uses `RequireAuth` middleware with RBAC permission checks per-handler via `require_permission(&user, Permission::X)`. Supporting modules: `sse.rs` (SSE broadcast), `flash.rs` (flash messages), `template_filters.rs` (Askama filters), `update_cache.rs` (Forge update cache), `raid_tracker.rs` (per-raid stats, fed by the Fika presence poller), `poller.rs` (polls Fika presence to derive raid start/end), `csrf.rs` (CSRF token protection), `nav.rs` (navigation helpers), `error.rs` (error rendering), `install.rs` (shared download/extract/record for mods+requests), `invite.rs` (invite code handling), `tasks.rs` (background task management), `mod_zip_cache.rs` (cached mod ZIP for join page).
- **`src/db/`** — SQLite via rusqlite (WAL mode, `busy_timeout=5000`). `schema.rs` runs migrations from `migrations/` directory; each migration is wrapped in a transaction (`unchecked_transaction`) that includes the version bump. `mods.rs` has mod CRUD, `addons.rs` has addon CRUD, `users.rs` has user/invite operations, `raids.rs` has raid and kill CRUD, `requests.rs` has mod request/voting operations, `backups.rs` has backup metadata CRUD, `rbac.rs` has role-based access control queries, `logs.rs` has log storage and querying for the SQLite log viewer. Database is wrapped in `Arc<parking_lot::Mutex<Database>>` for web access.
- **`src/forge/`** — HTTP client for SPT Forge API (`https://forge.sp-tarkov.com/api/v0`). `client.rs` is the reqwest-based client, `models.rs` defines API response types. Key quirk: `fika_compatibility` is a boolean on mod objects but a string enum on version objects.
- **`src/spt/`** — SPT directory interaction. `detect.rs` auto-detects SPT installs and reads version info from `core.json`. `mods.rs` handles archive extraction (ZIP/7z), file hashing, and mod file management. Both ZIP and 7z extraction reject symlink entries (tested via `zip_rejects_symlink` and `sevenz_rejects_symlink_entry`). `profiles.rs` reads SPT player profiles. `server.rs` handles SPT server HTTP communication (HTTPS with self-signed certs, zlib compression disabled via `responsecompressed: 0` header).
- **`src/ops.rs`** — Core mod operations: `install_mod_from_archive`, `update_mod_from_archive`, `remove_mod_by_id`. Both install and update extract to a `tempfile::tempdir()` staging directory before committing to the DB and moving files into place. Async updates use `apply_mod_update` with a `pending_updates` marker for crash recovery (`recover_pending_updates` runs on startup).
- **`src/backup.rs`** — Mod backup/restore system: per-mod and full snapshots of mod files, profiles, and config. Used by CLI `backup`/`restore` commands and web backup handler.
- **`src/health.rs`** — Health check system: server liveness, version verification, mod load verification, file integrity (SHA256).
- **`src/container.rs`** — Container management for SPT server lifecycle via bollard (Docker Engine API; tries `/var/run/docker.sock` first, falls back to the Podman rootless socket). Default SPT server image: `ghcr.io/dildz/spt-fika-server:latest` (local-fork default; used only when `setup` creates a container from scratch — on this box, wrap the compose-managed container instead). quma never creates a headless container.
- **`src/queue.rs`** — Change queue: mod operations are queued when SPT server is running, applied when stopped.
- **`src/server_detect.rs`** — Server running detection (Podman inspect or HTTP ping fallback).
- **`src/logging/`** — Structured logging with tracing. `mod.rs` has `LogBroadcast` (tokio broadcast + ring buffer), tracing subscriber setup, and per-layer target filtering. `compact.rs` is a custom compact console formatter. `writer.rs` is an async SQLite log writer for the log viewer. Supports console, file (with rotation), SQLite persistence, and web broadcast (SSE). Web log viewer caps DOM at 2000 entries with `trimOldEntries()` and disconnects SSE on hidden tabs.
- **`src/config.rs`** — Config types (serde TOML), env var overrides (`QUMA_*` prefix), and config resolution logic.
- **`src/modsync.rs`** — NarcoNet integration: regenerates `config.yaml` from installed mod state so clients auto-sync.
- **`src/client_files.rs`** — Classifies mod files as client-side or server-side. quma does not copy client files to the headless — ModSync owns that.
- **`src/tls.rs`** — TLS certificate loading/generation.
- **`src/invite.rs`** — Invite code generation and expiry parsing.
- **`src/spt/headless.rs`** — SPT server API types for headless client queries.
- **`src/spt/game_data.rs`** — Loads quest/trader/hideout metadata from SPT data files for profile display.
- **`src/svm/`** — Server Value Modifier (SVM) support. `metadata.rs` defines SVM categories and parameter metadata, `config.rs` handles reading/writing SVM config files.

### Web UI Stack

- **Templates**: Askama (compile-time checked) in `templates/`. Base layout in `base.html`, page templates extend it. Partials in `templates/partials/` and `templates/mods/partials/` for HTMX swap targets.
- **Frontend**: HTMX for interactivity (no JS build step). SSE for real-time updates (task progress, log streaming). Static assets (CSS, htmx.min.js, sse.js) embedded via rust-embed from `src/assets/`.
- **Sessions**: Signed cookies via actix-session (`CookieSessionStore`), 7-day TTL, SameSite=Strict, HttpOnly.
- **Rate limiting**: actix-governor on `/login` POST and `/register` (5 req/min/IP).
- **CSRF**: Token-based protection in `web/csrf.rs`.

### Headless clients — monitor only

quma does **not** own the headless clients: the compose stack starts them, and ModSync
delivers their mods. quma monitors them and can start/stop/restart the one container named
by `headless_container` (`QUMA_HEADLESS_CONTAINER`). Status and player lists come from the
Fika API (`/fika/headless/get`), not from any internal supervisor state.

The convergence/supervisor subsystem that used to create, scale, and pin quma-owned headless
containers (`src/client/`, `src/numa.rs`, `cli/headless.rs`) has been removed — do not
reintroduce a code path that creates a headless container.

### Raid stats

Raids are recorded by `web/poller.rs`, which polls Fika presence (`/fika/presence/get`) every
`fika_poll_secs` and derives raid start/end from players entering and leaving a map. Exit status
is recovered by diffing the profile's `Stats.Eft.OverallCounters` `ExitStatus/*` entries across
the raid; kills come from the `Victims` array; the killer from `Aggressor`. The SPT profile on
disk is the source of truth — quma has the mount.

### Key Patterns

- **CLI context resolution**: Most commands go through `cli::common::resolve_context()` which detects the SPT dir, loads config with env overrides, opens the DB, and creates the Forge client.
- **Web async DB access**: Database calls in web handlers use `web::block(move || { ... })` since rusqlite is synchronous. The DB is behind `Arc<parking_lot::Mutex<Database>>`.
- **Mod resolution**: Users can reference mods by name, slug, or numeric Forge ID. `common::resolve_mod()` handles disambiguation.
- **Archive extraction**: `spt::mods::extract_mod()` inspects archive directory structure to determine mod type (server mod → `SPT/user/mods/`, client mod → `BepInEx/plugins/`, hybrid → both).

### Database Migrations

SQL files in `migrations/` are numbered sequentially (001, 002, ...). The `schema::run_migrations()` function applies them in order, tracking applied migrations in a `schema_migrations` table.

## SPT Server Communication

The SPT server runs HTTPS on port 6969 (default) with a self-signed TLS certificate. Key endpoints:
- `GET /launcher/ping` → `"pong!"` (liveness)
- `GET /launcher/server/version` → version string
- `GET /launcher/server/loadedServerMods` → map of loaded mod metadata

Send `responsecompressed: 0` header to get raw JSON instead of zlib-compressed responses. TLS verification is disabled (self-signed cert).

## Git Workflow

**Always use a worktree for changes.** Never commit directly to main. Use `EnterWorktree` to create an isolated worktree before making any code changes — this keeps main clean and enables parallel work across multiple agents. Use relative paths inside worktrees (never hardcode absolute paths from the main repo). The `dev-*` recipes are worktree-aware and will auto-derive unique ports and container names per worktree.

## Autonomy — Subagent-Driven Development

When executing the `superpowers:subagent-driven-development` skill (or any SDD workflow), operate fully autonomously without asking for confirmation. This includes:
- Creating and removing git worktrees
- All git operations (add, commit, push, checkout, branch, merge, rebase, reset, stash, cherry-pick)
- Running builds, tests, lints, and the binary
- Deleting or overwriting files as needed during implementation
- Force-pushing feature branches (never force-push main)
- Cleaning up worktrees and temporary branches after completion

Do not pause for confirmation on any of these during SDD execution. The review checkpoint built into the skill is sufficient oversight.

## Forge API Quirks

- `fika_compatibility` is a **boolean** on mod objects, but a **string enum** (`"compatible"`, `"incompatible"`, `"unknown"`) on version objects.
- `include=versions` on list endpoint returns abbreviated versions (last 6, no `link`/`content_length`/`fika_compatibility`).
- `include=versions` on single-mod endpoint returns full versions (last 10, all fields).
- Dedicated versions endpoint (`GET /mod/{id}/versions`) supports filtering and pagination.

## Upstream Divergence

**Audit date: 2026-08-27.** Merge base `5f1a1f1` (2026-07-07). Upstream `cebarks/quartermaster@main`
tip at audit: `672224c` (2026-07-31). Gap: **21 ours ahead / 164 upstream behind**, 181 files.

### Do not merge upstream main

Upstream landed `refactor: remove NarcoNet/modsync code, convoy is the sole sync system`.
`src/web/handlers/modsync.rs` and `templates/modsync/` **no longer exist upstream** — they were
replaced by a new in-house delivery system (`src/convoy/`, +1257 lines, catalog + groups DB +
web UI + bundle predownload).

This fork uses Dildz/Corter ModSync and is not adopting Convoy. A merge or rebase onto upstream
main would delete the ModSync integration this fork depends on. **Cherry-pick only.**

Same story for the other two big upstream tracks, both aimed at subsystems this fork deliberately
removed (see "Headless clients — monitor only"):
- **Overlayfs mod isolation** — bind mounts replaced by Podman/fuse-overlayfs, all mod file ops
  redirected through an overlay dir. This fork is Docker + bollard and keeps direct mounts.
- **Headless rework** — service layer + JSON API, CLI as HTTP client, per-client images,
  purpose-built containers. This fork does not own headless clients.

### The `QumaDirs` ceiling

Upstream `a28645e` (`replace spt_dir with QumaDirs struct`) touches **56 files** and is the pivot
into the overlay layout. Every later upstream commit that touches paths assumes `QumaDirs`.

Treat it as a hard boundary: cherry-pick freely from the path-agnostic areas (Forge client,
dependency tree, invites, templates, migrations). Anything path-related above that line means
either porting `QumaDirs` — a large refactor that exists to serve overlayfs, which this fork does
not want — or hand-editing. Prefer hand-porting the specific hunk.

### Migration numbering — fix this first

Our `migrations/015_update_notifications.sql` was upstreamed and **renumbered to `021`**.
Upstream now has 018–022 where we have nothing, and upstream's 021 duplicates our 015.

Cherry-picking upstream migrations without renumbering gives the sequence 015, 018, 019, 020, 022
— with our 015 running *before* 018, where upstream runs the same SQL *after*. Renumber our
`015_update_notifications.sql` → `021_update_notifications.sql` as a deliberate first step,
before picking anything else. Note upstream itself has a duplicate `021_` prefix
(`021_mod_guid.sql` + `021_update_notifications.sql`).

### Already contributed upstream — do not re-pick

These are ours and already merged into upstream; `src/github.rs`, `src/notify.rs` and
`src/cli/reindex.rs` exist on both sides:
- `feat: add quma reindex to rebuild file tracking from Forge archives`
- `feat(web): add check-for-updates button to mods page` (#312)
- `feat: GitHub release updates and Discord mod-update notifications`
- `fix(web): gate privileged mod endpoints behind RBAC permission checks` (#307)
- `fix(forge): align Forge API client with current docs`

### Tier 1 — clean, isolated, take these

| Commit | What | Scope |
| --- | --- | --- |
| `97f5ef7` | ammonia XSS fix, RUSTSEC-2026-0213 | `Cargo.lock` only |
| `679baba` | Forge client-level rate limiter (prevents 429s) | 3 files, new `forge/rate_limit.rs` |
| `22008de` | Clamp retry-after floor to 1s, `MAX_RETRIES` → 3 | `forge/client.rs` |
| `5abb84e` | Remove hardcoded 500 MB download size limit | `forge/client.rs` |
| `b782861` | `/quma/files` renders blank — HTMX target inheritance | `templates/files.html` |
| `f904565` | Dependency tree schema migration (`018`) | `migrations/`, `dev/seed.sql` |
| `4e1a3f6` | `quma list --tree` | `cli/list.rs`, `cli/mod.rs`, `main.rs` |
| `2fd26d2` | `quma reindex --deps` dependency backfill | 4 files, builds on our `reindex.rs` |
| `7a44b4b` | Prevent duplicate pending mod-queue operations | migration `019` + 2 files |

The Forge cluster is the highest value — our `forge/client.rs` is already close to upstream's
(we upstreamed the API-docs alignment), so these should apply near-clean, and 429 handling
matters on bulk update runs.

`b782861` overlaps our own `9168555` ("stop the integrity poll from blanking the File Tracking
page") — diff both before taking; may be the same bug found twice or complementary.

### Tier 2 — real value, real work

- **`5479fb3`** ban IPs with excessive unhandled requests — new `src/web/scanner_guard.rs`, plus
  `config.rs` (we cut 670 lines) and `web/mod.rs`. Worth it if the box is internet-facing.
- **`e355b7a`** use real client IP behind reverse proxy — **directly relevant**: running behind a
  reverse proxy in Docker, logged and banned IPs are currently the proxy's. Touches `web/proxy.rs`
  (deleted here) and `handlers/convoy.rs` (absent). Hand-port the `scanner_guard.rs` portion only.
- **`672224c`** SHA-256 → xxHash3-64 file hashing — 4 files, tidy, real speedup on large mod dirs.
  Touches `spt/mods.rs`, near our file-tracking work. Ships migration `022` to clear old hashes.
- **`63f5db2`** multi-use invites — 12 files but all inside the user/invite subsystem we left
  untouched; should apply near-clean.
- **Requests page kanban → compact tabbed table** — several commits, self-contained in
  `templates/requests` + handlers. Adds a `time_ago` Askama filter and an HTMX tab-body endpoint.
- **RBAC hardening** (`harden RBAC — profile access control, group permissions, role sync`,
  `harden RBAC access controls`) — extends our own endpoint gating.
- **`fix: fail signup atomically when SPT profile creation returns empty AID`** — real bug.
- **`fix(raids): use correct profile key for scav character snapshots`** — will not apply (we
  rewrote `web/raid_tracker.rs`), but read the diff; the bug may exist here too.

### Skip

All Convoy (~25 commits), all headless (~20), all overlayfs (~10), NUMA, the purpose-built
container images, and CI fixes for the release/publish-crate workflows this fork deleted.
`0d8e9c2` (bollard CPU stats) touches `src/client/supervisor.rs`, which no longer exists here.

## AI Disclosure

This project uses LLM-based tools (Claude Code) for implementation assistance. All architecture, design, and technical direction are human-driven — the LLM operates as an implementation aid under continuous supervision and review. This CLAUDE.md file itself is how the LLM receives project context and conventions.
