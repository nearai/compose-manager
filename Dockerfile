FROM rust:1.92.0-bookworm@sha256:9676d0547a259997add8f5924eb6b959c589ed39055338e23b99aba7958d6d31 AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim@sha256:78d2f66e0fec9e5a39fb2c72ea5e052b548df75602b5215ed01a17171529f706
# Reproducibility:
#  - docker-ce-cli / docker-compose-plugin are version-pinned so a rebuild on a
#    later day installs the exact same bytes (the live repo otherwise drifts).
#  - apt/dpkg/ldconfig write wall-clock timestamps and a non-deterministic
#    ldconfig aux-cache into /var/log and /var/cache. rewrite-timestamp only
#    normalizes tar mtimes, not file *contents*, so these break byte-for-byte
#    rebuilds (same size, different bytes every build). Remove them in the same
#    layer. Mirrors inference-proxy #153.
RUN apt-get update && apt-get install -y ca-certificates curl gnupg && \
    install -m 0755 -d /etc/apt/keyrings && \
    curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc && \
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian bookworm stable" > /etc/apt/sources.list.d/docker.list && \
    apt-get update && apt-get install -y \
        docker-ce-cli=5:29.5.3-1~debian.12~bookworm \
        docker-compose-plugin=5.1.4-1~debian.12~bookworm && \
    apt-get purge -y curl gnupg && apt-get autoremove -y && \
    rm -rf /var/lib/apt/lists/* \
           /var/log/apt/* /var/log/dpkg.log /var/log/alternatives.log \
           /var/cache/ldconfig/aux-cache
COPY --from=builder /app/target/release/compose-manager /usr/local/bin/
CMD ["compose-manager"]
