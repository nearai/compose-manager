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

### GET /version
Returns the currently deployed tag.

**Response:**
```json
{"status": "ok", "tag": "v1.0.0"}
```

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
