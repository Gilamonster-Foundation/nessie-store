# nessie-store — ONTAP-on-ZFS daemon image.
#
# Build the daemon, then run it on a minimal image with ZFS + NFS userland. The
# entrypoint bootstraps a file-backed ZFS pool on first run (needs --privileged),
# so a hobbyist gets a working ONTAP endpoint with one `docker run`.

# ---- builder ----------------------------------------------------------------
FROM rust:1.88-bookworm AS builder
WORKDIR /src
# Cache deps: copy manifests first, then sources.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p nessie-store

# ---- runtime ----------------------------------------------------------------
FROM ubuntu:24.04
# No nfs-kernel-server: nessie-store serves NFS itself (embedded userspace NFSv3).
RUN apt-get update \
    && apt-get install -y --no-install-recommends zfsutils-linux ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/nessie-store /usr/bin/nessie-store
COPY scripts/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Pool + vdev defaults (override at `docker run -e …`).
ENV NESSIE_DATA_DIR=/data \
    NESSIE_ZFS_POOL=ontap-sim \
    NESSIE_VDEV_SIZE=1G \
    NESSIE_ADMIN_PASSWORD=admin
VOLUME ["/data"]
# 8443 = ONTAP REST (control); 2049 = embedded NFSv3 (data).
EXPOSE 8443 2049

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["nessie-store", "serve", "--config", "/data/config.toml"]
