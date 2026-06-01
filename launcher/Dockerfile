FROM alpine:3.21

RUN apk add --no-cache \
    bash \
    curl \
    jq \
    docker-cli \
    docker-cli-compose

# Install cosign static binary. MUST match the cosign-release used to SIGN in
# .github/workflows/build.yml (sigstore/cosign-installer's cosign-release, v3.0.6).
# The CI signer stores signatures via the OCI 1.1 referrers API in the sigstore
# bundle v0.3 format; cosign < 3 (e.g. the old v2.4.3 pin here) does not discover
# them and fails verification with "no signatures found", so the launcher could
# never apply an update. Keep this in lockstep with build.yml on every cosign bump.
RUN curl -fsSL https://github.com/sigstore/cosign/releases/download/v3.0.6/cosign-linux-amd64 \
    -o /usr/local/bin/cosign && chmod +x /usr/local/bin/cosign

COPY launcher.sh /app/launcher.sh
COPY docker-compose.yml /app/docker-compose.yml
RUN chmod +x /app/launcher.sh

ENTRYPOINT ["/app/launcher.sh"]
