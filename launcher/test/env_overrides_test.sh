#!/usr/bin/env bash
# Tests for the launcher's env-override handling (launcher/launcher.sh).
#
# Plain bash, no network, no real docker: launcher.sh is sourced per case in a
# subshell and `docker`, `curl` and `sleep` are replaced by shell functions.
# Covers both override targets (compose-manager, otelcol-management).
# Needs jq (the launcher itself uses it). Prints one PASS/FAIL line per case
# and exits non-zero if any case failed. All secrets here are fake values.

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LAUNCHER="${HERE}/../launcher.sh"
COMPOSE_YML="${HERE}/../docker-compose.yml"
RUST_SRC="${HERE}/../../src/env_overrides.rs"
README="${HERE}/../../README.md"

SECRET="s3cr3t-FAKE-value-never-log-me"
# Valid MONITORING_INGEST_TOKEN values (16..512 chars of [A-Za-z0-9+/=_.~-]).
INGEST="FAKEingestTOKEN0123456789_a"
INGEST2="FAKEingestTOKEN0123456789_b"
ROOT="$(mktemp -d "${TMPDIR:-/tmp}/launcher-envtest.XXXXXX")"
trap 'rm -rf "$ROOT"' EXIT

command -v jq > /dev/null || { echo "FAIL: jq is required"; exit 2; }

PASSED=0
FAILED=0

# ── Per-case environment (runs inside the case's subshell) ────────────

setup() {
    T="$(mktemp -d "${ROOT}/case.XXXXXX")"
    export COMPOSE_MANAGER_IMAGE_REPO="example/compose-manager"
    export LAUNCHER_ENV_OVERRIDES_FILE="${T}/env-overrides"
    export LAUNCHER_STATE_FILE="${T}/launcher-state.json"
    export LAUNCHER_ENV_FILE="${T}/.env.launcher"
    export LAUNCHER_BASE_ENV_FILE="${T}/dstack-env"
    export LAUNCHER_COMPOSE_FILE="${T}/compose-manager.yml"
    export LAUNCHER_APP_COMPOSE_FILE="${T}/app-compose.yaml"
    export LAUNCHER_HEALTH_TIMEOUT=3   # exactly one health probe per wait
    export LAUNCHER_POLL_INTERVAL=12
    export LAUNCHER_ENV_WATCH_INTERVAL=5
    export LAUNCHER_OTEL_STABLE_SECONDS=9   # three stability samples after the first
    unset LAUNCHER_TELEMETRY_PROBE_URL
    # shellcheck source=../launcher.sh
    source "$LAUNCHER"
    set +e   # assertions below decide; the launcher's own set -e is not under test
    : > "$T/docker.log"
    : > "$T/health.seq"
    : > "$T/inspect.seq"
    echo '{}' > "$STATE_FILE"
    : > "$BASE_ENV_FILE"
    : > "$APP_COMPOSE_FILE"

    if ! command -v sha256sum > /dev/null; then
        sha256sum() { shasum -a 256 "$@"; }
    fi
    # Records each invocation plus the whole-file overrides hash at call time.
    #  - `inspect` pops a line from inspect.seq (default "true false 0")
    #  - `config --services` prints $T/services (default: both services)
    #  - `up` copies the otel temp env file (content, mode, path) for the
    #    assertions, may replace the overrides file once (hook_overwrite), and
    #    fails when docker_fail exists or its args contain docker_fail_on
    docker() {
        echo "docker $* | overrides_sha=$(overrides_hash)" >> "$T/docker.log"
        case " $* " in
            *" inspect "*)
                local first; first="$(head -n1 "$T/inspect.seq")"
                tail -n +2 "$T/inspect.seq" > "$T/inspect.seq.new"; mv "$T/inspect.seq.new" "$T/inspect.seq"
                echo "${first:-true false 0}"
                return 0 ;;
            *" config --services"*)
                if [ -f "$T/services" ]; then cat "$T/services"; else printf 'compose-manager\notelcol-management\n'; fi
                return 0 ;;
        esac
        local prev="" a
        for a in "$@"; do
            if [ "$prev" = "--env-file" ]; then
                case "$a" in
                    *.otel-env.*)
                        cp "$a" "$T/otel.env.content"
                        ls -l "$a" | cut -c1-10 > "$T/otel.env.mode"
                        echo "$a" > "$T/otel.env.path" ;;
                esac
            fi
            prev="$a"
        done
        if [ -f "$T/hook_overwrite" ]; then
            cp "$T/hook_overwrite" "$OVERRIDES_FILE"
            rm -f "$T/hook_overwrite"
        fi
        [ ! -f "$T/docker_fail" ] || return 1
        if [ -f "$T/docker_fail_on" ]; then
            case " $* " in *"$(cat "$T/docker_fail_on")"*) return 1 ;; esac
        fi
        return 0
    }
    # Health probe: pops one line (ok|fail) from health.seq; empty means ok.
    # Telemetry probe (http://probe.test/...): answers $T/probe.mode (default
    # 200; "neterr" fails to connect) and stores the header file it was given.
    curl() {
        local a hdr="" probe=0 first mode
        for a in "$@"; do
            case "$a" in
                http://probe.test/*) probe=1 ;;
                @*) hdr="${a#@}" ;;
            esac
        done
        if [ "$probe" = 1 ]; then
            echo "curl $*" >> "$T/curl.log"
            [ -z "$hdr" ] || cat "$hdr" > "$T/probe.hdr"
            mode="$(cat "$T/probe.mode" 2> /dev/null)"
            case "${mode:-200}" in
                neterr) return 7 ;;
                *) printf '%s' "${mode:-200}"; return 0 ;;
            esac
        fi
        first="$(head -n1 "$T/health.seq")"
        tail -n +2 "$T/health.seq" > "$T/health.seq.new"; mv "$T/health.seq.new" "$T/health.seq"
        [ "${first:-ok}" = "ok" ]
    }
    sleep() { echo "$1" >> "$T/sleep.log"; }
}

fail() { echo "ASSERT: $*" >&2; exit 1; }
eq()   { [ "$1" = "$2" ] || fail "$3: expected '$2', got '$1'"; }
up_log()       { grep ' up ' "$T/docker.log"; }
docker_calls() { grep -c ' up ' "$T/docker.log"; }
state() { jq -r ".[\"$1\"] // \"<unset>\"" "$STATE_FILE"; }
file_sha() { sha256sum "$1" | cut -d' ' -f1; }
# Per-target hash: sha256 over the given lines, each newline-terminated.
lines_hash() { printf '%s\n' "$@" | sha256sum | cut -d' ' -f1; }
mode_of() { ls -l "$1" | cut -c1-10; }
write_overrides() { printf '%s\n' "$@" > "$OVERRIDES_FILE"; }
reset_log() { : > "$T/docker.log"; }
CM_APPLIED=env_overrides_applied_sha256_compose_manager
OT_APPLIED=env_overrides_applied_sha256_otelcol_management

# ── compose-manager target ─────────────────────────────────────────────

case_compose_up_puts_overrides_last() {
    setup
    echo 'COMPOSE_MANAGER_IMAGE=example/compose-manager@sha256:abc' > "$ENV_FILE"
    write_overrides "BEARER_TOKEN='${SECRET}'" "CM_MIN_TAG_AGE_HOURS='7'"
    compose_up -d --no-deps compose-manager
    local line; line="$(head -n1 "$T/docker.log")"
    case "$line" in
        *"--env-file ${BASE_ENV_FILE} --env-file ${ENV_FILE} --env-file ${OVERRIDES_FILE} up -d --no-deps compose-manager"*) ;;
        *) fail "overrides not last: $line" ;;
    esac
    # Missing and empty files add nothing.
    : > "$T/docker.log"; rm -f "$OVERRIDES_FILE"
    compose_up -d --no-deps compose-manager
    case "$(head -n1 "$T/docker.log")" in *"env-overrides"*) fail "missing file passed" ;; esac
    : > "$OVERRIDES_FILE"; : > "$T/docker.log"
    compose_up -d --no-deps compose-manager
    case "$(head -n1 "$T/docker.log")" in *"env-overrides"*) fail "empty file passed" ;; esac
    # An invalid file is never handed to docker compose.
    write_overrides "COMPOSE_MANAGER_IMAGE='evil'"; : > "$T/docker.log"
    compose_up -d --no-deps compose-manager 2> /dev/null
    case "$(head -n1 "$T/docker.log")" in *"env-overrides"*) fail "invalid file passed" ;; esac
}

case_apply_success_updates_state_and_last_good() {
    setup
    write_overrides "BEARER_TOKEN='${SECRET}'" "CM_MIN_TAG_AGE_HOURS='7'"
    local sha; sha="$(lines_hash "BEARER_TOKEN='${SECRET}'" "CM_MIN_TAG_AGE_HOURS='7'")"
    apply_env_overrides
    eq "$(docker_calls)" 1 "docker calls"
    up_log | grep -q 'up -d --no-deps compose-manager' || fail "wrong compose args"
    up_log | grep -q -- "-f ${COMPOSE_FILE} " || fail "must use the bundled compose file"
    eq "$(state $CM_APPLIED)" "$sha" "applied sha"
    state env_overrides_applied_at_compose_manager | grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z$' || fail "applied_at format"
    eq "$(state env_overrides_error_compose_manager)" "" "error cleared"
    eq "$(state env_overrides_rejected_sha256_compose_manager)" "" "rejected cleared"
    eq "$(state $OT_APPLIED)" "<unset>" "other target untouched"
    cmp -s "$OVERRIDES_FILE" "$OVERRIDES_LAST_GOOD" || fail "last-good differs"
    eq "$(mode_of "$OVERRIDES_LAST_GOOD")" "-rw-------" "last-good mode"
    [ ! -e "${OVERRIDES_LAST_GOOD}.candidate" ] || fail "candidate left behind"
}

case_state_uses_per_target_keys_only() {
    setup
    write_overrides "CM_MIN_TAG_AGE_HOURS='7'" "MONITORING_INGEST_TOKEN='${INGEST}'"
    apply_env_overrides
    local keys; keys="$(jq -r 'keys[]' "$STATE_FILE" | grep '^env_overrides' | sort | tr '\n' ' ')"
    eq "$keys" "env_overrides_applied_at_compose_manager env_overrides_applied_at_otelcol_management env_overrides_applied_sha256_compose_manager env_overrides_applied_sha256_otelcol_management env_overrides_error_compose_manager env_overrides_error_otelcol_management env_overrides_rejected_sha256_compose_manager env_overrides_rejected_sha256_otelcol_management " "state keys"
}

case_unchanged_hash_is_a_noop() {
    setup
    write_overrides "CM_MIN_TAG_AGE_HOURS='7'"
    apply_env_overrides
    eq "$(docker_calls)" 1 "first apply"
    apply_env_overrides
    apply_env_overrides
    eq "$(docker_calls)" 1 "repeat applies must not call docker"
    # No file and nothing ever applied: also nothing to do.
    setup
    apply_env_overrides
    eq "$(docker_calls)" 0 "no file, no state"
}

case_unhealthy_restores_last_good_and_records_rejection() {
    setup
    write_overrides "CM_MIN_TAG_AGE_HOURS='7'"
    apply_env_overrides
    local good_sha; good_sha="$(file_sha "$OVERRIDES_FILE")"
    local good_t; good_t="$(lines_hash "CM_MIN_TAG_AGE_HOURS='7'")"
    reset_log

    write_overrides "BEARER_TOKEN='${SECRET}'" "CM_MIN_TAG_AGE_HOURS='9'"
    local bad_sha; bad_sha="$(file_sha "$OVERRIDES_FILE")"
    local bad_t; bad_t="$(lines_hash "BEARER_TOKEN='${SECRET}'" "CM_MIN_TAG_AGE_HOURS='9'")"
    printf 'fail\nok\n' > "$T/health.seq"
    apply_env_overrides
    eq "$(docker_calls)" 2 "attempt + restore recreate"
    up_log | sed -n 1p | grep -q "overrides_sha=${bad_sha}" || fail "first recreate must use the new file"
    up_log | sed -n 2p | grep -q "overrides_sha=${good_sha}" || fail "second recreate must use restored file"
    eq "$(file_sha "$OVERRIDES_FILE")" "$good_sha" "file restored"
    eq "$(state $CM_APPLIED)" "$good_t" "applied stays last good"
    eq "$(state env_overrides_rejected_sha256_compose_manager)" "$bad_t" "rejected sha"
    [ "$(state env_overrides_error_compose_manager)" != "" ] || fail "error not recorded"
    [ ! -e "${OVERRIDES_LAST_GOOD}.candidate" ] || fail "candidate left behind"
    # Nothing is retried, and the restored file is already applied.
    reset_log
    apply_env_overrides
    eq "$(docker_calls)" 0 "no retry loop"
}

case_rejected_value_can_be_retried_after_the_file_moves_on() {
    setup
    write_overrides "CM_MIN_TAG_AGE_HOURS='7'"
    apply_env_overrides
    write_overrides "CM_MIN_TAG_AGE_HOURS='9'"
    printf 'fail\nok\n' > "$T/health.seq"
    apply_env_overrides                       # rejected, file restored to 7
    apply_env_overrides                       # moves on; rejected hash forgotten
    reset_log
    write_overrides "CM_MIN_TAG_AGE_HOURS='9'"   # same value again
    apply_env_overrides
    eq "$(docker_calls)" 1 "same value is tried again"
    eq "$(state env_overrides_error_compose_manager)" "" "success clears the error"
}

case_unhealthy_without_last_good_removes_file() {
    setup
    write_overrides "CM_MIN_TAG_AGE_HOURS='9'"
    local bad_t; bad_t="$(lines_hash "CM_MIN_TAG_AGE_HOURS='9'")"
    printf 'fail\nok\n' > "$T/health.seq"
    apply_env_overrides
    eq "$(docker_calls)" 2 "attempt + restore recreate"
    [ ! -e "$OVERRIDES_FILE" ] || fail "bad file must be removed when no last-good exists"
    up_log | sed -n 2p | grep -q 'overrides_sha=$' || fail "restore recreate must run without overrides"
    eq "$(state env_overrides_rejected_sha256_compose_manager)" "$bad_t" "rejected sha"
    eq "$(state $CM_APPLIED)" "" "restored state: nothing applied"
}

case_docker_failure_is_handled_like_unhealthy() {
    setup
    write_overrides "CM_MIN_TAG_AGE_HOURS='9'"
    touch "$T/docker_fail"
    apply_env_overrides > /dev/null 2>&1
    [ ! -e "$OVERRIDES_FILE" ] || fail "file must be removed"
    [ "$(state env_overrides_error_compose_manager)" != "" ] || fail "error not recorded"
}

case_critical_when_restore_also_unhealthy() {
    setup
    write_overrides "CM_MIN_TAG_AGE_HOURS='9'"
    printf 'fail\nfail\n' > "$T/health.seq"
    apply_env_overrides 2>&1 | grep -q 'CRITICAL' || fail "CRITICAL not logged"
}

case_newer_file_written_mid_apply_is_not_clobbered() {
    setup
    write_overrides "CM_MIN_TAG_AGE_HOURS='9'"
    local bad_t; bad_t="$(lines_hash "CM_MIN_TAG_AGE_HOURS='9'")"
    printf "CM_MIN_TAG_AGE_HOURS='11'\n" > "$T/hook_overwrite"
    printf 'fail\nok\n' > "$T/health.seq"
    apply_env_overrides
    eq "$(cat "$OVERRIDES_FILE")" "CM_MIN_TAG_AGE_HOURS='11'" "newer file kept"
    eq "$(state env_overrides_rejected_sha256_compose_manager)" "$bad_t" "rejected the attempted one"
    # The next watch step applies the newer file.
    reset_log
    apply_env_overrides
    eq "$(docker_calls)" 1 "newer file applied"
    eq "$(state $CM_APPLIED)" "$(lines_hash "CM_MIN_TAG_AGE_HOURS='11'")" "applied newer"
}

case_invalid_file_makes_no_docker_call() {
    setup
    local n=0 bad
    for bad in \
        "COMPOSE_MANAGER_IMAGE='x'" \
        "BEARER_TOKEN=${SECRET}" \
        "BEARER_TOKEN='${SECRET}" \
        "bearer_token='${SECRET}'" \
        "BEARER_TOKEN='it'${SECRET}''" \
        "BEARER_TOKEN='a' extra" \
        "MONITORING_INGEST_TOKEN='short'" \
        "MONITORING_INGEST_TOKEN='${INGEST} with space'" \
        "MONITORING_INGEST_TOKEN='${INGEST},${INGEST2}'" \
        "MONITORING_INGEST_TOKEN='$(printf 'a%.0s' $(seq 1 513))'" \
        "garbage"; do
        n=$((n + 1))
        printf '%s\n' "$bad" > "$OVERRIDES_FILE"
        apply_env_overrides > /dev/null 2>&1
        eq "$(docker_calls)" 0 "docker called for invalid file #${n}"
        state env_overrides_error_compose_manager | grep -q '^invalid overrides file' || fail "cm error not set for #${n}"
        state env_overrides_error_otelcol_management | grep -q '^invalid overrides file' || fail "otel error not set for #${n}"
    done
    # Duplicate keys.
    write_overrides "BEARER_TOKEN='a'" "BEARER_TOKEN='b'"
    apply_env_overrides > /dev/null 2>&1
    eq "$(docker_calls)" 0 "duplicate keys"
    # CR and non-printable bytes.
    printf "BEARER_TOKEN='a'\r\n" > "$OVERRIDES_FILE"
    apply_env_overrides > /dev/null 2>&1
    printf "BEARER_TOKEN='a\tb'\n" > "$OVERRIDES_FILE"
    apply_env_overrides > /dev/null 2>&1
    eq "$(docker_calls)" 0 "CR / control characters"
    # Blank line in the middle.
    printf "BEARER_TOKEN='a'\n\nCM_MIN_TAG_AGE_HOURS='1'\n" > "$OVERRIDES_FILE"
    apply_env_overrides > /dev/null 2>&1
    eq "$(docker_calls)" 0 "blank line"
    # An unknown key next to a valid one must not apply the valid one either.
    write_overrides "BEARER_TOKEN='ok'" "WORK_DIR='/x'"
    apply_env_overrides > /dev/null 2>&1
    eq "$(docker_calls)" 0 "partially valid file"
    grep -q "$SECRET" "$STATE_FILE" && fail "state file leaked a value"
    # Once the file is valid again the stale error is cleared and it applies.
    write_overrides "CM_MIN_TAG_AGE_HOURS='1'"
    apply_env_overrides > /dev/null 2>&1
    eq "$(docker_calls)" 1 "fixed file applies"
    eq "$(state env_overrides_error_compose_manager)" "" "cm error cleared"
    eq "$(state env_overrides_error_otelcol_management)" "" "stale otel error cleared"
}

case_valid_file_with_special_characters_is_accepted() {
    setup
    write_overrides "BEARER_TOKEN='a\$b\"c\\d=e#f,g\`h'" "CM_MIN_TAG_AGE_HOURS='0'"
    apply_env_overrides
    eq "$(docker_calls)" 1 "special characters are literal"
}

case_clearing_overrides_reverts_and_drops_last_good() {
    setup
    write_overrides "CM_MIN_TAG_AGE_HOURS='7'"
    apply_env_overrides
    [ -f "$OVERRIDES_LAST_GOOD" ] || fail "no last-good after apply"
    rm -f "$OVERRIDES_FILE"; reset_log
    apply_env_overrides
    eq "$(docker_calls)" 1 "recreate to revert"
    up_log | sed -n 1p | grep -q 'overrides_sha=$' || fail "revert must run without overrides"
    eq "$(state $CM_APPLIED)" "" "applied empty"
    [ ! -e "$OVERRIDES_LAST_GOOD" ] || fail "stale last-good kept"
    apply_env_overrides
    eq "$(docker_calls)" 1 "stable afterwards"
}

# ── otelcol-management target ──────────────────────────────────────────

case_otel_happy_path() {
    setup
    write_overrides "MONITORING_INGEST_TOKEN='${INGEST}'"
    apply_env_overrides
    eq "$(docker_calls)" 1 "one recreate"
    local line; line="$(up_log | head -n1)"
    case "$line" in
        *"-f ${APP_COMPOSE_FILE} --env-file ${BASE_ENV_FILE} --env-file "*".otel-env."*" up -d --no-deps otelcol-management"*) ;;
        *) fail "unexpected compose args: $line" ;;
    esac
    case "$line" in *"$ENV_FILE"*|*"$OVERRIDES_FILE"*|*"--no-deps compose-manager"*) fail "must not pass .env.launcher, the full overrides or touch compose-manager: $line" ;; esac
    eq "$(cat "$T/otel.env.content")" "MONITORING_INGEST_TOKEN='${INGEST}'" "temp env content"
    eq "$(cat "$T/otel.env.mode")" "-rw-------" "temp env mode"
    [ ! -e "$(cat "$T/otel.env.path")" ] || fail "temp env file not removed"
    ls "$T"/.otel-env.* > /dev/null 2>&1 && fail "stray temp env file"
    eq "$(state $OT_APPLIED)" "$(lines_hash "MONITORING_INGEST_TOKEN='${INGEST}'")" "applied sha"
    eq "$(state $CM_APPLIED)" "<unset>" "compose-manager untouched"
    eq "$(grep -c ' inspect ' "$T/docker.log")" 4 "stability samples (1 + 9s/3s)"
    eq "$(tr '\n' ' ' < "$T/sleep.log")" "3 3 3 " "stability waits"
    cmp -s "$OVERRIDES_FILE" "$OVERRIDES_LAST_GOOD" || fail "last-good differs"
}

case_otel_only_gets_its_own_lines() {
    setup
    write_overrides "BEARER_TOKEN='${SECRET}'" "MONITORING_INGEST_TOKEN='${INGEST}'"
    apply_env_overrides
    eq "$(docker_calls)" 2 "both targets"
    eq "$(cat "$T/otel.env.content")" "MONITORING_INGEST_TOKEN='${INGEST}'" "only the otel key in the temp env"
    up_log | sed -n 1p | grep -q -- '--no-deps compose-manager' || fail "compose-manager first"
    up_log | sed -n 2p | grep -q -- '--no-deps otelcol-management' || fail "otelcol-management second"
}

case_changing_one_target_does_not_recreate_the_other() {
    setup
    write_overrides "BEARER_TOKEN='${SECRET}'" "CM_MIN_TAG_AGE_HOURS='7'" "MONITORING_INGEST_TOKEN='${INGEST}'"
    apply_env_overrides
    eq "$(docker_calls)" 2 "initial"
    reset_log
    write_overrides "BEARER_TOKEN='${SECRET}'" "CM_MIN_TAG_AGE_HOURS='7'" "MONITORING_INGEST_TOKEN='${INGEST2}'"
    apply_env_overrides
    eq "$(docker_calls)" 1 "only ingest token changed"
    up_log | grep -q -- '--no-deps otelcol-management' || fail "otel should have been recreated"
    reset_log
    write_overrides "BEARER_TOKEN='${SECRET}'" "CM_MIN_TAG_AGE_HOURS='8'" "MONITORING_INGEST_TOKEN='${INGEST2}'"
    apply_env_overrides
    eq "$(docker_calls)" 1 "only hours changed"
    up_log | grep -q -- '--no-deps compose-manager' || fail "compose-manager should have been recreated"
    # Removing the ingest token reverts only otelcol-management.
    reset_log
    write_overrides "BEARER_TOKEN='${SECRET}'" "CM_MIN_TAG_AGE_HOURS='8'"
    apply_env_overrides
    eq "$(docker_calls)" 1 "only ingest token removed"
    eq "$(cat "$T/otel.env.content" | wc -c | tr -d ' ')" 0 "revert uses an empty temp env"
    eq "$(state $OT_APPLIED)" "" "otel applied cleared"
}

case_otel_app_compose_missing_is_rejected_without_up() {
    setup
    rm -f "$APP_COMPOSE_FILE"
    write_overrides "MONITORING_INGEST_TOKEN='${INGEST}'"
    local h; h="$(lines_hash "MONITORING_INGEST_TOKEN='${INGEST}'")"
    apply_env_overrides > /dev/null 2>&1
    eq "$(docker_calls)" 0 "no compose up"
    state env_overrides_error_otelcol_management | grep -q 'not mounted' || fail "error must say the app-compose is not mounted"
    state env_overrides_error_otelcol_management | grep -q 'CVM template' || fail "error must mention the CVM template"
    eq "$(state env_overrides_rejected_sha256_otelcol_management)" "$h" "rejected sha"
    [ ! -e "$OVERRIDES_FILE" ] || fail "rejected file must be rolled back"
    grep -q "$INGEST" "$STATE_FILE" && fail "state leaked a value"
    # After the template gains the mount the same value can be submitted again.
    : > "$APP_COMPOSE_FILE"
    apply_env_overrides > /dev/null 2>&1
    write_overrides "MONITORING_INGEST_TOKEN='${INGEST}'"
    apply_env_overrides > /dev/null 2>&1
    eq "$(docker_calls)" 1 "applies once the mount exists"
}

case_otel_service_missing_is_rejected() {
    setup
    printf 'compose-manager\n' > "$T/services"
    write_overrides "MONITORING_INGEST_TOKEN='${INGEST}'"
    apply_env_overrides > /dev/null 2>&1
    eq "$(docker_calls)" 0 "no compose up"
    state env_overrides_error_otelcol_management | grep -q 'otelcol-management' || fail "error must name the service"
    grep -q 'config --services' "$T/docker.log" || fail "config --services not consulted"
}

case_preflight_failure_recreates_nothing() {
    setup
    rm -f "$APP_COMPOSE_FILE"
    write_overrides "CM_MIN_TAG_AGE_HOURS='7'" "MONITORING_INGEST_TOKEN='${INGEST}'"
    apply_env_overrides > /dev/null 2>&1
    eq "$(docker_calls)" 0 "compose-manager must not be recreated when otel preflight fails"
    [ ! -e "$OVERRIDES_FILE" ] || fail "whole file rolled back"
}

case_probe_rejects_401_and_403() {
    local code
    for code in 401 403; do
        setup
        export LAUNCHER_TELEMETRY_PROBE_URL="http://probe.test/v1/logs"
        # shellcheck disable=SC2034  # read by the sourced launcher
        TELEMETRY_PROBE_URL="$LAUNCHER_TELEMETRY_PROBE_URL"
        echo "$code" > "$T/probe.mode"
        write_overrides "MONITORING_INGEST_TOKEN='${INGEST}'"
        apply_env_overrides > /dev/null 2>&1
        eq "$(docker_calls)" 0 "no compose up on HTTP ${code}"
        state env_overrides_error_otelcol_management | grep -q "HTTP ${code}" || fail "error must name the status"
        [ ! -e "$OVERRIDES_FILE" ] || fail "rejected file must be rolled back"
    done
}

case_probe_200_and_network_error_proceed_and_token_stays_out_of_argv() {
    local mode
    for mode in 200 500 neterr; do
        setup
        # shellcheck disable=SC2034  # read by the sourced launcher
        TELEMETRY_PROBE_URL="http://probe.test/v1/logs"
        echo "$mode" > "$T/probe.mode"
        write_overrides "MONITORING_INGEST_TOKEN='${INGEST}'"
        apply_env_overrides > /dev/null 2>&1
        eq "$(docker_calls)" 1 "probe ${mode} must proceed"
        eq "$(cat "$T/probe.hdr")" "Authorization: Bearer ${INGEST}" "header file content"
        grep -q "$INGEST" "$T/curl.log" && fail "token in curl argv"
        grep -q -- '--max-time 10' "$T/curl.log" || fail "probe timeout"
        grep -q -- "-d {}" "$T/curl.log" || fail "probe body"
        grep -q 'Content-Type: application/json' "$T/curl.log" || fail "probe content type"
        grep -q "$INGEST" "$T/docker.log" && fail "token in docker argv"
    done
    # No probe configured: curl probe is never called.
    setup
    write_overrides "MONITORING_INGEST_TOKEN='${INGEST}'"
    apply_env_overrides > /dev/null 2>&1
    [ ! -e "$T/curl.log" ] || fail "probe called without LAUNCHER_TELEMETRY_PROBE_URL"
}

case_otel_instability_rolls_back_to_last_good() {
    setup
    write_overrides "MONITORING_INGEST_TOKEN='${INGEST}'"
    apply_env_overrides
    local good_t; good_t="$(lines_hash "MONITORING_INGEST_TOKEN='${INGEST}'")"
    local bad_t; bad_t="$(lines_hash "MONITORING_INGEST_TOKEN='${INGEST2}'")"
    local mode seq
    for mode in restart notrunning restarting; do
        reset_log
        case "$mode" in
            restart)    seq=$'true false 0\ntrue false 1' ;;     # RestartCount increased
            notrunning) seq=$'true false 0\nfalse false 0' ;;    # exited
            restarting) seq=$'true false 0\ntrue true 0' ;;      # restarting
        esac
        write_overrides "MONITORING_INGEST_TOKEN='${INGEST2}'"
        printf '%s\n' "$seq" > "$T/inspect.seq"
        apply_env_overrides > /dev/null 2>&1
        eq "$(docker_calls)" 2 "${mode}: attempt + restore"
        eq "$(cat "$OVERRIDES_FILE")" "MONITORING_INGEST_TOKEN='${INGEST}'" "${mode}: file restored"
        eq "$(cat "$T/otel.env.content")" "MONITORING_INGEST_TOKEN='${INGEST}'" "${mode}: restore uses the old token"
        eq "$(state $OT_APPLIED)" "$good_t" "${mode}: applied is last good"
        eq "$(state env_overrides_rejected_sha256_otelcol_management)" "$bad_t" "${mode}: rejected"
        [ "$(state env_overrides_error_otelcol_management)" != "" ] || fail "${mode}: error not set"
        ls "$T"/.otel-env.* > /dev/null 2>&1 && fail "${mode}: stray temp env file"
        : > "$T/inspect.seq"
        apply_env_overrides > /dev/null 2>&1   # next watch tick: forgets the rejected hash
    done
}

case_whole_file_rollback_when_second_target_fails() {
    setup
    write_overrides "CM_MIN_TAG_AGE_HOURS='7'" "MONITORING_INGEST_TOKEN='${INGEST}'"
    apply_env_overrides
    local cm_good; cm_good="$(lines_hash "CM_MIN_TAG_AGE_HOURS='7'")"
    local ot_good; ot_good="$(lines_hash "MONITORING_INGEST_TOKEN='${INGEST}'")"
    local ot_bad; ot_bad="$(lines_hash "MONITORING_INGEST_TOKEN='${INGEST2}'")"
    reset_log
    write_overrides "CM_MIN_TAG_AGE_HOURS='9'" "MONITORING_INGEST_TOKEN='${INGEST2}'"
    printf 'true false 0\ntrue false 5\n' > "$T/inspect.seq"
    apply_env_overrides > /dev/null 2>&1
    eq "$(docker_calls)" 4 "cm, otel, then both re-applied"
    up_log | sed -n 1p | grep -q -- '--no-deps compose-manager' || fail "1: cm"
    up_log | sed -n 2p | grep -q -- '--no-deps otelcol-management' || fail "2: otel"
    up_log | sed -n 3p | grep -q -- '--no-deps compose-manager' || fail "3: cm restore"
    up_log | sed -n 4p | grep -q -- '--no-deps otelcol-management' || fail "4: otel restore"
    eq "$(cat "$OVERRIDES_FILE")" "$(printf "CM_MIN_TAG_AGE_HOURS='7'\nMONITORING_INGEST_TOKEN='%s'" "$INGEST")" "whole file restored"
    eq "$(state $CM_APPLIED)" "$cm_good" "cm applied back to last good"
    eq "$(state $OT_APPLIED)" "$ot_good" "otel applied back to last good"
    eq "$(state env_overrides_rejected_sha256_otelcol_management)" "$ot_bad" "otel rejected"
    eq "$(state env_overrides_error_compose_manager)" "" "cm has no error"
    [ "$(state env_overrides_error_otelcol_management)" != "" ] || fail "otel error not set"
}

case_whole_file_rollback_without_last_good_deletes_file() {
    setup
    write_overrides "CM_MIN_TAG_AGE_HOURS='9'" "MONITORING_INGEST_TOKEN='${INGEST}'"
    printf 'true false 0\nfalse false 0\n' > "$T/inspect.seq"
    apply_env_overrides > /dev/null 2>&1
    [ ! -e "$OVERRIDES_FILE" ] || fail "file must be deleted"
    eq "$(docker_calls)" 4 "cm, otel, both reverted"
    up_log | sed -n 3p | grep -q 'overrides_sha=$' || fail "cm revert must run without overrides"
    eq "$(state $CM_APPLIED)" "" "cm reverted"
    eq "$(state $OT_APPLIED)" "" "otel reverted"
}

case_temp_env_removed_when_compose_up_fails() {
    setup
    write_overrides "MONITORING_INGEST_TOKEN='${INGEST}'"
    echo "otelcol-management" > "$T/docker_fail_on"
    apply_env_overrides > /dev/null 2>&1
    ls "$T"/.otel-env.* > /dev/null 2>&1 && fail "temp env file left after a failed compose up"
    [ ! -e "$(cat "$T/otel.env.path")" ] || fail "temp env file not removed"
    [ "$(state env_overrides_error_otelcol_management)" != "" ] || fail "error not set"
}

# ── Bootstrap and polling ──────────────────────────────────────────────

case_bootstrap_forces_apply_of_existing_file() {
    setup
    write_overrides "CM_MIN_TAG_AGE_HOURS='7'" "MONITORING_INGEST_TOKEN='${INGEST}'"
    apply_env_overrides
    reset_log
    # Same hashes as already applied: a CVM reboot recreated both containers
    # without overrides, so bootstrap must re-apply both anyway.
    bootstrap_env_overrides
    eq "$(docker_calls)" 2 "forced apply of both targets"
    up_log | sed -n 1p | grep -q -- '--no-deps compose-manager' || fail "compose-manager first"
    up_log | sed -n 1p | grep -q "overrides_sha=$(file_sha "$OVERRIDES_FILE")" || fail "forced apply must use the file"
    up_log | sed -n 2p | grep -q -- '--no-deps otelcol-management' || fail "otelcol-management second"
    # A target without keys is not touched.
    write_overrides "MONITORING_INGEST_TOKEN='${INGEST}'"
    apply_env_overrides
    reset_log
    bootstrap_env_overrides
    eq "$(docker_calls)" 1 "only the target with keys"
}

case_bootstrap_without_file_marks_launcher_as_reporting() {
    setup
    touch "$T/.otel-env.stale"
    bootstrap_env_overrides
    eq "$(docker_calls)" 0 "no file, no docker call"
    eq "$(state $CM_APPLIED)" "" "cm key present and empty"
    eq "$(state $OT_APPLIED)" "" "otel key present and empty"
    [ ! -e "$T/.otel-env.stale" ] || fail "stale temp env file kept"
    # Does not clobber a previously recorded value.
    write_state "$CM_APPLIED" "abc"
    bootstrap_env_overrides
    eq "$(state $CM_APPLIED)" "abc" "existing state preserved"
}

case_poll_wait_checks_overrides_in_steps() {
    setup
    : > "$T/apply.calls"
    apply_env_overrides() { echo x >> "$T/apply.calls"; }
    wait_with_env_watch
    eq "$(tr '\n' ' ' < "$T/sleep.log")" "5 5 2 " "sleep steps sum to POLL_INTERVAL"
    eq "$(wc -l < "$T/apply.calls" | tr -d ' ')" 3 "apply calls"
    # A nonsense watch interval falls back instead of spinning.
    : > "$T/sleep.log"; ENV_WATCH_INTERVAL=0 POLL_INTERVAL=10 wait_with_env_watch
    eq "$(tr '\n' ' ' < "$T/sleep.log")" "5 5 " "fallback interval"
}

# ── Parity ─────────────────────────────────────────────────────────────

case_allowlist_and_target_parity() {
    local launcher compose rust readme map_launcher map_rust
    launcher="$(bash -c 'COMPOSE_MANAGER_IMAGE_REPO=x source "$1"; printf "%s\n" "${ALLOWED_OVERRIDE_KEYS[@]}"' _ "$LAUNCHER" | sort | tr '\n' ' ')"
    rust="$(grep -E '^(pub(\(crate\))? )?const ALLOWED_KEYS' "$RUST_SRC" | grep -o '"[A-Z_][A-Z0-9_]*"' | tr -d '"' | sort | tr '\n' ' ')"
    readme="$(sed -n '/allowed-keys:start/,/allowed-keys:end/p' "$README" | grep -o '^| `[A-Z_][A-Z0-9_]*`' | tr -d '|` ' | sort | tr '\n' ' ')"
    [ -n "$launcher" ] || fail "could not read launcher allowlist"
    eq "$rust" "$launcher" "rust vs launcher allowlist"
    eq "$readme" "$launcher" "README table vs launcher allowlist"
    # compose-manager keys must be the non-plumbing substitutions of the bundled compose.
    compose="$(grep -o '\${[A-Z_][A-Z0-9_]*' "$COMPOSE_YML" | tr -d '${' | sort -u \
        | grep -vx -e COMPOSE_MANAGER_IMAGE -e CM_GITHUB_REPO | tr '\n' ' ')"
    eq "$compose" "$(bash -c 'COMPOSE_MANAGER_IMAGE_REPO=x source "$1"; printf "%s\n" "${TARGET_COMPOSE_MANAGER_KEYS[@]}"' _ "$LAUNCHER" | sort | tr '\n' ' ')" "compose substitutions vs compose-manager target keys"
    # key -> target mapping: launcher arrays vs Rust KEY_TARGETS.
    map_launcher="$(bash -c 'COMPOSE_MANAGER_IMAGE_REPO=x source "$1"
        for k in "${TARGET_COMPOSE_MANAGER_KEYS[@]}"; do echo "$k=compose-manager"; done
        for k in "${TARGET_OTELCOL_MANAGEMENT_KEYS[@]}"; do echo "$k=otelcol-management"; done' _ "$LAUNCHER" | sort | tr '\n' ' ')"
    map_rust="$(sed -n '/^const KEY_TARGETS/,/^];/p' "$RUST_SRC" \
        | grep -o '("[A-Z_][A-Z0-9_]*", TARGET_[A-Z_]*)' \
        | sed -e 's/("\([A-Z_0-9]*\)", TARGET_COMPOSE_MANAGER)/\1=compose-manager/' \
              -e 's/("\([A-Z_0-9]*\)", TARGET_OTELCOL)/\1=otelcol-management/' \
        | sort | tr '\n' ' ')"
    eq "$map_rust" "$map_launcher" "rust KEY_TARGETS vs launcher target arrays"
}

CASES="
compose_up_puts_overrides_last
apply_success_updates_state_and_last_good
state_uses_per_target_keys_only
unchanged_hash_is_a_noop
unhealthy_restores_last_good_and_records_rejection
rejected_value_can_be_retried_after_the_file_moves_on
unhealthy_without_last_good_removes_file
docker_failure_is_handled_like_unhealthy
critical_when_restore_also_unhealthy
newer_file_written_mid_apply_is_not_clobbered
invalid_file_makes_no_docker_call
valid_file_with_special_characters_is_accepted
clearing_overrides_reverts_and_drops_last_good
otel_happy_path
otel_only_gets_its_own_lines
changing_one_target_does_not_recreate_the_other
otel_app_compose_missing_is_rejected_without_up
otel_service_missing_is_rejected
preflight_failure_recreates_nothing
probe_rejects_401_and_403
probe_200_and_network_error_proceed_and_token_stays_out_of_argv
otel_instability_rolls_back_to_last_good
whole_file_rollback_when_second_target_fails
whole_file_rollback_without_last_good_deletes_file
temp_env_removed_when_compose_up_fails
bootstrap_forces_apply_of_existing_file
bootstrap_without_file_marks_launcher_as_reporting
poll_wait_checks_overrides_in_steps
allowlist_and_target_parity
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

# No case may have printed or stored a value anywhere.
if grep -rqF -e "$SECRET" -e "$INGEST" -e "$INGEST2" "$ROOT" --include='*.out' --include='*.json' --include='*.log'; then
    echo "FAIL no_value_in_logs_or_state"
    FAILED=$((FAILED + 1))
else
    echo "PASS no_value_in_logs_or_state"
    PASSED=$((PASSED + 1))
fi

echo "${PASSED} passed, ${FAILED} failed"
[ "$FAILED" -eq 0 ]
