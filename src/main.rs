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
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command as AsyncCommand,
    sync::{Mutex, RwLock},
};
use tracing::{error, info};

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

async fn persist_actions_to_disk(work_dir: &Path, actions: &[DeploymentAction]) -> std::io::Result<()> {
    let json = serde_json::to_string(actions)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = work_dir.join("actions.json.tmp");
    tokio::fs::write(&tmp, &json).await?;
    tokio::fs::rename(&tmp, actions_file(work_dir)).await?;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
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
}

/// Tracks the currently-running mutating docker/compose operation (if any)
/// for observability and nicer 409 Conflict error messages.
#[derive(Clone, Debug, Serialize)]
struct InFlightOp {
    action: String,
    started_at: String,
    tag: Option<String>,
}

/// Error returned when a mutating docker operation is already in progress.
#[derive(Debug)]
struct ComposeLockBusy;

/// RAII guard that holds the compose lock and clears `in_flight` metadata on Drop.
/// This ensures `in_flight` is always cleaned up regardless of how the guard
/// goes out of scope — early returns, handler completion, or NdjsonStream drop.
struct ComposeGuard {
    _inner: tokio::sync::OwnedMutexGuard<()>,
    in_flight: Arc<RwLock<Option<InFlightOp>>>,
}

impl Drop for ComposeGuard {
    fn drop(&mut self) {
        // Best-effort: use try_write to avoid blocking in a drop context.
        if let Ok(mut guard) = self.in_flight.try_write() {
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
    deployed_tag: RwLock<Option<String>>,
    deployed_commit: RwLock<Option<String>>,
    deployed_file: RwLock<Option<String>>,
    deployed_file_sha256: RwLock<Option<String>>,
    actions: RwLock<Vec<DeploymentAction>>,
    http: reqwest::Client,
    /// Mutual-exclusion lock for mutating docker/compose operations.
    /// Uses `try_lock` semantics — concurrent requests receive HTTP 409.
    compose_lock: Arc<Mutex<()>>,
    /// Metadata about the currently-running operation (for 409 messages & /status).
    in_flight: Arc<RwLock<Option<InFlightOp>>>,
}

impl AppState {
    /// Try to acquire the compose lock. Returns `Ok(ComposeGuard)` if the lock was
    /// available, or `Err(ComposeLockBusy)` if another operation is in progress.
    /// The returned `ComposeGuard` clears `in_flight` on Drop.
    async fn try_acquire_compose_lock(
        self: &Arc<Self>,
        action: &str,
        tag: Option<String>,
    ) -> Result<ComposeGuard, ComposeLockBusy> {
        match self.compose_lock.clone().try_lock_owned() {
            Ok(guard) => {
                *self.in_flight.write().await = Some(InFlightOp {
                    action: action.to_string(),
                    started_at: Utc::now().to_rfc3339(),
                    tag: tag.clone(),
                });
                info!(action = action, tag = ?tag, "Acquired compose lock");
                Ok(ComposeGuard {
                    _inner: guard,
                    in_flight: self.in_flight.clone(),
                })
            }
            Err(_) => {
                let in_flight = self.in_flight.read().await.clone();
                error!(action = action, in_flight = ?in_flight, "Compose lock busy — rejecting request");
                Err(ComposeLockBusy)
            }
        }
    }

    /// Build a 409 Conflict error message describing the currently in-flight operation.
    async fn conflict_message(&self) -> String {
        let in_flight = self.in_flight.read().await.clone();
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
}

type ApiResult = (StatusCode, Json<StatusResponse>);

fn ok(tag: Option<String>) -> ApiResult {
    (StatusCode::OK, Json(StatusResponse { status: "ok".into(), tag, commit: None, file: None, file_sha256: None, output: None, exit_code: None, error: None }))
}

fn ok_output(output: String) -> ApiResult {
    (StatusCode::OK, Json(StatusResponse { status: "ok".into(), tag: None, commit: None, file: None, file_sha256: None, output: Some(output), exit_code: None, error: None }))
}

fn ok_systemctl(output: String, exit_code: Option<i32>) -> ApiResult {
    (StatusCode::OK, Json(StatusResponse { status: "ok".into(), tag: None, commit: None, file: None, file_sha256: None, output: Some(output), exit_code, error: None }))
}

fn err(code: StatusCode, msg: impl Into<String>) -> ApiResult {
    (code, Json(StatusResponse { status: "error".into(), tag: None, commit: None, file: None, file_sha256: None, output: None, exit_code: None, error: Some(msg.into()) }))
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
    })
    .unwrap();
    Response::builder()
        .status(code)
        .header("Content-Type", "application/json")
        .body(Body::from(body))
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
}

#[derive(Deserialize)]
struct CleanRequest {
    #[serde(default)]
    volumes: bool,
    #[serde(default)]
    images: bool,
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

// --- NDJSON Streaming ---

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
}

impl Stream for NdjsonStream {
    type Item = Result<String, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if this.done {
            return Poll::Ready(None);
        }

        // Poll stderr first
        if let Some(ref mut stderr) = this.stderr {
            match Pin::new(stderr).poll_next_line(cx) {
                Poll::Ready(Ok(Some(line))) => {
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

                        this.done = true;

                        // Clean up temp env file
                        if let Some(ref path) = this.temp_env_file {
                            let _ = std::fs::remove_file(path);
                        }

                        let event = NdjsonEvent {
                            event: "done".into(),
                            data: None,
                            success: Some(status.success()),
                            exit_code: status.code(),
                        };
                        let mut json = serde_json::to_string(&event).unwrap();
                        json.push('\n');
                        return Poll::Ready(Some(Ok(json)));
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

fn build_compose_cmd(
    work_dir: &Path,
    args: &[&str],
    file: &str,
    env_files: &[String],
    services: &[String],
    temp_env_file: Option<&Path>,
) -> AsyncCommand {
    let mut cmd = AsyncCommand::new("docker");
    cmd.args(["compose", "-f", file]);
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

fn stream_docker_compose_phased(
    work_dir: &Path,
    phases: &[&[&str]],
    file: &str,
    env_files: &[String],
    services: &[String],
    temp_env_file: Option<PathBuf>,
) -> Result<NdjsonStream> {
    let all_env_files: Vec<&str> = env_files.iter().map(|s| s.as_str())
        .chain(temp_env_file.as_ref().map(|p| p.to_str().unwrap()))
        .collect();

    info!(
        command = "docker compose",
        file = file,
        phases = ?phases,
        env_files = ?all_env_files,
        services = ?services,
        work_dir = %work_dir.display(),
        "Running streaming command"
    );

    let mut commands: VecDeque<AsyncCommand> = phases.iter()
        .map(|args| build_compose_cmd(work_dir, args, file, env_files, services, temp_env_file.as_deref()))
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
    })
}

fn stream_docker_compose(
    work_dir: &Path,
    args: &[&str],
    file: &str,
    env_files: &[String],
    services: &[String],
    temp_env_file: Option<PathBuf>,
) -> Result<NdjsonStream> {
    stream_docker_compose_phased(work_dir, &[args], file, env_files, services, temp_env_file)
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

    // Acquire compose lock early (before GitHub fetch) to prevent parallel
    // operations from racing on the same compose file on disk.
    let guard = match state.try_acquire_compose_lock("compose_up", Some(payload.tag.clone())).await {
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

    let file = payload.file.unwrap_or_else(|| "docker-compose.yml".into());

    // Fetch compose file from GitHub and write to work directory
    let content = match fetch_github_file(&state, &payload.tag, &file).await {
        Ok(c) => c,
        Err(e) => return err_response(StatusCode::BAD_REQUEST, e.to_string()),
    };

    let file_sha256 = hex::encode(sha2::Sha256::digest(content.as_bytes()));

    if let Err(e) = tokio::fs::create_dir_all(&state.work_dir).await {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create work dir: {}", e));
    }
    if let Err(e) = tokio::fs::write(state.work_dir.join(&file), &content).await {
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

    let mut up_args = vec!["up", "-d", "--remove-orphans"];
    if payload.force_recreate {
        up_args.push("--force-recreate");
    }

    let mut stream = match stream_docker_compose_phased(
        &state.work_dir,
        &[&["pull", "--ignore-buildable"], &["build"], &up_args],
        &file,
        &state.env_files,
        &payload.services,
        temp_env_file,
    ) {
        Ok(s) => s,
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    stream.compose_guard = Some(guard);

    {
        let mut actions = state.actions.write().await;
        actions.push(DeploymentAction {
            timestamp: Utc::now().to_rfc3339(),
            action: "compose_up".into(),
            tag: Some(payload.tag.clone()),
            commit: Some(tag_info.commit_sha.clone()),
            file: Some(file.clone()),
            file_sha256: Some(file_sha256.clone()),
            services: payload.services,
            container: None,
        });
        if let Err(e) = persist_actions_to_disk(&state.work_dir, &*actions).await {
            error!(error = %e, "Failed to persist action log to disk");
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist action log");
        }
    }
    *state.deployed_tag.write().await = Some(payload.tag);
    *state.deployed_commit.write().await = Some(tag_info.commit_sha);
    *state.deployed_file.write().await = Some(file);
    *state.deployed_file_sha256.write().await = Some(file_sha256);

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

    let guard = match state.try_acquire_compose_lock("compose_down", Some(payload.tag.clone())).await {
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

    let file = payload.file.unwrap_or_else(|| "docker-compose.yml".into());
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

    {
        let mut actions = state.actions.write().await;
        actions.push(DeploymentAction {
            timestamp: Utc::now().to_rfc3339(),
            action: "compose_down".into(),
            tag: Some(payload.tag),
            commit: Some(tag_info.commit_sha),
            file: Some(file),
            file_sha256: None,
            services: payload.services,
            container: None,
        });
        if let Err(e) = persist_actions_to_disk(&state.work_dir, &*actions).await {
            error!(error = %e, "Failed to persist action log to disk");
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist action log");
        }
    }

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

async fn docker_restart(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<RestartRequest>,
) -> impl IntoResponse {
    if let Err(e) = verify_bearer_token(&headers, &state.bearer_token) {
        return e;
    }

    let _guard = match state.try_acquire_compose_lock("docker_restart", None).await {
        Ok(g) => g,
        Err(_) => {
            return err(StatusCode::CONFLICT, state.conflict_message().await);
        }
    };

    info!(command = "docker restart", container = %payload.container, "Running command");

    match run_command("docker", &["restart", &payload.container]) {
        Ok(_) => {
            {
                let mut actions = state.actions.write().await;
                actions.push(DeploymentAction {
                    timestamp: Utc::now().to_rfc3339(),
                    action: "docker_restart".into(),
                    tag: None,
                    commit: None,
                    file: None,
                    file_sha256: None,
                    services: vec![],
                    container: Some(payload.container),
                });
                if let Err(e) = persist_actions_to_disk(&state.work_dir, &*actions).await {
                    error!(error = %e, "Failed to persist action log to disk");
                    return err(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist action log");
                }
            }
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

    if !payload.volumes && !payload.images {
        return err(StatusCode::BAD_REQUEST, "At least one of 'volumes' or 'images' must be true");
    }

    let _guard = match state.try_acquire_compose_lock("docker_clean", None).await {
        Ok(g) => g,
        Err(_) => {
            return err(StatusCode::CONFLICT, state.conflict_message().await);
        }
    };

    match run_docker_prune(payload.volumes, payload.images) {
        Ok(_) => {
            {
                let mut actions = state.actions.write().await;
                actions.push(DeploymentAction {
                    timestamp: Utc::now().to_rfc3339(),
                    action: "docker_clean".into(),
                    tag: None,
                    commit: None,
                    file: None,
                    file_sha256: None,
                    services: vec![],
                    container: None,
                });
                if let Err(e) = persist_actions_to_disk(&state.work_dir, &*actions).await {
                    error!(error = %e, "Failed to persist action log to disk");
                    return err(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist action log");
                }
            }
            ok(None)
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
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

    let actions_json = serde_json::to_string(&actions).unwrap();
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
    let tag = state.deployed_tag.read().await.clone();
    let commit = state.deployed_commit.read().await.clone();
    let file = state.deployed_file.read().await.clone();
    let file_sha256 = state.deployed_file_sha256.read().await.clone();
    (StatusCode::OK, Json(StatusResponse { status: "ok".into(), tag, commit, file, file_sha256, output: None, exit_code: None, error: None }))
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
    let action_name = match &result {
        Ok(r) if r.success => "kernel_algif_blacklist_ok",
        Ok(_) => "kernel_algif_blacklist_script_failed",
        Err(_) => "kernel_algif_blacklist_invocation_failed",
    };
    {
        let mut actions = state.actions.write().await;
        actions.push(DeploymentAction {
            timestamp: Utc::now().to_rfc3339(),
            action: action_name.into(),
            tag: None,
            commit: None,
            file: None,
            file_sha256: None,
            services: vec![],
            container: None,
        });
        if let Err(e) = persist_actions_to_disk(&state.work_dir, &actions).await {
            error!(error = %e, "Failed to persist action log to disk");
        }
    }

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
    {
        let mut actions = state.actions.write().await;
        actions.push(DeploymentAction {
            timestamp: Utc::now().to_rfc3339(),
            action: format!("dstack_agent_{}", action),
            tag: None,
            commit: None,
            file: None,
            file_sha256: None,
            services: vec![],
            container: None,
        });
        if let Err(e) = persist_actions_to_disk(&state.work_dir, &*actions).await {
            error!(error = %e, "Failed to persist action log to disk");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist action log");
        }
    }

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

fn run_docker_compose(work_dir: &Path, args: &[&str], file: &str, env_files: &[String], services: &[String]) -> Result<String> {
    info!(command = "docker compose", file = file, args = ?args, env_files = ?env_files, services = ?services, work_dir = %work_dir.display(), "Running command");
    let mut cmd = Command::new("docker");
    cmd.args(["compose", "-f", file]);
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

fn run_docker_prune(volumes: bool, images: bool) -> Result<String> {
    let mut output_text = String::new();

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
    tracing_subscriber::fmt::init();

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

    let state = Arc::new(AppState {
        bearer_token,
        github_owner,
        github_repo_name,
        min_tag_age_hours,
        work_dir,
        env_files,
        deployed_tag: RwLock::new(None),
        deployed_commit: RwLock::new(None),
        deployed_file: RwLock::new(None),
        deployed_file_sha256: RwLock::new(None),
        actions: RwLock::new(initial_actions),
        http: reqwest::Client::new(),
        compose_lock: Arc::new(Mutex::new(())),
        in_flight: Arc::new(RwLock::new(None)),
    });

    let app = Router::new()
        .route("/compose/up", post(compose_up))
        .route("/compose/down", post(compose_down))
        .route("/compose/logs", post(compose_logs))
        .route("/docker/clean", post(docker_clean))
        .route("/docker/ps", get(docker_ps))
        .route("/docker/restart", post(docker_restart))
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
            deployed_tag: RwLock::new(None),
            deployed_commit: RwLock::new(None),
            deployed_file: RwLock::new(None),
            deployed_file_sha256: RwLock::new(None),
            actions: RwLock::new(vec![]),
            http: reqwest::Client::new(),
            compose_lock: Arc::new(Mutex::new(())),
            in_flight: Arc::new(RwLock::new(None)),
        })
    }

    #[tokio::test]
    async fn compose_lock_acquire_succeeds_when_free() {
        let state = make_test_state();
        let result = state.try_acquire_compose_lock("compose_up", Some("v1".into())).await;
        assert!(result.is_ok(), "lock should be acquirable when no one holds it");
    }

    #[tokio::test]
    async fn compose_lock_rejects_concurrent_request() {
        let state = make_test_state();
        let _guard = state.try_acquire_compose_lock("compose_up", Some("v1".into())).await.unwrap();
        // While the first guard is held, a second acquire must fail.
        let result = state.try_acquire_compose_lock("compose_down", Some("v2".into())).await;
        assert!(result.is_err(), "lock should reject when already held");
    }

    #[tokio::test]
    async fn compose_lock_releases_on_drop() {
        let state = make_test_state();
        {
            let _guard = state.try_acquire_compose_lock("compose_up", Some("v1".into())).await.unwrap();
        } // guard dropped here
        // After dropping, the lock should be available again.
        let result = state.try_acquire_compose_lock("compose_down", Some("v2".into())).await;
        assert!(result.is_ok(), "lock should be available after guard is dropped");
    }

    #[tokio::test]
    async fn in_flight_cleared_on_drop() {
        let state = make_test_state();
        {
            let _guard = state.try_acquire_compose_lock("compose_up", Some("v1".into())).await.unwrap();
            let in_flight = state.in_flight.read().await.clone();
            assert!(in_flight.is_some(), "in_flight should be set while lock is held");
            assert_eq!(in_flight.unwrap().action, "compose_up");
        }
        // After dropping the ComposeGuard, in_flight must be cleared
        // (ComposeGuard::Drop clears it).
        let in_flight = state.in_flight.read().await.clone();
        assert!(in_flight.is_none(), "in_flight should be cleared after ComposeGuard drops");
    }

    #[tokio::test]
    async fn in_flight_reflects_current_action() {
        let state = make_test_state();
        let _guard = state.try_acquire_compose_lock("docker_clean", None).await.unwrap();
        let in_flight = state.in_flight.read().await.clone().unwrap();
        assert_eq!(in_flight.action, "docker_clean");
        assert_eq!(in_flight.tag, None);
    }
}
