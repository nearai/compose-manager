# Docker Compose Manager

A minimalistic Rust HTTP service that manages Docker Compose deployments from a GitHub repository.

## Features

- **Remote deployment control**: Start and stop Docker Compose services via HTTP API
- **Multiple compose files**: Specify which compose file to use per request
- **Git tag checkout**: Checkout specific repository tags with configurable age validation
- **Docker cleanup**: Prune unused volumes and images to save disk space
- **Bearer token authentication**: Secure all endpoints with one or more shared secrets, supporting
  gradual rotation (see [Authentication & token rotation](#authentication--token-rotation))
- **Runtime env overrides**: change `BEARER_TOKEN`, `CM_MIN_TAG_AGE_HOURS` and the telemetry
  `MONITORING_INGEST_TOKEN` through the launcher without a CVM restart (see
  [Changing compose-manager env without a CVM restart](#changing-compose-manager-env-without-a-cvm-restart))

## API Endpoints

### POST /compose/up
Start containers with `docker compose up -d`.

**Request body (optional):**
```json
{"file": "docker-compose.prod.yml"}
```

**`dry_run` (plan-only):** set `"dry_run": true` to PLAN a change without applying
it. compose-manager simulates ONLY the `up` phase with docker compose's global
`--dry-run`, so automation can see the per-service config-hash recreate verdict
(which `file_sha256` cannot predict) before SIGTERMing a model mid-warmup. It is
genuinely read-only: no action is recorded, no deployed state is written, and
nothing is pulled/built/spawned. All scoping fields (`tag`, `file`, `project`,
`services`, `env`, `force_recreate`) are honored so the plan reflects the EXACT
command a real apply with the same body would run.

The streamed NDJSON carries the RAW dry-run output lines (the documented
contract) plus a terminal additive `plan` event with a best-effort parsed
verdict:
```json
{"event":"plan","success":true,"exit_code":0,
 "plan":{"create":["glm-vllm-1"],"recreate":[],"remove":["orphan-1"],"unchanged":["glm-nginx-1"]}}
```
The parsed `plan` is ADVISORY and Compose-version-sensitive (it keys on
Compose's progress-writer status verbs); it is pinned to the
`docker-compose-plugin` version in the attested image
(`5.1.4-1~debian.12~bookworm`; see Dockerfile). Read the raw lines, not `plan`,
when you need a guarantee.

**`materialize_only` (stage without activating):** set `"materialize_only": true`
to pre-materialize a model's artifacts on the target host — pull images, run any
in-CVM `build:`, and download weights via the compose file's model-downloader
service — WITHOUT activating it. The old model keeps serving and GPU idle at
cutover is near-zero. compose-manager runs the `pull` + `build` phases and STOPS
before `up`.

- It records a DISTINCT lower-privilege `compose_stage` action (NOT compose_up)
  and never writes deployed state: staging activates nothing, so /version and
  the attested action log keep reporting the model that is actually running.
- Weight download uses the compose file's EXISTING model-downloader service,
  selected via the `services` field, with platform-controlled `HF_HUB_OFFLINE`
  passed in `env`. There is no separate downloader trigger.
- The stream ends with a terminal additive `staged` done-marker so the platform
  knows materialization finished and what landed:
  ```json
  {"event":"staged","tag":"v0.0.211","file":"GLM-5.1.yaml","file_sha256":"…",
   "images":["sha256:…","sha256:…"]}
  ```
  `images` is the output of `docker compose images -q` for the staged project
  (best-effort: empty if the probe fails — the marker still fires).

> STAGING PRECONDITION (zero-GPU-idle): `materialize_only` only guarantees the
> running model is untouched on CVMs where each model lives in its OWN
> non-reserved compose project. The default `work` project is shared and cannot
> be overridden to `work`/`dstack` (see `project`), so on a shared-project CVM a
> later `up` of the staged model can still recreate co-located services.

### POST /compose/down
Stop containers with `docker compose down`.

**Request body (optional):**
```json
{"file": "docker-compose.prod.yml", "volumes": true}
```

- `file`: Specify compose file (optional)
- `volumes`: Also remove volumes with `-v` flag (default: false)

### POST /compose/logs

Read bounded logs for the selected Compose services. Use the same `file`,
`project` and `services` as the deployment:

```json
{"file":"prod/preflight.yaml","project":"migration-preflight","services":["preflight"],"tail":15}
```

`project` is optional: omission preserves the working-directory project default.
Explicit projects use the same canonical-name validation and reserved-name
rejection as `/compose/up` and `/compose/down`. Invalid values return HTTP 400
before Docker runs. This endpoint is read-only and does not change deployment
state. Managers predating this field silently ignore it; upgrade before relying
on isolated-project logs (an empty result is not evidence of a healthy service).

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
| `BEARER_TOKEN` | Yes | - | Bearer token(s) for authenticating requests. Comma-separated to accept more than one at once (see [Authentication & token rotation](#authentication--token-rotation)) |
| `WORK_DIR` | No | `/app/work` | Directory for downloaded compose files |
| `MIN_TAG_AGE_HOURS` | No | `48` | Minimum tag age in hours before checkout is allowed |
| `TLS_MODE` | No | `container` | Where the TLS certificate comes from: `container`, `files` or `off` (see [TLS](#tls)) |
| `TLS_CERT_SERVICE` | No | `dstack/certbot` | `container` mode: compose `<project>/<service>` whose container holds the certificate |
| `TLS_CERT_DIR` | No | `/etc/letsencrypt/live/completions.near.ai` | `container` mode: directory with `fullchain.pem` and `privkey.pem` |
| `TLS_CERT_FILE`, `TLS_KEY_FILE` | `files` mode | - | PEM certificate chain and private key |

## TLS

Port 8080 serves both TLS and plain HTTP. Each connection is routed by its first byte: a TLS
ClientHello starts with `0x16`, which no HTTP request does. Remote clients should use
`https://`. Plain HTTP remains for local callers inside the CVM (the launcher health check, the
attestation fetch from the model proxies).

The certificate is the one the CVM already maintains: by default compose-manager copies
`fullchain.pem` and `privkey.pem` out of the certbot container with `docker cp -L`, so no extra
volume mount is needed. It is re-read every 6 hours to pick up renewals (every minute until the
first successful load). If no certificate can be loaded, or `TLS_MODE` is invalid, the port serves
plain HTTP only and logs an error; startup never fails because of TLS.

Requests that carry an `Authorization` header over plain HTTP are logged as
`plain-http authenticated request` with the peer address, so callers that still need to move to
`https://` show up in the logs.

```bash
curl https://<host-name>:8080/version
```

## Authentication & token rotation

Every endpoint (except where noted) requires an `Authorization: Bearer <token>` header. The
`BEARER_TOKEN` environment variable accepts a **comma-separated list** of tokens, all of which are
accepted — this allows rotating the shared secret without a hard cutover that would reject every
in-flight client at once. Tokens are compared in constant time, whitespace around each entry is
trimmed, and empty entries are ignored. At least one non-empty token is required; the process fails
to start otherwise. Token values are never logged — only the number of configured tokens.

The dashboard (`dashboard/`) has the identical convention for its own inbound API: set
`DASHBOARD_TOKEN` to a comma-separated list to accept more than one caller-facing token at a time.
This is separate from the per-instance `bearer_token` stored in the dashboard's instance config,
which is the single outbound token the dashboard sends *to* each compose-manager instance — that
field is unaffected by this rotation mechanism and still takes exactly one token per instance.

**Rotation flow:**

1. **Add** the new token as a second, comma-separated entry: `BEARER_TOKEN=old-token,new-token`
   (same for `DASHBOARD_TOKEN`), then redeploy/restart. Both tokens are now accepted.
2. **Move** clients (ansible inventory, dashboard instance config, CI secrets, etc.) over to the
   new token, one at a time, at whatever pace is safe for the deployment.
3. **Remove** the old token once nothing is using it any more: `BEARER_TOKEN=new-token`, then
   redeploy/restart.

> **Caution:** only ever combine tokens as `"old,new"` on an instance already running a binary
> built from this change. An older compose-manager/dashboard image has no comma-splitting logic and
> would treat `"old,new"` as one literal token, rejecting every client until it is rolled back or
> upgraded. Roll the new image out first, then start rotating tokens.

## Changing compose-manager env without a CVM restart

Container environments on the CVM are normally fixed when the CVM boots, so changing one used to
need a CVM upgrade (hours of model downtime). With the launcher, a few variables can instead be
changed at runtime: an authenticated API call writes an overrides file on the shared `work` volume,
and the launcher (`launcher/launcher.sh`) notices it within about 5 seconds and recreates **only**
the container that consumes the changed variable. No CVM restart, no model restart.

**Allowed keys** (anything else is rejected with `400`). Each key belongs to one target container:

<!-- allowed-keys:start -->
| Key | Target | Value rules |
|-----|--------|-------------|
| `BEARER_TOKEN` | `compose-manager` | Comma-separated token list, no whitespace, no empty entries (same format as the env var) |
| `CM_MIN_TAG_AGE_HOURS` | `compose-manager` | Non-negative integer (maps to `MIN_TAG_AGE_HOURS` in the container) |
| `MONITORING_INGEST_TOKEN` | `otelcol-management` | 16 to 512 characters from `A-Za-z0-9+/=_.~-` (no whitespace, quotes or commas) |
<!-- allowed-keys:end -->

Every value is also 1 to 4096 bytes of printable ASCII without a single quote. Values are never
logged, never returned by the API, and never written to the action log; only key names and sha256
hashes are.

| Target | Compose file | Health check after the recreate |
|--------|--------------|---------------------------------|
| `compose-manager` | the launcher's bundled `docker-compose.yml` (base env, then `.env.launcher`, then the overrides last) | `GET /version` answers within `LAUNCHER_HEALTH_TIMEOUT` |
| `otelcol-management` | the CVM app-compose, mounted at `LAUNCHER_APP_COMPOSE_FILE` (default `/dstack-app/docker-compose.yaml`); the base env plus a temporary `0600` env file holding only this target's lines | the container stays running and does not restart for `LAUNCHER_OTEL_STABLE_SECONDS` (default 15) |

```bash
# Set (keys merge into the existing overrides; "unset" removes keys)
curl -X PUT https://<host-name>:8080/admin/env \
  -H "Authorization: Bearer <current-token>" \
  -H "Content-Type: application/json" \
  -d '{"set": {"CM_MIN_TAG_AGE_HOURS": "0"}, "unset": []}'
# 202 {"status":"accepted","keys":["CM_MIN_TAG_AGE_HOURS"],"sha256":"<hex>","targets":["compose-manager"]}

# Rotate the telemetry ingest token (recreates only otelcol-management)
curl -X PUT https://<host-name>:8080/admin/env \
  -H "Authorization: Bearer <current-token>" \
  -H "Content-Type: application/json" \
  -d '{"set": {"MONITORING_INGEST_TOKEN": "<new-ingest-token>"}}'

# Revert a key to the value from the CVM env
curl -X PUT https://<host-name>:8080/admin/env \
  -H "Authorization: Bearer <current-token>" \
  -H "Content-Type: application/json" \
  -d '{"unset": ["MONITORING_INGEST_TOKEN"]}'

# Confirm it was applied
curl https://<host-name>:8080/admin/env -H "Authorization: Bearer <current-token>"
# {"status":"ok","launcher_reported":true,"targets":{
#   "compose-manager":{"keys":[...],"sha256":"<hex>","applied_sha256":"<hex>","pending":false,"error":null},
#   "otelcol-management":{"keys":[...],"sha256":"<hex>","applied_sha256":"<hex>","pending":false,"error":null}}}
```

`202` means the file was written, not that it is live. Poll `GET /admin/env`: a target's change is
applied when its `pending` is `false` and `applied_sha256` equals `sha256`. The per-target `sha256`
is a hash over that target's lines only. The compose-manager container restarts during its apply,
so a request made at that moment may fail to connect; retry.

**Failure and rollback.** If a target fails (not healthy, not stable, preflight rejected), the
launcher restores the **whole** overrides file from the last known good copy (or deletes it if there
is none), re-applies every container it had already recreated, and reports the reason in that
target's `error`. Because the rollback covers the whole file, send changes for one target per
request if they must succeed or fail independently. Preflight checks for every target run before
any container is recreated.

**Telemetry token caveats.** The collector keeps running when the ingest token is wrong (it only
logs 401s), so the stability check proves the container is up, not that the token is valid. Set
`LAUNCHER_TELEMETRY_PROBE_URL` to an ingest endpoint: before recreating, the launcher POSTs `{}` with
the new token (sent via a header file, never on a command line or in logs) and rejects the change on
HTTP 401 or 403. Any other result, including a network error, proceeds with a warning, so the probe
is a typo guard and not a connectivity gate.

`GET /admin/env` never returns values. Both endpoints require the bearer token and are audited as
`env_override_set` / `env_override_unset` entries (key names only). The overrides file is not
covered by the dstack app-compose attestation. The telemetry token is write-only ingest, and
`BEARER_TOKEN` changes require the current token.

### Rotating `BEARER_TOKEN` (two steps)

A request that sets `BEARER_TOKEN` must keep the token it was authenticated with in the new list,
otherwise it is rejected with `400` (it would lock you out). Rotation is therefore two calls:

```bash
# 1. Add the new token next to the current one, authenticated with the current token.
curl -X PUT https://<host-name>:8080/admin/env -H "Authorization: Bearer <current-token>" \
  -H "Content-Type: application/json" \
  -d '{"set": {"BEARER_TOKEN": "<current-token>,<new-token>"}}'
# wait until the compose-manager target shows pending=false, then move clients to <new-token>

# 2. Drop the old token, authenticated with the NEW token.
curl -X PUT https://<host-name>:8080/admin/env -H "Authorization: Bearer <new-token>" \
  -H "Content-Type: application/json" \
  -d '{"set": {"BEARER_TOKEN": "<new-token>"}}'
```

Unsetting `BEARER_TOKEN` is allowed and reverts to the CVM-env value. Do it only if the token you
are authenticating with is also valid there, or you lose access until you can use a CVM-env token.

### Rollout requirements and limits

- **Launcher image:** the launcher image is pinned in the CVM app-compose, so a host only gains this
  after a normal compose-manager CVM upgrade that bumps the launcher digest. Until then
  `GET /admin/env` reports `launcher_reported: false` and nothing is applied.
- **Telemetry target:** `otelcol-management` is not in the launcher's bundled compose; it lives in
  the CVM app-compose that dstack writes at `/dstack/docker-compose.yaml` (compose project
  `dstack`). The CVM template (cvm-ansible-playbooks, a separate change) must bind-mount that file
  read-only into the launcher at `/dstack-app/docker-compose.yaml` and set
  `LAUNCHER_TELEMETRY_PROBE_URL`; both reach a host with the same one-time compose-manager CVM
  upgrade. Until the mount exists the `otelcol-management` target reports an error ("app-compose is
  not mounted") and nothing is changed. The launcher recreates the container with
  `docker compose -p dstack -f <app-compose> --env-file <decrypted env> --env-file <temp file>
  up -d --no-deps otelcol-management`.
- **Reboot:** after a CVM reboot dstack recreates both containers from the app-compose without the
  overrides; the launcher re-applies the file once at startup (a no-op when nothing differs).
  Overrides also survive image updates and rollbacks of compose-manager, because the file is passed
  on every recreate.
- **Only substituted variables are overridable:** for the compose-manager target, exactly the
  variables substituted in `launcher/docker-compose.yml` (other than the image and repo plumbing).
  Adding a key means editing, together: that compose file or the app-compose consumer, the target
  arrays and `ALLOWED_OVERRIDE_KEYS` in `launcher/launcher.sh`, `ALLOWED_KEYS` and `KEY_TARGETS` in
  `src/env_overrides.rs`, the table above, and the tests; the parity tests fail if they drift.
- **Launcher tunables:** `LAUNCHER_ENV_OVERRIDES_FILE` (default `/app/work/env-overrides`),
  `LAUNCHER_ENV_WATCH_INTERVAL` (seconds between file checks, default `5`),
  `LAUNCHER_APP_COMPOSE_FILE`, `LAUNCHER_TELEMETRY_PROBE_URL`, `LAUNCHER_OTEL_STABLE_SECONDS`.
  Per-target state keys in `launcher-state.json` (suffix `_compose_manager` or
  `_otelcol_management`): `env_overrides_applied_sha256_*`, `env_overrides_applied_at_*`,
  `env_overrides_error_*`, `env_overrides_rejected_sha256_*`.

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
