# Locally assembled digest; publication and anonymous pull verification pending.
FROM nearaidev/apt-bundle-compose-manager@sha256:3ca8a75cf13ab4cb76f9a2c09e0d3b1c22e4d7eeb4f6d0ebcf685b534f76867f AS apt-bundle
FROM scratch AS apt-bundle-files
COPY --from=apt-bundle /apt-bundle /apt-bundle

FROM rust:1.92.0-bookworm@sha256:9676d0547a259997add8f5924eb6b959c589ed39055338e23b99aba7958d6d31 AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim@sha256:78d2f66e0fec9e5a39fb2c72ea5e052b548df75602b5215ed01a17171529f706
# Reproducibility (byte-for-byte runtime image):
#  - Debian packages (ca-certificates/openssl/gpgv/…) are pinned to the SAME
#    snapshot the base image was built from (20251020T000000Z, per the base's
#    /etc/apt/sources.list.d/debian.sources). Installing from the live archive
#    otherwise drifts across build days even though the base digest is pinned.
#  - --no-install-recommends so unpinned extras are not pulled in; the docker
#    packages we DO need are version-pinned in the signed offline bundle:
#    docker-ce-cli, docker-compose-plugin, docker-buildx-plugin.
#    buildx is REQUIRED, not optional: compose files build images from
#    `dockerfile_inline`, which is a BuildKit-only feature. Without the buildx
#    plugin `docker compose build` silently falls back to the legacy builder,
#    which can't synthesize an inline Dockerfile and instead looks for a literal
#    ./Dockerfile, failing every inline-build deploy. buildx used to arrive
#    implicitly as a docker-ce-cli *recommends*; --no-install-recommends dropped
#    it, so it must be pinned explicitly here.
#  - apt/dpkg/ldconfig write wall-clock timestamps and a non-deterministic
#    ldconfig aux-cache into /var/log and /var/cache. rewrite-timestamp only
#    normalizes tar mtimes, not file *contents*, so these break byte-for-byte
#    rebuilds (same size, different bytes every build). Remove them in the same
#    layer. Mirrors inference-proxy #153.
#  - eMudhra intermediate CA key compromise (2026-10-20): bookworm has no fixed
#    ca-certificates yet, so distrust the two roots the compromised intermediates
#    chain to. Remove once the pinned snapshot ships a fixed ca-certificates.
RUN --network=none \
    --mount=type=bind,from=apt-bundle-files,source=/apt-bundle,target=/opt/apt-bundle,ro \
    --mount=type=bind,source=build/apt-bundle/prepare-apt.sh,target=/run/prepare-apt.sh,ro \
    --mount=type=bind,source=build/apt-bundle/docker.asc,target=/run/docker.asc,ro \
    set -e; \
    bash /run/prepare-apt.sh --bundle /opt/apt-bundle --snapshot 20251020T000000Z \
        --config /tmp/apt-bundle.conf --debian-keyring /usr/share/keyrings/debian-archive-keyring.gpg \
        --status /var/lib/dpkg/status --docker-keyring /run/docker.asc && \
    if [ -f /var/lib/apt/extended_states ]; then \
        cp /var/lib/apt/extended_states /tmp/apt-bundle.conf.d/state/extended_states; \
    fi && \
    export APT_CONFIG=/tmp/apt-bundle.conf && \
    apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl gnupg && \
    apt-get install -y --no-install-recommends \
        docker-ce-cli=5:29.5.3-1~debian.12~bookworm \
        docker-compose-plugin=5.1.4-1~debian.12~bookworm \
        docker-buildx-plugin=0.34.1-1~debian.12~bookworm && \
    sed -i -e 's|^mozilla/emSign_ECC_Root_CA_-_G3.crt$|!&|' \
           -e 's|^mozilla/Comodo_AAA_Services_root.crt$|!&|' /etc/ca-certificates.conf && \
    update-ca-certificates && \
    test ! -e /etc/ssl/certs/emSign_ECC_Root_CA_-_G3.pem && \
    test ! -e /etc/ssl/certs/Comodo_AAA_Services_root.pem && \
    apt-get purge -y curl gnupg && apt-get autoremove -y && \
    cp /tmp/apt-bundle.conf.d/state/extended_states /var/lib/apt/extended_states && \
    rm -rf /tmp/apt-bundle.conf /tmp/apt-bundle.conf.d \
           /var/lib/apt/lists/* \
           /var/log/apt/* /var/log/dpkg.log /var/log/alternatives.log \
           /var/cache/ldconfig/aux-cache
COPY --from=builder /app/target/release/compose-manager /usr/local/bin/
CMD ["compose-manager"]
