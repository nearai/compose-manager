# Docker Compose Manager

A minimalistic Rust HTTP service that manages Docker Compose deployments from a GitHub repository.

## Features

- **Remote deployment control**: Start and stop Docker Compose services via HTTP API
- **Multiple compose files**: Specify which compose file to use per request
- **Git tag checkout**: Checkout specific repository tags with configurable age validation
- **Docker cleanup**: Prune unused volumes and images to save disk space
- **Bearer token authentication**: Secure all endpoints with a shared secret

## API Endpoints

### POST /compose/up
Start containers with `docker compose up -d`.

**Request body (optional):**
```json
{"file": "docker-compose.prod.yml"}
```

### POST /compose/down
Stop containers with `docker compose down`.

**Request body (optional):**
```json
{"file": "docker-compose.prod.yml", "volumes": true}
```

- `file`: Specify compose file (optional)
- `volumes`: Also remove volumes with `-v` flag (default: false)

### POST /docker/clean
Prune unused Docker resources.

**Request body:**
```json
{"volumes": true, "images": true}
```

- `volumes`: Prune unused volumes (default: false)
- `images`: Prune all unused images (default: false)

At least one option must be true.

### POST /docker/evict
Selectively evict ONE model's weights/cache from the shared HuggingFace cache
volume, leaving every other model's warm cache intact. This is the targeted
alternative to `/docker/clean`'s blind `docker volume prune -f`, and mirrors the
`cleanup-hf-model.yaml` fleet maintenance pattern as a first-class API.

**Request body:**
```json
{"model": "zai-org/GLM-5.2-FP8", "target": "weights", "cache_volume": "huggingface_cache"}
```

- `model` (required): HF repo id `org/repo`. Mapped to the on-disk
  `hub/models--org--repo` directory.
- `target` (default `weights`): one of `weights`, `cache`, `both`.
  - `weights`: remove `hub/models--org--repo`.
  - `cache`: best-effort removal of model-named compile/kernel cache subdirs.
    Compile caches (torchinductor/triton/deep_gemm) are hash-keyed and NOT
    cleanly attributable to a single model, so `cache` only clears model-named
    subdirs if they happen to exist — it never touches the whole cache.
  - `both`: weights + best-effort caches.
- `cache_volume` (optional): override the autodetected volume name. By default
  the volume is autodetected from `docker volume ls`, handling the correct
  `huggingface_cache`, the known typo `hugginface_cache`, and any
  project-prefixed form (e.g. `small-models_hugginface_cache`).

**Safety guard:** when `target` includes weights, the request is rejected with
`409 Conflict` if a RUNNING container is actively serving the SAME model
(matched on an exact `--model-path`/`--model` arg value or a `MODEL_NAME`-style
env value). Evicting a *different* model's weights while others run is allowed —
that is the point of being selective. If the running set can't be determined the
request fails closed (500) rather than deleting.

Only that one model's subtree is ever removed; the volume itself is never
pruned. A missing volume returns `404` and a missing model dir returns a clear
"nothing to evict" success rather than an error.

### GET /version
Returns the currently deployed tag.

**Response:**
```json
{"status": "ok", "tag": "v1.0.0"}
```

Post-#46 a CVM can run N Compose projects, so `/version` ALSO returns an additive
per-project `projects` map alongside the legacy top-level tuple. The top-level
`tag`/`commit`/`file`/`file_sha256` fields are unchanged (they mirror the
last-written project's `current`), so single-project CVMs and existing callers
see no shape change. `projects` is omitted entirely when empty.

```json
{
  "status": "ok",
  "tag": "v0.0.211",
  "commit": "abc123",
  "file": "GLM-5.1.yaml",
  "file_sha256": "…",
  "projects": {
    "glm-5-1": {
      "current":  {"tag": "v0.0.211", "commit": "abc123", "file": "GLM-5.1.yaml", "file_sha256": "…"},
      "previous": {"tag": "v0.0.210", "commit": "…",      "file": "GLM-5.1.yaml", "file_sha256": "…"}
    }
  }
}
```

- A project's `current` is its last-SUCCEEDED `compose_up`; `previous` is the
  last-known-good before it (the rollback target). `previous` is absent until the
  first successful re-deploy rotates a `current` into it.
- A FAILED `compose_up` never rotates state. `compose_down` deliberately LEAVES
  `current` intact — teardown is observed via `/docker/ps`, not by clearing
  deployed state. `materialize_only` (`compose_stage`) activates nothing and so
  never touches deployed state.
- A pre-existing single-tuple `deployed.json` migrates into `projects["work"]`
  with `previous` absent.

### GET /status
Returns the currently running mutating Docker/Compose operation, if any.

**Idle response:**
```json
{"status": "ok", "in_flight": null}
```

**Busy response:**
```json
{
  "status": "ok",
  "in_flight": {
    "action": "compose_down",
    "started_at": "2026-06-17T14:32:00.123456+00:00",
    "tag": "v1.0.0",
    "file": "docker-compose.prod.yml",
    "services": ["api"]
  }
}
```

### GET /host/gpu
Read-only per-GPU allocation map for this host, for the control plane to plan GPU usage. Never mutates state and never takes the compose lock.

The authoritative source is Docker: for each running container, the handler reads the NVIDIA GPU reservation (`HostConfig.DeviceRequests[*].DeviceIDs` and the `NVIDIA_VISIBLE_DEVICES` env) and maps each numeric GPU index to the containers claiming it. Non-numeric device IDs (UUIDs) and `all` are ignored since they can't be mapped to a GPU index.

Per-GPU `memory_total_mb` / `memory_used_mb` / `utilization_pct` are a best-effort overlay from `nvidia-smi --query-gpu=index,memory.total,memory.used,utilization.gpu --format=csv,noheader,nounits` (tried directly first, then via `nsenter` into PID 1). If `nvidia-smi` is unreachable both ways, those fields are simply omitted — the endpoint still returns the Docker-derived allocation and never 500s.

**Response:**
```json
{"gpus": [
  {"index": 0, "memory_total_mb": 81920, "memory_used_mb": 41234, "utilization_pct": 92, "claimed_by": ["vllm"]},
  {"index": 1, "claimed_by": []}
]}
```

### GET /host/cache
Read-only state of model-weight and kernel caches on this host, for deciding what weights/caches to pre-stage. Never mutates state.

Enumerates Docker volumes whose names contain a cache marker (`huggingface_cache`, `hugginface_cache` (known fleet typo), `vllm_cache`, `compile_cache`, `kernel_cache`, `deep_gemm`). Per-volume `size_bytes` is a best-effort parse of `docker system df -v` (omitted if unparseable). `weights` lists the `models--*` entries under `hub/` in the HuggingFace cache volume(s), read via a throwaway `docker run --rm -v <vol>:/v:ro alpine ls /v/hub`. Every step degrades gracefully — a missing volume or unparseable size just drops that field/entry.

**Response:**
```json
{
  "volumes": [
    {"name": "huggingface_cache", "size_bytes": 120000000000},
    {"name": "vllm_cache"}
  ],
  "weights": ["models--Qwen--Qwen2.5-7B", "models--meta-llama--Llama-3.1-8B"]
}
```

### POST /dstack-agent/:action
Manage the `dstack-guest-agent.service` running on the CVM host. Supported actions: `start`, `stop`, `restart`, `status`.

The handler runs `nsenter -t 1 -m -u -i -n -p -- systemctl <action> dstack-guest-agent.service`, so the container must have `pid: host` and `CAP_SYS_ADMIN` (already set in the bundled compose templates). Each call has a 120s timeout to bound the worst case (a stuck unit's `TimeoutStopSec` is typically 90s).

**Examples:**
```bash
# Restart the dstack guest agent (e.g. to retry a stuck TDX quote attempt)
curl -X POST http://localhost:8080/dstack-agent/restart \
  -H "Authorization: Bearer your-secret-token"

# Check status
curl -X POST http://localhost:8080/dstack-agent/status \
  -H "Authorization: Bearer your-secret-token"
```

**Response:** standard JSON with `output` (combined stdout+stderr) and `exit_code` (the systemctl exit). For `status`, useful exit codes are `0` (active), `3` (inactive), `4` (unit not found).

**HTTP status codes:**
- `200` — `status` always returns 200 on a successful systemctl invocation, regardless of the unit's active state. `start`/`stop`/`restart` return 200 only when systemctl exits 0.
- `400` — invalid action.
- `401` — missing/invalid bearer.
- `500` — infrastructure error (nsenter missing, missing capability, timeout) **or** non-zero systemctl exit on `start`/`stop`/`restart`. The error body always includes the exit code (or `signal`) and combined output.

**Side effects on attestation:** `start`/`stop`/`restart` (whether successful or not) append a `dstack_agent_<action>` entry to the deployment action log included in `/v1/attestation/report`. `status` is read-only and is not logged.

**Caveat — self-attestation gap:** restart briefly takes the dstack guest agent offline. While the agent is down, `/v1/attestation/report` will fail with `dstack unavailable: Connection refused` because compose-manager fetches its TDX quote from the same agent. The window is typically 1–10s; clients should retry attestation after a restart.

### POST /git/checkout
Checkout a specific git tag.

**Request body:**
```json
{"tag": "v1.0.0"}
```

**Validation:** The tag's commit must be at least `MIN_TAG_AGE_HOURS` old (default: 48 hours).

## Configuration

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `GITHUB_REPO` | Yes | - | GitHub repository URL (e.g., `https://github.com/owner/repo`) |
| `BEARER_TOKEN` | Yes | - | Bearer token for authenticating requests |
| `WORK_DIR` | No | `/app/work` | Directory for downloaded compose files |
| `MIN_TAG_AGE_HOURS` | No | `48` | Minimum tag age in hours before checkout is allowed |

## Usage

### Running locally

```bash
export GITHUB_REPO="https://github.com/owner/repo"
export BEARER_TOKEN="your-secret-token"
export WORK_DIR="/tmp/work"
export MIN_TAG_AGE_HOURS="0"  # Optional: disable age check for testing

cargo run --release
```

### API Examples

```bash
# Checkout a tag
curl -X POST http://localhost:8080/git/checkout \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"tag": "v1.0.0"}'

# Start containers (default compose file)
curl -X POST http://localhost:8080/compose/up \
  -H "Authorization: Bearer your-secret-token"

# Start containers (specific compose file)
curl -X POST http://localhost:8080/compose/up \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"file": "docker-compose.prod.yml"}'

# Stop containers
curl -X POST http://localhost:8080/compose/down \
  -H "Authorization: Bearer your-secret-token"

# Stop containers and remove volumes
curl -X POST http://localhost:8080/compose/down \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"volumes": true}'

# Clean up unused volumes and images
curl -X POST http://localhost:8080/docker/clean \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"volumes": true, "images": true}'
```

### Docker Compose

```yaml
services:
  compose-manager:
    image: ${DOCKER_REGISTRY_USER}/compose-manager:latest
    restart: unless-stopped
    ports:
      - "8080:8080"
    environment:
      GITHUB_REPO: "https://github.com/owner/repo"
      BEARER_TOKEN: "${BEARER_TOKEN}"
      WORK_DIR: "/app/work"
      MIN_TAG_AGE_HOURS: "48"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
      - work:/app/work

volumes:
  work:
```

### Docker

```bash
docker build -t compose-manager .

docker run -d \
  -e GITHUB_REPO="https://github.com/owner/repo" \
  -e BEARER_TOKEN="your-secret-token" \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -p 8080:8080 \
  compose-manager
```

## Response Format

**Success:**
```json
{"status": "ok"}
```

**Success (checkout):**
```json
{"status": "ok", "tag": "v1.0.0"}
```

**Error:**
```json
{"status": "error", "error": "error message"}
```

## HTTP Status Codes

| Code | Description |
|------|-------------|
| 200 | Success |
| 400 | Bad request (tag not found, tag too recent) |
| 401 | Unauthorized (missing or invalid token) |
| 500 | Internal server error (git/docker command failed) |

## License

MIT
