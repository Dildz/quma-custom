# quma — SPT/Fika mod-manager. Multi-stage: Rust build -> slim runtime.
# Binary is fully self-contained (migrations include_str!, assets rust-embed,
# sqlite bundled, archives pure-Rust). Runtime needs ca-certificates (rustls
# trust roots) + git (config_mgmt/git.rs shells out to it), plus gosu + passwd
# so the entrypoint can align to the docker-socket group and drop privileges.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --bin quma

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git gosu passwd \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/quma /usr/local/bin/quma
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh
# spt_dir (mods + quartermaster.toml/.db) and docker.sock come in as mounts.
# Start as root: the entrypoint reads the mounted socket's gid, joins that group,
# then gosu-drops to PUID:PGID (default 1000). Compose passes PUID/PGID, NOT `user:`.
# Entrypoint self-bootstraps (setup) on first boot, then serves.
EXPOSE 9190
ENTRYPOINT ["docker-entrypoint.sh"]
