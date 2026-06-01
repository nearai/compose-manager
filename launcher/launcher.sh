#!/usr/bin/env bash
set -euo pipefail

# ── Configuration (all from environment) ─────────────────────────────

IMAGE_REPO="${COMPOSE_MANAGER_IMAGE_REPO:?COMPOSE_MANAGER_IMAGE_REPO is required}"
CHANNEL="${LAUNCHER_CHANNEL:-latest}"
POLL_INTERVAL="${LAUNCHER_POLL_INTERVAL:-300}"
COMPOSE_PROJECT="${LAUNCHER_COMPOSE_PROJECT:-dstack}"
# Base env file: the dstack decrypted env, mounted read-only into the launcher.
# Passed to `docker compose --env-file` so secrets (BEARER_TOKEN, etc.) are
# available for substitution in the bundled compose file without the launcher
# ever holding those values in its own environment.
BASE_ENV_FILE="${LAUNCHER_BASE_ENV_FILE:-/dstack-env}"
# Override env file: written by the launcher to record the current image digest.
# Also sourced by the CVM prelaunch script so reboots don't revert the image.
ENV_FILE="${LAUNCHER_ENV_FILE:-/app/work/.env.launcher}"
# Compose file path: launcher copies the bundled file here on first start.
COMPOSE_FILE="${LAUNCHER_COMPOSE_FILE:-/app/work/compose-manager.yml}"
HEALTH_URL="${LAUNCHER_HEALTH_URL:-http://127.0.0.1:8080/version}"
HEALTH_TIMEOUT="${LAUNCHER_HEALTH_TIMEOUT:-60}"
STATE_FILE="${LAUNCHER_STATE_FILE:-/app/work/launcher-state.json}"
COSIGN_IDENTITY="${LAUNCHER_COSIGN_IDENTITY_REGEXP:-}"
COSIGN_ISSUER="${LAUNCHER_COSIGN_ISSUER:-https://token.actions.githubusercontent.com}"

POLL_CYCLE=0

# ── Logging ───────────────────────────────────────────────────────────

log()       { echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] [launcher] $*"; }
log_error() { echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] [launcher] ERROR: $*" >&2; }

# ── State management ──────────────────────────────────────────────────

init_state() {
    mkdir -p "$(dirname "$STATE_FILE")"
    if [ ! -f "$STATE_FILE" ]; then
        echo '{}' > "$STATE_FILE"
        log "Created state file at ${STATE_FILE}"
    else
        log "Loaded state file from ${STATE_FILE}"
    fi
}

read_state() {
    jq -r ".[\"$1\"] // empty" "$STATE_FILE" 2>/dev/null || echo ""
}

write_state() {
    local tmp="${STATE_FILE}.tmp"
    jq --arg k "$1" --arg v "$2" '.[$k] = $v' "$STATE_FILE" > "$tmp" && mv "$tmp" "$STATE_FILE"
}

# ── Env file helpers ──────────────────────────────────────────────────

write_env_var() {
    local key="$1" value="$2"
    touch "$ENV_FILE"
    if grep -q "^${key}=" "$ENV_FILE" 2>/dev/null; then
        local tmp="${ENV_FILE}.tmp"
        # Use awk instead of sed: sed's s/// misinterprets |, \, & in the replacement
        # string, which would corrupt values like image refs containing @sha256:...
        awk -v k="$key" -v v="$value" \
            'index($0, k "=") == 1 { print k "=" v; next } { print }' \
            "$ENV_FILE" > "$tmp" && mv "$tmp" "$ENV_FILE"
    else
        printf '%s=%s\n' "$key" "$value" >> "$ENV_FILE"
    fi
}

# ── Registry helpers ──────────────────────────────────────────────────

parse_image() {
    local image="$1" registry repo
    if [[ "$image" == *"/"*"/"* ]]; then
        registry="${image%%/*}"; repo="${image#*/}"
    elif [[ "$image" == *"/"* ]]; then
        local first="${image%%/*}"
        if [[ "$first" == *.* ]] || [[ "$first" == *:* ]] || [[ "$first" == "localhost" ]]; then
            registry="$first"; repo="${image#*/}"
        else
            registry="registry-1.docker.io"; repo="$image"
        fi
    else
        registry="registry-1.docker.io"; repo="library/$image"
    fi
    [ "$registry" = "docker.io" ] && registry="registry-1.docker.io"
    echo "$registry" "$repo"
}

get_auth_token() {
    local repo="$1"
    local username="${DOCKER_REGISTRY_USER:-}" password="${DOCKER_REGISTRY_TOKEN:-}"
    local curl_args=(-fsSL "https://auth.docker.io/token?service=registry.docker.io&scope=repository:${repo}:pull")
    [ -n "$username" ] && [ -n "$password" ] && curl_args=(-u "${username}:${password}" "${curl_args[@]}")
    curl "${curl_args[@]}" | jq -r '.token'
}

fetch_remote_digest() {
    local image="$1" tag="$2"
    local registry repo token digest
    read -r registry repo <<< "$(parse_image "$image")"

    if [ "$registry" = "registry-1.docker.io" ]; then
        token="$(get_auth_token "$repo")"
        digest="$(curl -fsSL \
            -H "Authorization: Bearer $token" \
            -H "Accept: application/vnd.docker.distribution.manifest.v2+json" \
            -H "Accept: application/vnd.oci.image.index.v1+json" \
            --head \
            "https://${registry}/v2/${repo}/manifests/${tag}" 2>/dev/null \
            | grep -i 'docker-content-digest' | awk '{print $2}' | tr -d '\r')"
    else
        digest="$(curl -fsSL \
            -H "Accept: application/vnd.docker.distribution.manifest.v2+json" \
            -H "Accept: application/vnd.oci.image.index.v1+json" \
            --head \
            "https://${registry}/v2/${repo}/manifests/${tag}" 2>/dev/null \
            | grep -i 'docker-content-digest' | awk '{print $2}' | tr -d '\r')"
    fi

    echo "$digest"
}

# ── Digest validation ────────────────────────────────────────────────

valid_digest() {
    [[ "$1" =~ ^sha256:[0-9a-f]{64}$ ]]
}

# ── Cosign verification ───────────────────────────────────────────────

verify_attestation() {
    local image_ref="$1"

    if [ "${LAUNCHER_SKIP_VERIFY:-0}" = "1" ]; then
        log "SKIP_VERIFY: skipping cosign verification for ${image_ref}"
        return 0
    fi

    if [ -z "$COSIGN_IDENTITY" ]; then
        log_error "LAUNCHER_COSIGN_IDENTITY_REGEXP is not set"
        return 1
    fi

    log "Verifying cosign signature for ${image_ref}..."
    if cosign verify \
        --certificate-identity-regexp="$COSIGN_IDENTITY" \
        --certificate-oidc-issuer="$COSIGN_ISSUER" \
        "$image_ref" > /dev/null 2>&1; then
        log "Signature verified"
    else
        log_error "Signature verification FAILED for ${image_ref}"
        return 1
    fi
}

# ── Health check ──────────────────────────────────────────────────────

wait_for_healthy() {
    local timeout="$1" elapsed=0 interval=3
    log "Waiting up to ${timeout}s for ${HEALTH_URL}..."
    while [ "$elapsed" -lt "$timeout" ]; do
        if curl -fsSL --max-time 5 "$HEALTH_URL" > /dev/null 2>&1; then
            log "Health check passed after ${elapsed}s"
            return 0
        fi
        sleep "$interval"
        elapsed=$((elapsed + interval))
    done
    log_error "Health check failed after ${timeout}s"
    return 1
}

# ── Compose helper ────────────────────────────────────────────────────

compose_up() {
    local env_file_args=()
    [ -f "$BASE_ENV_FILE" ] && env_file_args+=(--env-file "$BASE_ENV_FILE")
    [ -f "$ENV_FILE" ]      && env_file_args+=(--env-file "$ENV_FILE")
    docker compose -p "$COMPOSE_PROJECT" -f "$COMPOSE_FILE" "${env_file_args[@]}" up "$@"
}

# ── Docker Hub login ──────────────────────────────────────────────────

docker_login() {
    local username="${DOCKER_REGISTRY_USER:-}" password="${DOCKER_REGISTRY_TOKEN:-}"
    if [ -z "$username" ] || [ -z "$password" ]; then
        log "No Docker Hub credentials configured, pulling anonymously (rate limits apply)"
        return 0
    fi
    if printf '%s' "$password" | docker login -u "$username" --password-stdin; then
        log "Docker Hub login successful"
    else
        log_error "Docker Hub login failed — will pull anonymously"
    fi
}

# ── Rollback ──────────────────────────────────────────────────────────

rollback() {
    local old_image="$1"
    if [ -z "$old_image" ]; then
        log_error "No previous image to roll back to — manual intervention required"
        return 1
    fi
    log "Rolling back to ${old_image}..."
    write_env_var "COMPOSE_MANAGER_IMAGE" "$old_image"
    compose_up -d --no-deps compose-manager || true
    if wait_for_healthy "$HEALTH_TIMEOUT"; then
        log "Rollback successful"
    else
        log_error "CRITICAL: Rollback also failed — manual intervention required"
    fi
    local backoff_epoch; backoff_epoch=$(( $(date +%s) + 1800 ))
    write_state "backoff_until_epoch" "$backoff_epoch"
    log "Entering 30-minute backoff (until epoch ${backoff_epoch})"
}

# ── Bootstrap ─────────────────────────────────────────────────────────

bootstrap() {
    log "Bootstrap: copying bundled compose file to ${COMPOSE_FILE}..."
    mkdir -p "$(dirname "$COMPOSE_FILE")"
    cp /app/docker-compose.yml "$COMPOSE_FILE"
    touch "$ENV_FILE"

    # Seed the current image digest from the running container so the first
    # poll cycle has a baseline and doesn't re-deploy an already-current image.
    if [ -z "$(read_state "compose_manager_digest")" ]; then
        local current
        current="$(docker inspect compose-manager --format '{{.Image}}' 2>/dev/null || echo "")"
        if [ -n "$current" ]; then
            write_state "compose_manager_digest" "$current"
            write_env_var "COMPOSE_MANAGER_IMAGE" "$current"
            log "Seeded digest from running container: ${current}"
        else
            log "No running compose-manager container — will deploy on first poll"
        fi
    else
        log "Existing digest in state: $(read_state "compose_manager_digest")"
        # Ensure the env file reflects the state (may have been wiped on container restart)
        local stored; stored="$(read_state "compose_manager_digest")"
        if ! grep -q "^COMPOSE_MANAGER_IMAGE=" "$ENV_FILE" 2>/dev/null; then
            write_env_var "COMPOSE_MANAGER_IMAGE" "${IMAGE_REPO}@${stored}"
        fi
    fi
}

# ── Poll cycle ────────────────────────────────────────────────────────

_poll_cycle() {
    POLL_CYCLE=$((POLL_CYCLE + 1))
    log "Poll cycle #${POLL_CYCLE} (${IMAGE_REPO}:${CHANNEL})"

    # Backoff check
    local backoff_until_epoch; backoff_until_epoch="$(read_state "backoff_until_epoch")"
    if [ -n "$backoff_until_epoch" ]; then
        if [ "$(date +%s)" -lt "$backoff_until_epoch" ]; then
            log "In backoff (until epoch ${backoff_until_epoch}), skipping"
            return 0
        else
            write_state "backoff_until_epoch" ""
            log "Backoff expired, resuming"
        fi
    fi

    local remote_digest
    remote_digest="$(fetch_remote_digest "$IMAGE_REPO" "$CHANNEL")"
    if [ -z "$remote_digest" ]; then
        log_error "Failed to fetch remote digest for ${IMAGE_REPO}:${CHANNEL}"
        return 1
    fi
    if ! valid_digest "$remote_digest"; then
        log_error "Remote digest has unexpected format: '${remote_digest}'"
        return 1
    fi

    local current_digest; current_digest="$(read_state "compose_manager_digest")"
    if [ "$remote_digest" = "$current_digest" ]; then
        log "Up to date (${remote_digest:0:19}...)"
        return 0
    fi

    log "New image detected: ${remote_digest} (current: ${current_digest:-unknown})"
    local image_ref="${IMAGE_REPO}@${remote_digest}"
    local old_image; old_image="$(grep "^COMPOSE_MANAGER_IMAGE=" "$ENV_FILE" 2>/dev/null | cut -d'=' -f2- || echo "")"

    log "Pulling ${image_ref}..."
    if ! docker pull "$image_ref"; then
        log_error "Pull failed for ${image_ref}"
        return 1
    fi

    if ! verify_attestation "$image_ref"; then
        log_error "Attestation failed — skipping update"
        return 1
    fi

    write_env_var "COMPOSE_MANAGER_IMAGE" "$image_ref"

    log "Applying update..."
    if ! compose_up -d --no-deps compose-manager; then
        log_error "docker compose up failed"
        rollback "$old_image"
        return 1
    fi

    if wait_for_healthy "$HEALTH_TIMEOUT"; then
        write_state "compose_manager_digest" "$remote_digest"
        write_state "last_update" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        log "Update successful: ${remote_digest}"
    else
        log_error "Health check failed after update"
        rollback "$old_image"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────

main() {
    log "Starting compose-manager-launcher"
    log "  image repo:  ${IMAGE_REPO}"
    log "  channel:     ${CHANNEL}"
    log "  poll:        ${POLL_INTERVAL}s"
    log "  project:     ${COMPOSE_PROJECT}"
    log "  compose:     ${COMPOSE_FILE}"
    log "  health:      ${HEALTH_URL}"
    log "  state:       ${STATE_FILE}"

    init_state
    docker_login
    bootstrap

    log "Entering poll loop"
    while true; do
        if ! _poll_cycle; then
            log_error "Poll cycle failed — will retry in ${POLL_INTERVAL}s"
        fi
        sleep "$POLL_INTERVAL"
    done
}

main "$@"
