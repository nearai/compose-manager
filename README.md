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
