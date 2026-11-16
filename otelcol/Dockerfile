# OpenTelemetry Collector (contrib) 0.120.0 for the management telemetry
# collector. The base image is pinned by digest; bump it deliberately.
# The image keeps the base image's user; the CVM compose decides which user
# runs it (it needs to read the Docker socket and container log files).
FROM otel/opentelemetry-collector-contrib@sha256:85ac41c2db88d0df9bd6145e608a3cb023f5d8443868adbfbbf66efb51087917

COPY config.yaml /etc/otelcol-management/config.yaml

CMD ["--config=/etc/otelcol-management/config.yaml"]
