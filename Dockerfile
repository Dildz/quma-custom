# quma — SPT/Fika mod-manager. Multi-stage: Rust build -> slim runtime.
# Binary is fully self-contained (migrations include_str!, assets rust-embed,
# sqlite bundled, archives pure-Rust). Runtime only needs ca-certificates
# (rustls trust roots) + git (config_mgmt/git.rs shells out to it).
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --bin quma

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/quma /usr/local/bin/quma
# spt_dir (mods + quartermaster.toml/.db) and docker.sock come in as mounts;
# runtime user/uid is set by compose (1000:1000, group_add docker gid).
EXPOSE 9190
ENTRYPOINT ["quma"]
CMD ["serve"]
