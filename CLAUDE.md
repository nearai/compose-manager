# CLAUDE.md

## Project Overview

Docker Compose Manager is a Rust HTTP service that provides remote control over Docker Compose deployments. It fetches compose files directly from GitHub (by tag) and runs docker compose commands.

## Architecture

Single-file Rust application (`src/main.rs`) using:
- **axum** - HTTP framework
- **tokio** - Async runtime
- **reqwest** - GitHub API client + raw file fetching
- **chrono** - Date/time handling for tag age validation

## Key Components

1. **AppState** - Holds configuration (token, owner/repo, work dir) and current tag (`RwLock`)
2. **Authentication** - `verify_bearer_token()` validates Authorization header
3. **Handlers** - `compose_up`, `compose_down`, `docker_clean`, `git_checkout`, `version`
4. **GitHub** - `get_tag_commit_date()` for tag validation, `fetch_github_file()` to download compose files from `raw.githubusercontent.com`
5. **Shell commands** - `run_docker_compose()`, `run_docker_prune()`

## Flow

1. `POST /git/checkout {"tag": "v1.0"}` — validates tag age via GitHub API, stores tag
2. `POST /compose/up {"file": "docker-compose.yml"}` — fetches file from GitHub at stored tag, writes to work dir, runs `docker compose up -d`
3. `POST /compose/down` — runs `docker compose down` in work dir
4. `POST /docker/clean {"volumes": true, "images": true}` — prunes Docker resources
5. `GET /version` — returns currently deployed tag

## Build & Run

```bash
cargo build --release
GITHUB_REPO="..." BEARER_TOKEN="..." ./target/release/compose-manager
```

## Environment Variables

- `GITHUB_REPO` (required) — GitHub repo URL (e.g. `https://github.com/owner/repo`)
- `BEARER_TOKEN` (required) — Auth token for API requests
- `WORK_DIR` (default: `/app/work`) — Directory for downloaded compose files
- `MIN_TAG_AGE_HOURS` (default: `48`) — Minimum tag age; set to `0` for development

## Docker

Multi-stage build: `rust:1.75-slim` builder, `debian:bookworm-slim` runtime. Runtime needs docker-compose-plugin (no git required).

## CI/CD

GitHub Actions workflow (`.github/workflows/build.yml`) builds a reproducible Docker image using `build-image.sh` and pushes to Docker Hub. Triggers on pushes to `master` (tagged `latest`) and `v*` tags (tagged with version number). Includes artifact attestation via Sigstore.
