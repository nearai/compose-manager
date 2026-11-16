"""Content checks for the management collector config.

`otelcol-contrib validate` proves the config loads; these checks pin what it
must and must not do. Run: python3 otelcol/check_config.py [path]
"""

import re
import sys

import yaml

# Env vars the config may read. The launcher, README and app compose rely on
# exactly this set.
ALLOWED_ENV = {
    "CVM_HOST",
    "CVM_HOSTMETRICS_ROOT_PATH",
    "CVM_NAME",
    "DD_HOSTNAME",
    "ENV",
    "MONITORING_INGEST_TOKEN",
    "MONITORING_OTLP_ENDPOINT",
    "OTELCOL_MEMORY_LIMIT_MIB",
    "OTELCOL_MEMORY_SPIKE_LIMIT_MIB",
}

# Only management-plane container logs may leave the CVM.
MANAGEMENT_LOG_SERVICES = {"compose-manager", "compose-manager-launcher", "dns-cloudflare"}

# Inputs and listeners this collector must not have.
FORBIDDEN_FRAGMENTS = (
    "journald:",
    "/run/log/journal",
    "/var/log/journal",
    "docker_sd_configs",
    "0.0.0.0:",
    "traces:",
    "$${",
)


def check(text: str) -> list[str]:
    errors = []
    cfg = yaml.safe_load(text)
    receivers = cfg["receivers"]
    processors = cfg["processors"]
    pipelines = cfg["service"]["pipelines"]

    used_env = set(re.findall(r"\$\{env:([A-Za-z0-9_]+)\}", text))
    if used_env - ALLOWED_ENV:
        errors.append(f"unexpected env vars: {sorted(used_env - ALLOWED_ENV)}")
    for fragment in FORBIDDEN_FRAGMENTS:
        if fragment in text:
            errors.append(f"forbidden fragment present: {fragment!r}")

    if set(receivers) != {"filelog/docker_containers", "hostmetrics/cvm", "docker_stats/cvm", "prometheus/self"}:
        errors.append(f"unexpected receivers: {sorted(receivers)}")
    if "otlp" in receivers:
        errors.append("the collector must not accept OTLP input")

    # Docker Stats: inventory fields used by the Grafana dashboard.
    stats = receivers["docker_stats/cvm"]
    if stats.get("api_version") != "1.44":
        errors.append("docker_stats api_version must be 1.44")
    labels = stats.get("container_labels_to_metric_labels", {})
    for label, name in (
        ("com.docker.compose.service", "compose_service"),
        ("com.docker.compose.project", "compose_project"),
        ("com.docker.compose.project.config_files", "compose_config_path"),
    ):
        if labels.get(label) != name:
            errors.append(f"docker_stats must map {label} to {name}")
    if stats.get("resource_attributes", {}).get("container.image.id", {}).get("enabled") is not True:
        errors.append("docker_stats must report container.image.id")
    for metric in ("container.uptime", "container.restarts", "container.state.health.status"):
        if stats.get("metrics", {}).get(metric, {}).get("enabled") is not True:
            errors.append(f"docker_stats must enable {metric}")
    if receivers["hostmetrics/cvm"].get("root_path") != "${env:CVM_HOSTMETRICS_ROOT_PATH}":
        errors.append("hostmetrics root_path must come from CVM_HOSTMETRICS_ROOT_PATH")

    # The raw Compose config path label must be dropped before export.
    system = pipelines["metrics/cvm_system"]
    if system["receivers"] != ["hostmetrics/cvm", "docker_stats/cvm"]:
        errors.append("metrics/cvm_system must read hostmetrics/cvm and docker_stats/cvm only")
    if "transform/compose_inventory" not in system["processors"]:
        errors.append("metrics/cvm_system must run transform/compose_inventory")
    statements = processors.get("transform/compose_inventory", {}).get("metric_statements", [{}])[0].get("statements", [])
    if 'delete_key(attributes, "compose_config_path")' not in statements:
        errors.append("transform/compose_inventory must delete compose_config_path")

    # Logs: only management services leave the CVM.
    logs = pipelines["logs/cvm"]
    if logs["receivers"] != ["filelog/docker_containers"]:
        errors.append("logs/cvm must read filelog/docker_containers only")
    procs = logs["processors"]
    if "filter/management_logs" not in procs or "transform/docker_metadata" not in procs:
        errors.append("logs/cvm must run transform/docker_metadata and filter/management_logs")
    elif procs.index("transform/docker_metadata") > procs.index("filter/management_logs"):
        errors.append("transform/docker_metadata must run before filter/management_logs")
    conditions = processors.get("filter/management_logs", {}).get("logs", {}).get("log_record", [])
    allowed = set(re.findall(r'attributes\["service\.name"\] != "([^"]+)"', " ".join(conditions)))
    if len(conditions) != 1 or allowed != MANAGEMENT_LOG_SERVICES:
        errors.append(f"filter/management_logs must keep only {sorted(MANAGEMENT_LOG_SERVICES)}")

    # Every pipeline exports only to the gateway, in order.
    for name, pipeline in pipelines.items():
        if pipeline.get("exporters") != ["otlphttp/gateway"]:
            errors.append(f"{name} must export only to otlphttp/gateway")
        if pipeline.get("processors", [None])[0] != "memory_limiter":
            errors.append(f"{name} must start with memory_limiter")
    if set(cfg["exporters"]) != {"otlphttp/gateway"}:
        errors.append(f"unexpected exporters: {sorted(cfg['exporters'])}")
    gateway = cfg["exporters"]["otlphttp/gateway"]
    if gateway.get("endpoint") != "${env:MONITORING_OTLP_ENDPOINT}":
        errors.append("gateway endpoint must come from MONITORING_OTLP_ENDPOINT")
    if gateway.get("headers", {}).get("Authorization") != "Bearer ${env:MONITORING_INGEST_TOKEN}":
        errors.append("gateway auth must come from MONITORING_INGEST_TOKEN")
    queue = gateway.get("sending_queue", {})
    if queue.get("num_consumers") != 1 or queue.get("storage") != "file_storage":
        errors.append("gateway queue must be FIFO (num_consumers 1) and persisted to file_storage")
    limiter = processors.get("memory_limiter", {})
    if limiter.get("limit_mib") != "${env:OTELCOL_MEMORY_LIMIT_MIB}" or limiter.get(
        "spike_limit_mib"
    ) != "${env:OTELCOL_MEMORY_SPIKE_LIMIT_MIB}":
        errors.append("memory_limiter limits must come from env")

    # The collector's own metrics: served on localhost:8888 for prometheus/self,
    # keeping the service_* labels dashboards group by.
    readers = cfg["service"].get("telemetry", {}).get("metrics", {}).get("readers", [])
    prom = readers[0].get("pull", {}).get("exporter", {}).get("prometheus", {}) if len(readers) == 1 else {}
    if prom.get("host") != "localhost" or prom.get("port") != 8888:
        errors.append("self metrics must use one pull reader on localhost:8888")
    included = set(prom.get("with_resource_constant_labels", {}).get("included", []))
    if not {"service.name", "service.instance.id", "service.version"} <= included:
        errors.append("self metrics must keep service.name, service.instance.id and service.version labels")
    return errors


def main() -> int:
    path = sys.argv[1] if len(sys.argv) > 1 else "otelcol/config.yaml"
    with open(path) as fh:
        errors = check(fh.read())
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    if not errors:
        print(f"{path}: collector config checks passed")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
