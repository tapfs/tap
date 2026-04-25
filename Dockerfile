FROM rust:1.81-bookworm AS builder

# Install FUSE dev headers
RUN apt-get update && apt-get install -y libfuse-dev pkg-config && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY connectors/ connectors/

RUN cargo build --release

# Runtime image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y fuse3 libfuse2 ca-certificates && rm -rf /var/lib/apt/lists/*

# Allow non-root FUSE mounts
RUN echo "user_allow_other" >> /etc/fuse.conf

COPY --from=builder /app/target/release/tap /usr/local/bin/tap
COPY connectors/ /etc/tapfs/connectors/

# Create mount point and data dirs
RUN mkdir -p /mnt/tap /var/lib/tapfs

COPY docker-entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ENV TAPFS_CONNECTOR=jsonplaceholder
ENV TAPFS_MOUNT=/mnt/tap
ENV TAPFS_DATA=/var/lib/tapfs

ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["mount"]
