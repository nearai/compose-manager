#!/usr/bin/env bash
# Tests for the launcher's collector image update (otelcol-management) and the
# restart-request handling (launcher/launcher.sh).
#
# Plain bash, no network, no real docker or cosign: launcher.sh is sourced per
# case in a subshell and `docker`, `curl`, `cosign` and `sleep` are replaced by
# shell functions. Needs jq. Prints one PASS/FAIL line per case and exits
# non-zero if any case failed. All values here are fake.

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LAUNCHER="${HERE}/../launcher.sh"
RUST_RESTART="${HERE}/../../src/launcher_restart.rs"
README="${HERE}/../../README.md"

REPO="example/compose-manager-otelcol"
D0="sha256:$(printf '0%.0s' $(seq 1 64))"   # pinned default in the app-compose
D1="sha256:$(printf '1%.0s' $(seq 1 64))"   # previously applied
D2="sha256:$(printf '2%.0s' $(seq 1 64))"   # new channel digest
INGEST="FAKEingestTOKEN0123456789_a"
ROOT="$(mktemp -d "${TMPDIR:-/tmp}/launcher-otelrestart.XXXXXX")"
trap 'rm -rf "$ROOT"' EXIT

command -v jq > /dev/null || { echo "FAIL: jq is required"; exit 2; }

PASSED=0
FAILED=0

# ── Per-case environment (runs inside the case's subshell) ────────────

setup() {
    T="$(mktemp -d "${ROOT}/case.XXXXXX")"
    export COMPOSE_MANAGER_IMAGE_REPO="example/compose-manager"
    export OTELCOL_IMAGE_REPO="$REPO"
    export LAUNCHER_CHANNEL="staging"
    export LAUNCHER_COSIGN_IDENTITY_REGEXP='^https://example\.invalid/.*$'
    export LAUNCHER_ENV_OVERRIDES_FILE="${T}/env-overrides"
    export LAUNCHER_STATE_FILE="${T}/launcher-state.json"
    export LAUNCHER_ENV_FILE="${T}/.env.launcher"
    export LAUNCHER_BASE_ENV_FILE="${T}/dstack-env"
    export LAUNCHER_COMPOSE_FILE="${T}/compose-manager.yml"
    export LAUNCHER_APP_COMPOSE_FILE="${T}/app-compose.yaml"
    export LAUNCHER_RESTART_REQUEST_FILE="${T}/launcher-restart-request.json"
    export LAUNCHER_HEALTH_TIMEOUT=3   # exactly one health probe per wait
    export LAUNCHER_POLL_INTERVAL=12
    export LAUNCHER_ENV_WATCH_INTERVAL=5
    export LAUNCHER_OTEL_STABLE_SECONDS=9   # three stability samples after the first
    unset LAUNCHER_TELEMETRY_PROBE_URL LAUNCHER_SKIP_VERIFY
    # shellcheck source=../launcher.sh
    source "$LAUNCHER"
    set +e   # assertions below decide; the launcher's own set -e is not under test
    : > "$T/docker.log"; : > "$T/health.seq"; : > "$T/inspect.seq"
    : > "$T/cosign.log"; : > "$T/sleep.log"
    echo '{}' > "$STATE_FILE"
    : > "$BASE_ENV_FILE"
    : > "$APP_COMPOSE_FILE"
    echo "${REPO}@${D0}" > "$T/config.image"
    echo "$D1" > "$T/remote.digest"

    if ! command -v sha256sum > /dev/null; then
        sha256sum() { shasum -a 256 "$@"; }
    fi
    # Records each invocation. Behaviour switches (files in $T):
    #  - inspect.seq: lines "running restarting restartcount" popped per stability
    #    sample (default "true false 0"); `--format {{.Config.Image}}` prints config.image
    #  - `up` copies the otel temp env file, sets config.image from its image line
    #    (or the app-compose default), fails when docker_fail exists or the first
    #    N calls when docker_fail_first holds N
    #  - pull_fail / restart_fail make `pull` / `restart` fail
    #  - hook_nested runs a command string from inside `up` (serialization test)
    docker() {
        echo "docker $*" >> "$T/docker.log"
        case "$1" in
            pull) [ ! -f "$T/pull_fail" ]; return ;;
            restart) [ ! -f "$T/restart_fail" ]; return ;;
            login) return 0 ;;
            inspect)
                case "$*" in
                    *"{{.Config.Image}}"*) cat "$T/config.image"; return 0 ;;
                esac
                local first; first="$(head -n1 "$T/inspect.seq")"
                tail -n +2 "$T/inspect.seq" > "$T/inspect.seq.new"; mv "$T/inspect.seq.new" "$T/inspect.seq"
                echo "${first:-true false 0}"
                return 0 ;;
        esac
        case " $* " in
            *" config --services"*) printf 'compose-manager\notelcol-management\n'; return 0 ;;
        esac
        # compose up
        local prev="" a img=""
        for a in "$@"; do
            if [ "$prev" = "--env-file" ]; then
                case "$a" in
                    *.otel-env.*)
                        cp "$a" "$T/otel.env.content"
                        ls -l "$a" | cut -c1-10 > "$T/otel.env.mode"
                        img="$(grep "^${OTELCOL_IMAGE_KEY}=" "$a" | cut -d= -f2-)" ;;
                esac
            fi
            prev="$a"
        done
        if [ -f "$T/hook_nested" ]; then
            local cmd; cmd="$(cat "$T/hook_nested")"; rm -f "$T/hook_nested"
            eval "$cmd" >> "$T/nested.out" 2>&1
            echo "$?" >> "$T/nested.rc"
        fi
        [ ! -f "$T/docker_fail" ] || return 1
        if [ -f "$T/docker_fail_first" ]; then
            local n; n="$(cat "$T/docker_fail_first")"
            if [ "$n" -gt 0 ]; then echo $((n - 1)) > "$T/docker_fail_first"; return 1; fi
        fi
        case " $* " in
            *" otelcol-management"*) echo "${img:-${REPO}@${D0}}" > "$T/config.image" ;;
        esac
        return 0
    }
    # Registry digest lookups (token + manifest HEAD) and compose-manager health.
    curl() {
        local a url="" first
        for a in "$@"; do case "$a" in http*) url="$a" ;; esac; done
        case "$url" in
            *auth.docker.io*) echo '{"token":"t"}'; return 0 ;;
            *"/manifests/"*) printf 'HTTP/2 200\r\ndocker-content-digest: %s\r\n' "$(cat "$T/remote.digest")"; return 0 ;;
        esac
        first="$(head -n1 "$T/health.seq")"
        tail -n +2 "$T/health.seq" > "$T/health.seq.new"; mv "$T/health.seq.new" "$T/health.seq"
        [ "${first:-ok}" = "ok" ]
    }
    cosign() { echo "cosign $*" >> "$T/cosign.log"; [ ! -f "$T/cosign_fail" ]; }
    sleep() { echo "$1" >> "$T/sleep.log"; }
}

fail() { echo "ASSERT: $*" >&2; exit 1; }
eq()   { [ "$1" = "$2" ] || fail "$3: expected '$2', got '$1'"; }
up_log()       { grep ' up ' "$T/docker.log"; }
up_calls()     { grep -c ' up ' "$T/docker.log"; }
docker_count() { grep -c "^docker $1" "$T/docker.log"; }
state() { jq -r ".[\"$1\"] // \"<unset>\"" "$STATE_FILE"; }
env_image() { grep "^${OTELCOL_IMAGE_KEY}=" "$ENV_FILE" | cut -d= -f2-; }
write_request() { printf '{"id":"%s","target":"%s","requested_at":"2026-01-01T00:00:00Z"}\n' "$1" "$2" > "$RESTART_REQUEST_FILE"; }
RID1="0123456789abcdef0123456789abcdef"
RID2="fedcba9876543210fedcba9876543210"

# ── Collector update ───────────────────────────────────────────────────

case_update_success() {
    setup
    write_state otelcol_digest "$D1"
    echo "${OTELCOL_IMAGE_KEY}=${REPO}@${D1}" > "$ENV_FILE"
    printf "MONITORING_INGEST_TOKEN='%s'\n" "$INGEST" > "$OVERRIDES_FILE"
    echo "$D2" > "$T/remote.digest"
    _poll_otelcol > "$T/out" 2>&1 || fail "poll failed"
    grep -q "^docker pull ${REPO}@${D2}\$" "$T/docker.log" || fail "image not pulled by digest"
    grep -qF -- "--certificate-identity-regexp=${COSIGN_IDENTITY}" "$T/cosign.log" || fail "cosign identity"
    grep -q -- "--certificate-oidc-issuer=${COSIGN_ISSUER}" "$T/cosign.log" || fail "cosign issuer"
    grep -q "${REPO}@${D2}\$" "$T/cosign.log" || fail "cosign verified the wrong ref"
    eq "$(env_image)" "${REPO}@${D2}" "env file image"
    eq "$(up_calls)" 1 "one recreate"
    local line; line="$(up_log | head -n1)"
    case "$line" in
        *"-p dstack -f ${APP_COMPOSE_FILE} --env-file ${BASE_ENV_FILE} --env-file "*".otel-env."*" up -d --no-deps --force-recreate otelcol-management"*) ;;
        *) fail "unexpected compose args: $line" ;;
    esac
    case "$line" in *"$ENV_FILE"*|*"$OVERRIDES_FILE"*|*compose-manager\ *) fail "layering: $line" ;; esac
    grep -q "^${OTELCOL_IMAGE_KEY}=${REPO}@${D2}\$" "$T/otel.env.content" || fail "image missing from the env file passed to compose"
    grep -q "^MONITORING_INGEST_TOKEN='${INGEST}'\$" "$T/otel.env.content" || fail "ingest override not preserved"
    eq "$(cat "$T/otel.env.mode")" "-rw-------" "temp env mode"
    ls "$T"/.otel-env.* > /dev/null 2>&1 && fail "temp env left behind"
    eq "$(state otelcol_digest)" "$D2" "digest recorded"
    eq "$(state otelcol_error)" "" "error cleared"
    state otelcol_last_update | grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z$' || fail "last_update"
    eq "$(docker_count inspect)" 4 "stability samples"
    eq "$(docker_count restart)" 0 "no plain restart"
    # Up to date now: nothing happens.
    : > "$T/docker.log"
    _poll_otelcol > /dev/null 2>&1
    eq "$(up_calls)" 0 "no recreate when up to date"
    eq "$(docker_count pull)" 0 "no pull when up to date"
}

case_cosign_failure_changes_nothing() {
    setup
    write_state otelcol_digest "$D1"
    echo "${OTELCOL_IMAGE_KEY}=${REPO}@${D1}" > "$ENV_FILE"
    echo "$D2" > "$T/remote.digest"
    : > "$T/cosign_fail"
    _poll_otelcol > /dev/null 2>&1 && fail "poll must fail"
    eq "$(up_calls)" 0 "no recreate"
    eq "$(env_image)" "${REPO}@${D1}" "env file untouched"
    eq "$(state otelcol_digest)" "$D1" "digest untouched"
}

case_invalid_digest_is_rejected_before_any_pull() {
    setup
    local bad
    for bad in "sha256:zzz" "md5:abc" "sha256:$(printf '2%.0s' $(seq 1 63))" ""; do
        echo "$bad" > "$T/remote.digest"
        _poll_otelcol > /dev/null 2>&1 && fail "poll must fail for '${bad}'"
        eq "$(docker_count pull)" 0 "pull for '${bad}'"
        eq "$(up_calls)" 0 "recreate for '${bad}'"
    done
    [ ! -s "$T/cosign.log" ] || fail "cosign called"
}

case_stability_failure_rolls_back_and_backs_off() {
    setup
    write_state otelcol_digest "$D1"
    echo "${OTELCOL_IMAGE_KEY}=${REPO}@${D1}" > "$ENV_FILE"
    printf "MONITORING_INGEST_TOKEN='%s'\n" "$INGEST" > "$OVERRIDES_FILE"
    echo "$D2" > "$T/remote.digest"
    printf 'true false 0\ntrue false 1\n' > "$T/inspect.seq"   # restart count increased
    _poll_otelcol > "$T/out" 2>&1 && fail "poll must report failure"
    eq "$(up_calls)" 2 "attempt + rollback recreate"
    grep -q "^${OTELCOL_IMAGE_KEY}=${REPO}@${D1}\$" "$T/otel.env.content" || fail "rollback must pass the previous image"
    grep -q "^MONITORING_INGEST_TOKEN='${INGEST}'\$" "$T/otel.env.content" || fail "rollback must keep the ingest override"
    eq "$(env_image)" "${REPO}@${D1}" "env file restored"
    eq "$(state otelcol_digest)" "$D1" "digest stays on the previous one"
    state otelcol_error | grep -q 'previous image restored' || fail "error not recorded: $(state otelcol_error)"
    local until; until="$(state otelcol_backoff_until_epoch)"
    [ "$until" -gt "$(date +%s)" ] || fail "no backoff recorded"
    # In backoff: the next poll does nothing, not even a digest lookup.
    : > "$T/docker.log"
    _poll_otelcol > "$T/out2" 2>&1
    eq "$(docker_count pull)" 0 "no pull in backoff"
    grep -q 'backoff' "$T/out2" || fail "backoff not logged"
    # Once it expires the update is retried.
    write_state otelcol_backoff_until_epoch "$(( $(date +%s) - 1 ))"
    : > "$T/inspect.seq"
    _poll_otelcol > /dev/null 2>&1 || fail "retry after backoff failed"
    eq "$(state otelcol_digest)" "$D2" "retry applied"
    eq "$(state otelcol_backoff_until_epoch)" "" "backoff cleared"
}

case_rollback_without_previous_image_drops_the_variable() {
    setup
    echo "$D2" > "$T/remote.digest"
    printf 'true false 0\nfalse false 0\n' > "$T/inspect.seq"   # exited
    _poll_otelcol > /dev/null 2>&1 && fail "poll must report failure"
    eq "$(up_calls)" 2 "attempt + rollback recreate"
    [ -z "$(env_image)" ] || fail "image variable must be removed (app-compose default applies)"
    ! grep -q "^${OTELCOL_IMAGE_KEY}=" "$T/otel.env.content" || fail "rollback must not pass an image"
    eq "$(state otelcol_digest)" "<unset>" "no digest recorded"
}

case_compose_failure_and_failed_rollback_is_critical() {
    setup
    write_state otelcol_digest "$D1"
    echo "${OTELCOL_IMAGE_KEY}=${REPO}@${D1}" > "$ENV_FILE"
    echo "$D2" > "$T/remote.digest"
    : > "$T/docker_fail"
    _poll_otelcol > "$T/out" 2>&1 && fail "poll must report failure"
    grep -q 'CRITICAL' "$T/out" || fail "CRITICAL not logged"
    state otelcol_error | grep -q 'CRITICAL' || fail "CRITICAL not recorded"
    ls "$T"/.otel-env.* > /dev/null 2>&1 && fail "temp env left behind"
    eq "$(env_image)" "${REPO}@${D1}" "env file restored"
}

case_missing_service_in_app_compose_skips_the_update() {
    setup
    echo "$D2" > "$T/remote.digest"
    docker() {
        echo "docker $*" >> "$T/docker.log"
        case " $* " in *" config --services"*) echo compose-manager; return 0 ;; esac
        return 0
    }
    _poll_otelcol > /dev/null 2>&1 && fail "poll must fail"
    eq "$(docker_count pull)" 0 "no pull"
    state otelcol_error | grep -q 'otelcol-management' || fail "error must name the service"
}

case_disabled_without_repo_or_app_compose() {
    setup
    # shellcheck disable=SC2034  # read by the sourced launcher
    OTELCOL_REPO=""
    _poll_otelcol > "$T/out" 2>&1 || fail "disabled poll must succeed"
    _poll_otelcol >> "$T/out" 2>&1
    bootstrap_otelcol >> "$T/out" 2>&1
    [ ! -s "$T/docker.log" ] || fail "docker must not be called when disabled"
    eq "$(state otelcol_status)" "disabled: OTELCOL_IMAGE_REPO is not set" "status"
    eq "$(grep -c 'Collector image update: disabled' "$T/out")" 1 "reported once"
    # App-compose not mounted.
    setup
    rm -f "$APP_COMPOSE_FILE"
    _poll_otelcol > "$T/out" 2>&1 || fail "disabled poll must succeed"
    bootstrap_otelcol >> "$T/out" 2>&1
    [ ! -s "$T/docker.log" ] || fail "docker must not be called without the app-compose"
    state otelcol_status | grep -q '^disabled: app-compose is not mounted' || fail "status: $(state otelcol_status)"
    # The compose-manager poll is unaffected (still runs when the collector is off).
    : > "$T/docker.log"
    _poll_cycle > /dev/null 2>&1
    grep -q '^docker pull example/compose-manager@' "$T/docker.log" || fail "compose-manager poll must still run"
    eq "$(up_calls)" 1 "only compose-manager recreated"
    up_log | grep -q -- '--no-deps compose-manager' || fail "compose-manager recreate"
}

# ── Reboot re-apply ────────────────────────────────────────────────────

case_reboot_reapplies_the_recorded_image() {
    setup
    write_state otelcol_digest "$D2"
    # After a CVM reboot the container runs the app-compose default.
    bootstrap_otelcol > "$T/out" 2>&1
    eq "$(up_calls)" 1 "one recreate"
    up_log | head -n1 | grep -q -- '--no-deps --force-recreate otelcol-management' || fail "args"
    grep -q "^${OTELCOL_IMAGE_KEY}=${REPO}@${D2}\$" "$T/otel.env.content" || fail "recorded image not passed"
    eq "$(env_image)" "${REPO}@${D2}" "env file restored from state"
    # Already running the recorded image: nothing to do.
    : > "$T/docker.log"
    bootstrap_otelcol > /dev/null 2>&1
    eq "$(up_calls)" 0 "no recreate when already running it"
}

case_reboot_reapply_failure_reverts_to_the_app_compose_image() {
    setup
    write_state otelcol_digest "$D2"
    printf 'true false 0\nfalse false 0\n' > "$T/inspect.seq"
    bootstrap_otelcol > "$T/out" 2>&1
    eq "$(up_calls)" 2 "attempt + revert"
    [ -z "$(env_image)" ] || fail "variable must be removed"
    eq "$(state otelcol_digest)" "" "digest cleared so the next poll retries"
    [ "$(state otelcol_backoff_until_epoch)" -gt "$(date +%s)" ] || fail "backoff"
    state otelcol_error | grep -q 're-applying' || fail "error not recorded"
}

case_bootstrap_seeds_the_digest_from_the_running_container() {
    setup
    echo "${REPO}@${D1}" > "$T/config.image"
    bootstrap_otelcol > /dev/null 2>&1
    eq "$(up_calls)" 0 "no recreate"
    eq "$(state otelcol_digest)" "$D1" "seeded"
    eq "$(env_image)" "${REPO}@${D1}" "env file seeded"
    # A foreign image is not adopted.
    setup
    echo "someone/else@${D1}" > "$T/config.image"
    bootstrap_otelcol > /dev/null 2>&1
    eq "$(state otelcol_digest)" "<unset>" "foreign image ignored"
}

case_reboot_with_overrides_recreates_the_collector_only_once() {
    setup
    write_state otelcol_digest "$D2"
    printf "MONITORING_INGEST_TOKEN='%s'\n" "$INGEST" > "$OVERRIDES_FILE"
    echo "${OTELCOL_IMAGE_KEY}=${REPO}@${D2}" > "$ENV_FILE"
    bootstrap_env_overrides > /dev/null 2>&1
    bootstrap_otelcol > /dev/null 2>&1
    eq "$(up_calls)" 1 "overrides re-apply already carried the image"
    grep -q "^${OTELCOL_IMAGE_KEY}=${REPO}@${D2}\$" "$T/otel.env.content" || fail "image in the override recreate"
    grep -q "^MONITORING_INGEST_TOKEN='${INGEST}'\$" "$T/otel.env.content" || fail "override in the recreate"
}

case_env_override_apply_keeps_the_recorded_image() {
    setup
    echo "${OTELCOL_IMAGE_KEY}=${REPO}@${D2}" > "$ENV_FILE"
    printf "MONITORING_INGEST_TOKEN='%s'\n" "$INGEST" > "$OVERRIDES_FILE"
    apply_env_overrides > /dev/null 2>&1
    eq "$(up_calls)" 1 "one recreate"
    up_log | head -n1 | grep -q -- '--no-deps otelcol-management' || fail "args"
    grep -q "^${OTELCOL_IMAGE_KEY}=${REPO}@${D2}\$" "$T/otel.env.content" || fail "override apply dropped the image"
}

# ── Restart requests ───────────────────────────────────────────────────

case_restart_compose_manager_success() {
    setup
    write_request "$RID1" compose-manager
    process_restart_request > "$T/out" 2>&1
    eq "$(docker_count restart)" 1 "restart calls"
    grep -q '^docker restart compose-manager$' "$T/docker.log" || fail "wrong restart command"
    eq "$(state restart_last_id)" "$RID1" "id"
    eq "$(state restart_last_target)" compose-manager "target"
    eq "$(state restart_last_outcome)" ok "outcome"
    eq "$(state restart_last_reason)" "" "reason"
    state restart_last_finished_at | grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z$' || fail "finished_at"
    eq "$(state restart_supported)" 1 "supported marker"
    # Handled once: the request file stays but is not run again.
    process_restart_request > /dev/null 2>&1
    eq "$(docker_count restart)" 1 "no second restart"
    # A new request id runs again.
    write_request "$RID2" compose-manager
    process_restart_request > /dev/null 2>&1
    eq "$(docker_count restart)" 2 "new id restarts again"
    eq "$(state restart_last_id)" "$RID2" "new id recorded"
}

case_restart_otelcol_success() {
    setup
    write_request "$RID1" otelcol-management
    process_restart_request > /dev/null 2>&1
    grep -q '^docker restart otelcol-management$' "$T/docker.log" || fail "wrong restart command"
    eq "$(docker_count inspect)" 4 "stability samples"
    eq "$(state restart_last_outcome)" ok "outcome"
    eq "$(state restart_last_target)" otelcol-management "target"
    eq "$(up_calls)" 0 "restart must not recreate"
}

case_restart_health_failures_are_reported() {
    setup
    write_request "$RID1" compose-manager
    echo fail > "$T/health.seq"
    process_restart_request > /dev/null 2>&1
    eq "$(state restart_last_outcome)" failed "cm outcome"
    state restart_last_reason | grep -q 'did not become healthy' || fail "cm reason: $(state restart_last_reason)"
    eq "$(state restart_last_id)" "$RID1" "cm id"
    # Collector: restart count increases right after the restart.
    write_request "$RID2" otelcol-management
    printf 'true false 0\ntrue false 1\n' > "$T/inspect.seq"
    process_restart_request > /dev/null 2>&1
    eq "$(state restart_last_outcome)" failed "otel outcome"
    state restart_last_reason | grep -q 'restart' || fail "otel reason"
    eq "$(state restart_last_id)" "$RID2" "otel id"
    # docker restart itself failing.
    write_request "abcdefabcdefabcdefabcdefabcdef12" compose-manager
    : > "$T/restart_fail"
    process_restart_request > /dev/null 2>&1
    eq "$(state restart_last_outcome)" failed "docker failure outcome"
    eq "$(state restart_last_reason)" "docker restart failed" "docker failure reason"
}

case_disallowed_targets_are_rejected_without_docker() {
    setup
    local t n=0 id
    for t in "model-vllm" "compose-manager;rm" "otelcol-management " "../x" "" "Compose-Manager" "dstack-kms"; do
        n=$((n + 1))
        id="$(printf '%032d' "$n")"
        write_request "$id" "$t"
        process_restart_request > "$T/out" 2>&1
        eq "$(docker_count restart)" 0 "restart for '${t}'"
        eq "$(state restart_last_outcome)" rejected "outcome for '${t}'"
        eq "$(state restart_last_id)" "$id" "id for '${t}'"
        grep -q 'not a managed container' "$T/out" || fail "rejection not logged for '${t}'"
    done
    # The rejected name is logged (sanitized), not executed.
    write_request "$RID1" "model-vllm"
    process_restart_request > "$T/out" 2>&1
    grep -q "target 'model-vllm'" "$T/out" || fail "name not logged"
    # Malformed files and ids are rejected once.
    for t in 'not json' '[]' '{"id":"short","target":"compose-manager"}' '{"target":"compose-manager"}'; do
        echo "$t" > "$RESTART_REQUEST_FILE"
        process_restart_request > /dev/null 2>&1
        eq "$(docker_count restart)" 0 "restart for malformed '${t}'"
        eq "$(state restart_last_outcome)" rejected "malformed outcome"
        state restart_last_id | grep -q '^invalid-[0-9a-f]\{12\}$' || fail "synthetic id: $(state restart_last_id)"
        : > "$T/before"; state restart_last_finished_at > "$T/before"
        process_restart_request > /dev/null 2>&1
        eq "$(docker_count restart)" 0 "still no restart"
    done
}

case_restart_never_touches_other_containers_in_the_source() {
    # Static guard: the only `docker restart` in the launcher is the one that
    # takes the validated request target.
    local n
    n="$(grep -c 'docker restart "' "$LAUNCHER")"
    eq "$n" 1 "docker restart occurrences"
    grep -q 'docker restart "\$REQ_TARGET"' "$LAUNCHER" || fail "restart must use the validated target"
    grep -E 'docker (stop|rm|kill|compose .* down)' "$LAUNCHER" | grep -v '^ *#' && fail "unexpected container control"
    true
}

case_pending_request_is_abandoned_on_launcher_start() {
    setup
    write_request "$RID1" compose-manager
    bootstrap_restart_requests > /dev/null 2>&1
    eq "$(docker_count restart)" 0 "no restart after a launcher start"
    eq "$(state restart_last_outcome)" abandoned "outcome"
    eq "$(state restart_last_id)" "$RID1" "id"
    eq "$(state restart_supported)" 1 "supported marker"
    process_restart_request > /dev/null 2>&1
    eq "$(docker_count restart)" 0 "abandoned request stays closed"
}

case_restart_runs_in_the_watch_loop() {
    setup
    write_request "$RID1" otelcol-management
    : > "$T/apply.calls"
    apply_env_overrides() { echo x >> "$T/apply.calls"; }
    wait_with_env_watch > /dev/null 2>&1
    eq "$(docker_count restart)" 1 "restarted once in the watch loop"
    eq "$(state restart_last_outcome)" ok "outcome"
}

# ── Serialization ──────────────────────────────────────────────────────

case_operations_never_overlap() {
    setup
    printf "MONITORING_INGEST_TOKEN='%s'\n" "$INGEST" > "$OVERRIDES_FILE"
    write_request "$RID1" compose-manager
    # While the env-override apply is inside `docker compose up`, a restart
    # request and a collector poll try to start: both must be refused.
    echo 'process_restart_request; echo "rc=$?"; with_lock _poll_otelcol; echo "rc=$?"; with_lock _poll_compose_manager; echo "rc=$?"' > "$T/hook_nested"
    apply_env_overrides > /dev/null 2>&1
    eq "$(docker_count restart)" 0 "no restart during the apply"
    eq "$(grep -c '^rc=75$' "$T/nested.out")" 3 "nested operations refused"
    eq "$(docker_count pull)" 0 "no pull during the apply"
    [ ! -d "$LOCK_DIR" ] || fail "lock not released"
    # The refused request is picked up on the next tick.
    process_restart_request > /dev/null 2>&1
    eq "$(docker_count restart)" 1 "restart after the apply"
    eq "$(state restart_last_outcome)" ok "outcome"
    [ ! -d "$LOCK_DIR" ] || fail "lock not released after restart"
    # The lock is released on failure paths too.
    write_request "$RID2" compose-manager
    : > "$T/restart_fail"
    process_restart_request > /dev/null 2>&1
    [ ! -d "$LOCK_DIR" ] || fail "lock not released after a failed restart"
}

case_stale_lock_is_cleared_at_start() {
    setup
    mkdir "$LOCK_DIR"
    with_lock true > /dev/null 2>&1
    eq "$?" 75 "a held lock refuses"
    rmdir "$LOCK_DIR"
    with_lock true > /dev/null 2>&1 || fail "free lock must run"
}

# ── Parity and hygiene ─────────────────────────────────────────────────

case_managed_containers_parity() {
    local launcher rust
    launcher="$(bash -c 'COMPOSE_MANAGER_IMAGE_REPO=x source "$1"; printf "%s\n" "${MANAGED_CONTAINERS[@]}"' _ "$LAUNCHER" | sort | tr '\n' ' ')"
    rust="$(grep -E '^pub\(crate\) const TARGETS' "$RUST_RESTART" | grep -o '"[a-z-]*"' | tr -d '"' | sort | tr '\n' ' ')"
    eq "$rust" "$launcher" "rust TARGETS vs launcher MANAGED_CONTAINERS"
    eq "$launcher" "compose-manager otelcol-management " "exactly the two managed containers"
    grep -q '/admin/launcher/restart' "$README" || fail "README must document the restart API"
    grep -q 'OTELCOL_MANAGEMENT_IMAGE' "$README" || fail "README must document the image variable"
    grep -q 'OTELCOL_IMAGE_REPO' "$README" || fail "README must document OTELCOL_IMAGE_REPO"
}

CASES="
update_success
cosign_failure_changes_nothing
invalid_digest_is_rejected_before_any_pull
stability_failure_rolls_back_and_backs_off
rollback_without_previous_image_drops_the_variable
compose_failure_and_failed_rollback_is_critical
missing_service_in_app_compose_skips_the_update
disabled_without_repo_or_app_compose
reboot_reapplies_the_recorded_image
reboot_reapply_failure_reverts_to_the_app_compose_image
bootstrap_seeds_the_digest_from_the_running_container
reboot_with_overrides_recreates_the_collector_only_once
env_override_apply_keeps_the_recorded_image
restart_compose_manager_success
restart_otelcol_success
restart_health_failures_are_reported
disallowed_targets_are_rejected_without_docker
restart_never_touches_other_containers_in_the_source
pending_request_is_abandoned_on_launcher_start
restart_runs_in_the_watch_loop
operations_never_overlap
stale_lock_is_cleared_at_start
managed_containers_parity
"

for c in $CASES; do
    out="${ROOT}/${c}.out"
    if ( "case_${c}" ) > "$out" 2>&1; then
        echo "PASS ${c}"
        PASSED=$((PASSED + 1))
    else
        echo "FAIL ${c}"
        sed 's/^/    /' "$out" | tail -n 20
        FAILED=$((FAILED + 1))
    fi
done

# No case may have printed or stored a token value in logs or state.
if grep -rqF "$INGEST" "$ROOT" --include='*.out' --include='*.json' --include='*.log' --include='out*'; then
    echo "FAIL no_value_in_logs_or_state"
    FAILED=$((FAILED + 1))
else
    echo "PASS no_value_in_logs_or_state"
    PASSED=$((PASSED + 1))
fi

echo "${PASSED} passed, ${FAILED} failed"
[ "$FAILED" -eq 0 ]
