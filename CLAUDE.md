# CLAUDE.md

## Project Overview

Docker Compose Manager is a Rust HTTP service that provides remote control over Docker Compose deployments. It clones a GitHub repository, checks out specific tags, and runs docker compose commands.

## Architecture

Single-file Rust application (`src/main.rs`) using:
- **axum** - HTTP framework
- **tokio** - Async runtime
- **reqwest** - GitHub API client
- **chrono** - Date/time handling for tag age validation

## Key Components

1. **AppState** - Holds configuration (repo URL, token, paths, parsed owner/repo)
2. **Authentication** - `verify_bearer_token()` validates Authorization header
3. **Handlers** - `compose_up`, `compose_down`, `git_checkout`
4. **GitHub API** - `get_tag_commit_sha()` and `get_commit_date()` for tag validation
5. **Shell commands** - `run_docker_compose()`, `run_git()`, `run_git_clone()`

## Build & Run

```bash
cargo build --release
GITHUB_REPO="..." BEARER_TOKEN="..." ./target/release/compose-manager
```

## Testing

Set `MIN_TAG_AGE_HOURS=0` to bypass tag age validation during development.

## Docker

The Dockerfile uses a multi-stage build with `rust:1.75-slim` builder and `debian:bookworm-slim` runtime. Runtime needs git and docker-compose-plugin installed.
