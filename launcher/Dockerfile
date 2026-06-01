FROM alpine:3.21

RUN apk add --no-cache \
    bash \
    curl \
    jq \
    docker-cli \
    docker-cli-compose

# Install cosign static binary (same version as main image CI)
RUN curl -fsSL https://github.com/sigstore/cosign/releases/download/v2.4.3/cosign-linux-amd64 \
    -o /usr/local/bin/cosign && chmod +x /usr/local/bin/cosign

COPY launcher.sh /app/launcher.sh
COPY docker-compose.yml /app/docker-compose.yml
RUN chmod +x /app/launcher.sh

ENTRYPOINT ["/app/launcher.sh"]
