"""Check that registry-to-registry image copies in every workflow use --all.

Images are pushed as an index (image plus attestation manifests). Without
--all, skopeo copies a single platform manifest, so the copy gets a different
digest than the image that was signed. Run:
python3 .github/test/skopeo_copy_all_test.py
"""

import re
import sys
from pathlib import Path

import yaml

WORKFLOWS = Path(__file__).resolve().parent.parent / "workflows"
# Only copies whose source is a registry image (the oci-archive copy is exempt).
COPY = re.compile(r'skopeo copy(?P<args>(?:\s+-\S+)*)\s+"docker://')


def check(text: str) -> list[str]:
    errors = []
    for number, line in enumerate(text.splitlines(), start=1):
        match = COPY.search(line)
        if match and "--all" not in match.group("args").split():
            errors.append(f"line {number}: skopeo copy from a registry without --all")
    return errors


def main() -> int:
    failed = False
    for path in sorted(WORKFLOWS.glob("*.yml")):
        text = path.read_text()
        yaml.safe_load(text)
        for error in check(text):
            print(f"{path.name}: {error}", file=sys.stderr)
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
