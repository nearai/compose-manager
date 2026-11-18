//! Restart requests for the two containers the launcher manages.
//!
//! `POST /admin/launcher/restart` writes `$WORK_DIR/launcher-restart-request.json`
//! on the shared `work` volume; the launcher (launcher/launcher.sh) notices it,
//! runs `docker restart <target>` for an allowlisted target, health checks it
//! and reports the result in `launcher-state.json` (`restart_last_*` keys).
//! `GET /admin/launcher/restart` merges both views. Restarting compose-manager
//! restarts the process serving this API; the launcher does the work and
//! records the outcome, so the result is readable afterwards.
//!
//! Contract shared with the launcher (keep in sync):
//! - request file: `{"id": "<32 hex>", "target": "<name>", "requested_at": "<rfc3339>"}`,
//!   mode 0600, replaced atomically (tmp file + rename);
//! - targets: [`TARGETS`], which must equal the launcher's `MANAGED_CONTAINERS`;
//! - a request is handled once the state key `restart_last_id` equals its id;
//! - `restart_last_target`, `restart_last_outcome` (ok | failed | rejected |
//!   abandoned), `restart_last_reason` (a fixed launcher string) and
//!   `restart_last_finished_at` describe the last handled request.
//!
//! Nothing here carries environment values or tokens.

use crate::{
    err, extract_actor, notify, record_action, verify_bearer_token, AppState, DeploymentAction,
};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use crate::timestamp_now;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use serde::{Deserialize, Serialize};
use std::{
    io,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Containers the launcher may restart. Keep identical to the launcher's
/// `MANAGED_CONTAINERS`.
pub(crate) const TARGETS: &[&str] = &["compose-manager", "otelcol-management"];

const REQUEST_FILE_NAME: &str = "launcher-restart-request.json";
const LAUNCHER_STATE_FILE_NAME: &str = "launcher-state.json";
/// A request the launcher never reported on (old launcher, launcher down) stops
/// blocking new requests after this long.
const PENDING_MAX_AGE_SECS: i64 = 900;
/// Launcher text is relayed to API clients; bound it.
const MAX_LAUNCHER_TEXT_LEN: usize = 300;

/// Serializes the read-check-write cycle of POST.
static RESTART_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct RestartRequest {
    id: String,
    target: String,
    requested_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestartBody {
    target: String,
}

fn request_path(work_dir: &Path) -> PathBuf {
    work_dir.join(REQUEST_FILE_NAME)
}

fn new_request_id() -> String {
    format!("{:016x}{:016x}", rand::random::<u64>(), rand::random::<u64>())
}

/// What the launcher reported for the last request it handled.
#[derive(Default, Clone, Serialize)]
struct LauncherResult {
    id: String,
    target: String,
    outcome: String,
    reason: String,
    finished_at: String,
}

struct LauncherView {
    /// The launcher understands restart requests (it wrote `restart_supported`).
    reported: bool,
    result: Option<LauncherResult>,
}

fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_LAUNCHER_TEXT_LEN)
        .collect()
}

async fn read_launcher_view(work_dir: &Path) -> LauncherView {
    let mut view = LauncherView { reported: false, result: None };
    let Ok(text) = tokio::fs::read_to_string(work_dir.join(LAUNCHER_STATE_FILE_NAME)).await else {
        return view;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return view;
    };
    let get = |k: &str| json.get(k).and_then(|v| v.as_str()).map(clean);
    view.reported = get("restart_supported").is_some() || get("restart_last_id").is_some();
    if let Some(id) = get("restart_last_id").filter(|s| !s.is_empty()) {
        view.result = Some(LauncherResult {
            id,
            target: get("restart_last_target").unwrap_or_default(),
            outcome: get("restart_last_outcome").unwrap_or_default(),
            reason: get("restart_last_reason").unwrap_or_default(),
            finished_at: get("restart_last_finished_at").unwrap_or_default(),
        });
    }
    view
}

async fn read_request(work_dir: &Path) -> io::Result<Option<RestartRequest>> {
    match tokio::fs::read(request_path(work_dir)).await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Deletes a path on drop (best-effort); after a successful rename it is a no-op.
struct RemoveTmpOnDrop<'a>(&'a Path);

impl Drop for RemoveTmpOnDrop<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

/// Writes the request atomically: tmp file (0600) in the same directory, fsync,
/// rename.
async fn write_request(work_dir: &Path, req: &RestartRequest) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let content = serde_json::to_vec(req).map_err(io::Error::other)?;
    let tmp = work_dir.join(format!(".{}.{:016x}.tmp", REQUEST_FILE_NAME, rand::random::<u64>()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let _remove_tmp = RemoveTmpOnDrop(&tmp);
    let mut f = tokio::fs::File::from_std(file);
    f.write_all(&content).await?;
    f.flush().await?;
    f.sync_all().await?;
    drop(f);
    tokio::fs::rename(&tmp, request_path(work_dir)).await?;
    if let Ok(d) = tokio::fs::File::open(work_dir).await {
        let _ = d.sync_all().await;
    }
    Ok(())
}

/// A request is pending while the launcher has not reported its id and it is
/// not older than [`PENDING_MAX_AGE_SECS`].
fn is_pending(req: &RestartRequest, result: Option<&LauncherResult>, now: OffsetDateTime) -> bool {
    if result.is_some_and(|r| r.id == req.id) {
        return false;
    }
    match OffsetDateTime::parse(&req.requested_at, &Rfc3339) {
        Ok(at) => (now - at).whole_seconds() < PENDING_MAX_AGE_SECS,
        Err(_) => false,
    }
}

/// `POST /admin/launcher/restart` with `{"target": "compose-manager" |
/// "otelcol-management"}`: queue a restart for the launcher.
pub(crate) async fn post_restart(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.bearer_tokens) {
        return e.into_response();
    }
    // Parsed by hand so auth runs first and serde errors never reach the client.
    let target = match serde_json::from_slice::<RestartBody>(&body) {
        Ok(b) => b.target,
        Err(_) => {
            return err(
                StatusCode::BAD_REQUEST,
                "invalid JSON body; expected {\"target\": \"compose-manager\" | \"otelcol-management\"}",
            )
            .into_response();
        }
    };
    if !TARGETS.contains(&target.as_str()) {
        return err(
            StatusCode::BAD_REQUEST,
            format!("target must be one of: {}", TARGETS.join(", ")),
        )
        .into_response();
    }
    let actor = extract_actor(&headers);

    let request = {
        let _guard = RESTART_LOCK.lock().await;
        let view = read_launcher_view(&state.work_dir).await;
        match read_request(&state.work_dir).await {
            Ok(Some(existing)) if is_pending(&existing, view.result.as_ref(), OffsetDateTime::now_utc()) => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "status": "error",
                        "error": "a restart request is already pending",
                        "pending_request_id": existing.id,
                        "target": existing.target,
                    })),
                )
                    .into_response();
            }
            Ok(_) => {}
            Err(e) => {
                warn!(error_kind = ?e.kind(), "Failed to read launcher restart request file");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "failed to read restart request file")
                    .into_response();
            }
        }
        let request = RestartRequest {
            id: new_request_id(),
            target,
            requested_at: timestamp_now(),
        };
        if let Err(e) = write_request(&state.work_dir, &request).await {
            warn!(error_kind = ?e.kind(), "Failed to write launcher restart request file");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "failed to write restart request file")
                .into_response();
        }
        info!(request_id = %request.id, target = %request.target, "Launcher restart requested");
        request
    };

    let action = DeploymentAction {
        timestamp: timestamp_now(),
        action: "launcher_restart".into(),
        file: Some(REQUEST_FILE_NAME.into()),
        services: vec![request.target.clone()],
        ..Default::default()
    };
    let _ = record_action(&state, &action).await;
    notify::spawn_action(&state, &action, &actor, Some(true));

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "accepted",
            "request_id": request.id,
            "target": request.target,
            "requested_at": request.requested_at,
        })),
    )
        .into_response()
}

/// `GET /admin/launcher/restart`: the latest request and the launcher's result.
pub(crate) async fn get_restart(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.bearer_tokens) {
        return e.into_response();
    }
    let request = match read_request(&state.work_dir).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error_kind = ?e.kind(), "Failed to read launcher restart request file");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "failed to read restart request file")
                .into_response();
        }
    };
    let view = read_launcher_view(&state.work_dir).await;
    let request_json = request.as_ref().map(|r| {
        let handled = view.result.as_ref().is_some_and(|res| res.id == r.id);
        let pending = is_pending(r, view.result.as_ref(), OffsetDateTime::now_utc());
        let status = if handled {
            "handled"
        } else if pending {
            "pending"
        } else {
            "unhandled"
        };
        serde_json::json!({
            "id": r.id,
            "target": r.target,
            "requested_at": r.requested_at,
            "status": status,
        })
    });
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "launcher_reported": view.reported,
            "targets": TARGETS,
            "request": request_json,
            "result": view.result,
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

    fn temp_work_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cm-restart-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_state(work_dir: &Path) -> Arc<AppState> {
        Arc::new(AppState {
            bearer_tokens: vec![TOKEN.into()],
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

    async fn call(
        state: &Arc<AppState>,
        method: &str,
        token: Option<&str>,
        body: &str,
    ) -> (StatusCode, String) {
        let app = Router::new()
            .route("/admin/launcher/restart", get(get_restart).post(post_restart))
            .with_state(state.clone());
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri("/admin/launcher/restart")
            .header("Content-Type", "application/json");
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let resp = app.oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn json(body: &str) -> serde_json::Value {
        serde_json::from_str(body).unwrap()
    }

    fn write_state_file(dir: &Path, value: serde_json::Value) {
        std::fs::write(dir.join(LAUNCHER_STATE_FILE_NAME), value.to_string()).unwrap();
    }

    #[tokio::test]
    async fn auth_is_required_for_post_and_get() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        for method in ["POST", "GET"] {
            for token in [None, Some("wrong")] {
                let (status, _) = call(&state, method, token, r#"{"target":"compose-manager"}"#).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {token:?}");
            }
        }
        assert!(!request_path(&dir).exists(), "no file may be written without auth");
    }

    #[tokio::test]
    async fn target_validation_rejects_everything_but_the_two_containers() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        for bad in [
            r#"{"target":"model-vllm"}"#,
            r#"{"target":""}"#,
            r#"{"target":"Compose-Manager"}"#,
            r#"{"target":"compose-manager "}"#,
            r#"{"target":"compose-manager,otelcol-management"}"#,
            r#"{"target":"../etc"}"#,
            r#"{"target":null}"#,
            r#"{"target":"compose-manager","extra":1}"#,
            r#"{}"#,
            r#"[]"#,
            "not json",
            "",
        ] {
            let (status, body) = call(&state, "POST", Some(TOKEN), bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}");
            assert!(!body.contains("model-vllm") && !body.contains("../etc"), "input echoed: {body}");
        }
        assert!(!request_path(&dir).exists());
    }

    #[tokio::test]
    async fn accepted_request_is_written_atomically_with_mode_0600() {
        for target in TARGETS {
            let dir = temp_work_dir();
            let state = test_state(&dir);
            let (status, body) =
                call(&state, "POST", Some(TOKEN), &format!(r#"{{"target":"{target}"}}"#)).await;
            assert_eq!(status, StatusCode::ACCEPTED, "{body}");
            let resp = json(&body);
            let id = resp["request_id"].as_str().unwrap();
            assert_eq!(id.len(), 32);
            assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
            let file = request_path(&dir);
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let on_disk: RestartRequest = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            assert_eq!(on_disk.id, id);
            assert_eq!(on_disk.target, *target);
            assert!(OffsetDateTime::parse(&on_disk.requested_at, &Rfc3339).is_ok());
            // No temp file left behind.
            let leftovers: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
                .collect();
            assert!(leftovers.is_empty(), "temp file left: {leftovers:?}");
        }
    }

    #[tokio::test]
    async fn second_request_while_pending_gets_409_and_keeps_the_first() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        let (status, body) = call(&state, "POST", Some(TOKEN), r#"{"target":"otelcol-management"}"#).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let first = json(&body)["request_id"].as_str().unwrap().to_string();
        let (status, body) = call(&state, "POST", Some(TOKEN), r#"{"target":"compose-manager"}"#).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(json(&body)["pending_request_id"], first);
        let on_disk: RestartRequest =
            serde_json::from_slice(&std::fs::read(request_path(&dir)).unwrap()).unwrap();
        assert_eq!(on_disk.id, first);
        assert_eq!(on_disk.target, "otelcol-management");
    }

    #[tokio::test]
    async fn new_request_is_accepted_once_the_launcher_reported_the_old_one() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        let (_, body) = call(&state, "POST", Some(TOKEN), r#"{"target":"compose-manager"}"#).await;
        let first = json(&body)["request_id"].as_str().unwrap().to_string();
        write_state_file(
            &dir,
            serde_json::json!({
                "restart_supported": "1",
                "restart_last_id": first,
                "restart_last_target": "compose-manager",
                "restart_last_outcome": "ok",
                "restart_last_reason": "",
                "restart_last_finished_at": "2026-01-01T00:00:00Z",
            }),
        );
        let (status, body) = call(&state, "POST", Some(TOKEN), r#"{"target":"otelcol-management"}"#).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_ne!(json(&body)["request_id"], first);
    }

    #[tokio::test]
    async fn stale_unreported_request_does_not_block_forever() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        let old = RestartRequest {
            id: "a".repeat(32),
            target: "compose-manager".into(),
            requested_at: (OffsetDateTime::now_utc() - time::Duration::seconds(PENDING_MAX_AGE_SECS + 5)).format(&Rfc3339).unwrap(),
        };
        write_request(&dir, &old).await.unwrap();
        let (status, body) = call(&state, "POST", Some(TOKEN), r#"{"target":"compose-manager"}"#).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        // GET shows an unreported request past the limit as unhandled.
        let mut stale = old.clone();
        stale.id = "b".repeat(32);
        write_request(&dir, &stale).await.unwrap();
        let (_, got) = call(&state, "GET", Some(TOKEN), "").await;
        assert_eq!(json(&got)["request"]["status"], "unhandled");
    }

    #[tokio::test]
    async fn get_shape_without_any_request() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        let (status, body) = call(&state, "GET", Some(TOKEN), "").await;
        assert_eq!(status, StatusCode::OK);
        let v = json(&body);
        assert_eq!(v["status"], "ok");
        assert_eq!(v["launcher_reported"], false);
        assert_eq!(v["targets"], serde_json::json!(["compose-manager", "otelcol-management"]));
        assert!(v["request"].is_null());
        assert!(v["result"].is_null());
    }

    #[tokio::test]
    async fn get_reports_pending_then_handled() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        let (_, body) = call(&state, "POST", Some(TOKEN), r#"{"target":"otelcol-management"}"#).await;
        let id = json(&body)["request_id"].as_str().unwrap().to_string();
        let (_, body) = call(&state, "GET", Some(TOKEN), "").await;
        let v = json(&body);
        assert_eq!(v["request"]["id"], id);
        assert_eq!(v["request"]["target"], "otelcol-management");
        assert_eq!(v["request"]["status"], "pending");
        assert!(v["result"].is_null());

        write_state_file(
            &dir,
            serde_json::json!({
                "restart_supported": "1",
                "restart_last_id": id,
                "restart_last_target": "otelcol-management",
                "restart_last_outcome": "failed",
                "restart_last_reason": "otelcol-management stopped\nor is restarting",
                "restart_last_finished_at": "2026-01-01T00:00:00Z",
            }),
        );
        let (_, body) = call(&state, "GET", Some(TOKEN), "").await;
        let v = json(&body);
        assert_eq!(v["launcher_reported"], true);
        assert_eq!(v["request"]["status"], "handled");
        assert_eq!(v["result"]["id"], id);
        assert_eq!(v["result"]["outcome"], "failed");
        assert_eq!(v["result"]["finished_at"], "2026-01-01T00:00:00Z");
        assert_eq!(v["result"]["reason"], "otelcol-management stoppedor is restarting");
    }

    #[tokio::test]
    async fn responses_never_carry_state_secrets() {
        let dir = temp_work_dir();
        let state = test_state(&dir);
        // The state file also holds env-override bookkeeping; only restart_*
        // keys may be surfaced.
        write_state_file(
            &dir,
            serde_json::json!({
                "env_overrides_error_compose_manager": "FAKE-SECRET-VALUE",
                "compose_manager_digest": "sha256:abc",
                "restart_supported": "1",
            }),
        );
        let (_, post) = call(&state, "POST", Some(TOKEN), r#"{"target":"compose-manager"}"#).await;
        let (_, get) = call(&state, "GET", Some(TOKEN), "").await;
        for body in [post, get] {
            assert!(!body.contains("FAKE-SECRET-VALUE"), "{body}");
            assert!(!body.contains(TOKEN), "{body}");
            assert!(!body.contains("sha256:abc"), "{body}");
        }
    }
}
