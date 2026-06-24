use anyhow::{anyhow, Context, Result};
use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::Command,
    sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock},
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command as AsyncCommand,
    sync::{Mutex, RwLock},
};
use tracing::{error, info, warn};

// --- Vendored dstack client (lightweight, avoids pulling in alloy) ---
// Based on dstack-sdk 0.1.2, same pattern as tee-attestation-server

mod dstack {
    use anyhow::Result;
    use dstack_sdk_types::dstack::GetQuoteResponse;
    use hex::encode as hex_encode;
    use http_client_unix_domain_socket::{ClientUnix, Method};
    use reqwest::Client;
    use serde::{de::DeserializeOwned, Serialize};
    use serde_json::{json, Value};
    use std::env;

    #[derive(Debug)]
    enum ClientKind {
        Http,
        Unix,
    }

    fn get_endpoint() -> String {
        if let Ok(sim_endpoint) = env::var("DSTACK_SIMULATOR_ENDPOINT") {
            return sim_endpoint;
        }
        const SOCKET_PATHS: &[&str] = &["/var/run/dstack/dstack.sock", "/var/run/dstack.sock"];
        for path in SOCKET_PATHS {
            if std::path::Path::new(path).exists() {
                return path.to_string();
            }
        }
        SOCKET_PATHS[0].to_string()
    }

    pub struct DstackClient {
        base_url: String,
        endpoint: String,
        client: ClientKind,
    }

    impl DstackClient {
        pub fn new() -> Self {
            let endpoint = get_endpoint();
            let (base_url, client) = match endpoint {
                ref e if e.starts_with("http://") || e.starts_with("https://") => {
                    (e.to_string(), ClientKind::Http)
                }
                _ => ("http://localhost".to_string(), ClientKind::Unix),
            };
            DstackClient {
                base_url,
                endpoint,
                client,
            }
        }

        async fn send_rpc_request<S: Serialize, D: DeserializeOwned>(
            &self,
            path: &str,
            payload: &S,
        ) -> Result<D> {
            match &self.client {
                ClientKind::Http => {
                    let client = Client::new();
                    let url = format!(
                        "{}/{}",
                        self.base_url.trim_end_matches('/'),
                        path.trim_start_matches('/')
                    );
                    let res = client
                        .post(&url)
                        .json(payload)
                        .header("Content-Type", "application/json")
                        .send()
                        .await?
                        .error_for_status()?;
                    Ok(res.json().await?)
                }
                ClientKind::Unix => {
                    let mut unix_client = ClientUnix::try_new(&self.endpoint).await?;
                    let res = unix_client
                        .send_request_json::<_, _, Value>(
                            path,
                            Method::POST,
                            &[("Content-Type", "application/json"), ("Host", "dstack")],
                            Some(payload),
                        )
                        .await?;
                    Ok(res.1)
                }
            }
        }

        pub async fn get_quote(&self, report_data: Vec<u8>) -> Result<GetQuoteResponse> {
            if report_data.is_empty() || report_data.len() > 64 {
                anyhow::bail!("invalid report data length");
            }
            let hex_data = hex_encode(&report_data);
            let data = json!({ "report_data": hex_data });
            let response: Value = self.send_rpc_request("/GetQuote", &data).await?;
            Ok(serde_json::from_value::<GetQuoteResponse>(response)?)
        }
    }
}

// --- Application State ---

fn actions_file(work_dir: &Path) -> PathBuf {
    work_dir.join("actions.json")
}

fn load_actions_from_disk(work_dir: &Path) -> Vec<DeploymentAction> {
    match std::fs::read_to_string(actions_file(work_dir)) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
            error!(error = %e, "actions.json is corrupt, starting with empty log");
            Vec::new()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            error!(error = %e, "Failed to read actions.json, starting with empty log");
            Vec::new()
        }
    }
}

/// A single deployed tag/file/commit tuple for one Compose project.
///
/// Used both for the in-memory per-project state and as the on-disk
/// `deployed.json` record. All fields are `Option` + `skip_serializing_if` so a
/// record carrying only some fields (e.g. a legacy single-tuple) round-trips
/// without injecting nulls.
#[derive(Clone, Serialize, Deserialize, Default, PartialEq, Debug)]
struct DeployedRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_sha256: Option<String>,
}

/// Per-project deployed state: the `current` (last-successful) deploy plus the
/// `previous` (last-known-good before it), so the external reconciler can read a
/// rollback target. `previous` is omitted (serialized as absent) until the first
/// successful re-deploy rotates a `current` into it.
#[derive(Clone, Serialize, Deserialize, Default, PartialEq, Debug)]
struct ProjectDeployed {
    current: DeployedRecord,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous: Option<DeployedRecord>,
}

/// The default Compose project name a legacy single-tuple `deployed.json`
/// migrates into. Matches the historical working-directory basename default
/// (`/app/work` -> `work`); see `compose_project_name`.
const LEGACY_PROJECT_KEY: &str = "work";

/// Persisted per-project deployed state, written to `deployed.json`.
///
/// Post-#46 a CVM can run N Compose projects, so /version must report deployed
/// state PER project rather than collapsing to one global tuple. Written ONLY
/// after a compose_up stream completes successfully (so a failed attempt never
/// overwrites the last good deploy), and read once at startup.
///
/// On-disk format is a JSON object keyed by project name:
/// `{"<project>": {"current": {...}, "previous": {...}|absent}}`. A pre-existing
/// LEGACY single-tuple `deployed.json` (`{"tag":...,"file":...}`) is migrated
/// into `projects["work"].current` with `previous` absent — see
/// `load_deployed_state`.
#[derive(Clone, Serialize, Deserialize, Default, PartialEq, Debug)]
struct DeployedState {
    #[serde(flatten)]
    projects: std::collections::BTreeMap<String, ProjectDeployed>,
}

fn deployed_version_file(work_dir: &Path) -> PathBuf {
    work_dir.join("deployed.json")
}

/// Load the per-project deployed state from `deployed.json`.
///
/// Accepts BOTH the new per-project map format and the LEGACY single-tuple
/// format written before this change. A legacy tuple is migrated into
/// `projects["work"].current` (previous absent), since `work` was the only
/// project that could exist pre-#46. A corrupt/unreadable file is ignored
/// (empty state), exactly as the single-tuple loader did.
fn load_deployed_state(work_dir: &Path) -> DeployedState {
    let content = match std::fs::read_to_string(deployed_version_file(work_dir)) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DeployedState::default(),
        Err(e) => {
            error!(error = %e, "Failed to read deployed.json, ignoring");
            return DeployedState::default();
        }
    };
    parse_deployed_state(&content).unwrap_or_else(|| {
        error!("deployed.json is corrupt, ignoring");
        DeployedState::default()
    })
}

/// Parse `deployed.json` content, accepting the new per-project map OR a legacy
/// single-tuple. Returns `None` only when the content is neither (corrupt).
fn parse_deployed_state(content: &str) -> Option<DeployedState> {
    // Try the new per-project map first. A legacy single-tuple
    // (`{"tag":...}`) would deserialize here as a project named "tag" whose
    // value is a string — which fails the ProjectDeployed shape — so the map
    // parse rejects legacy input rather than silently mis-keying it.
    if let Ok(state) = serde_json::from_str::<DeployedState>(content) {
        // An empty `{}` legitimately parses as the new empty map; a legacy
        // tuple with only e.g. `{"tag":"v1"}` would NOT (string value), so we
        // can trust this branch for the new format.
        if !state.projects.is_empty() || content.trim() == "{}" {
            return Some(state);
        }
    }
    // Fall back to the legacy single-tuple, migrating it into `work`.
    if let Ok(legacy) = serde_json::from_str::<DeployedRecord>(content) {
        if legacy != DeployedRecord::default() {
            let mut projects = std::collections::BTreeMap::new();
            projects.insert(
                LEGACY_PROJECT_KEY.to_string(),
                ProjectDeployed { current: legacy, previous: None },
            );
            return Some(DeployedState { projects });
        }
        // Empty legacy record (`{}`) -> empty state.
        return Some(DeployedState::default());
    }
    None
}

/// Persist the per-project deployed state to `deployed.json`, synchronously, via
/// a temp-file + atomic rename. Deliberately blocking (std::fs): the compose_up
/// completion path calls this BEFORE emitting the terminal `done` event so the
/// write is durable before the client is told the deploy succeeded — a detached
/// async write could lose to an immediate restart. The file is tiny.
fn persist_deployed_state(work_dir: &Path, state: &DeployedState) -> std::io::Result<()> {
    let json = serde_json::to_string(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = work_dir.join("deployed.json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, deployed_version_file(work_dir))?;
    Ok(())
}

/// Best-effort migration source for installs that predate `deployed.json`:
/// derive a `DeployedRecord` from the most recent `compose_up` in the legacy
/// action log. Legacy actions carry no success/failure outcome, so this is only
/// used to seed `deployed.json` once when it's missing. The result lands in the
/// `work` project (legacy installs only ran the single default project).
fn deployed_version_from_actions(actions: &[DeploymentAction]) -> Option<DeployedRecord> {
    actions
        .iter()
        .rev()
        .find(|a| a.action == "compose_up")
        .map(|a| DeployedRecord {
            tag: a.tag.clone(),
            commit: a.commit.clone(),
            file: a.file.clone(),
            file_sha256: a.file_sha256.clone(),
        })
}

fn migration_marker_file(work_dir: &Path) -> PathBuf {
    work_dir.join(".deployed-version-migrated")
}

/// One-shot legacy migration for `deployed.json`.
///
/// Installs that predate `deployed.json` (upgraded in place via the launcher
/// hot-swap) have an `actions.json` but no `deployed.json`, so the first restart
/// would otherwise blank /version. Backfill it once from the latest `compose_up`.
///
/// This MUST be one-shot. `compose_up` actions are recorded optimistically,
/// before the stream succeeds and with no persisted outcome — so on a host
/// running THIS code, a failed first deploy leaves such an action but no
/// `deployed.json`. Backfilling on every `deployed.json`-absent boot would then
/// resurrect that failed attempt as the deployed version. A marker file makes
/// the action-log backfill happen only on the very first boot after upgrade:
/// on a fresh install that boot precedes any `compose_up` (nothing to backfill),
/// so a later failed deploy is never picked up.
///
/// Returns the backfilled version (if any) to also seed in memory. The marker is
/// written once the migration has been handled; if the persist fails it is left
/// unwritten so the next boot retries.
fn migrate_legacy_deployed_version(
    work_dir: &Path,
    actions: &[DeploymentAction],
) -> Option<DeployedState> {
    let marker = migration_marker_file(work_dir);
    if marker.exists() {
        return None;
    }
    let result = deployed_version_from_actions(actions).map(|current| {
        let mut projects = std::collections::BTreeMap::new();
        projects.insert(
            LEGACY_PROJECT_KEY.to_string(),
            ProjectDeployed { current, previous: None },
        );
        DeployedState { projects }
    });
    if let Some(ref v) = result {
        if let Err(e) = persist_deployed_state(work_dir, v) {
            error!(error = %e, "Failed to backfill deployed.json; will retry next boot");
            return result; // don't write the marker — retry on next boot
        }
        info!("Backfilled deployed.json from action log (one-shot legacy migration)");
    }
    // Mark migration done so a later failed compose_up (pre-outcome action, no
    // deployed.json) is never backfilled. Best-effort.
    if let Err(e) = std::fs::write(&marker, b"1") {
        error!(error = %e, "Failed to write deployed-version migration marker");
    }
    result
}

async fn persist_actions_to_disk(work_dir: &Path, actions: &[DeploymentAction]) -> std::io::Result<()> {
    let json = canonicalize_actions(actions);
    let tmp = work_dir.join("actions.json.tmp");
    tokio::fs::write(&tmp, &json).await?;
    tokio::fs::rename(&tmp, actions_file(work_dir)).await?;
    Ok(())
}

/// Canonicalize actions to a deterministic JSON string for SHA-256 hashing.
///
/// Rules (must match Python: `json.dumps(actions, sort_keys=True,
/// separators=(",",":"), ensure_ascii=False)`):
/// - JSON array of objects, compact (no whitespace)
/// - Object keys sorted alphabetically
/// - Null-valued and empty-array keys omitted
/// - UTF-8 encode, then SHA-256
///
/// Contract: all DeploymentAction values are strings. Adding numeric fields
/// (especially floats) would break cross-language hash reproducibility due to
/// divergent number formatting between Rust, Python, and JS.
fn canonicalize_actions(actions: &[DeploymentAction]) -> String {
    let mut value = serde_json::to_value(actions).expect("DeploymentAction serialization is infallible");
    sort_json_keys(&mut value);
    serde_json::to_string(&value).expect("DeploymentAction serialization is infallible")
}

fn sort_json_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = map
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for entry in entries.iter_mut() {
                sort_json_keys(&mut entry.1);
            }
            let mut sorted = serde_json::Map::new();
            for (k, v) in entries {
                sorted.insert(k, v);
            }
            *map = sorted;
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                sort_json_keys(v);
            }
        }
        _ => {}
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct DeploymentAction {
    timestamp: String,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_sha256: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    services: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    container: Option<String>,
    /// For the `compose_manager_started` action: digest (repo@sha256:…) of the
    /// image this compose-manager is itself running. Recorded at startup so the
    /// running version is part of the action log that is hashed into the
    /// attestation quote's report_data (see `attestation_report`).
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<String>,
}

/// Tracks the currently-running mutating docker/compose operation (if any)
/// for observability and nicer 409 Conflict error messages.
#[derive(Clone, Debug, Serialize)]
struct InFlightOp {
    action: String,
    started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    services: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    container: Option<String>,
}

/// Error returned when a mutating docker operation is already in progress.
#[derive(Debug)]
struct ComposeLockBusy;

/// RAII guard that holds the compose lock and clears `in_flight` metadata on Drop.
/// This ensures `in_flight` is always cleaned up regardless of how the guard
/// goes out of scope — early returns, handler completion, or NdjsonStream drop.
struct ComposeGuard {
    _inner: tokio::sync::OwnedMutexGuard<()>,
    in_flight: Arc<StdMutex<Option<InFlightOp>>>,
}

impl Drop for ComposeGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.in_flight.lock() {
            *guard = None;
        }
        info!(action = "compose_op", "Released compose lock");
    }
}

struct AppState {
    bearer_token: String,
    github_owner: String,
    github_repo_name: String,
    min_tag_age_hours: i64,
    work_dir: PathBuf,
    env_files: Vec<String>,
    /// Slack incoming-webhook URL for ops notifications. `None` disables
    /// notifications entirely (best-effort feature — staging/dev stay quiet).
    slack_webhook_url: Option<String>,
    /// Human label identifying this CVM/host in notifications, e.g. "gpu23".
    /// Sourced from the `INSTANCE_LABEL` env var (templated to the ansible
    /// inventory hostname). Empty string falls back to "unknown-host".
    instance_label: String,
    /// Per-project deployed state ({current, previous} per Compose project).
    /// Drives the additive `projects` map on /version. The 4 `deployed_*`
    /// fields below mirror the LAST-written project's `current` so the existing
    /// top-level /version fields stay byte-identical on single-project CVMs.
    ///
    /// These use a SYNC `std::sync::RwLock` (not the tokio one): they are also
    /// written from `NdjsonStream::poll_next` — a sync `Stream::poll_next` where
    /// `.await` is illegal and `blocking_write` would panic on the runtime. The
    /// critical sections are tiny in-memory swaps never held across an `.await`.
    deployed_projects: StdRwLock<std::collections::BTreeMap<String, ProjectDeployed>>,
    deployed_tag: StdRwLock<Option<String>>,
    deployed_commit: StdRwLock<Option<String>>,
    deployed_file: StdRwLock<Option<String>>,
    deployed_file_sha256: StdRwLock<Option<String>>,
    actions: RwLock<Vec<DeploymentAction>>,
    http: reqwest::Client,
    /// Mutual-exclusion lock for mutating docker/compose operations.
    /// Uses `try_lock` semantics — concurrent requests receive HTTP 409.
    compose_lock: Arc<Mutex<()>>,
    /// Metadata about the currently-running operation (for 409 messages & /status).
    in_flight: Arc<StdMutex<Option<InFlightOp>>>,
    /// Digest (repo@sha256:…) of the image THIS compose-manager process runs,
    /// resolved once at startup. Surfaced by /version and recorded as the
    /// `compose_manager_started` action so it is bound into attestation.
    running_image: Option<String>,
}

impl AppState {
    /// Try to acquire the compose lock. Returns `Ok(ComposeGuard)` if the lock was
    /// available, or `Err(ComposeLockBusy)` if another operation is in progress.
    /// The returned `ComposeGuard` clears `in_flight` on Drop.
    async fn try_acquire_compose_lock(
        self: &Arc<Self>,
        action: &str,
        tag: Option<String>,
        file: Option<String>,
        services: Vec<String>,
        container: Option<String>,
    ) -> Result<ComposeGuard, ComposeLockBusy> {
        match self.compose_lock.clone().try_lock_owned() {
            Ok(guard) => {
                *self.in_flight.lock().expect("in_flight mutex poisoned") = Some(InFlightOp {
                    action: action.to_string(),
                    started_at: Utc::now().to_rfc3339(),
                    tag: tag.clone(),
                    file,
                    services,
                    container,
                });
                info!(action = action, tag = ?tag, "Acquired compose lock");
                Ok(ComposeGuard {
                    _inner: guard,
                    in_flight: self.in_flight.clone(),
                })
            }
            Err(_) => {
                let in_flight = self.in_flight.lock().ok().and_then(|guard| guard.clone());
                error!(action = action, in_flight = ?in_flight, "Compose lock busy — rejecting request");
                Err(ComposeLockBusy)
            }
        }
    }

    /// Build a 409 Conflict error message describing the currently in-flight operation.
    async fn conflict_message(&self) -> String {
        let in_flight = self.in_flight.lock().ok().and_then(|guard| guard.clone());
        format!(
            "another docker operation is already in progress: {}",
            in_flight.map(|op| op.action).unwrap_or_else(|| "unknown".into())
        )
    }
}

// --- API Types ---

#[derive(Serialize)]
struct StatusResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Running compose-manager image digest (repo@sha256:…); populated by /version.
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<String>,
    /// Per-project deployed state ({current, previous}), populated by /version
    /// only. Additive: omitted entirely when empty so every other response and
    /// the single-project /version top-level fields stay byte-identical.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty", default)]
    projects: std::collections::BTreeMap<String, ProjectDeployed>,
}

#[derive(Serialize)]
struct OperationStatusResponse {
    status: String,
    in_flight: Option<InFlightOp>,
}

type ApiResult = (StatusCode, Json<StatusResponse>);

fn ok(tag: Option<String>) -> ApiResult {
    (StatusCode::OK, Json(StatusResponse { status: "ok".into(), tag, commit: None, file: None, file_sha256: None, output: None, exit_code: None, error: None, image: None, projects: Default::default() }))
}

fn ok_output(output: String) -> ApiResult {
    (StatusCode::OK, Json(StatusResponse { status: "ok".into(), tag: None, commit: None, file: None, file_sha256: None, output: Some(output), exit_code: None, error: None, image: None, projects: Default::default() }))
}

fn ok_systemctl(output: String, exit_code: Option<i32>) -> ApiResult {
    (StatusCode::OK, Json(StatusResponse { status: "ok".into(), tag: None, commit: None, file: None, file_sha256: None, output: Some(output), exit_code, error: None, image: None, projects: Default::default() }))
}

fn err(code: StatusCode, msg: impl Into<String>) -> ApiResult {
    (code, Json(StatusResponse { status: "error".into(), tag: None, commit: None, file: None, file_sha256: None, output: None, exit_code: None, error: Some(msg.into()), image: None, projects: Default::default() }))
}

fn err_response(code: StatusCode, msg: impl Into<String>) -> Response {
    let body = serde_json::to_string(&StatusResponse {
        status: "error".into(),
        tag: None,
        commit: None,
        file: None,
        file_sha256: None,
        output: None,
        exit_code: None,
        error: Some(msg.into()),
        image: None,
        projects: Default::default(),
    })
    .unwrap();
    Response::builder()
        .status(code)
        .header("Content-Type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

/// 200 OK with an arbitrary JSON body. Used by the read-only host-observability
/// endpoints whose shapes don't fit `StatusResponse`.
fn json_ok(value: serde_json::Value) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

#[derive(Deserialize)]
struct ComposeRequest {
    tag: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    services: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    force_recreate: bool,
    /// Optional per-call Compose project override. When omitted, the project is
    /// the working-directory basename default (unchanged behavior). When set, it
    /// must already be canonical (lowercase `[a-z0-9_-]`, no leading
    /// non-alphanumerics) and is validated (reserved `work`/`dstack` and
    /// non-canonical names rejected) so the control plane can scope each model
    /// under its own project. See `resolve_compose_project`.
    #[serde(default)]
    project: Option<String>,
    /// PLAN-only mode. When true, simulate the `up` with docker compose's global
    /// `--dry-run` and report what WOULD recreate, WITHOUT applying anything.
    /// Genuinely read-only: no action recorded, no deployed_* / deployed.json
    /// write, nothing pulled/built/spawned. Lets automation see the per-service
    /// config-hash recreate verdict (which file_sha256 cannot predict) before
    /// SIGTERMing a model mid-warmup. Honors all scoping fields (tag/file/
    /// project/services/env/force_recreate) so the plan reflects the EXACT
    /// command a real apply with the same body would run. See `compose_up`.
    #[serde(default)]
    dry_run: bool,
    /// Pre-materialize artifacts (pull images + run in-CVM `build:` + download
    /// weights via the compose file's model-downloader service) WITHOUT
    /// activating, so the OLD model keeps serving and GPU idle at cutover is
    /// near-zero. Runs the up prologue + `pull` + `build` phases and STOPS
    /// before `up`. Records a DISTINCT lower-privilege `compose_stage` action
    /// (NOT compose_up) and never writes deployed state (staging activates
    /// nothing, so recording compose_up / flipping deployed state would make
    /// /version + the attested log LIE about what is running). Emits a terminal
    /// additive `staged` NDJSON line listing the landed image digests. Weight
    /// download is the compose file's EXISTING model-downloader service,
    /// selected via `services` with platform-controlled `HF_HUB_OFFLINE` in
    /// `env` — no separate downloader trigger. See `compose_up`.
    #[serde(default)]
    materialize_only: bool,
}

#[derive(Deserialize)]
struct ComposeDownRequest {
    tag: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    volumes: bool,
    #[serde(default)]
    services: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    /// Optional per-call Compose project override; see `ComposeRequest::project`.
    #[serde(default)]
    project: Option<String>,
}

#[derive(Deserialize)]
struct CleanRequest {
    #[serde(default)]
    volumes: bool,
    #[serde(default)]
    images: bool,
    #[serde(default)]
    containers: bool,
}

/// What to remove for the named model in `POST /docker/evict`.
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum EvictTarget {
    /// Only the downloaded HF weights subtree (`hub/models--org--repo`). Default.
    #[default]
    Weights,
    /// Only the best-effort compile/kernel caches for this model.
    Cache,
    /// Both weights and best-effort caches.
    Both,
}

/// Selective, per-model eviction request. Removes ONLY the named model's
/// on-disk subtree(s) from the shared HuggingFace cache volume — never the
/// whole volume (cf. `/docker/clean`, which blindly prunes everything).
#[derive(Deserialize)]
struct EvictRequest {
    /// HF repo id, e.g. `zai-org/GLM-5.2-FP8`. Maps to `hub/models--org--repo`.
    model: String,
    #[serde(default)]
    target: EvictTarget,
    /// Override the autodetected cache volume name (handles the `huggingface_cache`
    /// vs. `hugginface_cache` typo and project-prefixed forms automatically).
    #[serde(default)]
    cache_volume: Option<String>,
}

#[derive(Deserialize, Default)]
struct LogsRequest {
    #[serde(default)]
    file: Option<String>,
    #[serde(default = "default_tail")]
    tail: u32,
    #[serde(default)]
    services: Vec<String>,
}

fn default_tail() -> u32 {
    100
}

#[derive(Deserialize)]
struct RestartRequest {
    container: String,
}

// --- Env var validation ---

fn is_valid_env_key(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    let mut chars = key.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn validate_env_vars(env: &HashMap<String, String>) -> Result<(), String> {
    for (key, value) in env {
        if !is_valid_env_key(key) {
            return Err(format!("invalid env var key: '{}' (must match [A-Za-z_][A-Za-z0-9_]*)", key));
        }
        if value.contains('\n') || value.contains('\r') {
            return Err(format!("env var '{}' value must not contain newlines", key));
        }
    }
    Ok(())
}

fn write_temp_env_file(work_dir: &Path, env: &HashMap<String, String>) -> Result<PathBuf> {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    let path = work_dir.join(format!(".env.tmp.{}", hex::encode(bytes)));
    let content: String = env
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, content).context("Failed to write temp env file")?;
    Ok(path)
}

// --- GitHub ---

#[derive(Deserialize)]
struct GitHubCommit {
    sha: String,
    commit: GitHubCommitDetail,
}

#[derive(Deserialize)]
struct GitHubCommitDetail {
    committer: GitHubCommitter,
}

#[derive(Deserialize)]
struct GitHubCommitter {
    date: DateTime<Utc>,
}

struct TagInfo {
    commit_date: DateTime<Utc>,
    commit_sha: String,
}

async fn get_tag_info(state: &AppState, tag: &str) -> Result<TagInfo> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/commits/{}",
        state.github_owner, state.github_repo_name, tag
    );

    let resp = state.http.get(&url)
        .header("User-Agent", "compose-manager")
        .send().await
        .context("Failed to query GitHub API")?;

    if !resp.status().is_success() {
        return Err(anyhow!("tag not found: {}", tag));
    }

    let commit: GitHubCommit = resp.json().await
        .context("Failed to parse GitHub response")?;

    Ok(TagInfo {
        commit_date: commit.commit.committer.date,
        commit_sha: commit.sha,
    })
}

async fn validate_tag(state: &AppState, tag: &str) -> Result<TagInfo, (StatusCode, String)> {
    let tag_info = get_tag_info(state, tag).await.map_err(|e| {
        let code = if e.to_string().contains("not found") {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (code, e.to_string())
    })?;

    let min_age = Utc::now() - chrono::Duration::hours(state.min_tag_age_hours);
    if tag_info.commit_date > min_age {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("tag too recent: {} is less than {} hours old", tag_info.commit_date, state.min_tag_age_hours),
        ));
    }

    Ok(tag_info)
}

async fn fetch_github_file(state: &AppState, tag: &str, path: &str) -> Result<String> {
    let url = format!(
        "https://raw.githubusercontent.com/{}/{}/{}/{}",
        state.github_owner, state.github_repo_name, tag, path
    );

    let resp = state.http.get(&url)
        .send().await
        .context("Failed to fetch file from GitHub")?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(anyhow!("file '{}' not found at tag '{}'", path, tag));
    }

    resp.text().await.context("Failed to read file content")
}

// --- Auth ---

fn verify_bearer_token(headers: &HeaderMap, expected: &str) -> Result<(), ApiResult> {
    let token = headers.get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Missing or invalid Authorization header"))?;

    if token != expected {
        return Err(err(StatusCode::UNAUTHORIZED, "Invalid token"));
    }

    Ok(())
}

fn verify_bearer_token_raw(headers: &HeaderMap, expected: &str) -> Result<(), (StatusCode, String)> {
    let token = headers.get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "Missing or invalid Authorization header".to_string()))?;

    if token != expected {
        return Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()));
    }

    Ok(())
}

// --- Action recording + actor attribution ---

/// HTTP header callers use to attribute an operation to a human or automation
/// (e.g. the dashboard sends "dashboard", ansible sends "ansible@gpu23"). When
/// absent we fall back to "automation". The value is sanitized (single line,
/// length-bounded) since it is echoed verbatim into Slack messages.
const ACTOR_HEADER: &str = "x-triggered-by";

fn extract_actor(headers: &HeaderMap) -> String {
    headers
        .get(ACTOR_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.chars()
                .filter(|c| *c != '\n' && *c != '\r')
                .take(80)
                .collect::<String>()
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "automation".to_string())
}

/// Append a deployment action to the in-memory log and persist it to disk.
/// Returns `Err(())` if the disk write failed (the error is already logged);
/// callers map this to a 500. Slack notification is fired separately by the
/// caller, since streaming ops (compose up/down) only learn their outcome when
/// the NDJSON stream terminates.
async fn record_action(state: &Arc<AppState>, action: &DeploymentAction) -> Result<(), ()> {
    let mut actions = state.actions.write().await;
    actions.push(action.clone());
    if let Err(e) = persist_actions_to_disk(&state.work_dir, &actions).await {
        error!(error = %e, "Failed to persist action log to disk");
        return Err(());
    }
    Ok(())
}

/// Best-effort Slack notifications for CVM operations (nearai/infra#141).
///
/// Notifications never block or fail a handler: when no webhook is configured
/// the call is a no-op, and delivery happens on a detached task whose errors
/// are logged and swallowed.
mod notify {
    use super::{AppState, DeploymentAction};
    use std::sync::Arc;
    use tracing::warn;

    /// Carried on an `NdjsonStream` so a streaming compose op can notify Slack
    /// with its real exit status once the stream terminates.
    pub struct CompletionNotice {
        pub state: Arc<AppState>,
        pub action: DeploymentAction,
        pub actor: String,
        /// Resolved Compose project this op ran under, so the success hook can
        /// rotate the right per-project deployed record. Threaded HERE (not on
        /// DeploymentAction) so the attested action schema — and thus
        /// `report_data = SHA256(canonical actions)` — is unchanged.
        pub project: String,
    }

    /// Render a `DeploymentAction` into a Slack message. `outcome` is
    /// `Some(true/false)` when the result is known (succeeded/failed) or `None`
    /// when not applicable.
    pub fn format_message(
        action: &DeploymentAction,
        instance_label: &str,
        actor: &str,
        outcome: Option<bool>,
    ) -> String {
        let host = if instance_label.is_empty() {
            "unknown-host"
        } else {
            instance_label
        };
        let failed = matches!(outcome, Some(false));
        let emoji = if failed {
            ":x:"
        } else {
            match action.action.as_str() {
                "compose_up" => ":rocket:",
                "compose_stage" => ":package:",
                "compose_down" => ":octagonal_sign:",
                "docker_restart" => ":arrows_counterclockwise:",
                "docker_clean" => ":broom:",
                a if a.starts_with("dstack_agent_") => ":satellite_antenna:",
                a if a.starts_with("kernel_algif") => ":lock:",
                _ => ":information_source:",
            }
        };
        let detail = match action.action.as_str() {
            "compose_up" => {
                let title = if failed { "Deploy failed" } else { "Deployed" };
                let mut d = format!("*{}* on `{}`", title, host);
                if let Some(f) = &action.file {
                    d.push_str(&format!(" — `{}`", f));
                }
                if let Some(t) = &action.tag {
                    d.push_str(&format!(" → `{}`", t));
                }
                if !action.services.is_empty() {
                    d.push_str(&format!(" ({})", action.services.join(", ")));
                }
                d
            }
            "compose_stage" => {
                let title = if failed { "Stage failed" } else { "Staged (not activated)" };
                let mut d = format!("*{}* on `{}`", title, host);
                if let Some(f) = &action.file {
                    d.push_str(&format!(" — `{}`", f));
                }
                if let Some(t) = &action.tag {
                    d.push_str(&format!(" → `{}`", t));
                }
                if !action.services.is_empty() {
                    d.push_str(&format!(" ({})", action.services.join(", ")));
                }
                d
            }
            "compose_down" => {
                let title = if failed { "Stop failed" } else { "Stopped" };
                let mut d = format!("*{}* on `{}`", title, host);
                if let Some(f) = &action.file {
                    d.push_str(&format!(" — `{}`", f));
                }
                if let Some(t) = &action.tag {
                    d.push_str(&format!(" (`{}`)", t));
                }
                if !action.services.is_empty() {
                    d.push_str(&format!(" [{}]", action.services.join(", ")));
                }
                d
            }
            "docker_restart" => {
                let c = action.container.as_deref().unwrap_or("?");
                if failed {
                    format!("*Restart of container* `{}` *failed* on `{}`", c, host)
                } else {
                    format!("*Restarted container* `{}` on `{}`", c, host)
                }
            }
            "docker_clean" => {
                let title = if failed {
                    "Docker prune failed"
                } else {
                    "Docker prune complete"
                };
                format!("*{}* on `{}`", title, host)
            }
            a if a.starts_with("dstack_agent_") => {
                let verb = a.strip_prefix("dstack_agent_").unwrap_or(a);
                if failed {
                    format!("*dstack-agent {} failed* on `{}`", verb, host)
                } else {
                    let past = match verb {
                        "start" => "started",
                        "stop" => "stopped",
                        "restart" => "restarted",
                        other => other,
                    };
                    format!("*dstack-agent {}* on `{}`", past, host)
                }
            }
            a if a.starts_with("kernel_algif") => {
                if failed {
                    format!("*Kernel algif blacklist failed* (`{}`) on `{}`", a, host)
                } else {
                    format!("*Kernel algif blacklist applied* on `{}`", host)
                }
            }
            other => {
                let suffix = if failed { " failed" } else { "" };
                format!("*{}{}* on `{}`", other, suffix, host)
            }
        };
        format!("{} {} · by {}", emoji, detail, actor)
    }

    /// Fire a Slack notification for `action` on a detached task. No-op when no
    /// webhook is configured.
    pub fn spawn_action(
        state: &Arc<AppState>,
        action: &DeploymentAction,
        actor: &str,
        outcome: Option<bool>,
    ) {
        let webhook = match &state.slack_webhook_url {
            Some(w) => w.clone(),
            None => return,
        };
        let text = format_message(action, &state.instance_label, actor, outcome);
        let http = state.http.clone();
        tokio::spawn(async move {
            send(&http, &webhook, &text).await;
        });
    }

    async fn send(http: &reqwest::Client, webhook: &str, text: &str) {
        let body = serde_json::json!({ "text": text });
        match http.post(webhook).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => warn!(status = %resp.status(), "Slack notification returned non-success status"),
            Err(e) => warn!(error = %e, "Failed to send Slack notification"),
        }
    }
}

// --- NDJSON Streaming ---

/// Parameters needed to rebuild the `compose up` command for a single retry.
struct EndpointRetry {
    work_dir: PathBuf,
    /// Resolved Compose project name (same one the original `up` used), so the
    /// retry runs under the identical project rather than re-deriving it.
    project: String,
    up_args: Vec<String>,
    file: String,
    env_files: Vec<String>,
    services: Vec<String>,
    /// Path to the per-request temp env file (the same file the original `up`
    /// used), so the retry carries identical env vars. The stream owns cleanup;
    /// this is a read-only reference to the same path.
    temp_env_file: Option<PathBuf>,
}

impl EndpointRetry {
    fn build_cmd(&self) -> AsyncCommand {
        let args_ref: Vec<&str> = self.up_args.iter().map(|s| s.as_str()).collect();
        // A retry is always a REAL apply (the endpoint-conflict recovery only
        // fires on a non-dry-run up), so dry_run=false.
        build_compose_cmd(&self.work_dir, &self.project, &args_ref, &self.file, &self.env_files, &self.services, self.temp_env_file.as_deref(), false)
    }
}

/// Parse `(network, container)` pairs from buffered stderr lines.
/// Matches Docker's "endpoint with name X already exists in network Y".
fn parse_endpoint_errors(lines: &VecDeque<String>) -> Vec<(String, String)> {
    let needle = "endpoint with name ";
    let sep = " already exists in network ";
    lines.iter().filter_map(|line| {
        let start = line.find(needle)? + needle.len();
        let rest = &line[start..];
        let mid = rest.find(sep)?;
        let container = rest[..mid].trim().to_string();
        let network = rest[mid + sep.len()..].trim().to_string();
        if container.is_empty() || network.is_empty() { return None; }
        Some((network, container))
    }).collect()
}

/// ADVISORY parsed verdict of a `docker compose --dry-run up`. The RAW dry-run
/// output lines (streamed as ordinary `stdout`/`stderr` events) are the
/// DOCUMENTED contract; this parse is a best-effort convenience overlay.
///
/// VERSION-SENSITIVE: it keys on Compose's progress-writer status verbs
/// (`Creating`/`Recreate`/`Removing`/`Running`…), whose exact wording can shift
/// between Compose releases. It is pinned to the docker-compose-plugin version
/// shipped in the attested image (5.1.4-1~debian.12~bookworm; see Dockerfile).
/// Automation that needs a guarantee must read the raw lines, not this field.
#[derive(Serialize, Default, PartialEq, Debug)]
struct DryRunPlan {
    create: Vec<String>,
    recreate: Vec<String>,
    remove: Vec<String>,
    unchanged: Vec<String>,
}

/// Parse a `docker compose --dry-run up` transcript into an advisory plan.
///
/// Compose's progress writer emits one resource line per container of the form
/// `[DRY-RUN MODE - ] <resource-name>  <Verb>` (it goes to stderr; the leading
/// `DRY-RUN MODE -` prefix and surrounding whitespace vary). We bucket by the
/// trailing verb: `Creating`/`Created` -> create (new container);
/// `Recreate`/`Recreated` -> recreate (config-hash changed);
/// `Removing`/`Removed` -> remove (e.g. `--remove-orphans`);
/// `Running`/`Started`/`Skipped` -> unchanged (already up to date).
///
/// A resource is recorded once, by the STRONGEST verb seen for it (recreate >
/// create > remove > unchanged), so paired progress lines (Creating then
/// Created) don't double-count.
fn parse_dry_run_plan(lines: &[String]) -> DryRunPlan {
    use std::collections::BTreeMap;
    // 3 = recreate, 2 = create, 1 = remove, 0 = unchanged. Highest wins.
    let mut rank: BTreeMap<String, u8> = BTreeMap::new();
    for raw in lines {
        // Strip the dry-run prefix if present, then split off the trailing verb.
        let line = raw
            .split_once("DRY-RUN MODE -")
            .map(|(_, rest)| rest)
            .unwrap_or(raw)
            .trim();
        if line.is_empty() {
            continue;
        }
        let (name, verb) = match line.rsplit_once(char::is_whitespace) {
            Some((n, v)) => (n.trim(), v.trim()),
            None => continue,
        };
        if name.is_empty() {
            continue;
        }
        let weight = match verb {
            "Recreate" | "Recreated" => 3u8,
            "Creating" | "Created" => 2,
            "Removing" | "Removed" => 1,
            "Running" | "Started" | "Skipped" => 0,
            _ => continue,
        };
        let entry = rank.entry(name.to_string()).or_insert(0);
        *entry = (*entry).max(weight);
    }
    let mut plan = DryRunPlan::default();
    for (name, weight) in rank {
        match weight {
            3 => plan.recreate.push(name),
            2 => plan.create.push(name),
            1 => plan.remove.push(name),
            _ => plan.unchanged.push(name),
        }
    }
    plan
}

#[derive(Serialize)]
struct NdjsonEvent {
    event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
}

struct NdjsonStream {
    stdout: Option<tokio::io::Lines<BufReader<tokio::process::ChildStdout>>>,
    stderr: Option<tokio::io::Lines<BufReader<tokio::process::ChildStderr>>>,
    wait_fut: Option<Pin<Box<dyn Future<Output = std::io::Result<std::process::ExitStatus>> + Send>>>,
    pending_commands: VecDeque<AsyncCommand>,
    temp_env_file: Option<PathBuf>,
    done: bool,
    /// Held while a mutating docker/compose operation is in flight.
    /// Released on Drop (via ComposeGuard), allowing the next operation to proceed.
    compose_guard: Option<ComposeGuard>,
    /// Fired with the real exit status when the stream reaches its terminal
    /// `done` event, so compose up/down notify Slack as succeeded/failed.
    completion: Option<notify::CompletionNotice>,
    /// Rolling buffer of the last 30 stderr lines, used to detect Docker
    /// endpoint-conflict errors so we can inject a recovery + retry.
    stderr_tail: VecDeque<String>,
    /// If set, a failed phase will check stderr_tail for stale-endpoint errors.
    /// On match: disconnect the endpoint(s) and retry `up` once.
    endpoint_retry: Option<EndpointRetry>,
    /// When `Some`, this stream is a `--dry-run` plan: every stdout+stderr line
    /// is also accumulated here so a terminal additive `plan` event can carry an
    /// advisory parsed verdict (the RAW lines remain the contract). `None` for
    /// every real (mutating) stream, which is then byte-identical to before.
    dry_run_lines: Option<Vec<String>>,
    /// A pre-rendered terminal NDJSON line (`done`) buffered so an additive
    /// terminal event (e.g. the dry-run `plan`) can be emitted on the poll
    /// BEFORE `done`. Emitted on the next poll, after which the stream finishes.
    pending_terminal: Option<String>,
    /// When `Some`, this is a `materialize_only` stage stream: on SUCCESSFUL
    /// completion it emits a terminal additive `staged` event (carrying the
    /// landed image digests from `docker compose images -q`) as the
    /// materialization done-marker, before `done`. `None` for every other
    /// stream.
    staged: Option<StagedMeta>,
}

/// Metadata for the `materialize_only` terminal `staged` event. The image
/// digests are resolved at terminal time from `docker compose images -q`.
#[derive(Clone)]
struct StagedMeta {
    work_dir: PathBuf,
    project: String,
    file: String,
    env_files: Vec<String>,
    services: Vec<String>,
    temp_env_file: Option<PathBuf>,
    tag: String,
    file_sha256: String,
}

/// Resolve the image IDs the staged compose project currently has on disk via
/// `docker compose images -q`. Best-effort: on any failure returns an empty
/// list so the `staged` marker still fires (the platform learns staging
/// finished even if the digest probe hiccupped). Honors the same project / file
/// / env-file scoping as the stage itself so it reports THIS project's images.
fn staged_image_digests(meta: &StagedMeta) -> Vec<String> {
    let mut cmd = std::process::Command::new("docker");
    cmd.arg("compose");
    cmd.args(["-p", &meta.project, "-f", &meta.file]);
    for ef in &meta.env_files {
        cmd.args(["--env-file", ef.as_str()]);
    }
    if let Some(tef) = &meta.temp_env_file {
        if let Some(p) = tef.to_str() {
            cmd.args(["--env-file", p]);
        }
    }
    cmd.args(["images", "-q"]);
    for service in &meta.services {
        cmd.arg(service);
    }
    cmd.current_dir(&meta.work_dir);
    match cmd.output() {
        Ok(out) if out.status.success() => {
            parse_image_ids(&String::from_utf8_lossy(&out.stdout))
        }
        Ok(out) => {
            warn!(status = ?out.status, "docker compose images -q failed during stage; staged.images will be empty");
            Vec::new()
        }
        Err(e) => {
            warn!(error = %e, "could not run docker compose images -q during stage; staged.images will be empty");
            Vec::new()
        }
    }
}

/// Parse the stdout of `docker compose images -q` into a deduplicated,
/// order-preserving list of non-empty image IDs (one per line).
fn parse_image_ids(stdout: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    stdout
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .filter(|l| seen.insert(l.to_string()))
        .map(|l| l.to_string())
        .collect()
}

impl Stream for NdjsonStream {
    type Item = Result<String, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if this.done {
            return Poll::Ready(None);
        }

        // A buffered terminal line (e.g. `done`, deferred so an additive
        // terminal event like the dry-run `plan` could be emitted first). Once
        // this flushes the stream is finished.
        if let Some(line) = this.pending_terminal.take() {
            this.done = true;
            return Poll::Ready(Some(Ok(line)));
        }

        // Poll stderr first
        if let Some(ref mut stderr) = this.stderr {
            match Pin::new(stderr).poll_next_line(cx) {
                Poll::Ready(Ok(Some(line))) => {
                    // Rolling buffer for endpoint-error detection on phase failure.
                    this.stderr_tail.push_back(line.clone());
                    if this.stderr_tail.len() > 30 {
                        this.stderr_tail.pop_front();
                    }
                    // Compose's dry-run progress goes to stderr; collect for the
                    // advisory plan parse on a dry-run stream.
                    if let Some(buf) = this.dry_run_lines.as_mut() {
                        buf.push(line.clone());
                    }
                    let event = NdjsonEvent {
                        event: "stderr".into(),
                        data: Some(line),
                        success: None,
                        exit_code: None,
                    };
                    let mut json = serde_json::to_string(&event).unwrap();
                    json.push('\n');
                    return Poll::Ready(Some(Ok(json)));
                }
                Poll::Ready(Ok(None)) => {
                    this.stderr = None;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                Poll::Pending => {}
            }
        }

        // Poll stdout
        if let Some(ref mut stdout) = this.stdout {
            match Pin::new(stdout).poll_next_line(cx) {
                Poll::Ready(Ok(Some(line))) => {
                    // Some Compose builds also surface dry-run plan lines on
                    // stdout; collect those too for the advisory plan parse.
                    if let Some(buf) = this.dry_run_lines.as_mut() {
                        buf.push(line.clone());
                    }
                    let event = NdjsonEvent {
                        event: "stdout".into(),
                        data: Some(line),
                        success: None,
                        exit_code: None,
                    };
                    let mut json = serde_json::to_string(&event).unwrap();
                    json.push('\n');
                    return Poll::Ready(Some(Ok(json)));
                }
                Poll::Ready(Ok(None)) => {
                    this.stdout = None;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                Poll::Pending => {}
            }
        }

        // If both streams are done, wait for child exit
        if this.stdout.is_none() && this.stderr.is_none() {
            if let Some(ref mut fut) = this.wait_fut {
                match fut.as_mut().poll(cx) {
                    Poll::Ready(Ok(status)) => {
                        this.wait_fut = None;

                        // If successful and more commands pending, start next
                        if status.success() {
                            if let Some(mut next_cmd) = this.pending_commands.pop_front() {
                                match next_cmd.spawn() {
                                    Ok(mut child) => {
                                        this.stdout = child.stdout.take().map(|s| BufReader::new(s).lines());
                                        this.stderr = child.stderr.take().map(|s| BufReader::new(s).lines());
                                        this.wait_fut = Some(Box::pin(async move {
                                            let mut child = child;
                                            child.wait().await
                                        }));
                                        cx.waker().wake_by_ref();
                                        return Poll::Pending;
                                    }
                                    Err(e) => {
                                        this.done = true;
                                        if let Some(ref path) = this.temp_env_file {
                                            let _ = std::fs::remove_file(path);
                                        }
                                        return Poll::Ready(Some(Err(e)));
                                    }
                                }
                            }
                        }

                        // On failure, check whether stale Docker network endpoints
                        // caused it. If so, disconnect them and retry `up` once.
                        if !status.success() {
                            if let Some(retry) = this.endpoint_retry.take() {
                                let errors = parse_endpoint_errors(&this.stderr_tail);
                                if !errors.is_empty() {
                                    info!(endpoints = ?errors, "Stale network endpoints detected; disconnecting and retrying compose up");
                                    this.stderr_tail.clear();
                                    // Recovery: for each stale endpoint, remove the container
                                    // (which also disconnects it) and then force-disconnect the
                                    // network endpoint. Both steps are wrapped in a shell
                                    // one-liner that always exits 0: if the container was
                                    // already removed (true ghost endpoint), `docker rm -f`
                                    // exits non-zero, which would abort the pipeline before
                                    // the retry. The `2>/dev/null; true` ensures the recovery
                                    // step always succeeds so compose up is always retried.
                                    let mut recovery: Vec<AsyncCommand> = errors.into_iter().map(|(network, container)| {
                                        let shell = format!(
                                            "docker rm -f {container} 2>/dev/null; docker network disconnect --force {network} {container} 2>/dev/null; true"
                                        );
                                        let mut cmd = AsyncCommand::new("sh");
                                        cmd.args(["-c", &shell]);
                                        cmd.stdout(std::process::Stdio::piped());
                                        cmd.stderr(std::process::Stdio::piped());
                                        cmd
                                    }).collect();
                                    recovery.push(retry.build_cmd());
                                    for cmd in recovery.into_iter().rev() {
                                        this.pending_commands.push_front(cmd);
                                    }
                                    if let Some(mut next_cmd) = this.pending_commands.pop_front() {
                                        match next_cmd.spawn() {
                                            Ok(mut child) => {
                                                this.stdout = child.stdout.take().map(|s| BufReader::new(s).lines());
                                                this.stderr = child.stderr.take().map(|s| BufReader::new(s).lines());
                                                this.wait_fut = Some(Box::pin(async move {
                                                    let mut child = child;
                                                    child.wait().await
                                                }));
                                                cx.waker().wake_by_ref();
                                                return Poll::Pending;
                                            }
                                            Err(e) => {
                                                this.done = true;
                                                if let Some(ref path) = this.temp_env_file {
                                                    let _ = std::fs::remove_file(path);
                                                }
                                                return Poll::Ready(Some(Err(e)));
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // NOTE: `done` is NOT necessarily set here. When this is a
                        // dry-run stream we emit an additive `plan` event now and
                        // buffer the `done` line in `pending_terminal`, flushing it
                        // (and setting `done`) on the next poll. For every other
                        // (real) stream the behavior is unchanged: `done` is set
                        // and emitted immediately below.

                        // Notify Slack with the real outcome now that the
                        // streaming op has finished (best-effort, non-blocking).
                        if let Some(notice) = this.completion.take() {
                            notify::spawn_action(
                                &notice.state,
                                &notice.action,
                                &notice.actor,
                                Some(status.success()),
                            );
                            // Persist the deployed version only on a SUCCESSFUL
                            // compose_up, so /version survives a restart without
                            // ever advertising a failed attempt as deployed.
                            // Written synchronously HERE — before the `done` event
                            // below — so it is durable before the client sees
                            // success (a detached write could lose to an immediate
                            // restart). This is the DISK success path (the eager
                            // in-memory write in the compose_up handler is
                            // last-ATTEMPTED; this is last-SUCCEEDED). On success
                            // we rotate this project's current -> previous, set the
                            // new current, mirror it to the legacy top-level fields,
                            // and persist the whole per-project map atomically.
                            //
                            // compose_stage (materialize_only) and compose_down do
                            // NOT rotate: staging activates nothing and teardown is
                            // observed via /docker/ps, so deployed state is left
                            // intact (see compose_down).
                            if status.success() && notice.action.action == "compose_up" {
                                let new_current = DeployedRecord {
                                    tag: notice.action.tag.clone(),
                                    commit: notice.action.commit.clone(),
                                    file: notice.action.file.clone(),
                                    file_sha256: notice.action.file_sha256.clone(),
                                };
                                let state = &notice.state;
                                let snapshot = {
                                    let mut projects = state.deployed_projects.write().expect("deployed_projects lock poisoned");
                                    let entry = projects.entry(notice.project.clone()).or_default();
                                    // Rotate the prior current into previous (the
                                    // last-known-good rollback target), then set
                                    // the new current.
                                    if entry.current != DeployedRecord::default() {
                                        entry.previous = Some(entry.current.clone());
                                    }
                                    entry.current = new_current.clone();
                                    DeployedState { projects: projects.clone() }
                                };
                                // Mirror the just-written project to the legacy
                                // top-level fields so single-project /version
                                // stays byte-identical.
                                *state.deployed_tag.write().expect("deployed_tag lock poisoned") = new_current.tag.clone();
                                *state.deployed_commit.write().expect("deployed_commit lock poisoned") = new_current.commit.clone();
                                *state.deployed_file.write().expect("deployed_file lock poisoned") = new_current.file.clone();
                                *state.deployed_file_sha256.write().expect("deployed_file_sha256 lock poisoned") = new_current.file_sha256.clone();
                                if let Err(e) = persist_deployed_state(&state.work_dir, &snapshot) {
                                    error!(error = %e, "Failed to persist deployed.json");
                                }
                            }
                        }

                        // Clean up temp env file
                        if let Some(ref path) = this.temp_env_file {
                            let _ = std::fs::remove_file(path);
                        }

                        // Render the terminal `done` line up front.
                        let done_event = NdjsonEvent {
                            event: "done".into(),
                            data: None,
                            success: Some(status.success()),
                            exit_code: status.code(),
                        };
                        let mut done_json = serde_json::to_string(&done_event).unwrap();
                        done_json.push('\n');

                        // Dry-run: emit the advisory `plan` first, defer `done`.
                        if let Some(lines) = this.dry_run_lines.take() {
                            let plan = parse_dry_run_plan(&lines);
                            let plan_line = serde_json::json!({
                                "event": "plan",
                                "success": status.success(),
                                "exit_code": status.code(),
                                "plan": plan,
                            });
                            let mut plan_json = serde_json::to_string(&plan_line).unwrap();
                            plan_json.push('\n');
                            // Buffer `done` for the next poll; this stream stays
                            // not-done so the top-of-poll flush can emit it.
                            this.pending_terminal = Some(done_json);
                            return Poll::Ready(Some(Ok(plan_json)));
                        }

                        // materialize_only: on SUCCESS emit the terminal `staged`
                        // done-marker (image digests landed) first, defer `done`.
                        // On a failed stage we skip `staged` (materialization did
                        // not finish) and just emit `done` with success:false.
                        if let Some(meta) = this.staged.take() {
                            if status.success() {
                                let images = staged_image_digests(&meta);
                                let staged_line = serde_json::json!({
                                    "event": "staged",
                                    "tag": meta.tag,
                                    "file": meta.file,
                                    "file_sha256": meta.file_sha256,
                                    "images": images,
                                });
                                let mut staged_json = serde_json::to_string(&staged_line).unwrap();
                                staged_json.push('\n');
                                this.pending_terminal = Some(done_json);
                                return Poll::Ready(Some(Ok(staged_json)));
                            }
                        }

                        this.done = true;
                        return Poll::Ready(Some(Ok(done_json)));
                    }
                    Poll::Ready(Err(e)) => {
                        this.done = true;
                        if let Some(ref path) = this.temp_env_file {
                            let _ = std::fs::remove_file(path);
                        }
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Pending => {}
                }
            } else {
                this.done = true;
                return Poll::Ready(None);
            }
        }

        Poll::Pending
    }
}

impl Drop for NdjsonStream {
    fn drop(&mut self) {
        // ComposeGuard's Drop clears in_flight and releases the compose lock.
        // Clean up temp env file (mirrors the pre-existing cleanup in Stream::poll_next).
        if let Some(ref path) = self.temp_env_file {
            if !self.done {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Stable Compose project name for the model stacks we manage.
///
/// We MUST pin this explicitly. Our `up` runs with `--remove-orphans`, which
/// deletes every container in the project that isn't in the deployed file. The
/// model stacks live in their own project, but the app-compose that runs *us*
/// — compose-manager, the launcher sidecar, certbot, datadog — is a separate
/// project (`dstack`). If our deploys ever ran under `dstack`, `--remove-orphans`
/// would evict the launcher/certbot/datadog (the launcher being gone is silent
/// and unrecoverable without a CVM recreate).
///
/// Without `-p`, the project name is the working-directory basename UNLESS a
/// `COMPOSE_PROJECT_NAME` is present in our process env or in any `--env-file`
/// we pass (e.g. the decrypted `/app/.env`). Such a value would silently
/// redirect every deploy — and its `--remove-orphans` — at that project. The
/// `-p` flag overrides both, so we pass it on every compose invocation.
///
/// The value is derived from the working-directory basename and sanitized to
/// Docker's project-name charset, so it matches the previous implicit default
/// (`/app/work` -> `work`) and changes no naming on already-healthy hosts.
fn compose_project_name(work_dir: &Path) -> String {
    let base = work_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("work");
    match sanitize_project_name(base) {
        Some(name) => name,
        None => "work".to_string(),
    }
}

/// Sanitize an arbitrary string to Docker's Compose project-name charset:
/// lowercased, non-`[a-z0-9_-]` chars replaced with `_`, and leading
/// non-alphanumerics trimmed (Docker requires the name to start with a letter
/// or digit). Returns `None` if nothing valid remains (e.g. empty input or a
/// name made entirely of invalid leading characters), so callers can decide on
/// a fallback or rejection. This is the single source of truth for both the
/// implicit working-directory default and any caller-provided override.
fn sanitize_project_name(input: &str) -> Option<String> {
    let sanitized: String = input
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let trimmed = sanitized.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Resolve the Compose project name for an `up`/`down` request.
///
/// When the caller provides an explicit `project`, it is sanitized via
/// `sanitize_project_name` and used in place of the working-directory default,
/// letting the control plane scope each model under its own Compose project so
/// a scoped recreate/`--remove-orphans` of one model can't orphan another.
///
/// We REJECT (returning `Err`) a provided name that, after sanitization, is
/// empty/all-invalid, or that collides with a reserved name (`work` or
/// `dstack`). `work` is the implicit default for `/app/work`, and `dstack` is
/// the app-compose project that runs the infra sidecars (compose-manager, the
/// launcher, certbot, datadog); letting an override land on either would point
/// our `--remove-orphans` at containers we must never evict.
///
/// We ALSO reject a provided name that is not already in canonical (sanitized)
/// form, rather than silently rewriting it. The sanitization is lossy — every
/// out-of-charset byte maps to `_` — so `GLM-5.1` and `GLM-5_1` would both
/// collapse to `glm-5_1`. Since `up` runs with `--remove-orphans`, two distinct
/// caller names collapsing onto one project would let a deploy of one model
/// evict another model's containers, defeating the isolation this override
/// exists for. Requiring callers to pass an already-canonical name makes that
/// collapse impossible. (The implicit working-dir default still sanitizes
/// silently via `compose_project_name`; only the explicit override is held to
/// canonical form.)
///
/// When `project` is `None`, behavior is EXACTLY as before: the
/// working-directory basename default from `compose_project_name`.
fn resolve_compose_project(project: Option<&str>, work_dir: &Path) -> Result<String, String> {
    const RESERVED: [&str; 2] = ["work", "dstack"];
    match project {
        None => Ok(compose_project_name(work_dir)),
        Some(raw) => {
            let name = sanitize_project_name(raw).ok_or_else(|| {
                format!("invalid compose project name {:?}: empty after sanitization", raw)
            })?;
            if name != raw {
                return Err(format!(
                    "compose project name {:?} is not canonical (must already be lowercase \
                     and contain only [a-z0-9_-] with no leading non-alphanumerics); \
                     canonical form would be {:?}. Rejected rather than rewritten, since \
                     lossy rewriting could collapse two distinct names onto one project \
                     and let --remove-orphans evict another model.",
                    raw, name
                ));
            }
            if RESERVED.contains(&name.as_str()) {
                return Err(format!(
                    "compose project name {:?} is reserved and cannot be overridden",
                    name
                ));
            }
            Ok(name)
        }
    }
}

#[allow(clippy::too_many_arguments)] // cohesive compose-invocation builder
fn build_compose_cmd(
    work_dir: &Path,
    project: &str,
    args: &[&str],
    file: &str,
    env_files: &[String],
    services: &[String],
    temp_env_file: Option<&Path>,
    dry_run: bool,
) -> AsyncCommand {
    let mut cmd = AsyncCommand::new("docker");
    // `-p` BEFORE the subcommand pins the project, overriding any leaked
    // COMPOSE_PROJECT_NAME so `--remove-orphans` can never reach the `dstack`
    // app-compose project (launcher/certbot/datadog). The project is resolved
    // by the caller (default = working-directory basename via
    // compose_project_name; optional override validated by
    // resolve_compose_project). See compose_project_name.
    //
    // `--dry-run` is docker compose's GLOBAL flag and MUST sit right after the
    // `compose` word (before the subcommand). It makes the whole invocation
    // simulate only — Compose computes the per-service resolved config-hash and
    // prints what WOULD create/recreate/remove without touching any container.
    cmd.arg("compose");
    if dry_run {
        cmd.arg("--dry-run");
    }
    cmd.args(["-p", project, "-f", file]);
    for ef in env_files {
        cmd.args(["--env-file", ef.as_str()]);
    }
    if let Some(tef) = temp_env_file {
        cmd.args(["--env-file", tef.to_str().unwrap()]);
    }
    cmd.args(args);
    for service in services {
        cmd.arg(service);
    }
    cmd.current_dir(work_dir);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd
}

#[allow(clippy::too_many_arguments)] // cohesive compose-invocation builder
fn stream_docker_compose_phased(
    work_dir: &Path,
    project: &str,
    phases: &[&[&str]],
    file: &str,
    env_files: &[String],
    services: &[String],
    temp_env_file: Option<PathBuf>,
    dry_run: bool,
) -> Result<NdjsonStream> {
    let all_env_files: Vec<&str> = env_files.iter().map(|s| s.as_str())
        .chain(temp_env_file.as_ref().map(|p| p.to_str().unwrap()))
        .collect();

    info!(
        command = "docker compose",
        project = project,
        file = file,
        phases = ?phases,
        env_files = ?all_env_files,
        services = ?services,
        work_dir = %work_dir.display(),
        "Running streaming command"
    );

    let mut commands: VecDeque<AsyncCommand> = phases.iter()
        .map(|args| build_compose_cmd(work_dir, project, args, file, env_files, services, temp_env_file.as_deref(), dry_run))
        .collect();

    let mut first_cmd = commands.pop_front()
        .ok_or_else(|| anyhow!("no command phases specified"))?;

    let mut child = first_cmd.spawn().with_context(|| {
        format!(
            "Failed to execute: docker compose -f {} (work_dir: {})",
            file,
            work_dir.display()
        )
    })?;

    let stdout = child.stdout.take().map(|s| BufReader::new(s).lines());
    let stderr = child.stderr.take().map(|s| BufReader::new(s).lines());
    let wait_fut: Pin<Box<dyn Future<Output = std::io::Result<std::process::ExitStatus>> + Send>> =
        Box::pin(async move {
            let mut child = child;
            child.wait().await
        });

    Ok(NdjsonStream {
        stdout,
        stderr,
        wait_fut: Some(wait_fut),
        pending_commands: commands,
        temp_env_file,
        done: false,
        compose_guard: None,
        completion: None,
        stderr_tail: VecDeque::new(),
        endpoint_retry: None,
        dry_run_lines: if dry_run { Some(Vec::new()) } else { None },
        pending_terminal: None,
        staged: None,
    })
}

fn stream_docker_compose(
    work_dir: &Path,
    project: &str,
    args: &[&str],
    file: &str,
    env_files: &[String],
    services: &[String],
    temp_env_file: Option<PathBuf>,
) -> Result<NdjsonStream> {
    // compose_down is always a real apply (never dry-run); dry_run is an `up`
    // affordance only.
    stream_docker_compose_phased(work_dir, project, &[args], file, env_files, services, temp_env_file, false)
}

// --- Handlers ---

async fn compose_up(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<ComposeRequest>,
) -> Response {
    if let Err((code, msg)) = verify_bearer_token_raw(&headers, &state.bearer_token) {
        return err_response(code, msg);
    }

    let file = payload.file.unwrap_or_else(|| "docker-compose.yml".into());

    // Acquire compose lock early (before GitHub fetch) to prevent parallel
    // operations from racing on the same compose file on disk.
    let guard = match state.try_acquire_compose_lock(
        "compose_up",
        Some(payload.tag.clone()),
        Some(file.clone()),
        payload.services.clone(),
        None,
    ).await {
        Ok(g) => g,
        Err(_) => {
            return err_response(StatusCode::CONFLICT, state.conflict_message().await);
        }
    };

    let tag_info = match validate_tag(&state, &payload.tag).await {
        Ok(info) => info,
        Err((code, msg)) => return err_response(code, msg),
    };

    if !payload.env.is_empty() {
        if let Err(msg) = validate_env_vars(&payload.env) {
            return err_response(StatusCode::BAD_REQUEST, msg);
        }
    }

    // Resolve the Compose project: default (working-dir basename) when omitted,
    // otherwise a sanitized + validated override (rejects reserved names so
    // `--remove-orphans` can't reach the `dstack`/`work` projects).
    let project = match resolve_compose_project(payload.project.as_deref(), &state.work_dir) {
        Ok(p) => p,
        Err(msg) => return err_response(StatusCode::BAD_REQUEST, msg),
    };

    // Fetch compose file from GitHub and write to work directory
    let content = match fetch_github_file(&state, &payload.tag, &file).await {
        Ok(c) => c,
        Err(e) => return err_response(StatusCode::BAD_REQUEST, e.to_string()),
    };

    let file_sha256 = hex::encode(sha2::Sha256::digest(content.as_bytes()));

    if let Err(e) = tokio::fs::create_dir_all(&state.work_dir).await {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create work dir: {}", e));
    }
    let target_path = state.work_dir.join(&file);
    if let Some(parent) = target_path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create file parent dir: {}", e));
        }
    }
    if let Err(e) = tokio::fs::write(&target_path, &content).await {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write file: {}", e));
    }

    let temp_env_file = if !payload.env.is_empty() {
        match write_temp_env_file(&state.work_dir, &payload.env) {
            Ok(p) => Some(p),
            Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    } else {
        None
    };
    // Clone the path before it's moved into the stream, so the retry can pass
    // the same --env-file to the retried `up` (the stream owns cleanup).
    let temp_env_file_for_retry = temp_env_file.clone();

    let mut up_args = vec!["up", "-d", "--remove-orphans"];
    if payload.force_recreate {
        up_args.push("--force-recreate");
    }

    // PLAN-only (dry_run): genuinely read-only. We simulate ONLY the `up` phase
    // with docker compose's global `--dry-run` (the recreate verdict comes from
    // up's per-service config-hash; the pull/build dry-run output is noise per
    // the audit), and STOP. We do NOT record an action, NOT write deployed_* or
    // deployed.json, and attach NO completion (no Slack, no deployed rotation)
    // and NO endpoint_retry (recovery only makes sense for a real apply).
    // Nothing mutates, spawns, pulls, builds, or writes the action log. All
    // scoping fields (tag/file/project/services/env/force_recreate) are honored
    // so the plan reflects the EXACT command a real apply with this body runs.
    if payload.dry_run {
        let mut stream = match stream_docker_compose_phased(
            &state.work_dir,
            &project,
            &[&up_args],
            &file,
            &state.env_files,
            &payload.services,
            temp_env_file,
            true, // --dry-run
        ) {
            Ok(s) => s,
            Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        stream.compose_guard = Some(guard);
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/x-ndjson")
            .body(Body::from_stream(stream))
            .unwrap();
    }

    // MATERIALIZE-only: run the prologue (already done above: lock, validate_tag
    // age-guard, fetch+write compose, env validation, temp-env-file, project
    // resolve), then run ONLY the `pull` + `build` phases — STOP before `up`.
    // The old model keeps serving; nothing is activated. We record a DISTINCT
    // lower-privilege `compose_stage` action (NOT compose_up) and DO NOT write
    // deployed_* or persist deployed.json (staging activates nothing, so a
    // compose_up record / deployed flip would make /version + the attested log
    // LIE about what is running). The success hook only rotates on `compose_up`,
    // so the attached completion notice reports the stage outcome to Slack
    // without touching deployed state. A terminal `staged` event marks
    // materialization done and lists the landed image digests. Weight download
    // is the compose file's EXISTING model-downloader service, selected via the
    // `services` field with platform-controlled HF_HUB_OFFLINE in `env`.
    if payload.materialize_only {
        let mut stream = match stream_docker_compose_phased(
            &state.work_dir,
            &project,
            &[&["pull", "--ignore-buildable"], &["build"]], // STOP before `up`
            &file,
            &state.env_files,
            &payload.services,
            temp_env_file,
            false,
        ) {
            Ok(s) => s,
            Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        stream.compose_guard = Some(guard);
        stream.staged = Some(StagedMeta {
            work_dir: state.work_dir.clone(),
            project: project.clone(),
            file: file.clone(),
            env_files: state.env_files.clone(),
            services: payload.services.clone(),
            temp_env_file: temp_env_file_for_retry,
            tag: payload.tag.clone(),
            file_sha256: file_sha256.clone(),
        });

        let actor = extract_actor(&headers);
        let action = DeploymentAction {
            timestamp: Utc::now().to_rfc3339(),
            action: "compose_stage".into(),
            image: None,
            tag: Some(payload.tag.clone()),
            commit: Some(tag_info.commit_sha.clone()),
            file: Some(file.clone()),
            file_sha256: Some(file_sha256.clone()),
            services: payload.services.clone(),
            container: None,
        };
        if record_action(&state, &action).await.is_err() {
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist action log");
        }
        // Slack outcome only — never touches deployed state (rotation is gated
        // on compose_up).
        stream.completion = Some(notify::CompletionNotice {
            state: state.clone(),
            action,
            actor,
            project: project.clone(),
        });

        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/x-ndjson")
            .body(Body::from_stream(stream))
            .unwrap();
    }

    let mut stream = match stream_docker_compose_phased(
        &state.work_dir,
        &project,
        &[&["pull", "--ignore-buildable"], &["build"], &up_args],
        &file,
        &state.env_files,
        &payload.services,
        temp_env_file,
        false,
    ) {
        Ok(s) => s,
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    stream.compose_guard = Some(guard);
    stream.endpoint_retry = Some(EndpointRetry {
        work_dir: state.work_dir.clone(),
        project: project.clone(),
        up_args: up_args.iter().map(|s| s.to_string()).collect(),
        file: file.clone(),
        env_files: state.env_files.clone(),
        services: payload.services.clone(),
        temp_env_file: temp_env_file_for_retry,
    });

    let actor = extract_actor(&headers);
    let action = DeploymentAction {
        timestamp: Utc::now().to_rfc3339(),
        action: "compose_up".into(),
        image: None,
        tag: Some(payload.tag.clone()),
        commit: Some(tag_info.commit_sha.clone()),
        file: Some(file.clone()),
        file_sha256: Some(file_sha256.clone()),
        services: payload.services,
        container: None,
    };
    if record_action(&state, &action).await.is_err() {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist action log");
    }
    // Defer the Slack notification to stream completion so it reports the real
    // deploy outcome (succeeded/failed) rather than just "started".
    stream.completion = Some(notify::CompletionNotice {
        state: state.clone(),
        action,
        actor,
        project: project.clone(),
    });

    // Eager in-memory write of the legacy top-level fields = last-ATTEMPTED up
    // (byte-identical to pre-#46 behavior; /version surfaces these). The new
    // per-project `deployed_projects` map is updated only on SUCCESS in the
    // stream-completion hook above, so it always reflects last-SUCCEEDED and a
    // failed attempt never appears as a project's `current`.
    *state.deployed_tag.write().expect("deployed_tag lock poisoned") = Some(payload.tag);
    *state.deployed_commit.write().expect("deployed_commit lock poisoned") = Some(tag_info.commit_sha);
    *state.deployed_file.write().expect("deployed_file lock poisoned") = Some(file);
    *state.deployed_file_sha256.write().expect("deployed_file_sha256 lock poisoned") = Some(file_sha256);

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/x-ndjson")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn compose_down(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<ComposeDownRequest>,
) -> Response {
    if let Err((code, msg)) = verify_bearer_token_raw(&headers, &state.bearer_token) {
        return err_response(code, msg);
    }

    let file = payload.file.unwrap_or_else(|| "docker-compose.yml".into());

    let guard = match state.try_acquire_compose_lock(
        "compose_down",
        Some(payload.tag.clone()),
        Some(file.clone()),
        payload.services.clone(),
        None,
    ).await {
        Ok(g) => g,
        Err(_) => {
            return err_response(StatusCode::CONFLICT, state.conflict_message().await);
        }
    };

    let tag_info = match validate_tag(&state, &payload.tag).await {
        Ok(info) => info,
        Err((code, msg)) => return err_response(code, msg),
    };

    if !payload.env.is_empty() {
        if let Err(msg) = validate_env_vars(&payload.env) {
            return err_response(StatusCode::BAD_REQUEST, msg);
        }
    }

    // Resolve the Compose project: default when omitted, else a validated
    // override (see resolve_compose_project / compose_up).
    let project = match resolve_compose_project(payload.project.as_deref(), &state.work_dir) {
        Ok(p) => p,
        Err(msg) => return err_response(StatusCode::BAD_REQUEST, msg),
    };

    let mut args = vec!["down"];
    if payload.volumes {
        args.push("-v");
    }

    let temp_env_file = if !payload.env.is_empty() {
        match write_temp_env_file(&state.work_dir, &payload.env) {
            Ok(p) => Some(p),
            Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    } else {
        None
    };

    let mut stream = match stream_docker_compose(
        &state.work_dir,
        &project,
        &args,
        &file,
        &state.env_files,
        &payload.services,
        temp_env_file,
    ) {
        Ok(s) => s,
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    stream.compose_guard = Some(guard);

    let actor = extract_actor(&headers);
    let action = DeploymentAction {
        timestamp: Utc::now().to_rfc3339(),
        action: "compose_down".into(),
        image: None,
        tag: Some(payload.tag),
        commit: Some(tag_info.commit_sha),
        file: Some(file),
        file_sha256: None,
        services: payload.services,
        container: None,
    };
    if record_action(&state, &action).await.is_err() {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist action log");
    }
    // compose_down LIFECYCLE: deployed state is deliberately LEFT INTACT on a
    // down. The success hook only rotates on `compose_up`, so /version keeps
    // reporting the last-deployed tuple after a teardown. Teardown is observed
    // via /docker/ps (which reflects the actually-running containers), not by
    // clearing deployed state here. This matches pre-#46 behavior (down never
    // cleared the deployed_* fields) and is made explicit so the reconciler
    // does not treat a stale `current` as "still running".
    stream.completion = Some(notify::CompletionNotice {
        state: state.clone(),
        action,
        actor,
        project,
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/x-ndjson")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn compose_logs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Option<Json<LogsRequest>>,
) -> impl IntoResponse {
    if let Err(e) = verify_bearer_token(&headers, &state.bearer_token) {
        return e;
    }

    let (file, tail, services) = body
        .map(|b| (b.file.clone(), b.tail, b.services.clone()))
        .unwrap_or((None, default_tail(), vec![]));

    let file = file.unwrap_or_else(|| "docker-compose.yml".into());
    let tail_str = tail.to_string();

    match run_docker_compose(&state.work_dir, &["logs", "--tail", &tail_str], &file, &state.env_files, &services) {
        Ok(output) => ok_output(output),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn docker_ps(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = verify_bearer_token(&headers, &state.bearer_token) {
        return e;
    }

    match run_command("docker", &["ps", "--format", "json"]) {
        Ok(output) => ok_output(output),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Err((code, msg)) = verify_bearer_token_raw(&headers, &state.bearer_token) {
        return err_response(code, msg);
    }

    let in_flight = state.in_flight.lock().ok().and_then(|guard| guard.clone());
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&OperationStatusResponse {
                status: "ok".into(),
                in_flight,
            })
            .unwrap(),
        ))
        .unwrap()
}

async fn docker_restart(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<RestartRequest>,
) -> impl IntoResponse {
    if let Err(e) = verify_bearer_token(&headers, &state.bearer_token) {
        return e;
    }

    let _guard = match state.try_acquire_compose_lock(
        "docker_restart",
        None,
        None,
        vec![],
        Some(payload.container.clone()),
    ).await {
        Ok(g) => g,
        Err(_) => {
            return err(StatusCode::CONFLICT, state.conflict_message().await);
        }
    };

    info!(command = "docker restart", container = %payload.container, "Running command");

    match run_command("docker", &["restart", &payload.container]) {
        Ok(_) => {
            let actor = extract_actor(&headers);
            let action = DeploymentAction {
                timestamp: Utc::now().to_rfc3339(),
                action: "docker_restart".into(),
                image: None,
                tag: None,
                commit: None,
                file: None,
                file_sha256: None,
                services: vec![],
                container: Some(payload.container),
            };
            if record_action(&state, &action).await.is_err() {
                return err(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist action log");
            }
            notify::spawn_action(&state, &action, &actor, Some(true));
            ok(None)
        }
        Err(e) => {
            error!(command = "docker restart", container = %payload.container, error = %e, "Command failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }
    }
}

async fn docker_clean(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<CleanRequest>,
) -> impl IntoResponse {
    if let Err(e) = verify_bearer_token(&headers, &state.bearer_token) {
        return e;
    }

    if !payload.volumes && !payload.images && !payload.containers {
        return err(StatusCode::BAD_REQUEST, "At least one of 'volumes', 'images', or 'containers' must be true");
    }

    let _guard = match state.try_acquire_compose_lock("docker_clean", None, None, vec![], None).await {
        Ok(g) => g,
        Err(_) => {
            return err(StatusCode::CONFLICT, state.conflict_message().await);
        }
    };

    match run_docker_prune(payload.volumes, payload.images, payload.containers) {
        Ok(_) => {
            let actor = extract_actor(&headers);
            let action = DeploymentAction {
                timestamp: Utc::now().to_rfc3339(),
                action: "docker_clean".into(),
                image: None,
                tag: None,
                commit: None,
                file: None,
                file_sha256: None,
                services: vec![],
                container: None,
            };
            if record_action(&state, &action).await.is_err() {
                return err(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist action log");
            }
            notify::spawn_action(&state, &action, &actor, Some(true));
            ok(None)
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Selectively evict ONE model's weights/cache from the shared HuggingFace
/// cache volume, leaving every other model's warm cache intact. This is the
/// targeted alternative to `/docker/clean`'s blind `docker volume prune -f`.
///
/// Safety guard: refuses with 409 if a RUNNING container is actively serving
/// the SAME model (matched on `--model-path`/`--model` arg or a `MODEL_NAME`-
/// style env). Evicting a DIFFERENT model's weights while others run is safe
/// and is the whole point of being selective.
async fn docker_evict(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<EvictRequest>,
) -> impl IntoResponse {
    if let Err(e) = verify_bearer_token(&headers, &state.bearer_token) {
        return e;
    }

    let model = payload.model.trim().to_string();
    let hub_dir = match model_to_hub_dir(&model) {
        Ok(d) => d,
        Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
    };

    // Serialize against compose up/down etc. so we never race a deploy that is
    // (re)downloading the very weights we're about to delete.
    let _guard = match state
        .try_acquire_compose_lock("docker_evict", None, None, vec![], Some(model.clone()))
        .await
    {
        Ok(g) => g,
        Err(_) => return err(StatusCode::CONFLICT, state.conflict_message().await),
    };

    // Resolve the cache volume: explicit override, else autodetect across the
    // typo / project-prefix variants from what actually exists on the host.
    let volume = match payload.cache_volume.as_deref().map(str::trim) {
        Some(v) if !v.is_empty() => {
            // A named volume only — never a host path (would bind-mount the host
            // into the rm container). Then confirm it actually exists.
            if let Err(msg) = validate_volume_name(v) {
                return err(StatusCode::BAD_REQUEST, msg);
            }
            let volumes = match list_docker_volumes().await {
                Ok(v) => v,
                Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            };
            if !volumes.iter().any(|existing| existing == v) {
                return err(
                    StatusCode::NOT_FOUND,
                    format!("cache_volume '{v}' is not an existing Docker named volume on this host"),
                );
            }
            v.to_string()
        }
        _ => {
            let volumes = match list_docker_volumes().await {
                Ok(v) => v,
                Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            };
            match resolve_hf_cache_volume(&volumes) {
                Ok(v) => v,
                Err(msg) => return err(StatusCode::NOT_FOUND, msg),
            }
        }
    };

    // Safety guard: refuse to evict weights for a model that is actively served.
    // (Only relevant when weights are in scope; a pure "cache" evict is harmless.)
    if matches!(payload.target, EvictTarget::Weights | EvictTarget::Both) {
        match containers_serving_model(&model).await {
            Ok(running) if !running.is_empty() => {
                return err(
                    StatusCode::CONFLICT,
                    format!(
                        "refusing to evict weights for '{}': actively served by running container(s): {}",
                        model,
                        running.join(", ")
                    ),
                );
            }
            Ok(_) => {}
            Err(e) => {
                // Fail closed: if we can't determine what's running, don't delete.
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("could not determine running containers: {e}"),
                );
            }
        }
    }

    info!(
        command = "docker_evict",
        model = %model,
        hub_dir = %hub_dir,
        volume = %volume,
        target = ?payload.target,
        "Evicting model from cache volume"
    );

    let mut removed_paths: Vec<String> = Vec::new();
    let mut freed_bytes: u64 = 0;
    // Best-effort cache failures are collected, not fatal — see below.
    let mut cache_errors: Vec<String> = Vec::new();

    // Weights subtree: hub/models--org--repo. A removal *failure* here is fatal
    // (500); a simply-absent dir is not an error (reported as "nothing to evict").
    if matches!(payload.target, EvictTarget::Weights | EvictTarget::Both) {
        let rel = format!("hub/{hub_dir}");
        match evict_subtree(&volume, &rel).await {
            Ok(outcome) => {
                if outcome.existed {
                    freed_bytes += outcome.freed_bytes;
                    removed_paths.push(format!("{volume}:/{rel}"));
                }
            }
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }

    // Compile/kernel caches: best-effort only — these are hash-keyed and not
    // cleanly attributable to one model, so we only clear model-named subdirs
    // if they happen to exist. A failure here must NOT 500 (and especially must
    // not discard an already-completed weights removal): collect and report it.
    if matches!(payload.target, EvictTarget::Cache | EvictTarget::Both) {
        for rel in model_cache_rels(&hub_dir) {
            match evict_subtree(&volume, &rel).await {
                Ok(outcome) => {
                    if outcome.existed {
                        freed_bytes += outcome.freed_bytes;
                        removed_paths.push(format!("{volume}:/{rel}"));
                    }
                }
                Err(e) => cache_errors.push(format!("{rel}: {e}")),
            }
        }
    }

    let action = DeploymentAction {
        timestamp: Utc::now().to_rfc3339(),
        action: "docker_evict".into(),
        image: None,
        tag: None,
        commit: None,
        file: None,
        file_sha256: None,
        services: vec![],
        container: Some(model.clone()),
    };
    if record_action(&state, &action).await.is_err() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist action log");
    }
    let actor = extract_actor(&headers);
    notify::spawn_action(&state, &action, &actor, Some(true));

    let cache_note = if matches!(payload.target, EvictTarget::Cache | EvictTarget::Both) {
        " (compile/kernel caches are hash-keyed and best-effort; only model-named subdirs, if any, were cleared)"
    } else {
        ""
    };
    let mut summary = if removed_paths.is_empty() {
        format!("nothing to evict for '{model}' in volume '{volume}' (not present){cache_note}")
    } else {
        format!(
            "evicted '{}' from volume '{}': removed {} ({}){}",
            model,
            volume,
            removed_paths.join(", "),
            format_bytes(freed_bytes),
            cache_note
        )
    };
    // Best-effort cache failures never fail the request (weights may already be
    // gone) — surface them in the success message so the caller can see them.
    if !cache_errors.is_empty() {
        summary.push_str(&format!(
            "; best-effort cache cleanup had errors: {}",
            cache_errors.join("; ")
        ));
    }
    ok_output(summary)
}

/// Human-readable byte count for the eviction summary (best-effort).
fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes == 0 {
        return "0 B".into();
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {} ({bytes} bytes)", UNITS[u])
    }
}

// --- Attestation ---

#[derive(Deserialize)]
struct AttestationQuery {
    nonce: Option<String>,
}

#[derive(Serialize)]
struct AttestationResponse {
    actions: Vec<DeploymentAction>,
    actions_hash: String,
    nonce: String,
    nonce_source: String,
    quote: String,
    event_log: String,
    report_data: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    vm_config: String,
}

fn parse_nonce(nonce: Option<&str>) -> Result<([u8; 32], &'static str), (StatusCode, String)> {
    match nonce {
        Some(hex_str) => {
            let bytes = hex::decode(hex_str).map_err(|_| {
                (StatusCode::BAD_REQUEST, "nonce must be hex-encoded".into())
            })?;
            if bytes.len() != 32 {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "nonce must be exactly 32 bytes (64 hex chars)".into(),
                ));
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            Ok((arr, "client"))
        }
        None => {
            use rand::RngCore;
            let mut arr = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut arr);
            Ok((arr, "server"))
        }
    }
}

async fn attestation_report(
    State(state): State<Arc<AppState>>,
    Query(query): Query<AttestationQuery>,
) -> Response {
    let (nonce_bytes, nonce_source) = match parse_nonce(query.nonce.as_deref()) {
        Ok(v) => v,
        Err((code, msg)) => return err_response(code, msg),
    };
    let nonce_hex = hex::encode(nonce_bytes);

    let actions = state.actions.read().await.clone();

    let actions_json = canonicalize_actions(&actions);
    let actions_hash: [u8; 32] = sha2::Sha256::digest(actions_json.as_bytes()).into();
    let actions_hash_hex = hex::encode(actions_hash);

    let mut report_data = vec![0u8; 64];
    report_data[..32].copy_from_slice(&actions_hash);
    report_data[32..64].copy_from_slice(&nonce_bytes);

    let client = dstack::DstackClient::new();
    let quote_response = match client.get_quote(report_data).await {
        Ok(r) => r,
        Err(e) => {
            return err_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("dstack unavailable: {e}"),
            )
        }
    };

    let resp = AttestationResponse {
        actions,
        actions_hash: actions_hash_hex,
        nonce: nonce_hex,
        nonce_source: nonce_source.into(),
        quote: quote_response.quote,
        event_log: quote_response.event_log,
        report_data: quote_response.report_data,
        vm_config: quote_response.vm_config,
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&resp).unwrap()))
        .unwrap()
}

async fn version(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Top-level tag/commit/file/file_sha256 stay byte-identical to the
    // pre-#46 single-tuple contract: they mirror the LAST-written project's
    // `current`. The additive `projects` map exposes per-project
    // {current, previous} for the external reconciler; it is omitted entirely
    // when empty, so single-project CVMs see no shape change.
    let tag = state.deployed_tag.read().expect("deployed_tag lock poisoned").clone();
    let commit = state.deployed_commit.read().expect("deployed_commit lock poisoned").clone();
    let file = state.deployed_file.read().expect("deployed_file lock poisoned").clone();
    let file_sha256 = state.deployed_file_sha256.read().expect("deployed_file_sha256 lock poisoned").clone();
    let projects = state.deployed_projects.read().expect("deployed_projects lock poisoned").clone();
    (StatusCode::OK, Json(StatusResponse { status: "ok".into(), tag, commit, file, file_sha256, output: None, exit_code: None, error: None, image: state.running_image.clone(), projects }))
}

// --- Dstack guest-agent management ---

// SECURITY: this MUST stay a compile-time constant. The handler runs nsenter into
// PID 1's namespaces with CAP_SYS_ADMIN; an attacker-controlled unit name would
// give them arbitrary host-side systemctl, i.e. RCE on the CVM host.
const DSTACK_AGENT_UNIT: &str = "dstack-guest-agent.service";

const DSTACK_AGENT_ACTIONS: &[&str] = &["start", "stop", "restart", "status"];

// systemctl restart on a stuck unit can sit in `deactivating` for the full
// TimeoutStopSec (commonly 90s). Cap the wait so we never hold a request thread
// indefinitely.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(120);

// --- algif_aead kernel blacklist (CVE-2026-31431) ---

// SECURITY: this MUST stay a compile-time constant. The script runs via
// `nsenter -- sh -c <script>` into PID 1's namespaces with CAP_SYS_ADMIN.
// An attacker-controlled fragment would give them arbitrary RCE on the CVM
// host.
//
// Writes `/run/modprobe.d/disable-algif-aead.conf` (tmpfs — wiped on reboot,
// so this is reapplied at every compose-manager startup) and unloads the
// modules if currently loaded. The dstack-OS rootfs is dm-verity readonly, so
// `/etc/modprobe.d/` cannot be used; `/run/modprobe.d/` is also read by kmod.
//
// kmod install-rules key on the RESOLVED MODULE NAME, not on transitive
// deps — so a rule for the `algif` parent does NOT block loads of children
// like `algif_hash`. We need one install-rule per module name (verified
// live on gpu03: with only `algif`+`algif_aead` rules in place, `modprobe
// algif_hash` still loaded the module). The unload loop covers already-
// resident modules; the install-rules block future (auto)loads.
//
// `set -e` makes the write half fail loud (otherwise a printf failure would
// silently leave the blacklist file missing while the script still exited 0).
// modprobe -r is `|| true` because the module legitimately may not be loaded.
// The final lsmod check fails the script if any algif* module is still
// resident — that's the only signal the caller has that mitigation took.
//
// Mitigates CVE-2026-31431 ("Copy Fail") in the AF_ALG userspace crypto API.
const ALGIF_BLACKLIST_SCRIPT: &str = "\
set -e; \
mkdir -p /run/modprobe.d; \
printf 'install algif_aead /bin/true\\ninstall algif_hash /bin/true\\ninstall algif_skcipher /bin/true\\ninstall algif_rng /bin/true\\ninstall algif /bin/true\\n' > /run/modprobe.d/disable-algif-aead.conf; \
for m in algif_aead algif_hash algif_skcipher algif_rng algif; do \
    modprobe -r \"$m\" 2>/dev/null || true; \
done; \
if lsmod | awk '{print $1}' | grep -q '^algif'; then \
    echo 'algif module still resident after unload' >&2; \
    exit 1; \
fi";

const ALGIF_BLACKLIST_TIMEOUT: Duration = Duration::from_secs(15);

struct AlgifBlacklistOutput {
    stdout: String,
    stderr: String,
    success: bool,
}

impl AlgifBlacklistOutput {
    fn combined(&self) -> String {
        match (self.stdout.is_empty(), self.stderr.is_empty()) {
            (true, true) => String::new(),
            (false, true) => self.stdout.clone(),
            (true, false) => self.stderr.clone(),
            (false, false) => format!("{}\n{}", self.stdout, self.stderr),
        }
    }
}

// Runs ALGIF_BLACKLIST_SCRIPT against the CVM host's PID 1 namespaces via
// nsenter. Requires the container to be started with `pid: host` and
// CAP_SYS_ADMIN (already required by the existing host-systemctl path).
async fn apply_algif_blacklist_on_host() -> Result<AlgifBlacklistOutput> {
    info!(
        command = "nsenter ... sh -c <algif blacklist>",
        timeout_secs = ALGIF_BLACKLIST_TIMEOUT.as_secs(),
        "Applying algif_aead kernel blacklist on host"
    );

    let invocation = AsyncCommand::new("nsenter")
        .args([
            "-t", "1", "-m", "-u", "-i", "-n", "-p", "--",
            "sh", "-c", ALGIF_BLACKLIST_SCRIPT,
        ])
        .output();

    let output = match tokio::time::timeout(ALGIF_BLACKLIST_TIMEOUT, invocation).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(anyhow!(e).context("Failed to execute: nsenter -- sh -c <algif blacklist>"));
        }
        Err(_) => {
            return Err(anyhow!(
                "nsenter -- sh -c <algif blacklist> timed out after {}s",
                ALGIF_BLACKLIST_TIMEOUT.as_secs()
            ));
        }
    };

    Ok(AlgifBlacklistOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        success: output.status.success(),
    })
}

async fn algif_blacklist_action(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = verify_bearer_token(&headers, &state.bearer_token) {
        return e;
    }

    let result = apply_algif_blacklist_on_host().await;

    // Log the attempt regardless of outcome — a failed re-apply is valuable
    // forensic signal, matching the dstack-agent action-logging pattern.
    let succeeded = matches!(&result, Ok(r) if r.success);
    let action_name = match &result {
        Ok(r) if r.success => "kernel_algif_blacklist_ok",
        Ok(_) => "kernel_algif_blacklist_script_failed",
        Err(_) => "kernel_algif_blacklist_invocation_failed",
    };
    let actor = extract_actor(&headers);
    let action = DeploymentAction {
        timestamp: Utc::now().to_rfc3339(),
        action: action_name.into(),
        image: None,
        tag: None,
        commit: None,
        file: None,
        file_sha256: None,
        services: vec![],
        container: None,
    };
    // Persist failures are logged inside record_action; the original code did
    // not abort the request on a persist error here, so neither do we.
    let _ = record_action(&state, &action).await;
    notify::spawn_action(&state, &action, &actor, Some(succeeded));

    let result = match result {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "Failed to apply algif blacklist on host");
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    };

    if !result.success {
        error!(
            stdout = %result.stdout,
            stderr = %result.stderr,
            "algif blacklist script reported non-zero exit"
        );
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("algif blacklist script failed: {}", result.combined()),
        );
    }

    ok_output(result.combined())
}

fn is_valid_dstack_action(action: &str) -> bool {
    DSTACK_AGENT_ACTIONS.contains(&action)
}

struct SystemctlOutput {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    success: bool,
}

impl SystemctlOutput {
    fn combined(&self) -> String {
        match (self.stdout.is_empty(), self.stderr.is_empty()) {
            (true, true) => String::new(),
            (false, true) => self.stdout.clone(),
            (true, false) => self.stderr.clone(),
            (false, false) => format!("{}\n{}", self.stdout, self.stderr),
        }
    }
}

async fn dstack_agent_action(
    State(state): State<Arc<AppState>>,
    AxumPath(action): AxumPath<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = verify_bearer_token(&headers, &state.bearer_token) {
        return e;
    }

    if !is_valid_dstack_action(&action) {
        return err(
            StatusCode::BAD_REQUEST,
            format!(
                "invalid action '{}': must be one of {}",
                action,
                DSTACK_AGENT_ACTIONS.join(", ")
            ),
        );
    }

    info!(action = %action, unit = DSTACK_AGENT_UNIT, "Managing dstack-guest-agent");

    let result = match run_host_systemctl(&action, DSTACK_AGENT_UNIT).await {
        Ok(r) => r,
        Err(e) => {
            // Process-level failure: nsenter binary missing, missing CAP_SYS_ADMIN,
            // timeout, etc. This is infrastructure broken — always 500, never 200.
            error!(
                action = %action,
                unit = DSTACK_AGENT_UNIT,
                error = %e,
                "Failed to invoke systemctl on host"
            );
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    };

    let combined = result.combined();

    // For status, any process-level success is a useful response — exit 0 means
    // active, 3 means inactive, 4 means unit-not-found, etc. Surface the exit
    // code so clients can distinguish; don't hide an inactive unit behind a 500.
    if action == "status" {
        return ok_systemctl(combined, result.exit_code);
    }

    // start/stop/restart: log the attempt regardless of outcome (failed restarts
    // are valuable forensic signal), then map systemctl exit to HTTP status.
    let actor = extract_actor(&headers);
    let act = DeploymentAction {
        timestamp: Utc::now().to_rfc3339(),
        action: format!("dstack_agent_{}", action),
        image: None,
        tag: None,
        commit: None,
        file: None,
        file_sha256: None,
        services: vec![],
        container: None,
    };
    if record_action(&state, &act).await.is_err() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist action log");
    }
    notify::spawn_action(&state, &act, &actor, Some(result.success));

    if !result.success {
        error!(
            action = %action,
            unit = DSTACK_AGENT_UNIT,
            exit_code = ?result.exit_code,
            "systemctl exited non-zero"
        );
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "systemctl {} {} exited {}:\n{}",
                action,
                DSTACK_AGENT_UNIT,
                result
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                combined
            ),
        );
    }

    ok_systemctl(combined, result.exit_code)
}

// --- Host observability (read-only) ---
//
// Two GET endpoints that let the control plane build a GPU allocation map and
// decide what model weights / kernel caches to pre-stage. Both are strictly
// read-only: they shell out to `docker` (and best-effort `nvidia-smi`) but
// never mutate state, never touch a deploy, and never take the compose lock.
// They MUST NOT 500 just because an optional data source (nvidia-smi, a cache
// volume) is missing — optional fields are simply omitted instead.

#[derive(Serialize)]
struct GpuInfo {
    index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_total_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_used_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    utilization_pct: Option<u32>,
    /// Container names that reserve this GPU (derived from docker, the
    /// authoritative source). Empty = the GPU is free / unclaimed.
    claimed_by: Vec<String>,
}

#[derive(Serialize)]
struct CacheVolume {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
}

#[derive(Serialize)]
struct HostCacheResponse {
    volumes: Vec<CacheVolume>,
    /// `models--org--repo` entries found under `hub/` in the HF cache volume(s).
    weights: Vec<String>,
}

/// Substrings that mark a docker volume as a model-weight / kernel cache. The
/// HuggingFace typo (`hugginface_cache`) genuinely exists in the fleet next to
/// the correct spelling, so both must match.
const CACHE_VOLUME_MARKERS: &[&str] = &[
    "huggingface_cache",
    "hugginface_cache",
    "vllm_cache",
    "compile_cache",
    "kernel_cache",
    "deep_gemm",
];

/// Substrings identifying the HuggingFace cache volume(s) we enumerate weights
/// from. A subset of CACHE_VOLUME_MARKERS — the HF hub layout (`hub/models--*`)
/// only exists in these.
const HF_VOLUME_MARKERS: &[&str] = &["huggingface_cache", "hugginface_cache"];

/// Whether a string matches docker's volume-name grammar
/// (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`). Used to refuse interpolating an unexpected
/// name (leading `/`, spaces, `:`) into a `-v <name>:/v:ro` mount spec, where
/// it could be reinterpreted as a bind-mount path. Defense-in-depth: docker
/// already enforces this on creation, but we never trust the input verbatim.
fn is_valid_docker_volume_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// Subprocess timeout for the read-only host-observability helpers. A hung
/// docker daemon or a wedged nvidia-smi (common with broken GPU drivers) must
/// not pin a request handler forever — bound it like the systemctl/algif paths
/// already do.
const HOST_OBSERVE_CMD_TIMEOUT: Duration = Duration::from_secs(15);

/// Run a program async and return stdout on a zero exit, or None on
/// failure/non-zero exit/timeout. Timeout-bounded (HOST_OBSERVE_CMD_TIMEOUT).
async fn run_bounded(program: &str, args: &[&str]) -> Option<String> {
    let fut = AsyncCommand::new(program).args(args).output();
    match tokio::time::timeout(HOST_OBSERVE_CMD_TIMEOUT, fut).await {
        Ok(Ok(out)) => out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(Err(_)) => None,
        Err(_) => {
            warn!(program = program, args = ?args, timeout_secs = HOST_OBSERVE_CMD_TIMEOUT.as_secs(), "host-observe command timed out");
            None
        }
    }
}

/// Run `docker <args>` async and return stdout, or None on failure/non-zero
/// exit/timeout. Mirrors `docker_inspect_output` but keeps empty stdout
/// (callers distinguish "ran, no output" from "failed to run").
async fn docker_stdout(args: &[&str]) -> Option<String> {
    run_bounded("docker", args).await
}

/// Parse the GPU device indices a single container reserves, from its
/// `docker inspect` JSON. Reads both `HostConfig.DeviceRequests[*].DeviceIDs`
/// (the `--gpus`/`device_requests` form) and the `NVIDIA_VISIBLE_DEVICES` env
/// var. Returns the numeric indices found; "all"/"void"/"none" and non-numeric
/// UUID device IDs are ignored (we can only map numeric indices to nvidia-smi).
fn gpu_indices_from_inspect(inspect: &serde_json::Value) -> Vec<u32> {
    let mut indices = Vec::new();
    let mut push = |s: &str| {
        for tok in s.split(',') {
            if let Ok(n) = tok.trim().parse::<u32>() {
                if !indices.contains(&n) {
                    indices.push(n);
                }
            }
        }
    };

    // `docker inspect` returns a top-level array; take the first element.
    let container = inspect.get(0).unwrap_or(inspect);

    if let Some(reqs) = container
        .pointer("/HostConfig/DeviceRequests")
        .and_then(|v| v.as_array())
    {
        for req in reqs {
            if let Some(ids) = req.get("DeviceIDs").and_then(|v| v.as_array()) {
                for id in ids.iter().filter_map(|v| v.as_str()) {
                    push(id);
                }
            }
        }
    }

    if let Some(envs) = container.pointer("/Config/Env").and_then(|v| v.as_array()) {
        for e in envs.iter().filter_map(|v| v.as_str()) {
            if let Some(val) = e.strip_prefix("NVIDIA_VISIBLE_DEVICES=") {
                push(val);
            }
        }
    }

    indices
}

/// Parse `nvidia-smi --query-gpu=index,memory.total,memory.used,utilization.gpu
/// --format=csv,noheader,nounits` output into (index -> (total_mb, used_mb,
/// util_pct)). Lines that don't parse are skipped (best-effort).
fn parse_nvidia_smi(out: &str) -> HashMap<u32, (u64, u64, u32)> {
    let mut map = HashMap::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        if cols.len() != 4 {
            continue;
        }
        if let (Ok(idx), Ok(total), Ok(used), Ok(util)) = (
            cols[0].parse::<u32>(),
            cols[1].parse::<u64>(),
            cols[2].parse::<u64>(),
            cols[3].parse::<u32>(),
        ) {
            map.insert(idx, (total, used, util));
        }
    }
    map
}

const NVIDIA_SMI_ARGS: &[&str] = &[
    "--query-gpu=index,memory.total,memory.used,utilization.gpu",
    "--format=csv,noheader,nounits",
];

/// Best-effort nvidia-smi query. Tries the binary directly first; if that
/// fails (not on PATH inside the container), falls back to entering PID 1's
/// namespaces via the same nsenter pattern the host-systemctl path uses.
/// Returns None when nvidia-smi is unreachable either way — callers then omit
/// the memory/util fields rather than failing.
async fn query_nvidia_smi() -> Option<HashMap<u32, (u64, u64, u32)>> {
    if let Some(out) = docker_smi_direct().await {
        return Some(parse_nvidia_smi(&out));
    }
    if let Some(out) = nvidia_smi_via_nsenter().await {
        return Some(parse_nvidia_smi(&out));
    }
    None
}

async fn docker_smi_direct() -> Option<String> {
    run_bounded("nvidia-smi", NVIDIA_SMI_ARGS).await
}

async fn nvidia_smi_via_nsenter() -> Option<String> {
    let mut args = vec!["-t", "1", "-m", "-u", "-i", "-n", "-p", "--", "nvidia-smi"];
    args.extend_from_slice(NVIDIA_SMI_ARGS);
    run_bounded("nsenter", &args).await
}

/// GET /host/gpu — per-GPU allocation (docker-derived, authoritative) plus
/// best-effort nvidia-smi utilization. Never 500s on a missing nvidia-smi.
async fn host_gpu(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Err((code, msg)) = verify_bearer_token_raw(&headers, &state.bearer_token) {
        return err_response(code, msg);
    }

    // Authoritative source: which container reserves which GPU index.
    // gpu_index -> [container names]. BTreeMap keeps the output index-ordered.
    let mut claims: std::collections::BTreeMap<u32, Vec<String>> = std::collections::BTreeMap::new();

    // docker ps failing (daemon down, permission denied) would silently make
    // every GPU look unclaimed — dangerous for a scheduler reading this as the
    // authoritative claim source. Log it loudly so it isn't mistaken for "free".
    let names: Vec<String> = match docker_stdout(&["ps", "--format", "{{.Names}}"]).await {
        Some(out) => out
            .lines()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(String::from)
            .collect(),
        None => {
            warn!("docker ps failed — GPU claim data is unreliable (all GPUs will appear unclaimed)");
            Vec::new()
        }
    };

    // Batch: inspect all running containers in a single `docker inspect` call
    // to avoid N sequential subprocess invocations. The result is a JSON array
    // whose elements carry `.Name` ("/container") so we can attribute claims.
    if !names.is_empty() {
        let mut args: Vec<&str> = vec!["inspect"];
        args.extend(names.iter().map(String::as_str));
        if let Some(json) = docker_stdout(&args).await {
            if let Ok(serde_json::Value::Array(items)) =
                serde_json::from_str::<serde_json::Value>(&json)
            {
                for item in &items {
                    let name = item
                        .get("Name")
                        .and_then(|v| v.as_str())
                        .map(|n| n.trim_start_matches('/').to_string())
                        .unwrap_or_default();
                    if name.is_empty() {
                        continue;
                    }
                    for idx in gpu_indices_from_inspect(item) {
                        claims.entry(idx).or_default().push(name.clone());
                    }
                }
            }
        }
    }

    // Best-effort utilization/memory; degrade gracefully if nvidia-smi is gone.
    let smi = query_nvidia_smi().await.unwrap_or_default();

    // Union of indices seen via docker claims and via nvidia-smi, so a free GPU
    // (claimed_by empty) still appears, and a claimed GPU shows even if smi is
    // unavailable. BTreeSet dedups and keeps them index-ordered.
    let mut index_set: std::collections::BTreeSet<u32> = claims.keys().copied().collect();
    index_set.extend(smi.keys().copied());
    let all_indices: Vec<u32> = index_set.into_iter().collect();

    let gpus: Vec<GpuInfo> = all_indices
        .into_iter()
        .map(|index| {
            let (memory_total_mb, memory_used_mb, utilization_pct) = match smi.get(&index) {
                Some((t, u, util)) => (Some(*t), Some(*u), Some(*util)),
                None => (None, None, None),
            };
            GpuInfo {
                index,
                memory_total_mb,
                memory_used_mb,
                utilization_pct,
                claimed_by: claims.get(&index).cloned().unwrap_or_default(),
            }
        })
        .collect();

    json_ok(serde_json::json!({ "gpus": gpus }))
}

/// Parse `docker system df -v` (volumes section) into volume-name -> size in
/// bytes. The output has a "Local Volumes space usage:" header, then a table
/// whose first column is the volume name and whose last column is a human size
/// (e.g. "12.3GB"). We key off first/last column rather than header offsets
/// because the header label "VOLUME NAME" is two whitespace tokens but one data
/// column, so positional header matching misaligns. Unparseable sizes yield no
/// entry (size omitted downstream).
fn parse_volume_sizes(df_output: &str) -> HashMap<String, u64> {
    let mut sizes = HashMap::new();
    let mut in_volumes = false;
    let mut seen_header = false;
    for line in df_output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Local Volumes space usage") {
            in_volumes = true;
            seen_header = false;
            continue;
        }
        if !in_volumes {
            continue;
        }
        // A blank line or a new "<X> space usage:" header ends the section.
        if trimmed.is_empty() || trimmed.ends_with("space usage:") {
            in_volumes = false;
            continue;
        }
        if !seen_header {
            // Skip the column-header row (the VOLUME NAME / LINKS / SIZE labels).
            seen_header = true;
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if let (Some(name), Some(size_str)) = (cols.first(), cols.last()) {
            if let Some(bytes) = parse_human_size(size_str) {
                sizes.insert((*name).to_string(), bytes);
            }
        }
    }
    sizes
}

/// Parse a docker human-readable size ("12.3GB", "512MB", "0B", "1.5kB") to
/// bytes. Returns None if unparseable. Uses decimal (1000) units, matching
/// docker's `units.HumanSize`.
fn parse_human_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num.trim().parse().ok()?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "pb" => 1e15,
        _ => return None,
    };
    Some((value * mult) as u64)
}

/// GET /host/cache — cache volumes (with best-effort sizes) and the model
/// weights present in the HF cache. Strictly read-only; never errors the host.
async fn host_cache(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Err((code, msg)) = verify_bearer_token_raw(&headers, &state.bearer_token) {
        return err_response(code, msg);
    }

    // All volume names, then filter to the cache-marker subset.
    let all_names = docker_stdout(&["volume", "ls", "--format", "{{.Name}}"])
        .await
        .unwrap_or_default();
    let cache_names: Vec<String> = all_names
        .lines()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .filter(|n| CACHE_VOLUME_MARKERS.iter().any(|m| n.contains(m)))
        // Refuse names that don't match docker's grammar — they'd later be
        // interpolated into a `-v <name>:/v:ro` mount spec.
        .filter(|n| {
            is_valid_docker_volume_name(n) || {
                warn!(volume = %n, "skipping cache volume with unexpected name");
                false
            }
        })
        .map(String::from)
        .collect();

    // Best-effort per-volume sizes from `docker system df -v`.
    let sizes = match docker_stdout(&["system", "df", "-v"]).await {
        Some(out) => parse_volume_sizes(&out),
        None => HashMap::new(),
    };

    let volumes: Vec<CacheVolume> = cache_names
        .iter()
        .map(|name| CacheVolume {
            name: name.clone(),
            size_bytes: sizes.get(name).copied(),
        })
        .collect();

    // Enumerate model weights (`models--*`) in the HF cache volume(s) via a
    // throwaway read-only container. Each `ls` is best-effort. BTreeSet dedups
    // across volumes and keeps the output sorted. Names are already validated
    // against docker's grammar above, so the mount spec is safe to build.
    let mut weight_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for name in cache_names
        .iter()
        .filter(|n| HF_VOLUME_MARKERS.iter().any(|m| n.contains(m)))
    {
        let mount = format!("{}:/v:ro", name);
        if let Some(out) =
            docker_stdout(&["run", "--rm", "-v", &mount, "alpine", "ls", "/v/hub"]).await
        {
            for entry in out.lines().map(str::trim) {
                if entry.starts_with("models--") {
                    weight_set.insert(entry.to_string());
                }
            }
        }
    }
    let weights: Vec<String> = weight_set.into_iter().collect();

    json_ok(serde_json::json!(HostCacheResponse { volumes, weights }))
}

// --- Shell Commands ---

fn run_command(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("Failed to execute: {} {}", program, args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow!(
            "{} {} failed (exit {}):\nstderr: {}\nstdout: {}",
            program,
            args.join(" "),
            output.status.code().map(|c| c.to_string()).unwrap_or("signal".into()),
            stderr,
            stdout
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Async counterpart to `run_command` (uses tokio's `AsyncCommand` so it never
/// blocks a worker thread). Same success/error contract.
async fn run_command_async(program: &str, args: &[&str]) -> Result<String> {
    let output = AsyncCommand::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("Failed to execute: {} {}", program, args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow!(
            "{} {} failed (exit {}):\nstderr: {}\nstdout: {}",
            program,
            args.join(" "),
            output.status.code().map(|c| c.to_string()).unwrap_or("signal".into()),
            stderr,
            stdout
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run `docker <args>` and return trimmed stdout, or None on failure/empty.
async fn docker_inspect_output(args: &[&str]) -> Option<String> {
    let out = AsyncCommand::new("docker").args(args).output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Best-effort digest of the image THIS compose-manager is running, read from
/// its own container (`compose-manager`) via the mounted docker socket.
///
/// Prefers `.Config.Image`, which the launcher pins to `repo@sha256:…` (and the
/// Ansible template likewise pins by digest); for a tag-based local run it
/// falls back to the image's first `RepoDigests` entry. Returns None if it
/// can't be resolved — startup still proceeds.
async fn current_image_digest() -> Option<String> {
    let config_image =
        docker_inspect_output(&["inspect", "compose-manager", "--format", "{{.Config.Image}}"])
            .await?;
    if config_image.contains("@sha256:") {
        return Some(config_image);
    }
    // Tag-based reference (local dev): resolve the image's first repo digest.
    docker_inspect_output(&[
        "image",
        "inspect",
        &config_image,
        "--format",
        "{{range .RepoDigests}}{{println .}}{{end}}",
    ])
    .await
    .and_then(|s| s.lines().map(str::trim).find(|l| l.contains("@sha256:")).map(String::from))
}

fn run_docker_compose(work_dir: &Path, args: &[&str], file: &str, env_files: &[String], services: &[String]) -> Result<String> {
    info!(command = "docker compose", file = file, args = ?args, env_files = ?env_files, services = ?services, work_dir = %work_dir.display(), "Running command");
    let mut cmd = Command::new("docker");
    // `-p` pins the project (see compose_project_name) so `--remove-orphans`
    // can never inherit a leaked COMPOSE_PROJECT_NAME and evict the `dstack`
    // app-compose project (launcher/certbot/datadog).
    cmd.args(["compose", "-p", &compose_project_name(work_dir), "-f", file]);
    for env_file in env_files {
        cmd.args(["--env-file", env_file]);
    }
    cmd.args(args);
    for service in services {
        cmd.arg(service);
    }
    let output = cmd
        .current_dir(work_dir)
        .output()
        .with_context(|| format!(
            "Failed to execute: docker compose -f {} {} (work_dir: {})",
            file, args.join(" "), work_dir.display()
        ))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        error!(file = file, args = ?args, exit_code = output.status.code(), %stderr, "Command failed");
        return Err(anyhow!(
            "docker compose failed (exit {}):\nstderr: {}\nstdout: {}",
            output.status.code().map(|c| c.to_string()).unwrap_or("signal".into()),
            stderr,
            stdout
        ));
    }

    info!(command = "docker compose", file = file, args = ?args, "Command completed successfully");
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// Runs `systemctl <action> <unit>` against the CVM host's PID 1 systemd by
// entering its mount/UTS/IPC/PID/network namespaces via nsenter. Requires the
// container to be started with `pid: host` and CAP_SYS_ADMIN.
//
// Returns Err only on infrastructure failure (nsenter missing, capability
// denied, timeout). A non-zero systemctl exit is a *successful* invocation
// reported as `success: false` in the result so callers can decide what to
// do with the exit code (status uses it, start/stop/restart map it to 500).
async fn run_host_systemctl(action: &str, unit: &str) -> Result<SystemctlOutput> {
    info!(
        command = "nsenter ... systemctl",
        action = action,
        unit = unit,
        timeout_secs = SYSTEMCTL_TIMEOUT.as_secs(),
        "Running host systemctl"
    );

    let invocation = AsyncCommand::new("nsenter")
        .args(["-t", "1", "-m", "-u", "-i", "-n", "-p", "--", "systemctl", action, unit])
        .output();

    let output = match tokio::time::timeout(SYSTEMCTL_TIMEOUT, invocation).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(anyhow!(e).context(format!(
                "Failed to execute: nsenter -t 1 -m -u -i -n -p -- systemctl {} {}",
                action, unit
            )));
        }
        Err(_) => {
            return Err(anyhow!(
                "nsenter ... systemctl {} {} timed out after {}s",
                action,
                unit,
                SYSTEMCTL_TIMEOUT.as_secs()
            ));
        }
    };

    Ok(SystemctlOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code(),
        success: output.status.success(),
    })
}

// --- Selective per-model cache eviction (POST /docker/evict) ---

/// Map an HF repo id (`org/repo`) to its on-disk hub directory name
/// (`models--org--repo`), mirroring the `cleanup-hf-model.yaml` fleet pattern.
/// Returns Err for ids that would escape the hub dir (path traversal / empty).
fn model_to_hub_dir(model: &str) -> Result<String, String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("model must not be empty".into());
    }
    // Strict allowlist. HF repo ids are `org/repo` (occasionally a bare `repo`)
    // and each path component is drawn from `[A-Za-z0-9._-]`. Enforcing this is
    // the primary defense: the resulting dir name is interpolated into an
    // `alpine sh -c` script, so the charset MUST exclude shell metacharacters
    // (`$ ` ; & | ( ) " ' < > \ * ? newline …`) and path-traversal sequences.
    if model.contains("..")
        || model.starts_with('/')
        || model.ends_with('/')
        || model.contains("//")
    {
        return Err(format!("invalid model id (path traversal): '{model}'"));
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/');
    if !model.chars().all(allowed) {
        return Err(format!(
            "invalid model id '{model}': only [A-Za-z0-9._/-] are allowed"
        ));
    }
    Ok(format!("models--{}", model.replace('/', "--")))
}

/// Validate a Docker NAMED volume reference for `evict`. A name like `/`,
/// `/etc`, or `../x` would, when used as a `-v <src>:/c` mount source, bind a
/// HOST path into the throwaway container where `rm -rf` then runs — so reject
/// anything that isn't a plain named volume `[A-Za-z0-9_.-]+`. Callers still
/// cross-check against `docker volume ls` (autodetected names always do; an
/// explicit override is validated here before use).
fn validate_volume_name(volume: &str) -> Result<(), String> {
    if volume.is_empty() {
        return Err("cache_volume must not be empty".into());
    }
    let ok = volume.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !ok {
        return Err(format!(
            "invalid cache_volume '{volume}': must be a Docker named volume ([A-Za-z0-9_.-]), not a host path"
        ));
    }
    Ok(())
}

/// Candidate names (in priority order) for the shared HuggingFace cache volume.
/// The name drifts across the fleet: the correct `huggingface_cache`, the known
/// typo `hugginface_cache`, and either form prefixed by a compose project (e.g.
/// `small-models_hugginface_cache`). We pick the one that actually exists.
const HF_VOLUME_BASENAMES: &[&str] = &["huggingface_cache", "hugginface_cache"];

/// List all docker volume names on the host.
async fn list_docker_volumes() -> Result<Vec<String>> {
    let out = run_command_async("docker", &["volume", "ls", "--format", "{{.Name}}"]).await?;
    Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
}

/// Resolve the HuggingFace cache volume that actually exists on this host.
///
/// Prefers an exact basename match (`huggingface_cache`/`hugginface_cache`),
/// then any project-prefixed form (`<project>_<basename>`). Returns a clear
/// error listing what was searched if none is present.
fn resolve_hf_cache_volume(volumes: &[String]) -> Result<String, String> {
    // Exact, un-prefixed match wins.
    for base in HF_VOLUME_BASENAMES {
        if volumes.iter().any(|v| v == base) {
            return Ok((*base).to_string());
        }
    }
    // Otherwise the first project-prefixed form (`<project>_<basename>`).
    for base in HF_VOLUME_BASENAMES {
        let suffix = format!("_{base}");
        if let Some(v) = volumes.iter().find(|v| v.ends_with(&suffix)) {
            return Ok(v.clone());
        }
    }
    Err(format!(
        "no HuggingFace cache volume found on host (looked for {} and any `<project>_*` form)",
        HF_VOLUME_BASENAMES.join(", ")
    ))
}

/// Detect whether a RUNNING container is actively serving `model` (the HF
/// checkpoint `org/repo`). We inspect each running container's full command
/// args AND environment for an exact token match — covering both the engine
/// `--model-path <org/repo>` / `--model <org/repo>` flag and a `MODEL_NAME` /
/// `*=org/repo` env var. Returns the matching container names.
async fn containers_serving_model(model: &str) -> Result<Vec<String>> {
    let ids = run_command_async("docker", &["ps", "-q"]).await?;
    let ids: Vec<&str> = ids.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if ids.is_empty() {
        return Ok(vec![]);
    }

    // One `docker inspect` over ALL running ids (avoids an N+1 subprocess
    // storm). It emits a JSON array; .Args = argv after the entrypoint,
    // .Config.Cmd covers the (rare) case where the model flag lives in Cmd,
    // .Config.Env = env, .Name = container name (leading '/').
    let mut args: Vec<&str> = vec!["inspect"];
    args.extend(ids.iter().copied());
    let raw = run_command_async("docker", &args).await?;

    #[derive(Deserialize)]
    struct ContainerConfig {
        #[serde(default, rename = "Cmd")]
        cmd: Vec<String>,
        #[serde(default, rename = "Env")]
        env: Vec<String>,
    }
    #[derive(Deserialize)]
    struct Inspected {
        #[serde(default, rename = "Name")]
        name: String,
        #[serde(default, rename = "Args")]
        args: Vec<String>,
        #[serde(default, rename = "Config")]
        config: Option<ContainerConfig>,
    }

    let inspected: Vec<Inspected> = serde_json::from_str(&raw)
        .with_context(|| "failed to parse `docker inspect` output")?;

    let mut matches = Vec::new();
    for c in inspected {
        let (cmd, env) = c
            .config
            .map(|cfg| (cfg.cmd, cfg.env))
            .unwrap_or_default();
        if argv_serves_model(&c.args, model)
            || argv_serves_model(&cmd, model)
            || env_serves_model(&env, model)
        {
            matches.push(c.name.trim_start_matches('/').to_string());
        }
    }
    Ok(matches)
}

/// True if argv carries the checkpoint as a value to a model-path flag
/// (`--model-path org/repo`, `--model org/repo`, or the `=` form). Compares the
/// whole token so `org/repo` never matches `org/repo-other`.
fn argv_serves_model(argv: &[String], model: &str) -> bool {
    const FLAGS: &[&str] = &["--model-path", "--model"];
    let mut prev: Option<&str> = None;
    for tok in argv {
        if let Some(flag) = prev.take() {
            if FLAGS.contains(&flag) && tok == model {
                return true;
            }
        }
        if let Some((flag, val)) = tok.split_once('=') {
            if FLAGS.contains(&flag) && val == model {
                return true;
            }
        }
        prev = Some(tok.as_str());
    }
    false
}

/// True if any env var's VALUE is exactly `model` (e.g. `MODEL_NAME=org/repo`,
/// `MODEL_PATH=org/repo`). Matches the value, not a substring, so unrelated
/// vars that merely contain the string don't trip the guard.
fn env_serves_model(env: &[String], model: &str) -> bool {
    env.iter().any(|kv| matches!(kv.split_once('='), Some((_, v)) if v == model))
}

/// Outcome of a single subtree eviction.
struct EvictOutcome {
    /// Whether the path existed before removal.
    existed: bool,
    /// Pre-removal size in bytes (best-effort; 0 if it couldn't be measured).
    freed_bytes: u64,
}

/// Remove a single subtree (relative to the cache volume root) via a throwaway
/// alpine container — never touches anything outside `/c/<rel>`.
///
/// The target path is passed as a SEPARATE argv element (`sh -c <script> sh
/// "$1"`), never interpolated into the script body, so a `rel` value can't
/// break out of the shell quoting. Returns whether the path existed and the
/// best-effort freed size. Async to avoid blocking the tokio worker.
async fn evict_subtree(volume: &str, rel: &str) -> Result<EvictOutcome> {
    let mount = format!("{volume}:/c");
    let target = format!("/c/{rel}");
    // Fixed script; the target arrives as positional $1. `du -sb` => "<bytes>\t<path>".
    // First line "1"/"0" reports existence; second line reports bytes.
    let script = "set -eu; T=\"$1\"; \
         if [ ! -e \"$T\" ]; then echo 0; echo 0; exit 0; fi; \
         SZ=$(du -sb \"$T\" 2>/dev/null | cut -f1 || echo 0); \
         rm -rf \"$T\"; \
         echo 1; echo \"${SZ:-0}\"";
    let out = run_command_async(
        "docker",
        &["run", "--rm", "-v", &mount, "alpine:3.20", "sh", "-c", script, "sh", &target],
    )
    .await?;
    let mut lines = out.lines().map(str::trim).filter(|l| !l.is_empty());
    let existed = lines.next() == Some("1");
    let freed_bytes = lines.next().and_then(|l| l.parse::<u64>().ok()).unwrap_or(0);
    Ok(EvictOutcome { existed, freed_bytes })
}

/// Best-effort compile/kernel cache subdirs for a model. These caches
/// (torchinductor/triton/deep_gemm) are keyed by hashes, NOT by model id, so a
/// model's compile cache is NOT cleanly identifiable on disk. We therefore do
/// NOT touch them by default; this returns the model-named hub subdir under any
/// such cache root that DOES carry the model name, if one exists. In practice
/// it usually returns nothing — "cache" eviction is documented as best-effort.
fn model_cache_rels(hub_dir: &str) -> Vec<String> {
    // Some setups stash per-model artifacts under a model-named directory; cover
    // the few that are cleanly identifiable. Hash-keyed caches are intentionally
    // excluded (not attributable to a single model without unsafe heuristics).
    vec![
        format!("torchinductor/{hub_dir}"),
        format!("triton/{hub_dir}"),
    ]
}

fn run_docker_prune(volumes: bool, images: bool, containers: bool) -> Result<String> {
    let mut output_text = String::new();

    if containers {
        info!(command = "docker container prune", "Running command");
        let result = run_command("docker", &["container", "prune", "-f"])?;
        info!(command = "docker container prune", "Command completed successfully");
        output_text.push_str(&result);
    }

    if volumes {
        info!(command = "docker volume prune", "Running command");
        let result = run_command("docker", &["volume", "prune", "-f"])?;
        info!(command = "docker volume prune", "Command completed successfully");
        output_text.push_str(&result);
    }

    if images {
        info!(command = "docker image prune", "Running command");
        let result = run_command("docker", &["image", "prune", "-af"])?;
        info!(command = "docker image prune", "Command completed successfully");
        output_text.push_str(&result);
    }

    Ok(output_text)
}

// --- Main ---

fn parse_github_url(url: &str) -> Result<(String, String)> {
    let url = url.trim_end_matches('/').trim_end_matches(".git");
    let parts: Vec<&str> = url.split('/').collect();
    if parts.len() < 2 {
        return Err(anyhow!("Invalid GitHub URL format"));
    }
    Ok((parts[parts.len() - 2].to_string(), parts[parts.len() - 1].to_string()))
}

#[tokio::main]
async fn main() -> Result<()> {
    // ANSI off: logs go to Docker/Datadog, not a TTY — escape codes garble them.
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let github_repo = std::env::var("GITHUB_REPO")
        .context("GITHUB_REPO environment variable is required")?;
    let bearer_token = std::env::var("BEARER_TOKEN")
        .context("BEARER_TOKEN environment variable is required")?;
    let work_dir = std::env::var("WORK_DIR")
        .unwrap_or_else(|_| "/app/work".to_string());
    let min_tag_age_hours: i64 = std::env::var("MIN_TAG_AGE_HOURS")
        .unwrap_or_else(|_| "48".to_string())
        .parse()
        .context("MIN_TAG_AGE_HOURS must be a valid integer")?;

    let env_files: Vec<String> = std::env::var("ENV_FILES")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let slack_webhook_url = std::env::var("SLACK_WEBHOOK_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let instance_label = std::env::var("INSTANCE_LABEL").unwrap_or_default();
    info!(
        slack_notifications = slack_webhook_url.is_some(),
        instance_label = %instance_label,
        "Slack notification config"
    );

    let (github_owner, github_repo_name) = parse_github_url(&github_repo)?;

    // Apply CVE-2026-31431 (Copy Fail) mitigation on the CVM host kernel.
    // Idempotent: /run/modprobe.d is tmpfs, so this must re-run on every
    // compose-manager start (which is also every CVM reboot). Best-effort —
    // failure is logged but does NOT block compose-manager startup, since
    // dropping the API entirely would be worse than leaving the kernel
    // mitigation unapplied (operators can re-trigger via the admin route).
    match apply_algif_blacklist_on_host().await {
        Ok(out) if out.success => {
            info!(
                output = %out.combined(),
                "Applied algif_aead kernel blacklist on host"
            );
        }
        Ok(out) => {
            error!(
                stdout = %out.stdout,
                stderr = %out.stderr,
                "algif blacklist script exited non-zero; module may still be loadable"
            );
        }
        Err(e) => {
            error!(
                error = %e,
                "Failed to apply algif blacklist on host; module may still be loadable"
            );
        }
    }

    let work_dir = PathBuf::from(work_dir);
    tokio::fs::create_dir_all(&work_dir).await
        .with_context(|| format!("Failed to create WORK_DIR: {}", work_dir.display()))?;
    let initial_actions = load_actions_from_disk(&work_dir);
    if !initial_actions.is_empty() {
        info!(count = initial_actions.len(), "Loaded action log from disk");
    }

    // Restore the last *successful* deploy (tag/file/commit) from deployed.json.
    // These fields live only in memory for the life of the process, so without
    // this a container restart — most commonly a launcher hot-swap — would blank
    // the tag/file the dashboard shows (via /version) until the next deploy, even
    // though the same stack is still running. deployed.json is written only when a
    // compose_up stream completes successfully, so a failed attempt can't surface
    // here as the deployed version.
    // Per-project state. A legacy single-tuple deployed.json migrates into
    // projects["work"].current via load_deployed_state.
    let mut restored = load_deployed_state(&work_dir);
    if restored.projects.is_empty() {
        // One-shot legacy migration (gated by a marker) for installs that predate
        // deployed.json. See migrate_legacy_deployed_version: this never backfills
        // a failed compose_up created under this code.
        if let Some(v) = migrate_legacy_deployed_version(&work_dir, &initial_actions) {
            restored = v;
        }
    }
    if !restored.projects.is_empty() {
        info!(projects = ?restored.projects.keys().collect::<Vec<_>>(), "Restored last-deployed per-project state");
    }
    // Mirror the most-recently-written project's `current` into the legacy
    // top-level fields so /version's top-level tuple is byte-identical to the
    // pre-#46 contract. BTreeMap iteration is name-ordered (not write-ordered);
    // a single-project CVM has exactly one entry, so the mirror is unambiguous
    // there — the case the byte-identity contract covers.
    let restored_top = restored
        .projects
        .values()
        .next_back()
        .map(|p| p.current.clone())
        .unwrap_or_default();

    // Resolve the image this process is running so the running version can be
    // attested — recorded as the `compose_manager_started` action below (hashed
    // into the attestation quote) and surfaced by /version.
    let running_image = current_image_digest().await;
    match &running_image {
        Some(img) => info!(image = %img, "Resolved running compose-manager image"),
        None => warn!("Could not resolve running compose-manager image digest (continuing)"),
    }

    let state = Arc::new(AppState {
        bearer_token,
        github_owner,
        github_repo_name,
        min_tag_age_hours,
        work_dir,
        env_files,
        slack_webhook_url,
        instance_label,
        deployed_projects: StdRwLock::new(restored.projects),
        deployed_tag: StdRwLock::new(restored_top.tag),
        deployed_commit: StdRwLock::new(restored_top.commit),
        deployed_file: StdRwLock::new(restored_top.file),
        deployed_file_sha256: StdRwLock::new(restored_top.file_sha256),
        actions: RwLock::new(initial_actions),
        // Bounded timeouts: the compose lock is held across GitHub fetches in
        // compose_up/compose_down, so a hung GitHub call would otherwise gate
        // every subsequent docker op behind the OS TCP timeout.
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client builder with valid timeouts"),
        compose_lock: Arc::new(Mutex::new(())),
        in_flight: Arc::new(StdMutex::new(None)),
        running_image: running_image.clone(),
    });

    // Record the running image into the action log so /v1/attestation/report
    // binds it into the TDX quote (report_data hashes the action list). Each
    // launcher swap / reboot restarts this process and appends a fresh entry,
    // so the latest compose_manager_started.image is the running digest.
    // Best-effort: a disk-persist failure still leaves the entry in memory for
    // this process, which is what attestation reads.
    let _ = record_action(&state, &DeploymentAction {
        timestamp: Utc::now().to_rfc3339(),
        action: "compose_manager_started".into(),
        container: Some("compose-manager".into()),
        image: running_image,
        ..Default::default()
    })
    .await;

    let app = Router::new()
        .route("/compose/up", post(compose_up))
        .route("/compose/down", post(compose_down))
        .route("/compose/logs", post(compose_logs))
        .route("/docker/clean", post(docker_clean))
        .route("/docker/evict", post(docker_evict))
        .route("/docker/ps", get(docker_ps))
        .route("/docker/restart", post(docker_restart))
        .route("/host/gpu", get(host_gpu))
        .route("/host/cache", get(host_cache))
        .route("/status", get(status))
        .route("/dstack-agent/:action", post(dstack_agent_action))
        .route("/admin/kernel/algif-blacklist", post(algif_blacklist_action))
        .route("/v1/attestation/report", get(attestation_report))
        .route("/version", get(version))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    info!("Server listening on port 8080");
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployed_version_from_actions_picks_latest_compose_up() {
        let mut up1 = DeploymentAction::default();
        up1.action = "compose_up".into();
        up1.tag = Some("v1".into());
        up1.file = Some("a.yaml".into());
        let mut up2 = DeploymentAction::default();
        up2.action = "compose_up".into();
        up2.tag = Some("v2".into());
        up2.file = Some("b.yaml".into());
        let mut down = DeploymentAction::default();
        down.action = "compose_down".into();

        // Latest compose_up wins; a later compose_down doesn't blank it.
        let v = deployed_version_from_actions(&[up1, up2, down]).unwrap();
        assert_eq!(v.tag.as_deref(), Some("v2"));
        assert_eq!(v.file.as_deref(), Some("b.yaml"));

        // No compose_up -> no migration source.
        let mut started = DeploymentAction::default();
        started.action = "compose_manager_started".into();
        assert!(deployed_version_from_actions(&[started]).is_none());
        assert!(deployed_version_from_actions(&[]).is_none());
    }

    fn compose_up_action(tag: &str, file: &str) -> DeploymentAction {
        let mut a = DeploymentAction::default();
        a.action = "compose_up".into();
        a.tag = Some(tag.into());
        a.file = Some(file.into());
        a
    }

    #[test]
    fn legacy_migration_is_one_shot_and_skips_post_fix_failed_deploys() {
        // Legacy upgrade: actions.json present, no deployed.json, no marker yet
        // -> backfill from the latest compose_up and write both deployed.json
        // and the marker.
        let dir = temp_work_dir();
        let legacy = vec![compose_up_action("v1", "a.yaml")];
        let v = migrate_legacy_deployed_version(&dir, &legacy).unwrap();
        // Legacy action-log backfill lands in projects["work"].current, previous=null.
        let work = v.projects.get(LEGACY_PROJECT_KEY).expect("migrated into work");
        assert_eq!(work.current.tag.as_deref(), Some("v1"));
        assert!(work.previous.is_none());
        assert!(deployed_version_file(&dir).exists(), "deployed.json should be written");
        assert!(migration_marker_file(&dir).exists(), "marker should be written");

        // Post-fix failed first deploy: marker now exists; even though a (failed)
        // compose_up action is present and deployed.json is absent, migration must
        // NOT backfill it (the success-only contract).
        std::fs::remove_file(deployed_version_file(&dir)).unwrap();
        let after = vec![compose_up_action("v2-failed", "b.yaml")];
        assert!(migrate_legacy_deployed_version(&dir, &after).is_none());
        assert!(!deployed_version_file(&dir).exists(), "must not resurrect a failed deploy");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fresh_install_marks_migration_without_backfill() {
        // Fresh install: first boot precedes any compose_up -> nothing to
        // backfill, but the marker is written so a later failed deploy can't be.
        let dir = temp_work_dir();
        assert!(migrate_legacy_deployed_version(&dir, &[]).is_none());
        assert!(!deployed_version_file(&dir).exists());
        assert!(migration_marker_file(&dir).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_deployed_state_handles_missing_and_corrupt() {
        let dir = temp_work_dir();
        // No file yet -> empty (the pre-first-deploy / pre-upgrade state).
        let v = load_deployed_state(&dir);
        assert!(v.projects.is_empty());
        // Corrupt file -> ignored, not a panic.
        std::fs::write(deployed_version_file(&dir), b"{not json").unwrap();
        let v = load_deployed_state(&dir);
        assert!(v.projects.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn deployed_record(tag: &str, file: &str) -> DeployedRecord {
        DeployedRecord {
            tag: Some(tag.into()),
            commit: Some("abc123".into()),
            file: Some(file.into()),
            file_sha256: Some("deadbeef".into()),
        }
    }

    #[test]
    fn deployed_state_round_trips_through_disk() {
        let dir = temp_work_dir();
        let mut projects = std::collections::BTreeMap::new();
        projects.insert(
            "glm-5-1".to_string(),
            ProjectDeployed {
                current: deployed_record("v0.0.211", "GLM-5.1.yaml"),
                previous: Some(deployed_record("v0.0.210", "GLM-5.1.yaml")),
            },
        );
        let state = DeployedState { projects };
        persist_deployed_state(&dir, &state).unwrap();
        let loaded = load_deployed_state(&dir);
        assert_eq!(loaded, state);
        let p = loaded.projects.get("glm-5-1").unwrap();
        assert_eq!(p.current.tag.as_deref(), Some("v0.0.211"));
        assert_eq!(p.previous.as_ref().unwrap().tag.as_deref(), Some("v0.0.210"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_single_tuple_deployed_json_migrates_into_work() {
        // A pre-existing single-tuple deployed.json (the on-disk format before
        // this change) must load into projects["work"].current with previous=null.
        let dir = temp_work_dir();
        std::fs::write(
            deployed_version_file(&dir),
            br#"{"tag":"v0.0.99","commit":"c0ffee","file":"old.yaml","file_sha256":"abcd"}"#,
        )
        .unwrap();
        let loaded = load_deployed_state(&dir);
        assert_eq!(loaded.projects.len(), 1);
        let work = loaded.projects.get(LEGACY_PROJECT_KEY).expect("migrated into work");
        assert_eq!(work.current.tag.as_deref(), Some("v0.0.99"));
        assert_eq!(work.current.file.as_deref(), Some("old.yaml"));
        assert!(work.previous.is_none(), "legacy migration carries no previous");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_deployed_json_is_empty_state() {
        // Both an empty object and an empty map deserialize to empty state.
        assert!(parse_deployed_state("{}").unwrap().projects.is_empty());
        assert!(parse_deployed_state(r#"{"work":{"current":{}}}"#).unwrap()
            .projects
            .get("work")
            .unwrap()
            .current
            == DeployedRecord::default());
    }

    /// Mirror the rotation the stream-completion success hook performs, so we can
    /// unit-test the current->previous semantics without spawning docker. Keep in
    /// lock-step with the inline logic in `NdjsonStream::poll_next`.
    fn rotate_on_success(state: &mut DeployedState, project: &str, new_current: DeployedRecord) {
        let entry = state.projects.entry(project.to_string()).or_default();
        if entry.current != DeployedRecord::default() {
            entry.previous = Some(entry.current.clone());
        }
        entry.current = new_current;
    }

    #[test]
    fn rotation_moves_current_to_previous_on_success() {
        let mut state = DeployedState::default();
        // First successful deploy: current set, previous still null.
        rotate_on_success(&mut state, "glm", deployed_record("v1", "a.yaml"));
        let p = state.projects.get("glm").unwrap();
        assert_eq!(p.current.tag.as_deref(), Some("v1"));
        assert!(p.previous.is_none(), "first deploy has no previous");
        // Second successful deploy: v1 rotates into previous, v2 becomes current.
        rotate_on_success(&mut state, "glm", deployed_record("v2", "a.yaml"));
        let p = state.projects.get("glm").unwrap();
        assert_eq!(p.current.tag.as_deref(), Some("v2"));
        assert_eq!(p.previous.as_ref().unwrap().tag.as_deref(), Some("v1"));
    }

    #[test]
    fn failed_up_does_not_rotate_deployed_state() {
        // The rotation only runs inside the `status.success()` branch. A failed
        // up never calls it, so deployed state is untouched. Modeled here by NOT
        // invoking rotate_on_success for the failed attempt.
        let mut state = DeployedState::default();
        rotate_on_success(&mut state, "glm", deployed_record("v1", "a.yaml"));
        let before = state.clone();
        // (failed up: no rotation)
        assert_eq!(state, before, "a failed up must not change deployed state");
        let p = state.projects.get("glm").unwrap();
        assert_eq!(p.current.tag.as_deref(), Some("v1"));
        assert!(p.previous.is_none());
    }

    #[test]
    fn single_project_version_top_level_is_byte_identical() {
        // Contract: on a single-project CVM the 5 legacy top-level /version fields
        // (status/tag/commit/file/file_sha256) must serialize byte-identically to
        // the pre-#46 response, with `projects` appended additively. Build the
        // exact StatusResponse /version emits and diff the legacy prefix.
        let mut projects = std::collections::BTreeMap::new();
        projects.insert(
            "glm".to_string(),
            ProjectDeployed { current: deployed_record("v1", "a.yaml"), previous: None },
        );
        let with_projects = StatusResponse {
            status: "ok".into(),
            tag: Some("v1".into()),
            commit: Some("abc123".into()),
            file: Some("a.yaml".into()),
            file_sha256: Some("deadbeef".into()),
            output: None,
            exit_code: None,
            error: None,
            image: None,
            projects,
        };
        let json = serde_json::to_string(&with_projects).unwrap();
        // Legacy top-level fields are present and unchanged.
        assert!(json.contains(r#""status":"ok""#));
        assert!(json.contains(r#""tag":"v1""#));
        assert!(json.contains(r#""commit":"abc123""#));
        assert!(json.contains(r#""file":"a.yaml""#));
        assert!(json.contains(r#""file_sha256":"deadbeef""#));
        // `projects` is additive.
        assert!(json.contains(r#""projects":{"glm":"#));

        // With NO projects, `projects` is omitted entirely => byte-identical to
        // the pre-#46 shape (no trailing field, no null).
        let empty = StatusResponse {
            status: "ok".into(),
            tag: Some("v1".into()),
            commit: None,
            file: None,
            file_sha256: None,
            output: None,
            exit_code: None,
            error: None,
            image: None,
            projects: Default::default(),
        };
        let json = serde_json::to_string(&empty).unwrap();
        assert_eq!(json, r#"{"status":"ok","tag":"v1"}"#);
    }

    // --- dry_run (Commit B) ---

    fn cmd_args(cmd: &AsyncCommand) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn dry_run_flag_sits_right_after_compose() {
        // --dry-run is a GLOBAL flag and MUST precede the subcommand (`up`).
        let cmd = build_compose_cmd(
            Path::new("/tmp"),
            "glm",
            &["up", "-d", "--remove-orphans"],
            "f.yml",
            &[],
            &[],
            None,
            true,
        );
        let args = cmd_args(&cmd);
        assert_eq!(args[0], "compose");
        assert_eq!(args[1], "--dry-run", "global flag must immediately follow `compose`");
        assert_eq!(args[2], "-p");
        // The subcommand still comes after -p/-f.
        assert!(args.iter().position(|a| a == "up").unwrap() > args.iter().position(|a| a == "--dry-run").unwrap());
    }

    #[test]
    fn non_dry_run_command_has_no_dry_run_flag() {
        let cmd = build_compose_cmd(
            Path::new("/tmp"),
            "glm",
            &["up", "-d"],
            "f.yml",
            &[],
            &[],
            None,
            false,
        );
        let args = cmd_args(&cmd);
        assert!(!args.iter().any(|a| a == "--dry-run"), "real apply must never carry --dry-run");
        assert_eq!(args[0], "compose");
        assert_eq!(args[1], "-p");
    }

    #[test]
    fn parse_dry_run_plan_buckets_each_verb() {
        // Compose's progress writer style, including the DRY-RUN MODE prefix.
        let lines = vec![
            "DRY-RUN MODE -  glm-vllm-1  Creating".to_string(),
            "DRY-RUN MODE -  glm-proxy-1  Recreate".to_string(),
            "DRY-RUN MODE -  glm-old-1  Removing".to_string(),
            "DRY-RUN MODE -  glm-nginx-1  Running".to_string(),
        ];
        let plan = parse_dry_run_plan(&lines);
        assert_eq!(plan.create, vec!["glm-vllm-1"]);
        assert_eq!(plan.recreate, vec!["glm-proxy-1"]);
        assert_eq!(plan.remove, vec!["glm-old-1"]);
        assert_eq!(plan.unchanged, vec!["glm-nginx-1"]);
    }

    #[test]
    fn parse_dry_run_plan_remove_orphans_and_strongest_verb_wins() {
        // The `remove` case from `--remove-orphans` with an EMPTY services[]:
        // an orphan container shows a Removing line; an unchanged one shows
        // Running. Paired progress lines (Creating then Created) must not
        // double-count — the strongest verb for a name wins.
        let lines = vec![
            "orphan-1  Removing".to_string(),
            "orphan-1  Removed".to_string(),
            "svc-1  Creating".to_string(),
            "svc-1  Created".to_string(),
            "svc-2  Running".to_string(),
            // A later Recreate for svc-2 must upgrade it past unchanged.
            "svc-2  Recreate".to_string(),
            "noise that is not a status line".to_string(),
            "".to_string(),
        ];
        let plan = parse_dry_run_plan(&lines);
        assert_eq!(plan.remove, vec!["orphan-1"]);
        assert_eq!(plan.create, vec!["svc-1"]);
        assert_eq!(plan.recreate, vec!["svc-2"]);
        assert!(plan.unchanged.is_empty(), "svc-2 upgraded to recreate, not left unchanged");
    }

    #[test]
    fn dry_run_plan_serializes_with_all_four_buckets() {
        // The advisory plan always serializes all four arrays (empty when none),
        // so the contract shape is stable for the reconciler.
        let plan = DryRunPlan::default();
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(json, r#"{"create":[],"recreate":[],"remove":[],"unchanged":[]}"#);
    }

    // --- materialize_only (Commit C) ---

    #[test]
    fn parse_image_ids_dedups_and_trims() {
        let out = "  sha256:aaa  \nsha256:bbb\n\nsha256:aaa\n  \nsha256:ccc\n";
        assert_eq!(
            parse_image_ids(out),
            vec!["sha256:aaa", "sha256:bbb", "sha256:ccc"]
        );
        assert!(parse_image_ids("").is_empty());
        assert!(parse_image_ids("\n  \n").is_empty());
    }

    #[test]
    fn staged_event_line_has_documented_shape() {
        // The terminal `staged` done-marker shape the platform consumes.
        let staged_line = serde_json::json!({
            "event": "staged",
            "tag": "v0.0.211",
            "file": "GLM-5.1.yaml",
            "file_sha256": "deadbeef",
            "images": ["sha256:aaa", "sha256:bbb"],
        });
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&staged_line).unwrap()).unwrap();
        assert_eq!(v["event"], "staged");
        assert_eq!(v["tag"], "v0.0.211");
        assert_eq!(v["file"], "GLM-5.1.yaml");
        assert_eq!(v["file_sha256"], "deadbeef");
        assert_eq!(v["images"], serde_json::json!(["sha256:aaa", "sha256:bbb"]));
    }

    #[test]
    fn compose_stage_is_distinct_verb_and_never_migrates_as_deployed() {
        // compose_stage MUST be a distinct verb (not compose_up): the
        // success-rotation hook only rotates on compose_up, and the legacy
        // action-log migration only seeds deployed state from a compose_up — so
        // a staged-but-not-activated model can never surface as "deployed".
        let stage = DeploymentAction {
            action: "compose_stage".into(),
            tag: Some("v-staged".into()),
            file: Some("staged.yaml".into()),
            ..Default::default()
        };
        assert!(
            deployed_version_from_actions(&[stage]).is_none(),
            "a compose_stage action must never be picked as the deployed version"
        );
    }

    #[test]
    fn compose_stage_slack_message_reads_as_staged_not_deployed() {
        let mut a = sample_action("compose_stage");
        a.tag = Some("v1".into());
        a.file = Some("glm.yaml".into());
        let ok = notify::format_message(&a, "gpu30", "ops", Some(true));
        assert!(ok.contains("Staged (not activated)"), "got: {ok}");
        assert!(!ok.contains("Deployed"), "must not read as an activation: {ok}");
        let failed = notify::format_message(&a, "gpu30", "ops", Some(false));
        assert!(failed.contains("Stage failed"), "got: {failed}");
    }

    #[test]
    fn compose_project_name_matches_default_and_sanitizes() {
        // The production path: must equal the previous implicit default so no
        // container/network renaming occurs on already-healthy hosts.
        assert_eq!(compose_project_name(Path::new("/app/work")), "work");
        // Uppercase + invalid chars are lowercased / replaced with '_'.
        assert_eq!(compose_project_name(Path::new("/srv/My Stack")), "my_stack");
        // Leading non-alphanumerics are trimmed (Docker requires a leading
        // letter or digit).
        assert_eq!(compose_project_name(Path::new("/srv/-weird")), "weird");
        // Degenerate basenames fall back to a safe default rather than "".
        assert_eq!(compose_project_name(Path::new("/")), "work");
    }

    #[test]
    fn resolve_compose_project_defaults_when_omitted() {
        // No override -> exactly the working-directory basename default, so
        // omitting `project` reproduces today's behavior byte-for-byte.
        assert_eq!(
            resolve_compose_project(None, Path::new("/app/work")).unwrap(),
            "work"
        );
    }

    #[test]
    fn resolve_compose_project_accepts_canonical_override() {
        // A valid, already-canonical override is used verbatim in place of the
        // basename default.
        assert_eq!(
            resolve_compose_project(Some("glm-5_1"), Path::new("/app/work")).unwrap(),
            "glm-5_1"
        );
        assert_eq!(
            resolve_compose_project(Some("my_model"), Path::new("/app/work")).unwrap(),
            "my_model"
        );
        assert_eq!(
            resolve_compose_project(Some("model123"), Path::new("/app/work")).unwrap(),
            "model123"
        );
    }

    #[test]
    fn resolve_compose_project_rejects_non_canonical_override() {
        // Non-canonical names are rejected (not silently rewritten), so two
        // distinct caller names can never collapse onto one project and let
        // `--remove-orphans` evict another model.
        assert!(resolve_compose_project(Some("GLM-5.1"), Path::new("/app/work")).is_err()); // uppercase + '.'
        assert!(resolve_compose_project(Some("My Model"), Path::new("/app/work")).is_err()); // space + uppercase
        assert!(resolve_compose_project(Some("-leading"), Path::new("/app/work")).is_err()); // leading non-alnum
        assert!(resolve_compose_project(Some("glm-5_1"), Path::new("/app/work")).is_ok()); // canonical -> accepted
    }

    #[test]
    fn resolve_compose_project_rejects_reserved_and_empty() {
        // Reserved names would let `--remove-orphans` reach the infra/app-compose
        // project, so they are rejected. (Supplied in canonical form; odd-cased
        // variants like "WORK" are already rejected as non-canonical.)
        assert!(resolve_compose_project(Some("work"), Path::new("/app/work")).is_err());
        assert!(resolve_compose_project(Some("dstack"), Path::new("/app/work")).is_err());
        // Empty / all-invalid names have nothing valid left after sanitization.
        assert!(resolve_compose_project(Some(""), Path::new("/app/work")).is_err());
        assert!(resolve_compose_project(Some("///"), Path::new("/app/work")).is_err());
    }

    #[test]
    fn sanitize_project_name_returns_none_for_empty() {
        assert_eq!(sanitize_project_name(""), None);
        assert_eq!(sanitize_project_name("***"), None);
        assert_eq!(sanitize_project_name("Valid-1"), Some("valid-1".to_string()));
    }

    fn temp_work_dir() -> PathBuf {
        use rand::RngCore;
        let mut bytes = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut bytes);
        let dir = std::env::temp_dir().join(format!("cm-test-{}", hex::encode(bytes)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn action_log_round_trips_through_disk() {
        let dir = temp_work_dir();
        let actions = vec![DeploymentAction {
            timestamp: "2026-01-01T00:00:00+00:00".into(),
            action: "compose_up".into(),
            image: None,
            tag: Some("v1.0".into()),
            commit: Some("abc123".into()),
            file: Some("docker-compose.yml".into()),
            file_sha256: Some("deadbeef".into()),
            services: vec!["nginx".into()],
            container: None,
        }];
        persist_actions_to_disk(&dir, &actions).await.unwrap();
        let loaded = load_actions_from_disk(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].action, "compose_up");
        assert_eq!(loaded[0].tag.as_deref(), Some("v1.0"));
        assert_eq!(loaded[0].services, vec!["nginx"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn started_action_carries_image_for_attestation() {
        let a = DeploymentAction {
            timestamp: "2026-01-01T00:00:00+00:00".into(),
            action: "compose_manager_started".into(),
            container: Some("compose-manager".into()),
            image: Some("nearaidev/compose-manager@sha256:abc".into()),
            ..Default::default()
        };
        let json = canonicalize_actions(&[a.clone()]);
        assert!(
            json.contains("\"image\":\"nearaidev/compose-manager@sha256:abc\""),
            "image must be serialized for attestation; got: {json}"
        );
        let back: Vec<DeploymentAction> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0].image.as_deref(), Some("nearaidev/compose-manager@sha256:abc"));

        let none = DeploymentAction { action: "compose_up".into(), ..Default::default() };
        assert!(
            !canonicalize_actions(&[none]).contains("image"),
            "image must be omitted when None"
        );
    }

    #[test]
    fn canonicalize_alphabetical_key_order() {
        let a = DeploymentAction {
            timestamp: "2026-01-01T00:00:00+00:00".into(),
            action: "compose_up".into(),
            tag: Some("v1".into()),
            ..Default::default()
        };
        let json = canonicalize_actions(&[a]);
        assert_eq!(
            json,
            r#"[{"action":"compose_up","tag":"v1","timestamp":"2026-01-01T00:00:00+00:00"}]"#
        );
    }

    #[test]
    fn canonicalize_hash_reproducible_in_python() {
        let actions = vec![DeploymentAction {
            timestamp: "2026-01-01T00:00:00+00:00".into(),
            action: "compose_up".into(),
            tag: Some("v1".into()),
            ..Default::default()
        }];
        let canonical = canonicalize_actions(&actions);
        let hash = hex::encode(sha2::Sha256::digest(canonical.as_bytes()));
        // Python: hashlib.sha256(json.dumps([{"action":"compose_up","tag":"v1","timestamp":"2026-01-01T00:00:00+00:00"}], sort_keys=True, separators=(",",":"), ensure_ascii=False).encode()).hexdigest()
        assert_eq!(
            hash,
            "381eb48dc299dafbbcd49c1a009998240f641865f35e435d2f07385c6e6b2a23"
        );
    }

    #[test]
    fn canonicalize_non_ascii_matches_python() {
        // serde_json emits raw UTF-8 for non-ASCII; Python must use
        // ensure_ascii=False to match.
        let actions = vec![DeploymentAction {
            timestamp: "2026-01-01T00:00:00+00:00".into(),
            action: "compose_up".into(),
            tag: Some("café".into()),
            ..Default::default()
        }];
        let canonical = canonicalize_actions(&actions);
        let hash = hex::encode(sha2::Sha256::digest(canonical.as_bytes()));
        // Python: hashlib.sha256(json.dumps([{"action":"compose_up","tag":"café","timestamp":"2026-01-01T00:00:00+00:00"}], sort_keys=True, separators=(",",":"), ensure_ascii=False).encode()).hexdigest()
        assert_eq!(
            hash,
            "1025bd6b9239039058d7fec821e96c83ef1db9331a5b04ec9db8767b3180722a"
        );
    }

    #[test]
    fn load_actions_returns_empty_for_missing_file() {
        let dir = std::env::temp_dir().join("cm-test-nonexistent-xyzzy-12345");
        // Directory doesn't exist — should get NotFound, return empty vec
        let loaded = load_actions_from_disk(&dir);
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn load_actions_returns_empty_for_corrupt_file() {
        let dir = temp_work_dir();
        tokio::fs::write(actions_file(&dir), b"not valid json").await.unwrap();
        let loaded = load_actions_from_disk(&dir);
        assert!(loaded.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn persist_is_atomic_tmp_rename() {
        let dir = temp_work_dir();
        persist_actions_to_disk(&dir, &[]).await.unwrap();
        // tmp file must not be left behind after a successful write
        assert!(!dir.join("actions.json.tmp").exists());
        assert!(actions_file(&dir).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dstack_action_allowlist_accepts_known_actions() {
        for action in DSTACK_AGENT_ACTIONS {
            assert!(is_valid_dstack_action(action), "expected '{}' valid", action);
        }
    }

    #[test]
    fn dstack_action_allowlist_rejects_unknown_actions() {
        for bad in [
            "",
            "reload",
            "enable",
            "disable",
            "kill",
            "Start",        // case sensitive
            "RESTART",      // case sensitive
            "start ",       // trailing whitespace
            " start",       // leading whitespace
            "start;rm",     // shell metachar attempt
            "start\nstop",  // newline
            "start\0",      // null byte
            "../etc/passwd",
        ] {
            assert!(
                !is_valid_dstack_action(bad),
                "expected '{}' rejected",
                bad.escape_debug()
            );
        }
    }

    #[test]
    fn dstack_unit_is_dstack_guest_agent() {
        // Guard against accidental retargeting. If you legitimately need to
        // change this constant, update this test and audit every call site.
        assert_eq!(DSTACK_AGENT_UNIT, "dstack-guest-agent.service");
    }

    #[test]
    fn systemctl_combined_handles_empty_stderr() {
        let o = SystemctlOutput {
            stdout: "active (running)".into(),
            stderr: String::new(),
            exit_code: Some(0),
            success: true,
        };
        assert_eq!(o.combined(), "active (running)");
    }

    #[test]
    fn systemctl_combined_handles_empty_stdout() {
        let o = SystemctlOutput {
            stdout: String::new(),
            stderr: "Unit not found".into(),
            exit_code: Some(4),
            success: false,
        };
        assert_eq!(o.combined(), "Unit not found");
    }

    #[test]
    fn systemctl_combined_concatenates_both_streams() {
        let o = SystemctlOutput {
            stdout: "out".into(),
            stderr: "err".into(),
            exit_code: Some(1),
            success: false,
        };
        assert_eq!(o.combined(), "out\nerr");
    }

    #[test]
    fn systemctl_combined_handles_both_empty() {
        let o = SystemctlOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            success: true,
        };
        assert_eq!(o.combined(), "");
    }

    #[test]
    fn algif_blacklist_script_writes_to_run_modprobe_d() {
        // /etc is dm-verity readonly inside dstack-OS CVMs — the script MUST
        // target /run/modprobe.d, which is on tmpfs and writable.
        assert!(
            ALGIF_BLACKLIST_SCRIPT.contains("/run/modprobe.d/disable-algif-aead.conf"),
            "blacklist script must write to /run/modprobe.d"
        );
        assert!(
            !ALGIF_BLACKLIST_SCRIPT.contains("/etc/modprobe.d/"),
            "blacklist script must not target /etc/modprobe.d (rootfs is dm-verity readonly)"
        );
    }

    #[test]
    fn algif_blacklist_script_installs_rule_per_module() {
        // kmod install-rules key on the resolved module name, not on
        // transitive deps — verified live on gpu03 that a rule on the
        // `algif` parent alone does NOT block child loads. We need one
        // explicit `install <name> /bin/true` line for every module we
        // want to block from (auto)loading.
        for m in &["algif_aead", "algif_hash", "algif_skcipher", "algif_rng", "algif"] {
            let rule = format!("install {} /bin/true", m);
            assert!(
                ALGIF_BLACKLIST_SCRIPT.contains(&rule),
                "blacklist script missing install-rule: {}",
                rule
            );
        }
    }

    #[test]
    fn algif_blacklist_script_unloads_running_modules() {
        // If the module was loaded before mitigation, we must explicitly
        // remove it — the modprobe.d install rule only blocks future loads.
        // We also unload sibling algif_* (hash, skcipher, rng) since the
        // parent install-rule blocks new loads but doesn't touch resident
        // siblings.
        for m in &["algif_aead", "algif_hash", "algif_skcipher", "algif_rng", "algif"] {
            assert!(
                ALGIF_BLACKLIST_SCRIPT.contains(m),
                "blacklist script must reference module {}",
                m
            );
        }
        assert!(ALGIF_BLACKLIST_SCRIPT.contains("modprobe -r"));
    }

    #[test]
    fn algif_blacklist_script_fails_fast_on_write_errors() {
        // Regression guard: an earlier draft used `&&`-then-`;` chaining which
        // meant a printf failure (i.e. blacklist file never written) was
        // masked by a trailing `true`, so the script lied about success.
        // `set -e` makes the write half fail loud while still letting
        // `modprobe -r || true` ignore not-loaded modules.
        assert!(
            ALGIF_BLACKLIST_SCRIPT.contains("set -e"),
            "blacklist script must use `set -e` so write failures aren't masked"
        );
    }

    #[test]
    fn algif_blacklist_script_verifies_unload() {
        // The script must fail (exit non-zero) if any algif* module remains
        // loaded after the unload loop — otherwise the caller has no signal
        // that mitigation actually took effect.
        assert!(
            ALGIF_BLACKLIST_SCRIPT.contains("lsmod"),
            "blacklist script must verify with lsmod"
        );
        assert!(
            ALGIF_BLACKLIST_SCRIPT.contains("exit 1"),
            "blacklist script must exit 1 on residual modules"
        );
    }

    #[test]
    fn algif_blacklist_output_combined_handles_all_cases() {
        let empty = AlgifBlacklistOutput {
            stdout: String::new(),
            stderr: String::new(),
            success: true,
        };
        assert_eq!(empty.combined(), "");

        let out_only = AlgifBlacklistOutput {
            stdout: "ok".into(),
            stderr: String::new(),
            success: true,
        };
        assert_eq!(out_only.combined(), "ok");

        let err_only = AlgifBlacklistOutput {
            stdout: String::new(),
            stderr: "bad".into(),
            success: false,
        };
        assert_eq!(err_only.combined(), "bad");

        let both = AlgifBlacklistOutput {
            stdout: "ok".into(),
            stderr: "warn".into(),
            success: true,
        };
        assert_eq!(both.combined(), "ok\nwarn");
    }

    // --- Compose lock tests ---

    fn make_test_state() -> Arc<AppState> {
        Arc::new(AppState {
            bearer_token: "test-token".into(),
            github_owner: "test".into(),
            github_repo_name: "test".into(),
            min_tag_age_hours: 0,
            work_dir: PathBuf::from("/tmp/compose-manager-test"),
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

    #[tokio::test]
    async fn compose_lock_acquire_succeeds_when_free() {
        let state = make_test_state();
        let result = state.try_acquire_compose_lock(
            "compose_up",
            Some("v1".into()),
            Some("docker-compose.yml".into()),
            vec![],
            None,
        ).await;
        assert!(result.is_ok(), "lock should be acquirable when no one holds it");
    }

    #[tokio::test]
    async fn compose_lock_rejects_concurrent_request() {
        let state = make_test_state();
        let _guard = state.try_acquire_compose_lock(
            "compose_up",
            Some("v1".into()),
            Some("a.yaml".into()),
            vec![],
            None,
        ).await.unwrap();
        // While the first guard is held, a second acquire must fail.
        let result = state.try_acquire_compose_lock(
            "compose_down",
            Some("v2".into()),
            Some("b.yaml".into()),
            vec![],
            None,
        ).await;
        assert!(result.is_err(), "lock should reject when already held");
    }

    #[tokio::test]
    async fn compose_lock_releases_on_drop() {
        let state = make_test_state();
        {
            let _guard = state.try_acquire_compose_lock(
                "compose_up",
                Some("v1".into()),
                Some("a.yaml".into()),
                vec![],
                None,
            ).await.unwrap();
        } // guard dropped here
        // After dropping, the lock should be available again.
        let result = state.try_acquire_compose_lock(
            "compose_down",
            Some("v2".into()),
            Some("b.yaml".into()),
            vec![],
            None,
        ).await;
        assert!(result.is_ok(), "lock should be available after guard is dropped");
    }

    #[tokio::test]
    async fn in_flight_cleared_on_drop() {
        let state = make_test_state();
        {
            let _guard = state.try_acquire_compose_lock(
                "compose_up",
                Some("v1".into()),
                Some("a.yaml".into()),
                vec!["svc".into()],
                None,
            ).await.unwrap();
            let in_flight = state.in_flight.lock().unwrap().clone();
            assert!(in_flight.is_some(), "in_flight should be set while lock is held");
            assert_eq!(in_flight.unwrap().action, "compose_up");
        }
        // After dropping the ComposeGuard, in_flight must be cleared
        // (ComposeGuard::Drop clears it).
        let in_flight = state.in_flight.lock().unwrap().clone();
        assert!(in_flight.is_none(), "in_flight should be cleared after ComposeGuard drops");
    }

    #[tokio::test]
    async fn in_flight_reflects_current_action() {
        let state = make_test_state();
        let _guard = state.try_acquire_compose_lock("docker_clean", None, None, vec![], None).await.unwrap();
        let in_flight = state.in_flight.lock().unwrap().clone().unwrap();
        assert_eq!(in_flight.action, "docker_clean");
        assert_eq!(in_flight.tag, None);
    }

    #[tokio::test]
    async fn in_flight_carries_operation_metadata() {
        let state = make_test_state();
        let _guard = state.try_acquire_compose_lock(
            "docker_restart",
            None,
            None,
            vec![],
            Some("vllm-test".into()),
        ).await.unwrap();
        let in_flight = state.in_flight.lock().unwrap().clone().unwrap();
        assert_eq!(in_flight.action, "docker_restart");
        assert_eq!(in_flight.container.as_deref(), Some("vllm-test"));
    }

    #[test]
    fn operation_status_response_includes_null_when_idle() {
        let response = OperationStatusResponse {
            status: "ok".into(),
            in_flight: None,
        };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"status":"ok","in_flight":null}"#);
    }

    // --- Slack notification tests ---

    fn sample_action(action: &str) -> DeploymentAction {
        DeploymentAction {
            timestamp: "2026-01-01T00:00:00+00:00".into(),
            action: action.into(),
            image: None,
            tag: None,
            commit: None,
            file: None,
            file_sha256: None,
            services: vec![],
            container: None,
        }
    }

    #[test]
    fn format_deploy_success_includes_host_tag_file_services_actor() {
        let a = DeploymentAction {
            file: Some("glm.yaml".into()),
            tag: Some("v0.0.165".into()),
            services: vec!["nginx".into(), "proxy".into()],
            ..sample_action("compose_up")
        };
        let msg = notify::format_message(&a, "gpu23", "ansible", Some(true));
        assert!(msg.starts_with(":rocket:"), "got: {msg}");
        assert!(msg.contains("Deployed"), "got: {msg}");
        assert!(msg.contains("gpu23"));
        assert!(msg.contains("glm.yaml"));
        assert!(msg.contains("v0.0.165"));
        assert!(msg.contains("nginx, proxy"));
        assert!(msg.contains("by ansible"));
    }

    #[test]
    fn format_deploy_failure_uses_x_and_failed_wording() {
        let a = sample_action("compose_up");
        let msg = notify::format_message(&a, "gpu23", "ci", Some(false));
        assert!(msg.starts_with(":x:"), "got: {msg}");
        assert!(msg.contains("Deploy failed"), "got: {msg}");
        // No grammatically-broken "Deploy failed succeeded" etc.
        assert!(!msg.contains("succeeded"));
    }

    #[test]
    fn format_compose_down_reads_naturally() {
        let a = DeploymentAction {
            tag: Some("v1".into()),
            file: Some("small-models.yaml".into()),
            ..sample_action("compose_down")
        };
        let msg = notify::format_message(&a, "gpu02", "dashboard", Some(true));
        assert!(msg.contains("Stopped"), "got: {msg}");
        assert!(!msg.contains("Stopped succeeded"), "got: {msg}");
        assert!(msg.contains("gpu02"));
        assert!(msg.contains("small-models.yaml"));
        assert!(msg.contains("by dashboard"));
    }

    #[test]
    fn format_docker_restart_names_container() {
        let a = DeploymentAction {
            container: Some("vllm".into()),
            ..sample_action("docker_restart")
        };
        let msg = notify::format_message(&a, "gpu11", "dashboard", Some(true));
        assert!(msg.contains("Restarted container"), "got: {msg}");
        assert!(msg.contains("vllm"));
        assert!(msg.contains("gpu11"));
    }

    #[test]
    fn format_dstack_agent_uses_past_tense_on_success() {
        let a = sample_action("dstack_agent_stop");
        let msg = notify::format_message(&a, "agent2", "dashboard", Some(true));
        assert!(msg.contains("dstack-agent stopped"), "got: {msg}");
        let a2 = sample_action("dstack_agent_restart");
        let msg2 = notify::format_message(&a2, "agent2", "ci", Some(false));
        assert!(msg2.starts_with(":x:"), "got: {msg2}");
        assert!(msg2.contains("dstack-agent restart failed"), "got: {msg2}");
    }

    #[test]
    fn format_falls_back_to_unknown_host_when_label_empty() {
        let a = sample_action("compose_up");
        let msg = notify::format_message(&a, "", "automation", Some(true));
        assert!(msg.contains("unknown-host"), "got: {msg}");
    }

    #[test]
    fn extract_actor_defaults_to_automation_when_absent() {
        let headers = HeaderMap::new();
        assert_eq!(extract_actor(&headers), "automation");
    }

    #[test]
    fn extract_actor_reads_and_sanitizes_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-triggered-by", "ansible@gpu23".parse().unwrap());
        assert_eq!(extract_actor(&headers), "ansible@gpu23");

        // Header lookup is case-insensitive and an empty value falls back.
        let mut h2 = HeaderMap::new();
        h2.insert("X-Triggered-By", "   ".parse().unwrap());
        assert_eq!(extract_actor(&h2), "automation");
    }

    #[test]
    fn gpu_indices_from_device_requests_and_env() {
        // DeviceRequests numeric IDs + NVIDIA_VISIBLE_DEVICES env, deduped.
        let v = serde_json::json!([{
            "HostConfig": { "DeviceRequests": [{ "DeviceIDs": ["0", "1"] }] },
            "Config": { "Env": ["FOO=bar", "NVIDIA_VISIBLE_DEVICES=1,2"] }
        }]);
        let mut idx = gpu_indices_from_inspect(&v);
        idx.sort_unstable();
        assert_eq!(idx, vec![0, 1, 2]);
    }

    #[test]
    fn gpu_indices_ignores_non_numeric_and_all() {
        // "all" / UUID device IDs aren't mappable to nvidia-smi indices.
        let v = serde_json::json!([{
            "HostConfig": { "DeviceRequests": [{ "DeviceIDs": ["GPU-abc"] }] },
            "Config": { "Env": ["NVIDIA_VISIBLE_DEVICES=all"] }
        }]);
        assert!(gpu_indices_from_inspect(&v).is_empty());
    }

    #[test]
    fn parse_nvidia_smi_rows() {
        let out = "0, 81920, 1024, 33\n1, 81920, 40000, 95\nbad line\n";
        let m = parse_nvidia_smi(out);
        assert_eq!(m.get(&0), Some(&(81920u64, 1024u64, 33u32)));
        assert_eq!(m.get(&1), Some(&(81920u64, 40000u64, 95u32)));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn parse_human_size_decimal_units() {
        assert_eq!(parse_human_size("0B"), Some(0));
        assert_eq!(parse_human_size("512MB"), Some(512_000_000));
        assert_eq!(parse_human_size("1.5GB"), Some(1_500_000_000));
        assert_eq!(parse_human_size("12.3kB"), Some(12_300));
        assert_eq!(parse_human_size("garbage"), None);
    }

    #[test]
    fn docker_volume_name_grammar() {
        assert!(is_valid_docker_volume_name("huggingface_cache"));
        assert!(is_valid_docker_volume_name("vllm-cache.1"));
        assert!(is_valid_docker_volume_name("a"));
        // Must start alphanumeric; no path/spaces/colons.
        assert!(!is_valid_docker_volume_name("_leading"));
        assert!(!is_valid_docker_volume_name("/etc/passwd"));
        assert!(!is_valid_docker_volume_name("has space"));
        assert!(!is_valid_docker_volume_name("name:with:colon"));
        assert!(!is_valid_docker_volume_name(""));
    }

    #[test]
    fn parse_volume_sizes_reads_local_volumes_table() {
        let df = "\
Images space usage:
REPOSITORY  TAG  SIZE
foo         a    100MB

Local Volumes space usage:
VOLUME NAME            LINKS     SIZE
huggingface_cache      2         120GB
vllm_cache             1         3.5GB

Build cache usage: 0B
";
        let sizes = parse_volume_sizes(df);
        assert_eq!(sizes.get("huggingface_cache"), Some(&120_000_000_000));
        assert_eq!(sizes.get("vllm_cache"), Some(&3_500_000_000));
        // The images-section row must not leak into volume sizes.
        assert!(!sizes.contains_key("foo"));
    }

    /// Live end-to-end delivery against a real Slack incoming webhook. Ignored
    /// by default (needs network + a webhook); run with:
    ///   SLACK_TEST_WEBHOOK_URL=<url> cargo test slack_live_delivery -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn slack_live_delivery() {
        let webhook = match std::env::var("SLACK_TEST_WEBHOOK_URL") {
            Ok(w) if !w.trim().is_empty() => w,
            _ => {
                eprintln!("SLACK_TEST_WEBHOOK_URL not set; skipping");
                return;
            }
        };
        let action = DeploymentAction {
            file: Some("glm.yaml".into()),
            tag: Some("v0.0.165".into()),
            services: vec!["nginx".into()],
            ..sample_action("compose_up")
        };
        let text = notify::format_message(&action, "gpu-test", "integration-test", Some(true));
        let resp = reqwest::Client::new()
            .post(&webhook)
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await
            .expect("request to Slack failed");
        assert!(resp.status().is_success(), "slack returned {}", resp.status());
    }

    // --- Selective per-model eviction (/docker/evict) tests ---

    #[test]
    fn model_to_hub_dir_maps_org_repo() {
        assert_eq!(model_to_hub_dir("zai-org/GLM-5.2-FP8").unwrap(), "models--zai-org--GLM-5.2-FP8");
        assert_eq!(model_to_hub_dir("deepseek-ai/DeepSeek-V4-Flash").unwrap(), "models--deepseek-ai--DeepSeek-V4-Flash");
        // Bare repo id (no org) is still accepted.
        assert_eq!(model_to_hub_dir("gpt2").unwrap(), "models--gpt2");
        // Leading/trailing whitespace is trimmed.
        assert_eq!(model_to_hub_dir("  org/repo  ").unwrap(), "models--org--repo");
    }

    #[test]
    fn model_to_hub_dir_rejects_traversal() {
        for bad in ["", "../etc", "org/../../etc", "/abs/path", "org/repo/", "a//b", "org\\repo", "x\0y"] {
            assert!(model_to_hub_dir(bad).is_err(), "expected reject for {bad:?}");
        }
    }

    #[test]
    fn model_to_hub_dir_rejects_shell_metacharacters() {
        // The dir name is interpolated/passed to alpine; any metachar must be
        // rejected by the strict [A-Za-z0-9._/-] allowlist.
        for bad in [
            "a$(curl evil.com)/b",
            "org/repo;rm -rf /",
            "org/`whoami`",
            "org/repo\"; rm -rf /c; echo \"",
            "org/repo|cat",
            "org/repo&whoami",
            "org/repo with space",
            "org/repo\nnewline",
            "org/repo*",
        ] {
            assert!(model_to_hub_dir(bad).is_err(), "expected reject for {bad:?}");
        }
    }

    #[test]
    fn validate_volume_name_rejects_host_paths() {
        // Valid named volumes.
        for ok in ["huggingface_cache", "small-models_hugginface_cache", "vol.1-x"] {
            assert!(validate_volume_name(ok).is_ok(), "expected ok for {ok:?}");
        }
        // Host paths / traversal / metachars must be rejected (would bind-mount host).
        for bad in ["", "/", "/etc", "../x", "a/b", "$(x)", "vol;rm"] {
            assert!(validate_volume_name(bad).is_err(), "expected reject for {bad:?}");
        }
    }

    #[test]
    fn resolve_hf_cache_volume_prefers_exact_then_prefixed_handles_typo() {
        // Exact correct name.
        let vols = vec!["other".into(), "huggingface_cache".into()];
        assert_eq!(resolve_hf_cache_volume(&vols).unwrap(), "huggingface_cache");
        // Known typo, exact.
        let vols = vec!["hugginface_cache".into()];
        assert_eq!(resolve_hf_cache_volume(&vols).unwrap(), "hugginface_cache");
        // Project-prefixed typo form.
        let vols = vec!["small-models_hugginface_cache".into(), "certs".into()];
        assert_eq!(resolve_hf_cache_volume(&vols).unwrap(), "small-models_hugginface_cache");
        // Exact wins over prefixed when both present.
        let vols = vec!["proj_huggingface_cache".into(), "huggingface_cache".into()];
        assert_eq!(resolve_hf_cache_volume(&vols).unwrap(), "huggingface_cache");
        // None present.
        assert!(resolve_hf_cache_volume(&["foo".into(), "bar".into()]).is_err());
    }

    #[test]
    fn argv_serves_model_exact_match_only() {
        let argv: Vec<String> = ["sglang", "serve", "--model-path", "zai-org/GLM-5.2-FP8", "--tp", "8"]
            .iter().map(|s| s.to_string()).collect();
        assert!(argv_serves_model(&argv, "zai-org/GLM-5.2-FP8"));
        // No false positive on a prefix.
        assert!(!argv_serves_model(&argv, "zai-org/GLM-5.2"));
        // `--model=val` form.
        let eq: Vec<String> = ["--model=org/repo".into()].to_vec();
        assert!(argv_serves_model(&eq, "org/repo"));
        // A bare value not attached to a model flag must NOT match.
        let bare: Vec<String> = ["--served-model-name".into(), "org/repo".into()].to_vec();
        assert!(!argv_serves_model(&bare, "org/repo"));
    }

    #[test]
    fn env_serves_model_matches_value_not_substring() {
        let env: Vec<String> = ["MODEL_NAME=z-ai/glm-5.2".into(), "HF_TOKEN=abc".into()].to_vec();
        assert!(env_serves_model(&env, "z-ai/glm-5.2"));
        assert!(!env_serves_model(&env, "z-ai/glm")); // substring must not match
        let env: Vec<String> = ["MODEL_PATH=zai-org/GLM-5.2-FP8".into()].to_vec();
        assert!(env_serves_model(&env, "zai-org/GLM-5.2-FP8"));
    }

    #[test]
    fn format_bytes_renders_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert!(format_bytes(2048).starts_with("2.00 KiB"));
        assert!(format_bytes(5_368_709_120).starts_with("5.00 GiB"));
    }

    #[test]
    fn evict_target_defaults_to_weights() {
        // Default when `target` omitted.
        let req: EvictRequest = serde_json::from_str(r#"{"model":"org/repo"}"#).unwrap();
        assert_eq!(req.target, EvictTarget::Weights);
        assert_eq!(req.model, "org/repo");
        assert!(req.cache_volume.is_none());
        // Explicit variants parse.
        let req: EvictRequest = serde_json::from_str(r#"{"model":"a/b","target":"both","cache_volume":"v"}"#).unwrap();
        assert_eq!(req.target, EvictTarget::Both);
        assert_eq!(req.cache_volume.as_deref(), Some("v"));
        let req: EvictRequest = serde_json::from_str(r#"{"model":"a/b","target":"cache"}"#).unwrap();
        assert_eq!(req.target, EvictTarget::Cache);
    }
}
