# OpenTelemetry Collector (contrib) 0.165.0 for the management telemetry
# collector. The base image is pinned by digest; bump it deliberately.
# The image keeps the base image's user; the CVM compose decides which user
# runs it (it needs to read the Docker socket and container log files).
FROM otel/opentelemetry-collector-contrib@sha256:e40d61f10c9a617cca1ffd1be4d7e2b3a4b0f0001798ff07e58cabe82e618cea

COPY config.yaml /etc/otelcol-management/config.yaml

CMD ["--config=/etc/otelcol-management/config.yaml"]
