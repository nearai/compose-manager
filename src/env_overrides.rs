//! Runtime overrides for compose-manager's own container environment.
//!
//! `PUT /admin/env` writes `$WORK_DIR/env-overrides` on the shared `work`
//! volume. The launcher (launcher/launcher.sh) notices the file, recreates ONLY
//! the compose-manager container with the file as the last `--env-file`, and
//! reports the outcome in `launcher-state.json`. `GET /admin/env` merges both
//! views. No CVM restart and no model restart is involved.
//!
//! Contract shared with the launcher (keep in sync, parity is tested):
//! - file format: one `KEY='VALUE'` line per key, keys sorted, trailing
//!   newline, mode 0600; a missing or empty file means "no overrides";
//! - allowlist: [`ALLOWED_KEYS`], which must equal the launcher's
//!   `ALLOWED_OVERRIDE_KEYS` and the README table;
//! - targets: each key belongs to exactly one container the launcher
//!   recreates ([`KEY_TARGETS`]). `BEARER_TOKEN` and `CM_MIN_TAG_AGE_HOURS`
//!   are substitutions in `launcher/docker-compose.yml` (compose-manager);
//!   `MONITORING_INGEST_TOKEN` is consumed by `otelcol-management`, which
//!   lives in the CVM app-compose. The launcher keeps applied/rejected state
//!   per target in `launcher-state.json`.
//!
//! Values are secrets (BEARER_TOKEN): they are never logged, never echoed in
//! responses or error messages, and never recorded in the action log.

use crate::{
    err, extract_actor, notify, record_action, token_eq, verify_bearer_token, AppState,
    DeploymentAction,
};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use sha2::Digest;
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Environment variables that may be overridden. Keep identical to the
/// launcher's `ALLOWED_OVERRIDE_KEYS` and the README table.
pub(crate) const ALLOWED_KEYS: &[&str] = &["BEARER_TOKEN", "CM_MIN_TAG_AGE_HOURS", "MONITORING_INGEST_TOKEN"];

const TARGET_COMPOSE_MANAGER: &str = "compose-manager";
const TARGET_OTELCOL: &str = "otelcol-management";
/// Targets in launcher apply order.
const TARGETS: &[&str] = &[TARGET_COMPOSE_MANAGER, TARGET_OTELCOL];
/// Key to target mapping. Mirrors the launcher's `TARGET_*_KEYS` arrays.
const KEY_TARGETS: &[(&str, &str)] = &[
    ("BEARER_TOKEN", TARGET_COMPOSE_MANAGER),
    ("CM_MIN_TAG_AGE_HOURS", TARGET_COMPOSE_MANAGER),
    ("MONITORING_INGEST_TOKEN", TARGET_OTELCOL),
];

const OVERRIDES_FILE_NAME: &str = "env-overrides";
const LAUNCHER_STATE_FILE_NAME: &str = "launcher-state.json";
const MAX_VALUE_LEN: usize = 4096;
/// Launcher error text is relayed to API clients; bound it.
const MAX_LAUNCHER_ERROR_LEN: usize = 500;

/// Serializes read-modify-write cycles on the overrides file. Deliberately NOT
/// the compose lock: changing env must not wait for (or block) a long
/// `compose up`.
static ENV_OVERRIDES_LOCK: Mutex<()> = Mutex::const_new(());

fn allowed_keys_list() -> String {
    ALLOWED_KEYS.join(", ")
}

fn is_allowed_key(key: &str) -> bool {
    ALLOWED_KEYS.contains(&key)
}

fn target_of(key: &str) -> Option<&'static str> {
    KEY_TARGETS.iter().find(|(k, _)| *k == key).map(|(_, t)| *t)
}

/// Suffix of the launcher state keys for a target (`otelcol-management` ->
/// `otelcol_management`).
fn state_suffix(target: &str) -> String {
    target.replace('-', "_")
}

/// The part of `map` that belongs to `target`.
fn target_subset(map: &BTreeMap<String, String>, target: &str) -> BTreeMap<String, String> {
    map.iter()
        .filter(|(k, _)| target_of(k) == Some(target))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Per-target hash: sha256 over that target's rendered lines only (what the
/// launcher hashes); `None` when the target has no keys.
fn target_sha256(map: &BTreeMap<String, String>, target: &str) -> Option<String> {
    let subset = target_subset(map, target);
    if subset.is_empty() {
        None
    } else {
        Some(sha256_hex(render(&subset).as_bytes()))
    }
}

fn overrides_path(work_dir: &Path) -> PathBuf {
    work_dir.join(OVERRIDES_FILE_NAME)
}

// --- Validation (error messages never include the offending value) ---

/// Validates `value` for `key`. The returned message names the key and the
/// rule that failed, never the value itself.
fn validate_value(key: &str, value: &str) -> Result<(), String> {
    if !is_allowed_key(key) {
        return Err(format!("unknown key; allowed keys: {}", allowed_keys_list()));
    }
    if value.is_empty() || value.len() > MAX_VALUE_LEN {
        return Err(format!("{key}: value must be 1..={MAX_VALUE_LEN} bytes"));
    }
    if !value.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(format!("{key}: value must be printable ASCII"));
    }
    if value.contains('\'') {
        return Err(format!("{key}: value must not contain a single quote"));
    }
    match key {
        "BEARER_TOKEN" => {
            if value.bytes().any(|b| b == b' ') {
                return Err(format!("{key}: value must not contain whitespace"));
            }
            if value.split(',').any(str::is_empty) {
                return Err(format!(
                    "{key}: comma-separated list must not contain empty tokens"
                ));
            }
        }
        "CM_MIN_TAG_AGE_HOURS" => {
            if !value.bytes().all(|b| b.is_ascii_digit()) || value.parse::<u32>().is_err() {
                return Err(format!("{key}: value must be a non-negative integer (u32)"));
            }
        }
        "MONITORING_INGEST_TOKEN" => {
            let ok_len = (16..=512).contains(&value.len());
            let ok_chars = value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/=_.~-".contains(&b));
            if !ok_len || !ok_chars {
                return Err(format!(
                    "{key}: value must be 16..=512 characters from [A-Za-z0-9+/=_.~-]"
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct EnvRequest {
    #[serde(default)]
    set: BTreeMap<String, String>,
    #[serde(default)]
    unset: Vec<String>,
}

/// Validates a whole request against the authenticating token. Lockout guard:
/// a new BEARER_TOKEN list must still contain the token that authenticated
/// this request.
fn validate_request(req: &EnvRequest, authed_token: &str) -> Result<(), String> {
    if req.set.is_empty() && req.unset.is_empty() {
        return Err("body must contain a non-empty \"set\" object or \"unset\" array".into());
    }
    for (key, value) in &req.set {
        validate_value(key, value)?;
    }
    for key in &req.unset {
        if !is_allowed_key(key) {
            return Err(format!("unknown key; allowed keys: {}", allowed_keys_list()));
        }
        if req.set.contains_key(key) {
            return Err(format!("{key}: cannot be in both \"set\" and \"unset\""));
        }
    }
    if let Some(list) = req.set.get("BEARER_TOKEN") {
        if !list.split(',').any(|t| token_eq(t, authed_token)) {
            return Err(
                "BEARER_TOKEN must still contain the token used to authenticate this request, \
                 otherwise you would lock yourself out. Rotation is two-step: first set \
                 BEARER_TOKEN to \"<current>,<new>\", then make a second call authenticated \
                 with the new token that sets BEARER_TOKEN to \"<new>\" alone."
                    .into(),
            );
        }
    }
    Ok(())
}

// --- File format ---

/// Renders overrides as `KEY='VALUE'\n` lines. `BTreeMap` iteration is sorted.
fn render(map: &BTreeMap<String, String>) -> String {
    map.iter().map(|(k, v)| format!("{k}='{v}'\n")).collect()
}

/// Parses file content, keeping only well-formed lines for allowed keys with
/// valid values. Returns the map and the number of lines dropped (callers log
/// the count only, never the content).
fn parse(content: &str) -> (BTreeMap<String, String>, usize) {
    let mut map = BTreeMap::new();
    let mut dropped = 0;
    for line in content.lines().filter(|l| !l.is_empty()) {
        let parsed = line
            .split_once("='")
            .and_then(|(k, rest)| rest.strip_suffix('\'').map(|v| (k, v)))
            .filter(|(k, v)| validate_value(k, v).is_ok());
        match parsed {
            Some((k, v)) => {
                map.insert(k.to_string(), v.to_string());
            }
            None => dropped += 1,
        }
    }
    (map, dropped)
}

fn apply_update(current: &mut BTreeMap<String, String>, set: &BTreeMap<String, String>, unset: &[String]) {
    for (k, v) in set {
        current.insert(k.clone(), v.clone());
    }
    for k in unset {
        current.remove(k);
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Reads the overrides file. Missing or empty file yields an empty map and no
/// hash. The hash is over the raw file bytes (what the launcher hashes).
async fn read_overrides(work_dir: &Path) -> io::Result<(BTreeMap<String, String>, Option<String>)> {
    let bytes = match tokio::fs::read(overrides_path(work_dir)).await {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((BTreeMap::new(), None)),
        Err(e) => return Err(e),
    };
    if bytes.is_empty() {
        return Ok((BTreeMap::new(), None));
    }
    let sha = sha256_hex(&bytes);
    let (map, dropped) = parse(&String::from_utf8_lossy(&bytes));
    if dropped > 0 {
        warn!(dropped_lines = dropped, "Ignoring malformed lines in env overrides file");
    }
    Ok((map, Some(sha)))
}

/// Writes the overrides atomically (tmp file in the same dir, 0600, fsync,
/// rename), or removes the file when `map` is empty. Returns the sha256 of the
/// written bytes (`None` when removed).
async fn write_overrides(work_dir: &Path, map: &BTreeMap<String, String>) -> io::Result<Option<String>> {
    use tokio::io::AsyncWriteExt;

    let target = overrides_path(work_dir);
    if map.is_empty() {
        match tokio::fs::remove_file(&target).await {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        fsync_dir(work_dir).await;
        return Ok(None);
    }

    let content = render(map);
    let tmp = work_dir.join(format!(".{}.{:016x}.tmp", OVERRIDES_FILE_NAME, rand::random::<u64>()));
    let res = async {
        let mut f = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .await?;
        f.write_all(content.as_bytes()).await?;
        f.flush().await?;
        f.sync_all().await?;
        drop(f);
        tokio::fs::rename(&tmp, &target).await
    }
    .await;
    if let Err(e) = res {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    fsync_dir(work_dir).await;
    Ok(Some(sha256_hex(content.as_bytes())))
}

/// Best-effort: persist a rename/unlink by fsyncing the parent directory.
async fn fsync_dir(dir: &Path) {
    if let Ok(d) = tokio::fs::File::open(dir).await {
        let _ = d.sync_all().await;
    }
}

// --- Launcher state ---

/// What the launcher reported for one target.
#[derive(Default)]
struct TargetView {
    applied_sha256: Option<String>,
    error: Option<String>,
}

struct LauncherView {
    /// True when the state file has at least one per-target applied key. An
    /// older launcher image that ignores the overrides file never writes them.
    reported: bool,
    targets: BTreeMap<&'static str, TargetView>,
}

/// Reads the launcher's `launcher-state.json`. A missing/unparseable file, or a
/// state with none of the per-target `env_overrides_applied_sha256_<target>`
/// keys, means the launcher has not reported.
async fn read_launcher_view(work_dir: &Path) -> LauncherView {
    let mut view = LauncherView {
        reported: false,
        targets: TARGETS.iter().map(|t| (*t, TargetView::default())).collect(),
    };
    let Ok(text) = tokio::fs::read_to_string(work_dir.join(LAUNCHER_STATE_FILE_NAME)).await else {
        return view;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return view;
    };
    for target in TARGETS {
        let suffix = state_suffix(target);
        let get = |prefix: &str| {
            json.get(format!("{prefix}_{suffix}"))
                .and_then(|v| v.as_str())
        };
        let tv = view.targets.entry(target).or_default();
        if let Some(applied) = get("env_overrides_applied_sha256") {
            view.reported = true;
            tv.applied_sha256 = Some(applied.to_string()).filter(|s| !s.is_empty());
        }
        tv.error = get("env_overrides_error")
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().filter(|c| !c.is_control()).take(MAX_LAUNCHER_ERROR_LEN).collect());
    }
    view
}

// --- Handlers ---

/// `PUT /admin/env`: merge `set`/`unset` into the overrides file.
pub(crate) async fn put_env(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.bearer_tokens) {
        return e.into_response();
    }
    // Authenticated, so the header is well-formed; this is the token the
    // lockout guard protects.
    let authed_token = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();

    // Parsed by hand (not the Json extractor) so auth runs first and serde
    // errors, which can quote input, never reach the client.
    let req: EnvRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            return err(
                StatusCode::BAD_REQUEST,
                "invalid JSON body; expected {\"set\": {\"KEY\": \"VALUE\"}, \"unset\": [\"KEY\"]}",
            )
            .into_response();
        }
    };
    if let Err(msg) = validate_request(&req, authed_token) {
        return err(StatusCode::BAD_REQUEST, msg).into_response();
    }

    let (keys, sha256, targets) = {
        let _guard = ENV_OVERRIDES_LOCK.lock().await;
        let (mut current, _) = match read_overrides(&state.work_dir).await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "Failed to read env overrides file");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "failed to read env overrides file")
                    .into_response();
            }
        };
        apply_update(&mut current, &req.set, &req.unset);
        let sha256 = match write_overrides(&state.work_dir, &current).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "Failed to write env overrides file");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "failed to write env overrides file")
                    .into_response();
            }
        };
        let targets: Vec<&str> = TARGETS
            .iter()
            .copied()
            .filter(|t| !target_subset(&current, t).is_empty())
            .collect();
        (current.keys().cloned().collect::<Vec<_>>(), sha256, targets)
    };

    let actor = extract_actor(&headers);
    info!(
        set_keys = ?req.set.keys().collect::<Vec<_>>(),
        unset_keys = ?req.unset,
        sha256_prefix = sha256.as_deref().map(|s| &s[..12]).unwrap_or("none"),
        actor = %actor,
        "Env overrides updated"
    );
    for (name, changed) in [
        ("env_override_set", req.set.keys().cloned().collect::<Vec<_>>()),
        ("env_override_unset", req.unset.clone()),
    ] {
        if changed.is_empty() {
            continue;
        }
        // Key names only, never values. The audit record carries the changed
        // keys in `services` and the resulting file hash.
        let action = DeploymentAction {
            timestamp: Utc::now().to_rfc3339(),
            action: name.into(),
            file: Some(OVERRIDES_FILE_NAME.into()),
            file_sha256: sha256.clone(),
            services: changed,
            container: Some("compose-manager".into()),
            ..Default::default()
        };
        // Persist failures are logged inside record_action; the file is
        // already written, so the request still succeeds.
        let _ = record_action(&state, &action).await;
        notify::spawn_action(&state, &action, &actor, Some(true));
    }

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "accepted",
            "keys": keys,
            "sha256": sha256,
            "targets": targets,
        })),
    )
        .into_response()
}

/// `GET /admin/env`: current overrides (keys and hash only) plus the
/// launcher's applied state.
pub(crate) async fn get_env(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.bearer_tokens) {
        return e.into_response();
    }
    let (map, _) = match read_overrides(&state.work_dir).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Failed to read env overrides file");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "failed to read env overrides file")
                .into_response();
        }
    };
    let launcher = read_launcher_view(&state.work_dir).await;
    let mut targets = serde_json::Map::new();
    for target in TARGETS {
        let sha256 = target_sha256(&map, target);
        let view = launcher.targets.get(target);
        let applied = view.and_then(|v| v.applied_sha256.clone());
        let pending = sha256 != applied;
        targets.insert(
            (*target).to_string(),
            serde_json::json!({
                "keys": target_subset(&map, target).keys().collect::<Vec<_>>(),
                "sha256": sha256,
                "applied_sha256": applied,
                "pending": pending,
                "error": view.and_then(|v| v.error.clone()),
            }),
        );
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "launcher_reported": launcher.reported,
            "targets": targets,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, routing::get, Router};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Mutex as StdMutex, RwLock as StdRwLock};
    use tokio::sync::{Mutex, RwLock};
    use tower::ServiceExt;

    const TOKEN: &str = "test-token";
    const SECRET: &str = "s3cr3t-VALUE-must-never-leak";

    fn temp_work_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cm-envov-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_state(work_dir: &Path) -> Arc<AppState> {
        test_state_with_tokens(work_dir, vec![TOKEN.into(), "other-token".into()])
    }

    fn test_state_with_tokens(work_dir: &Path, bearer_tokens: Vec<String>) -> Arc<AppState> {
        Arc::new(AppState {
            bearer_tokens,
            github_owner: "test".into(),
            github_repo_name: "test".into(),
            min_tag_age_hours: 0,
            work_dir: work_dir.to_path_buf(),
            env_files: vec![],
            slack_webhook_url: None,
            instance_label: "test-host".into(),
            deployed_projects: StdRwLock::new(Default::default()),
            deployed_tag: StdRwLock::new(None),
            deployed_commit: StdRwLock::new(None),
            deployed_file: StdRwLock::new(None),
            deployed_file_sha256: StdRwLock::new(None),
            actions: RwLock::new(vec![]),
            http: reqwest::Client::new(),
            compose_lock: Arc::new(Mutex::new(())),
            in_flight: Arc::new(StdMutex::new(None)),
            running_image: None,
        })
    }

    fn app(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/admin/env", get(get_env).put(put_env))
            .with_state(state)
    }

    async fn call(
        state: &Arc<AppState>,
        method: &str,
        token: Option<&str>,
        body: &str,
    ) -> (StatusCode, String) {
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri("/admin/env")
            .header("Content-Type", "application/json");
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let resp = app(state.clone())
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn json(body: &str) -> serde_json::Value {
        serde_json::from_str(body).unwrap()
    }

    // --- value validation ---

    #[test]
    fn bearer_token_value_rules() {
        for good in ["abc", "a,b", "tok-en_1.2~x", "a,b,c", &"a".repeat(4096), "p@ss$w\"rd\\"] {
            assert!(validate_value("BEARER_TOKEN", good).is_ok(), "should accept {good:?}");
        }
        for bad in [
            "", "a b", " a", "a ", "a,", ",a", "a,,b", ",", "it's", "caf\u{e9}", "a\tb", "a\nb",
            "a\rb", "a\0b", &"a".repeat(4097),
        ] {
            assert!(validate_value("BEARER_TOKEN", bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn min_tag_age_value_rules() {
        for good in ["0", "2", "48", "007", "4294967295"] {
            assert!(validate_value("CM_MIN_TAG_AGE_HOURS", good).is_ok(), "should accept {good:?}");
        }
        for bad in ["", "-1", "+1", "1.5", "abc", "1 ", " 1", "4294967296", "1e3", "0x10", "1'", "\u{661}"] {
            assert!(validate_value("CM_MIN_TAG_AGE_HOURS", bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn unknown_keys_are_rejected_and_not_echoed() {
        for key in ["COMPOSE_MANAGER_IMAGE", "CM_GITHUB_REPO", "WORK_DIR", "ENV_FILES", "PATH", "bearer_token", ""] {
            let e = validate_value(key, "1").unwrap_err();
            assert!(e.contains("BEARER_TOKEN") && e.contains("CM_MIN_TAG_AGE_HOURS"), "{e}");
        }
        let req = EnvRequest { unset: vec!["WORK_DIR".into()], ..Default::default() };
        let e = validate_request(&req, TOKEN).unwrap_err();
        assert!(!e.contains("WORK_DIR"), "unknown key name must not be echoed: {e}");
    }

    #[test]
    fn validation_errors_never_contain_the_value() {
        let bad = format!("{SECRET} with space");
        let e = validate_value("BEARER_TOKEN", &bad).unwrap_err();
        assert!(!e.contains(SECRET), "{e}");
        let e = validate_value("CM_MIN_TAG_AGE_HOURS", SECRET).unwrap_err();
        assert!(!e.contains(SECRET), "{e}");
    }

    #[test]
    fn request_rules() {
        let empty = EnvRequest::default();
        assert!(validate_request(&empty, TOKEN).is_err());
        let both = EnvRequest {
            set: map(&[("CM_MIN_TAG_AGE_HOURS", "1")]),
            unset: vec!["CM_MIN_TAG_AGE_HOURS".into()],
        };
        assert!(validate_request(&both, TOKEN).is_err());
        let ok = EnvRequest { set: map(&[("CM_MIN_TAG_AGE_HOURS", "1")]), unset: vec!["BEARER_TOKEN".into()] };
        assert!(validate_request(&ok, TOKEN).is_ok());
    }

    #[test]
    fn lockout_guard_requires_authenticating_token() {
        let without = EnvRequest { set: map(&[("BEARER_TOKEN", "new-token")]), ..Default::default() };
        let e = validate_request(&without, TOKEN).unwrap_err();
        assert!(e.contains("two-step"), "{e}");
        assert!(!e.contains("new-token") && !e.contains(TOKEN), "{e}");
        let alongside = EnvRequest { set: map(&[("BEARER_TOKEN", "test-token,new-token")]), ..Default::default() };
        assert!(validate_request(&alongside, TOKEN).is_ok());
        let new_only_with_new = EnvRequest { set: map(&[("BEARER_TOKEN", "new-token")]), ..Default::default() };
        assert!(validate_request(&new_only_with_new, "new-token").is_ok());
        // A prefix/superstring of the authenticating token is not a match.
        let near = EnvRequest { set: map(&[("BEARER_TOKEN", "test-token2,test-toke")]), ..Default::default() };
        assert!(validate_request(&near, TOKEN).is_err());
        // Unsetting BEARER_TOKEN is allowed (reverts to the CVM env value).
        let unset = EnvRequest { unset: vec!["BEARER_TOKEN".into()], ..Default::default() };
        assert!(validate_request(&unset, TOKEN).is_ok());
    }

    // --- file format and merge ---

    #[test]
    fn render_parse_round_trip() {
        let m = map(&[
            ("CM_MIN_TAG_AGE_HOURS", "12"),
            ("BEARER_TOKEN", "a$b\"c\\d=e#f,g`h"),
        ]);
        let text = render(&m);
        assert_eq!(
            text,
            "BEARER_TOKEN='a$b\"c\\d=e#f,g`h'\nCM_MIN_TAG_AGE_HOURS='12'\n"
        );
        let (back, dropped) = parse(&text);
        assert_eq!(back, m);
        assert_eq!(dropped, 0);
        assert_eq!(render(&BTreeMap::new()), "");
    }

    #[test]
    fn parse_drops_malformed_lines() {
        let text = "BEARER_TOKEN='ok'\nNOPE='x'\ngarbage\nCM_MIN_TAG_AGE_HOURS='abc'\nCM_MIN_TAG_AGE_HOURS=5\n\n";
        let (m, dropped) = parse(text);
        assert_eq!(m, map(&[("BEARER_TOKEN", "ok")]));
        assert_eq!(dropped, 4);
    }

    #[test]
    fn merge_and_unset_semantics() {
        let mut cur = map(&[("BEARER_TOKEN", "a"), ("CM_MIN_TAG_AGE_HOURS", "1")]);
        apply_update(&mut cur, &map(&[("CM_MIN_TAG_AGE_HOURS", "9")]), &[]);
        assert_eq!(cur, map(&[("BEARER_TOKEN", "a"), ("CM_MIN_TAG_AGE_HOURS", "9")]));
        apply_update(&mut cur, &BTreeMap::new(), &["BEARER_TOKEN".into(), "BEARER_TOKEN".into()]);
        assert_eq!(cur, map(&[("CM_MIN_TAG_AGE_HOURS", "9")]));
        apply_update(&mut cur, &BTreeMap::new(), &["BEARER_TOKEN".into()]);
        assert_eq!(cur, map(&[("CM_MIN_TAG_AGE_HOURS", "9")]));
    }

    #[tokio::test]
    async fn atomic_write_is_0600_leaves_no_temp_and_empty_removes_file() {
        let dir = temp_work_dir();
        let m = map(&[("BEARER_TOKEN", "tok")]);
        let sha = write_overrides(&dir, &m).await.unwrap().unwrap();
        let path = overrides_path(&dir);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "BEARER_TOKEN='tok'\n");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(sha, sha256_hex(b"BEARER_TOKEN='tok'\n"));
        // Overwrite keeps 0600 and replaces content.
        write_overrides(&dir, &map(&[("BEARER_TOKEN", "tok2")])).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "BEARER_TOKEN='tok2'\n");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("env-overrides")], "temp file left behind");
        // Empty result removes the file; removing again is fine.
        assert_eq!(write_overrides(&dir, &BTreeMap::new()).await.unwrap(), None);
        assert!(!path.exists());
        assert_eq!(write_overrides(&dir, &BTreeMap::new()).await.unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failed_write_leaves_no_temp_file() {
        let dir = temp_work_dir();
        std::fs::create_dir(overrides_path(&dir)).unwrap(); // rename over a directory fails
        let r = write_overrides(&dir, &map(&[("BEARER_TOKEN", "tok")])).await;
        assert!(r.is_err());
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "temp file left behind: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_missing_or_empty_file_is_no_overrides() {
        let dir = temp_work_dir();
        assert_eq!(read_overrides(&dir).await.unwrap(), (BTreeMap::new(), None));
        std::fs::write(overrides_path(&dir), "").unwrap();
        assert_eq!(read_overrides(&dir).await.unwrap(), (BTreeMap::new(), None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- handlers ---

    #[tokio::test]
    async fn handlers_require_a_valid_bearer_token() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        let body = r#"{"set":{"CM_MIN_TAG_AGE_HOURS":"1"}}"#;
        for method in ["PUT", "GET"] {
            let (s, _) = call(&state, method, None, body).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{method} without token");
            let (s, _) = call(&state, method, Some("wrong"), body).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{method} wrong token");
        }
        // Auth runs before body parsing.
        let (s, _) = call(&state, "PUT", Some("wrong"), "not json").await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert!(!overrides_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_rejects_bad_requests_with_400_and_writes_nothing() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        let cases = [
            r#"{}"#,
            r#"{"set":{},"unset":[]}"#,
            r#"{"set":{"WORK_DIR":"/x"}}"#,
            r#"{"unset":["COMPOSE_MANAGER_IMAGE"]}"#,
            r#"{"set":{"CM_MIN_TAG_AGE_HOURS":"abc"}}"#,
            r#"{"set":{"CM_MIN_TAG_AGE_HOURS":"-1"}}"#,
            r#"{"set":{"CM_MIN_TAG_AGE_HOURS":5}}"#,
            r#"{"set":{"BEARER_TOKEN":"has space"}}"#,
            r#"{"set":{"BEARER_TOKEN":"a,,b"}}"#,
            r#"{"set":{"BEARER_TOKEN":"it's"}}"#,
            r#"{"set":{"CM_MIN_TAG_AGE_HOURS":"1"},"unset":["CM_MIN_TAG_AGE_HOURS"]}"#,
            r#"{"bogus":1}"#,
            r#"[1]"#,
            "not json",
            "",
        ];
        for body in cases {
            let (s, msg) = call(&state, "PUT", Some(TOKEN), body).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {msg}");
        }
        assert!(!overrides_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_unknown_key_names_allowed_keys() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        let (s, msg) = call(&state, "PUT", Some(TOKEN), r#"{"set":{"WORK_DIR":"/x"}}"#).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(msg.contains("BEARER_TOKEN") && msg.contains("CM_MIN_TAG_AGE_HOURS"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_bad_value_response_does_not_echo_it() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        let body = serde_json::json!({"set": {"CM_MIN_TAG_AGE_HOURS": SECRET}}).to_string();
        let (s, msg) = call(&state, "PUT", Some(TOKEN), &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(!msg.contains(SECRET), "{msg}");
        let body = serde_json::json!({"set": {"BEARER_TOKEN": format!("{SECRET} x")}}).to_string();
        let (s, msg) = call(&state, "PUT", Some(TOKEN), &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(!msg.contains(SECRET), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_lockout_guard_returns_400() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        let body = serde_json::json!({"set": {"BEARER_TOKEN": SECRET}}).to_string();
        let (s, msg) = call(&state, "PUT", Some(TOKEN), &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(msg.contains("two-step"), "{msg}");
        assert!(!msg.contains(SECRET), "{msg}");
        assert!(!overrides_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_happy_path_get_and_unset() {
        let dir = temp_work_dir();
        let state = test_state(&dir);

        let body = serde_json::json!({"set": {
            "BEARER_TOKEN": format!("{TOKEN},{SECRET}"),
            "CM_MIN_TAG_AGE_HOURS": "6",
        }})
        .to_string();
        let (s, resp) = call(&state, "PUT", Some(TOKEN), &body).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{resp}");
        assert!(!resp.contains(SECRET), "PUT response leaked a value: {resp}");
        let v = json(&resp);
        assert_eq!(v["status"], "accepted");
        assert_eq!(v["keys"], serde_json::json!(["BEARER_TOKEN", "CM_MIN_TAG_AGE_HOURS"]));
        let file = std::fs::read_to_string(overrides_path(&dir)).unwrap();
        assert_eq!(
            file,
            format!("BEARER_TOKEN='{TOKEN},{SECRET}'\nCM_MIN_TAG_AGE_HOURS='6'\n")
        );
        assert_eq!(v["sha256"], sha256_hex(file.as_bytes()));
        assert_eq!(
            std::fs::metadata(overrides_path(&dir)).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Merge: only the hours key changes; BEARER_TOKEN is preserved.
        let (s, resp) = call(&state, "PUT", Some(TOKEN), r#"{"set":{"CM_MIN_TAG_AGE_HOURS":"0"}}"#).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{resp}");
        assert_eq!(json(&resp)["keys"], serde_json::json!(["BEARER_TOKEN", "CM_MIN_TAG_AGE_HOURS"]));

        // GET: keys and hashes only, never values.
        let (s, resp) = call(&state, "GET", Some(TOKEN), "").await;
        assert_eq!(s, StatusCode::OK);
        assert!(!resp.contains(SECRET), "GET response leaked a value: {resp}");
        let v = json(&resp);
        assert_eq!(v["status"], "ok");
        let cm = &v["targets"]["compose-manager"];
        assert_eq!(cm["keys"], serde_json::json!(["BEARER_TOKEN", "CM_MIN_TAG_AGE_HOURS"]));
        assert!(cm["sha256"].is_string());
        assert_eq!(v["targets"]["otelcol-management"]["keys"], serde_json::json!([]));

        // Unset one key, then the other: file disappears.
        let (s, resp) = call(&state, "PUT", Some(TOKEN), r#"{"unset":["BEARER_TOKEN"]}"#).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{resp}");
        assert_eq!(json(&resp)["keys"], serde_json::json!(["CM_MIN_TAG_AGE_HOURS"]));
        let (s, resp) = call(&state, "PUT", Some(TOKEN), r#"{"unset":["CM_MIN_TAG_AGE_HOURS"]}"#).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{resp}");
        let v = json(&resp);
        assert_eq!(v["keys"], serde_json::json!([]));
        assert!(v["sha256"].is_null());
        assert_eq!(v["targets"], serde_json::json!([]));
        assert!(!overrides_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_rotation_second_step_with_new_token() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        // Step 1: keep the current token, add the new one.
        let body = serde_json::json!({"set": {"BEARER_TOKEN": format!("{TOKEN},new-token")}}).to_string();
        assert_eq!(call(&state, "PUT", Some(TOKEN), &body).await.0, StatusCode::ACCEPTED);
        // Step 2 (as the launcher would have restarted us with both tokens): drop the old one.
        let state2 = test_state_with_tokens(&dir, vec![TOKEN.into(), "new-token".into()]);
        let body = serde_json::json!({"set": {"BEARER_TOKEN": "new-token"}}).to_string();
        // Old token may not drop itself...
        assert_eq!(call(&state2, "PUT", Some(TOKEN), &body).await.0, StatusCode::BAD_REQUEST);
        // ...but the new token may.
        assert_eq!(call(&state2, "PUT", Some("new-token"), &body).await.0, StatusCode::ACCEPTED);
        assert_eq!(std::fs::read_to_string(overrides_path(&dir)).unwrap(), "BEARER_TOKEN='new-token'\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_records_audit_action_without_values() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        let body = serde_json::json!({"set": {
            "BEARER_TOKEN": format!("{TOKEN},{SECRET}"),
            "CM_MIN_TAG_AGE_HOURS": "3",
        }, "unset": []})
        .to_string();
        assert_eq!(call(&state, "PUT", Some(TOKEN), &body).await.0, StatusCode::ACCEPTED);
        assert_eq!(
            call(&state, "PUT", Some(TOKEN), r#"{"unset":["CM_MIN_TAG_AGE_HOURS"]}"#).await.0,
            StatusCode::ACCEPTED
        );
        let actions = state.actions.read().await;
        let names: Vec<_> = actions.iter().map(|a| a.action.as_str()).collect();
        assert_eq!(names, vec!["env_override_set", "env_override_unset"]);
        assert_eq!(actions[0].services, vec!["BEARER_TOKEN", "CM_MIN_TAG_AGE_HOURS"]);
        assert_eq!(actions[1].services, vec!["CM_MIN_TAG_AGE_HOURS"]);
        let on_disk = std::fs::read_to_string(dir.join("actions.json")).unwrap();
        assert!(!on_disk.contains(SECRET), "action log leaked a value");
        let _ = std::fs::remove_dir_all(&dir);
    }

    const CM: &str = "compose-manager";
    const OTEL: &str = "otelcol-management";
    const INGEST: &str = "ingest-TOKEN_0123456789.abc";

    fn write_launcher_state(dir: &Path, json: &str) {
        std::fs::write(dir.join("launcher-state.json"), json).unwrap();
    }

    #[tokio::test]
    async fn get_reports_per_target_launcher_state() {
        let dir = temp_work_dir();
        let state = test_state(&dir);

        // No file, no launcher state: nothing pending, launcher has not reported.
        let (s, resp) = call(&state, "GET", Some(TOKEN), "").await;
        assert_eq!(s, StatusCode::OK);
        let v = json(&resp);
        assert_eq!(v["launcher_reported"], false);
        for t in [CM, OTEL] {
            let tv = &v["targets"][t];
            assert_eq!(tv["keys"], serde_json::json!([]), "{t}");
            assert!(tv["sha256"].is_null() && tv["applied_sha256"].is_null() && tv["error"].is_null());
            assert_eq!(tv["pending"], false);
        }

        // Keys on both targets, old launcher (state without per-target keys,
        // including the legacy single-target names): pending, not reported.
        write_launcher_state(
            &dir,
            r#"{"compose_manager_digest":"sha256:x","env_overrides_applied_sha256":"legacy"}"#,
        );
        let body = serde_json::json!({"set": {
            "CM_MIN_TAG_AGE_HOURS": "4",
            "MONITORING_INGEST_TOKEN": INGEST,
        }})
        .to_string();
        let (s, resp) = call(&state, "PUT", Some(TOKEN), &body).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{resp}");
        let sha_cm = sha256_hex(b"CM_MIN_TAG_AGE_HOURS='4'\n");
        let sha_otel = sha256_hex(format!("MONITORING_INGEST_TOKEN='{INGEST}'\n").as_bytes());
        let v = json(&call(&state, "GET", Some(TOKEN), "").await.1);
        assert_eq!(v["launcher_reported"], false);
        assert_eq!(v["targets"][CM]["sha256"], sha_cm);
        assert_eq!(v["targets"][OTEL]["sha256"], sha_otel);
        assert_eq!(v["targets"][CM]["pending"], true);
        assert_eq!(v["targets"][OTEL]["pending"], true);
        assert!(!v.to_string().contains(INGEST), "GET leaked a value");

        // Partial state: only compose-manager reported, with a stale hash.
        write_launcher_state(&dir, r#"{"env_overrides_applied_sha256_compose_manager":"deadbeef"}"#);
        let v = json(&call(&state, "GET", Some(TOKEN), "").await.1);
        assert_eq!(v["launcher_reported"], true);
        assert_eq!(v["targets"][CM]["applied_sha256"], "deadbeef");
        assert_eq!(v["targets"][CM]["pending"], true);
        assert!(v["targets"][OTEL]["applied_sha256"].is_null());
        assert_eq!(v["targets"][OTEL]["pending"], true);

        // Each target applied independently; the error is per target.
        write_launcher_state(
            &dir,
            &format!(
                r#"{{"env_overrides_applied_sha256_compose_manager":"{sha_cm}",
                    "env_overrides_applied_sha256_otelcol_management":"old",
                    "env_overrides_error_otelcol_management":"app-compose is not mounted",
                    "env_overrides_error_compose_manager":""}}"#
            ),
        );
        let v = json(&call(&state, "GET", Some(TOKEN), "").await.1);
        assert_eq!(v["targets"][CM]["pending"], false);
        assert!(v["targets"][CM]["error"].is_null());
        assert_eq!(v["targets"][OTEL]["pending"], true);
        assert_eq!(v["targets"][OTEL]["error"], "app-compose is not mounted");

        // Unsetting the ingest token: that target is pending (had keys applied,
        // file now has none) until the launcher applies the empty set.
        call(&state, "PUT", Some(TOKEN), r#"{"unset":["MONITORING_INGEST_TOKEN"]}"#).await;
        write_launcher_state(
            &dir,
            &format!(
                r#"{{"env_overrides_applied_sha256_compose_manager":"{sha_cm}",
                    "env_overrides_applied_sha256_otelcol_management":"{sha_otel}"}}"#
            ),
        );
        let v = json(&call(&state, "GET", Some(TOKEN), "").await.1);
        assert_eq!(v["targets"][CM]["pending"], false);
        assert_eq!(v["targets"][OTEL]["pending"], true);
        assert!(v["targets"][OTEL]["sha256"].is_null());
        write_launcher_state(
            &dir,
            &format!(
                r#"{{"env_overrides_applied_sha256_compose_manager":"{sha_cm}",
                    "env_overrides_applied_sha256_otelcol_management":""}}"#
            ),
        );
        let v = json(&call(&state, "GET", Some(TOKEN), "").await.1);
        assert_eq!(v["targets"][OTEL]["pending"], false);
        assert!(v["targets"][OTEL]["applied_sha256"].is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_merges_across_targets_and_reports_affected_targets() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        let body = serde_json::json!({"set": {"MONITORING_INGEST_TOKEN": INGEST}}).to_string();
        let (s, resp) = call(&state, "PUT", Some(TOKEN), &body).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{resp}");
        assert!(!resp.contains(INGEST), "PUT leaked a value");
        let v = json(&resp);
        assert_eq!(v["keys"], serde_json::json!(["MONITORING_INGEST_TOKEN"]));
        assert_eq!(v["targets"], serde_json::json!([OTEL]));
        // The ingest token needs no lockout guard: the BEARER_TOKEN is untouched.
        let (s, resp) = call(&state, "PUT", Some(TOKEN), r#"{"set":{"CM_MIN_TAG_AGE_HOURS":"1"}}"#).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{resp}");
        let v = json(&resp);
        assert_eq!(v["keys"], serde_json::json!(["CM_MIN_TAG_AGE_HOURS", "MONITORING_INGEST_TOKEN"]));
        assert_eq!(v["targets"], serde_json::json!([CM, OTEL]));
        assert_eq!(
            std::fs::read_to_string(overrides_path(&dir)).unwrap(),
            format!("CM_MIN_TAG_AGE_HOURS='1'\nMONITORING_INGEST_TOKEN='{INGEST}'\n")
        );
        // Drop the compose-manager key: only the otelcol target remains.
        let (_, resp) = call(&state, "PUT", Some(TOKEN), r#"{"unset":["CM_MIN_TAG_AGE_HOURS"]}"#).await;
        assert_eq!(json(&resp)["targets"], serde_json::json!([OTEL]));
        // Invalid ingest tokens are 400 and never echoed.
        for bad in ["short", "has space 0123456789", "quote'0123456789abcdef", "comma,0123456789abcdef"] {
            let body = serde_json::json!({"set": {"MONITORING_INGEST_TOKEN": bad}}).to_string();
            let (s, msg) = call(&state, "PUT", Some(TOKEN), &body).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
            assert!(!msg.contains(bad), "{msg}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ingest_token_value_rules() {
        for good in [
            "0123456789abcdef",
            "Abc+def/ghi=jkl_mno.pqr~stu-vwx",
            &"a".repeat(512),
        ] {
            assert!(validate_value("MONITORING_INGEST_TOKEN", good).is_ok(), "should accept {good:?}");
        }
        for bad in [
            "", "short-15-chars-", &"a".repeat(513), "has space 0123456789", "tab\t0123456789abcdef",
            "quote'0123456789abcdef", "dq\"0123456789abcdef", "comma,0123456789abcdef",
            "caf\u{e9}0123456789abcdef", "semi;0123456789abcdef", "dollar$0123456789abcdef",
            "back\\slash0123456789abcdef", "new\nline0123456789abcdef",
        ] {
            assert!(validate_value("MONITORING_INGEST_TOKEN", bad).is_err(), "should reject {bad:?}");
        }
        let e = validate_value("MONITORING_INGEST_TOKEN", "bad value here 12345").unwrap_err();
        assert!(!e.contains("bad value"), "{e}");
    }

    #[test]
    fn key_target_mapping() {
        assert_eq!(target_of("BEARER_TOKEN"), Some(CM));
        assert_eq!(target_of("CM_MIN_TAG_AGE_HOURS"), Some(CM));
        assert_eq!(target_of("MONITORING_INGEST_TOKEN"), Some(OTEL));
        assert_eq!(target_of("WORK_DIR"), None);
        assert_eq!(state_suffix(CM), "compose_manager");
        assert_eq!(state_suffix(OTEL), "otelcol_management");
        // Every allowed key has exactly one target and every mapped key is allowed.
        let mapped: Vec<&str> = KEY_TARGETS.iter().map(|(k, _)| *k).collect();
        assert_eq!(mapped, ALLOWED_KEYS);
        for (_, t) in KEY_TARGETS {
            assert!(TARGETS.contains(t));
        }
        let m = map(&[("BEARER_TOKEN", "a"), ("MONITORING_INGEST_TOKEN", INGEST)]);
        assert_eq!(target_subset(&m, CM), map(&[("BEARER_TOKEN", "a")]));
        assert_eq!(target_sha256(&m, CM), Some(sha256_hex(b"BEARER_TOKEN='a'\n")));
        assert_eq!(target_sha256(&map(&[("BEARER_TOKEN", "a")]), OTEL), None);
    }

    #[tokio::test]
    async fn get_ignores_malformed_lines_and_never_returns_values() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        std::fs::write(
            overrides_path(&dir),
            format!("BEARER_TOKEN='{SECRET}'\nWORK_DIR='/evil'\n"),
        )
        .unwrap();
        let (s, resp) = call(&state, "GET", Some(TOKEN), "").await;
        assert_eq!(s, StatusCode::OK);
        assert!(!resp.contains(SECRET) && !resp.contains("WORK_DIR"), "{resp}");
        assert_eq!(
            json(&resp)["targets"][CM]["keys"],
            serde_json::json!(["BEARER_TOKEN"])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- allowlist parity ---

    /// Names substituted as dollar-brace variables in the launcher compose file.
    fn compose_substitution_vars(text: &str) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        let mut rest = text;
        while let Some(i) = rest.find("${") {
            rest = &rest[i + 2..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                .collect();
            if !name.is_empty() {
                out.insert(name);
            }
        }
        out
    }

    #[test]
    fn compose_manager_keys_match_launcher_compose_substitutions() {
        let compose = include_str!("../launcher/docker-compose.yml");
        let mut vars = compose_substitution_vars(compose);
        for plumbing in ["COMPOSE_MANAGER_IMAGE", "CM_GITHUB_REPO"] {
            assert!(vars.remove(plumbing), "{plumbing} no longer in launcher compose; update this test");
        }
        let cm_keys: std::collections::BTreeSet<String> = KEY_TARGETS
            .iter()
            .filter(|(_, t)| *t == TARGET_COMPOSE_MANAGER)
            .map(|(k, _)| k.to_string())
            .collect();
        assert_eq!(
            cm_keys, vars,
            "compose-manager keys must equal the non-plumbing substitutions in launcher/docker-compose.yml"
        );
    }

    #[test]
    fn allowlist_matches_readme_table() {
        let readme = include_str!("../README.md");
        let start = readme.find("<!-- allowed-keys:start -->").expect("README start marker");
        let end = readme.find("<!-- allowed-keys:end -->").expect("README end marker");
        let mut from_readme: Vec<String> = readme[start..end]
            .lines()
            .filter_map(|l| l.strip_prefix("| `"))
            .filter_map(|l| l.split('`').next())
            .map(str::to_string)
            .collect();
        from_readme.sort();
        let mut allowed: Vec<String> = ALLOWED_KEYS.iter().map(|s| s.to_string()).collect();
        allowed.sort();
        assert_eq!(from_readme, allowed, "README allowed-keys table drifted from ALLOWED_KEYS");
    }
}
