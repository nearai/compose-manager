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
# compose-manager does TLS/cert, S3 and git work on startup, so a container
# recreate can take well over a minute to answer /version. 60s caused a real
# swap (or rollback) to be declared failed while the new container was still
# coming up; default to 180s.
HEALTH_TIMEOUT="${LAUNCHER_HEALTH_TIMEOUT:-180}"
STATE_FILE="${LAUNCHER_STATE_FILE:-/app/work/launcher-state.json}"
COSIGN_IDENTITY="${LAUNCHER_COSIGN_IDENTITY_REGEXP:-}"
COSIGN_ISSUER="${LAUNCHER_COSIGN_ISSUER:-https://token.actions.githubusercontent.com}"

# Runtime env overrides. The compose-manager API (PUT /admin/env) writes one
# file on the shared work volume; the launcher applies it by recreating ONLY
# the affected container(s), so settings can change without a CVM restart.
# Format: one KEY='VALUE' line per key (single quotes = literal in compose
# env-file syntax). Missing or empty file means no overrides. Values are
# secrets: they are never logged, only key names and a sha256 prefix.
#
# Each key belongs to one target (a container the launcher can recreate):
#   compose-manager     bundled $COMPOSE_FILE; health = $HEALTH_URL
#   otelcol-management  the CVM app-compose ($APP_COMPOSE_FILE, bind-mounted
#                       read-only by the CVM template); health = the container
#                       stays up and does not restart for a while
# State (applied/rejected/error) is tracked per target in the state file.
OVERRIDES_FILE="${LAUNCHER_ENV_OVERRIDES_FILE:-/app/work/env-overrides}"
OVERRIDES_LAST_GOOD="${OVERRIDES_FILE}.last-good"
# How often the (cheap) overrides hash check runs between image polls.
ENV_WATCH_INTERVAL="${LAUNCHER_ENV_WATCH_INTERVAL:-5}"
# CVM app-compose written by dstack at /dstack/docker-compose.yaml on the guest.
APP_COMPOSE_FILE="${LAUNCHER_APP_COMPOSE_FILE:-/dstack-app/docker-compose.yaml}"
# Optional: ingest endpoint used to sanity-check a new MONITORING_INGEST_TOKEN
# (HTTP 401/403 rejects it). The otel collector keeps running with a wrong
# token, so the stability check below cannot validate it.
TELEMETRY_PROBE_URL="${LAUNCHER_TELEMETRY_PROBE_URL:-}"
OTEL_STABLE_SECONDS="${LAUNCHER_OTEL_STABLE_SECONDS:-15}"
# Targets in apply order, and the keys each owns. Keep in sync with: the Rust
# KEY_TARGETS/ALLOWED_KEYS (src/env_overrides.rs), the README allowed-keys
# table, and for compose-manager the non-plumbing substitutions in
# launcher/docker-compose.yml. launcher/test/env_overrides_test.sh checks the
# parity.
OVERRIDE_TARGETS=(compose-manager otelcol-management)
TARGET_COMPOSE_MANAGER_KEYS=(BEARER_TOKEN CM_MIN_TAG_AGE_HOURS)
TARGET_OTELCOL_MANAGEMENT_KEYS=(MONITORING_INGEST_TOKEN)
ALLOWED_OVERRIDE_KEYS=(BEARER_TOKEN CM_MIN_TAG_AGE_HOURS MONITORING_INGEST_TOKEN)

# Scratch state for the current apply (reason text never contains a value).
APPLY_REASON=""
APPLY_TOUCHED=0
# Temp per-target env file for otelcol-management; removed after every compose
# call and by the EXIT trap.
OTEL_ENV_TMP=""

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

state_has_key() {
    jq -e --arg k "$1" 'has($k)' "$STATE_FILE" > /dev/null 2>&1
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
    # Runtime overrides go LAST so they win over the CVM env and the image
    # pin. Applies to every compose_up (image updates and rollback included)
    # so a recreate never silently drops them. A file that fails validation is
    # skipped rather than handed to docker compose.
    if [ -s "$OVERRIDES_FILE" ]; then
        if validate_overrides_file > /dev/null; then
            env_file_args+=(--env-file "$OVERRIDES_FILE")
        else
            log_error "Env overrides file is invalid — starting without it"
        fi
    fi
    docker compose -p "$COMPOSE_PROJECT" -f "$COMPOSE_FILE" ${env_file_args[@]+"${env_file_args[@]}"} up "$@"
}

# ── Env overrides ─────────────────────────────────────────────────────

target_keys() {
    case "$1" in
        compose-manager)    echo "${TARGET_COMPOSE_MANAGER_KEYS[*]}" ;;
        otelcol-management) echo "${TARGET_OTELCOL_MANAGEMENT_KEYS[*]}" ;;
    esac
}

# State keys are per target: env_overrides_<name>_<target with - as _>, where
# name is applied_sha256 | applied_at | error | rejected_sha256.
target_suffix() { echo "${1//-/_}"; }
tstate_read()   { read_state "env_overrides_${1}_$(target_suffix "$2")"; }
tstate_write()  { write_state "env_overrides_${1}_$(target_suffix "$2")" "$3"; }

# sha256 hex of the whole overrides file; empty string when missing or empty.
overrides_hash() {
    if [ -s "$OVERRIDES_FILE" ]; then
        sha256sum "$OVERRIDES_FILE" | cut -d' ' -f1
    fi
}

# The target's KEY='VALUE' lines from a file (default: the live file), sorted.
target_lines() {
    local file="${2:-$OVERRIDES_FILE}" k pattern=""
    [ -s "$file" ] || return 0
    for k in $(target_keys "$1"); do pattern="${pattern:+${pattern}|}${k}"; done
    { grep -E "^(${pattern})='" "$file" || true; } | LC_ALL=C sort
}

# Per-target hash: sha256 over that target's lines only; empty when it has none.
target_hash() {
    local lines
    lines="$(target_lines "$@")"
    [ -n "$lines" ] || return 0
    printf '%s\n' "$lines" | sha256sum | cut -d' ' -f1
}

# Comma-separated key names of a target (never values).
target_key_names() {
    target_lines "$@" | cut -d= -f1 | paste -sd, -
}

is_allowed_override_key() {
    local k
    for k in "${ALLOWED_OVERRIDE_KEYS[@]}"; do
        [ "$k" = "$1" ] && return 0
    done
    return 1
}

# Validates the overrides file. Prints a reason (never line content) on stdout
# and returns 1 when invalid. Every line must be KEY='VALUE' for an allowlisted
# key, printable ASCII only, no CR, no duplicate keys; MONITORING_INGEST_TOKEN
# values must be 16..512 characters of [A-Za-z0-9+/=_.~-].
validate_overrides_file() {
    local LC_ALL=C
    local line key value n=0 seen=" "
    local line_re="^([A-Z_][A-Z0-9_]*)='[^']*'\$"
    # (no {16,512} interval: regcomp caps repeat counts at 255 on musl/macOS)
    local ingest_re='^[A-Za-z0-9+/=_.~-]+$'
    [ -s "$OVERRIDES_FILE" ] || return 0
    while IFS= read -r line || [ -n "$line" ]; do
        n=$((n + 1))
        case "$line" in
            *[!\ -~]*) echo "line ${n}: contains non-printable or non-ASCII characters"; return 1 ;;
        esac
        if ! [[ "$line" =~ $line_re ]]; then
            echo "line ${n}: not in KEY='VALUE' form"
            return 1
        fi
        key="${BASH_REMATCH[1]}"
        if ! is_allowed_override_key "$key"; then
            echo "line ${n}: key ${key} is not allowed"
            return 1
        fi
        case "$seen" in
            *" ${key} "*) echo "line ${n}: duplicate key ${key}"; return 1 ;;
        esac
        seen="${seen}${key} "
        if [ "$key" = "MONITORING_INGEST_TOKEN" ]; then
            value="${line#*=\'}"
            value="${value%\'}"
            if [ "${#value}" -lt 16 ] || [ "${#value}" -gt 512 ] || ! [[ "$value" =~ $ingest_re ]]; then
                echo "line ${n}: ${key} value does not match the required format"
                return 1
            fi
        fi
    done < "$OVERRIDES_FILE"
    return 0
}

# Copy a file with mode 0600 via a temp file in the destination directory.
copy_private() {
    local src="$1" dst="$2" tmp="$2.tmp"
    ( umask 077; cp "$src" "$tmp" ) && chmod 600 "$tmp" && mv "$tmp" "$dst"
}

cleanup_otel_tmp() {
    [ -z "$OTEL_ENV_TMP" ] || rm -f "$OTEL_ENV_TMP"
    OTEL_ENV_TMP=""
}

# ── otelcol-management target ─────────────────────────────────────────

# Checks that must pass before anything is recreated; sets APPLY_REASON.
otel_preflight() {
    local file="$1" out line token code
    local base_args=()
    if [ ! -r "$APP_COMPOSE_FILE" ]; then
        APPLY_REASON="app-compose is not mounted at ${APP_COMPOSE_FILE}; the CVM template needs to bind-mount /dstack/docker-compose.yaml into the launcher (read-only)"
        return 1
    fi
    [ -f "$BASE_ENV_FILE" ] && base_args+=(--env-file "$BASE_ENV_FILE")
    # A failing `config` (unreadable or invalid file, docker error) is reported
    # separately from a readable file that lacks the service.
    if ! out="$(docker compose -p "$COMPOSE_PROJECT" -f "$APP_COMPOSE_FILE" ${base_args[@]+"${base_args[@]}"} config --services 2>/dev/null)"; then
        APPLY_REASON="could not read services from app-compose at ${APP_COMPOSE_FILE} (docker compose config failed); check the mounted file"
        return 1
    fi
    case $'\n'"${out}"$'\n' in
        *$'\n'otelcol-management$'\n'*) ;;
        *)
            APPLY_REASON="app-compose at ${APP_COMPOSE_FILE} does not define the otelcol-management service; the CVM template mount points at the wrong file"
            return 1
            ;;
    esac

    # Optional typo guard: only an explicit 401/403 from the ingest endpoint
    # rejects; anything else (including network errors) proceeds. The token is
    # passed to curl via a header file (process substitution), never in argv.
    [ -n "$TELEMETRY_PROBE_URL" ] || return 0
    line="$(target_lines otelcol-management "$file" | head -n1)"
    [ -n "$line" ] || return 0
    token="${line#*=\'}"
    token="${token%\'}"
    code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 10 -X POST \
        -H 'Content-Type: application/json' -d '{}' \
        -H @<(printf 'Authorization: Bearer %s\n' "$token") \
        "$TELEMETRY_PROBE_URL" 2>/dev/null)" || code=""
    token=""
    case "$code" in
        401|403)
            APPLY_REASON="telemetry probe rejected the new MONITORING_INGEST_TOKEN (HTTP ${code})"
            return 1
            ;;
        [0-9][0-9][0-9]) [ "$code" = "000" ] && log "WARNING: telemetry probe could not connect; proceeding" || log "Telemetry probe answered HTTP ${code}; proceeding" ;;
        *) log "WARNING: telemetry probe inconclusive; proceeding" ;;
    esac
    return 0
}

otel_inspect() {
    docker inspect --format '{{.State.Running}} {{.State.Restarting}} {{.RestartCount}}' otelcol-management 2>/dev/null
}

# The collector keeps running with a wrong ingest token (it only logs 401s), so
# this only proves the container is up and not crash-looping.
wait_for_otel_stable() {
    local state running restarting count base_count elapsed=0
    state="$(otel_inspect)" || state=""
    read -r running restarting base_count <<< "$state"
    if [ "$running" != "true" ] || [ "$restarting" != "false" ] || ! [[ "${base_count:-}" =~ ^[0-9]+$ ]]; then
        APPLY_REASON="otelcol-management is not running after the recreate"
        return 1
    fi
    log "Watching otelcol-management for ${OTEL_STABLE_SECONDS}s"
    while [ "$elapsed" -lt "$OTEL_STABLE_SECONDS" ]; do
        sleep 3
        elapsed=$((elapsed + 3))
        state="$(otel_inspect)" || state=""
        read -r running restarting count <<< "$state"
        if [ "$running" != "true" ] || [ "$restarting" != "false" ]; then
            APPLY_REASON="otelcol-management stopped or is restarting after the recreate"
            return 1
        fi
        if ! [[ "${count:-}" =~ ^[0-9]+$ ]] || [ "$count" -gt "$base_count" ]; then
            APPLY_REASON="otelcol-management restarted after the recreate (restart count increased)"
            return 1
        fi
    done
    return 0
}

# Recreates otelcol-management from the app-compose with the base env plus a
# temporary 0600 env file holding ONLY this target's lines (taken from $1).
# Neither .env.launcher nor the full overrides file is passed.
apply_otelcol_target() {
    local file="$1" rc dir envtmp
    local args=()
    dir="$(dirname "$OVERRIDES_FILE")"
    if ! envtmp="$(umask 077; mktemp "${dir}/.otel-env.XXXXXX")"; then
        APPLY_REASON="could not create a temporary env file in ${dir}"
        return 1
    fi
    OTEL_ENV_TMP="$envtmp"
    chmod 600 "$envtmp"
    target_lines otelcol-management "$file" > "$envtmp"
    [ -f "$BASE_ENV_FILE" ] && args+=(--env-file "$BASE_ENV_FILE")
    args+=(--env-file "$envtmp")
    APPLY_TOUCHED=1
    docker compose -p "$COMPOSE_PROJECT" -f "$APP_COMPOSE_FILE" "${args[@]}" up -d --no-deps otelcol-management && rc=0 || rc=$?
    cleanup_otel_tmp
    if [ "$rc" -ne 0 ]; then
        APPLY_REASON="docker compose up failed for otelcol-management"
        return 1
    fi
    wait_for_otel_stable
}

# ── Apply logic ───────────────────────────────────────────────────────

# Recreate one target from the overrides in file $2 (the live file or a
# snapshot of it). compose-manager uses compose_up (which reads the live
# file). Sets APPLY_REASON / APPLY_TOUCHED.
apply_target() {
    APPLY_REASON=""
    APPLY_TOUCHED=0
    case "$1" in
        compose-manager)
            APPLY_TOUCHED=1
            if compose_up -d --no-deps compose-manager && wait_for_healthy "$HEALTH_TIMEOUT"; then
                return 0
            fi
            APPLY_REASON="compose-manager did not come up healthy"
            return 1
            ;;
        otelcol-management)
            apply_otelcol_target "$2"
            ;;
    esac
}

# Checks run for every target before ANY container is recreated.
preflight_target() {
    APPLY_REASON=""
    case "$1" in
        otelcol-management) otel_preflight "$2" ;;
        *) return 0 ;;
    esac
}

# Should this target be applied now? Skips when its hash equals what was
# applied (unless forced and non-empty) or what was rejected. A rejected hash
# is forgotten once the file moves on, so the same value can be retried later
# (for example after the CVM template gains the app-compose mount).
target_needs_apply() {
    local t="$1" force="$2" hash applied rejected
    hash="$(target_hash "$t")"
    applied="$(tstate_read applied_sha256 "$t")"
    rejected="$(tstate_read rejected_sha256 "$t")"
    if [ -n "$rejected" ] && [ "$hash" != "$rejected" ]; then
        tstate_write rejected_sha256 "$t" ""
        rejected=""
    fi
    if [ -n "$hash" ] && [ "$hash" = "$rejected" ]; then
        return 1
    fi
    if [ "$hash" = "$applied" ]; then
        if [ "$force" = "force" ] && [ -n "$hash" ]; then
            return 0
        fi
        return 1
    fi
    return 0
}

# An invalid file is never applied; record why on every target (once).
record_invalid_overrides() {
    local hash="$1" reason="$2" t msg logged=0
    msg="invalid overrides file (sha256 ${hash:0:12}): ${reason}"
    for t in "${OVERRIDE_TARGETS[@]}"; do
        if [ "$(tstate_read error "$t")" != "$msg" ]; then
            [ "$logged" = 1 ] || log_error "Rejecting env overrides: ${msg}"
            logged=1
            tstate_write error "$t" "$msg"
        fi
    done
}

clear_invalid_errors() {
    local t
    for t in "${OVERRIDE_TARGETS[@]}"; do
        case "$(tstate_read error "$t")" in
            "invalid overrides file"*) tstate_write error "$t" "" ;;
        esac
    done
}

# Whole-file rollback: restore the last good overrides (or delete the file if
# there is none) and re-apply every target that was recreated this tick. Only
# done if the live file is still the one that failed; a newer file written
# meanwhile is left for the next watch step.
rollback_env_overrides() {
    local failed="$1" failed_hash="$2" reason="$3" tick_hash="$4" touched="$5"
    local t restored=0 critical="" short="${2:0:12}"
    [ -n "$short" ] || short="none"
    rm -f "${OVERRIDES_LAST_GOOD}.candidate"
    log_error "Env overrides for ${failed} (sha256 ${short}) failed: ${reason} — restoring the last known good overrides"
    if [ "$(overrides_hash)" = "$tick_hash" ]; then
        if [ -f "$OVERRIDES_LAST_GOOD" ]; then
            copy_private "$OVERRIDES_LAST_GOOD" "$OVERRIDES_FILE"
        else
            rm -f "$OVERRIDES_FILE"
        fi
        restored=1
        for t in "${OVERRIDE_TARGETS[@]}"; do
            case "$touched" in *" ${t} "*) ;; *) continue ;; esac
            if apply_target "$t" "$OVERRIDES_FILE"; then
                tstate_write applied_sha256 "$t" "$(target_hash "$t")"
                tstate_write applied_at "$t" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
                if [ "$t" != "$failed" ]; then
                    tstate_write error "$t" ""
                    tstate_write rejected_sha256 "$t" ""
                fi
                log "Restored previous env overrides for ${t}"
            else
                critical=" CRITICAL: ${t} unhealthy after restoring previous overrides (${APPLY_REASON})."
                log_error "CRITICAL: ${t} unhealthy after restoring previous env overrides — manual intervention required"
            fi
        done
    else
        log "A newer overrides file is present; leaving it for the next watch step"
    fi
    tstate_write rejected_sha256 "$failed" "$failed_hash"
    if [ "$restored" = 1 ]; then
        tstate_write error "$failed" "overrides (sha256 ${short}) failed: ${reason}; previous overrides restored.${critical}"
    else
        tstate_write error "$failed" "overrides (sha256 ${short}) failed: ${reason}; a newer overrides file is pending.${critical}"
    fi
}

# Applies the overrides file: for each target whose hash changed, recreate only
# that target's container. All preflight checks run before the first recreate.
# Any failure rolls back the WHOLE file (send changes for one target per
# request if they must succeed or fail independently). With "force", targets
# that have keys are re-applied even if the hash matches (used at bootstrap:
# after a CVM reboot dstack recreates the containers from the app-compose
# WITHOUT overrides). Independent of the image-update backoff.
apply_env_overrides() {
    local force="${1:-}" snap="${OVERRIDES_LAST_GOOD}.candidate"
    local tick_hash reason t hash
    local todo=()
    local failed="" failed_hash="" failed_reason="" touched=" "

    tick_hash="$(overrides_hash)"
    if [ -n "$tick_hash" ] && ! reason="$(validate_overrides_file)"; then
        record_invalid_overrides "$tick_hash" "$reason"
        return 0
    fi
    clear_invalid_errors

    for t in "${OVERRIDE_TARGETS[@]}"; do
        if target_needs_apply "$t" "$force"; then todo+=("$t"); fi
    done
    [ "${#todo[@]}" -gt 0 ] || return 0

    # Snapshot exactly what is being applied; it becomes last-good on success
    # even if the API rewrites the live file while we wait.
    if [ -n "$tick_hash" ]; then
        copy_private "$OVERRIDES_FILE" "$snap"
    else
        rm -f "$snap"
    fi

    for t in "${todo[@]}"; do
        if ! preflight_target "$t" "$snap"; then
            failed="$t"; failed_reason="$APPLY_REASON"; failed_hash="$(target_hash "$t" "$snap")"
            break
        fi
    done

    if [ -z "$failed" ]; then
        for t in "${todo[@]}"; do
            hash="$(target_hash "$t" "$snap")"
            if [ -n "$hash" ]; then
                log "Applying env overrides to ${t} (keys: $(target_key_names "$t" "$snap"), sha256 ${hash:0:12})"
            else
                log "Clearing env overrides on ${t} (reverting to the CVM env)"
            fi
            if apply_target "$t" "$snap"; then
                touched="${touched}${t} "
                tstate_write applied_sha256 "$t" "$hash"
                tstate_write applied_at "$t" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
                tstate_write error "$t" ""
                tstate_write rejected_sha256 "$t" ""
                log "Env overrides applied to ${t} (sha256 ${hash:0:12})"
            else
                [ "$APPLY_TOUCHED" = 1 ] && touched="${touched}${t} "
                failed="$t"; failed_reason="$APPLY_REASON"; failed_hash="$hash"
                break
            fi
        done
    fi

    if [ -n "$failed" ]; then
        rollback_env_overrides "$failed" "$failed_hash" "$failed_reason" "$tick_hash" "$touched"
        return 0
    fi

    if [ -n "$tick_hash" ]; then
        mv "$snap" "$OVERRIDES_LAST_GOOD" || log_error "Could not refresh ${OVERRIDES_LAST_GOOD}"
    else
        rm -f "$snap" "$OVERRIDES_LAST_GOOD"
    fi
}

# Bootstrap hook: drop stale temp files, record per target that this launcher
# understands overrides (so the API reports launcher_reported=true) when there
# is nothing to apply, and re-apply a pending file once.
bootstrap_env_overrides() {
    local t
    rm -f "$(dirname "$OVERRIDES_FILE")"/.otel-env.* "${OVERRIDES_LAST_GOOD}.candidate"
    for t in "${OVERRIDE_TARGETS[@]}"; do
        if [ -z "$(target_hash "$t")" ] && ! state_has_key "env_overrides_applied_sha256_$(target_suffix "$t")"; then
            tstate_write applied_sha256 "$t" ""
        fi
    done
    if [ -s "$OVERRIDES_FILE" ]; then
        apply_env_overrides force
    fi
}

# Sleeps up to POLL_INTERVAL in ENV_WATCH_INTERVAL steps, checking the overrides
# file after each step. Keeps the image-poll cadence at POLL_INTERVAL.
wait_with_env_watch() {
    local waited=0 step remaining
    local watch="$ENV_WATCH_INTERVAL" poll="$POLL_INTERVAL"
    [[ "$watch" =~ ^[0-9]+$ ]] && [ "$watch" -ge 1 ] || watch=5
    if ! [[ "$poll" =~ ^[0-9]+$ ]] || [ "$poll" -lt 1 ]; then
        if [ -z "${POLL_INTERVAL_WARNED:-}" ]; then
            log "WARNING: LAUNCHER_POLL_INTERVAL is not a positive integer; using 300s"
            POLL_INTERVAL_WARNED=1
        fi
        poll=300
    fi
    while [ "$waited" -lt "$poll" ]; do
        remaining=$((poll - waited))
        step="$watch"
        [ "$step" -gt "$remaining" ] && step="$remaining"
        sleep "$step"
        waited=$((waited + step))
        apply_env_overrides || log_error "Env overrides check failed"
    done
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
    #
    # We need the registry MANIFEST digest (what fetch_remote_digest returns),
    # NOT `{{.Image}}` — that is the local image CONFIG id, which never equals
    # the manifest digest, so seeding it makes every first boot see a "new
    # image" and perform a spurious swap.
    #
    # `.RepoDigests` lives on the IMAGE, not the container, so it must come from
    # `docker image inspect`, not `docker inspect <container>`. `.Config.Image`
    # is the reference the container was created with: in our CVMs that is
    # already `repo@sha256:<digest>` (deploys pin by digest), so prefer it and
    # fall back to resolving RepoDigests when the container was started by tag.
    if [ -z "$(read_state "compose_manager_digest")" ]; then
        local config_image current_ref current
        config_image="$(docker inspect compose-manager --format '{{.Config.Image}}' 2>/dev/null || true)"
        case "$config_image" in
            *@sha256:*) current_ref="$config_image" ;;
            ?*)         current_ref="$(docker image inspect "$config_image" \
                            --format '{{range .RepoDigests}}{{println .}}{{end}}' 2>/dev/null \
                            | grep "${IMAGE_REPO}@" | head -n1 || true)" ;;
            *)          current_ref="" ;;
        esac
        current="${current_ref##*@}"
        # current != current_ref guards against a ref with no '@' (no digest).
        if [ -n "$current_ref" ] && [ "$current" != "$current_ref" ]; then
            write_state "compose_manager_digest" "$current"
            write_env_var "COMPOSE_MANAGER_IMAGE" "$current_ref"
            log "Seeded digest from running container: ${current_ref}"
        else
            log "Could not resolve a manifest digest for running compose-manager — will deploy on first poll"
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
    log "  overrides:   ${OVERRIDES_FILE} (watch ${ENV_WATCH_INTERVAL}s)"
    log "  app-compose: ${APP_COMPOSE_FILE}"

    trap cleanup_otel_tmp EXIT
    init_state
    docker_login
    bootstrap
    bootstrap_env_overrides || log_error "Bootstrap env overrides apply failed"

    log "Entering poll loop"
    while true; do
        if ! _poll_cycle; then
            log_error "Poll cycle failed — will retry in ${POLL_INTERVAL}s"
        fi
        wait_with_env_watch
    done
}

# Run only when executed, so tests can `source` this file.
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
