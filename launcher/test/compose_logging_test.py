"""Check the logging options of compose-manager in launcher/docker-compose.yml.

The launcher recreates compose-manager from this file, replacing the container
the CVM app compose started. Its logging options must stay the same as the app
compose gives the other management containers: the log collector reads the
service name from the labels the json-file driver copies into each log line,
and drops lines without it. Run: python3 launcher/test/compose_logging_test.py
"""

import sys
from pathlib import Path

import yaml

COMPOSE = Path(__file__).resolve().parent.parent / "docker-compose.yml"

EXPECTED = {
    "driver": "json-file",
    "options": {
        "max-size": "100m",
        "max-file": "10",
        "labels": "com.datadoghq.ad.logs,com.docker.compose.service",
    },
}


def check(compose: dict) -> list[str]:
    errors = []
    service = compose.get("services", {}).get("compose-manager", {})
    logging = service.get("logging")
    if logging != EXPECTED:
        errors.append(f"compose-manager logging is {logging!r}, expected {EXPECTED!r}")
    # Every label named in the labels option must be on the container; Compose
    # adds com.docker.compose.* itself.
    labels = service.get("labels") or {}
    for name in EXPECTED["options"]["labels"].split(","):
        if not name.startswith("com.docker.compose.") and name not in labels:
            errors.append(f"compose-manager has no {name} label for the log driver to copy")
    return errors


def main() -> int:
    errors = check(yaml.safe_load(COMPOSE.read_text()))
    for error in errors:
        print(f"FAIL: {error}")
    if not errors:
        print(f"PASS: {COMPOSE.name} compose-manager logging options")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
