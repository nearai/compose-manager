use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};
use tokio::sync::RwLock;
use tracing::{error, info};

// --- Types ---

#[derive(Serialize, Deserialize, Clone)]
struct Instance {
    id: String,
    name: String,
    url: String,
    bearer_token: String,
    github_repo: String,
    #[serde(default)]
    env_vars: HashMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct Config {
    instances: Vec<Instance>,
}

struct CacheEntry {
    value: serde_json::Value,
    fetched_at: Instant,
}

const GITHUB_CACHE_TTL_SECS: u64 = 300; // 5 minutes

struct AppState {
    dashboard_token: String,
    config_path: PathBuf,
    instances: RwLock<Vec<Instance>>,
    http: reqwest::Client,
    github_cache: RwLock<HashMap<String, CacheEntry>>,
}

#[derive(Serialize)]
struct ErrorResponse {
    status: String,
    error: String,
}

#[derive(Deserialize)]
struct AddInstanceRequest {
    name: String,
    url: String,
    bearer_token: String,
    github_repo: String,
}

#[derive(Deserialize)]
struct GithubQuery {
    repo: String,
}

#[derive(Deserialize)]
struct GithubFilesQuery {
    repo: String,
    tag: String,
}

#[derive(Deserialize)]
struct GitTreeResponse {
    tree: Vec<GitTreeEntry>,
}

#[derive(Deserialize)]
struct GitTreeEntry {
    path: String,
    #[serde(rename = "type")]
    type_: String,
}

// --- Helpers ---

fn err_json(code: StatusCode, msg: impl Into<String>) -> Response {
    let body = serde_json::to_string(&ErrorResponse {
        status: "error".into(),
        error: msg.into(),
    })
    .unwrap();
    Response::builder()
        .status(code)
        .header("Content-Type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

fn verify_bearer_token(headers: &HeaderMap, expected: &str) -> Result<(), Response> {
    let token = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| err_json(StatusCode::UNAUTHORIZED, "Missing or invalid Authorization header"))?;

    if token != expected {
        return Err(err_json(StatusCode::UNAUTHORIZED, "Invalid token"));
    }
    Ok(())
}

fn parse_repo_param(repo: &str) -> Result<(String, String), Response> {
    // Accepts "owner/repo" or full URL "https://github.com/owner/repo"
    let repo = repo
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() < 2 {
        return Err(err_json(StatusCode::BAD_REQUEST, "Invalid repo format, expected owner/repo"));
    }
    Ok((
        parts[parts.len() - 2].to_string(),
        parts[parts.len() - 1].to_string(),
    ))
}

fn load_config(path: &std::path::Path) -> Result<Config> {
    let data = std::fs::read_to_string(path).context("Failed to read config file")?;
    serde_json::from_str(&data).context("Failed to parse config file")
}

fn save_config(path: &std::path::Path, config: &Config) -> Result<()> {
    let data = serde_json::to_string_pretty(config).context("Failed to serialize config")?;
    std::fs::write(path, data).context("Failed to write config file")
}

/// Build a proxied request to a compose-manager instance
async fn proxy_json(
    state: &AppState,
    instance_id: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
    timeout_secs: Option<u64>,
) -> Response {
    let instances = state.instances.read().await;
    let instance = match instances.iter().find(|i| i.id == instance_id) {
        Some(i) => i.clone(),
        None => return err_json(StatusCode::NOT_FOUND, "Instance not found"),
    };
    drop(instances);

    let url = format!("{}/{}", instance.url.trim_end_matches('/'), path);
    let mut req = state
        .http
        .request(method, &url)
        .header("Authorization", format!("Bearer {}", instance.bearer_token))
        // Attribute the action to the dashboard in compose-manager's Slack
        // notifications (nearai/infra#141). The dashboard uses a single shared
        // token, so this is the finest attribution available without per-user
        // auth.
        .header("X-Triggered-By", "dashboard")
        .timeout(std::time::Duration::from_secs(timeout_secs.unwrap_or(30)));

    if let Some(body) = body {
        req = req
            .header("Content-Type", "application/json")
            .json(&body);
    }

    match req.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let ct = resp
                .headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            let body_bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    return err_json(
                        StatusCode::BAD_GATEWAY,
                        format!("Failed to read response: {}", e),
                    )
                }
            };
            Response::builder()
                .status(status)
                .header("Content-Type", ct)
                .body(Body::from(body_bytes))
                .unwrap()
        }
        Err(e) => err_json(
            StatusCode::BAD_GATEWAY,
            format!("Failed to reach instance: {}", e),
        ),
    }
}

/// Proxy a streaming NDJSON request (compose up/down)
async fn proxy_stream(
    state: &AppState,
    instance_id: &str,
    path: &str,
    body: serde_json::Value,
) -> Response {
    let instances = state.instances.read().await;
    let instance = match instances.iter().find(|i| i.id == instance_id) {
        Some(i) => i.clone(),
        None => return err_json(StatusCode::NOT_FOUND, "Instance not found"),
    };
    drop(instances);

    let url = format!("{}/{}", instance.url.trim_end_matches('/'), path);
    let resp = match state
        .http
        .post(&url)
        .header("Authorization", format!("Bearer {}", instance.bearer_token))
        .header("X-Triggered-By", "dashboard")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return err_json(
                StatusCode::BAD_GATEWAY,
                format!("Failed to reach instance: {}", e),
            )
        }
    };

    if !resp.status().is_success() {
        let status =
            StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let body = resp.text().await.unwrap_or_default();
        return Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
    }

    // Stream NDJSON through
    let stream = resp.bytes_stream();
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/x-ndjson")
        .body(Body::from_stream(stream))
        .unwrap()
}

// --- Handlers ---

async fn serve_index() -> Html<&'static str> {
    Html(include_str!("../index.html"))
}

async fn list_instances(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }

    let instances = state.instances.read().await;
    let redacted: Vec<Instance> = instances
        .iter()
        .map(|i| Instance {
            id: i.id.clone(),
            name: i.name.clone(),
            url: i.url.clone(),
            bearer_token: "***".into(),
            github_repo: i.github_repo.clone(),
            env_vars: i.env_vars.clone(),
        })
        .collect();

    Json(redacted).into_response()
}

async fn add_instance(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<AddInstanceRequest>,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }

    let instance = Instance {
        id: uuid::Uuid::new_v4().to_string(),
        name: payload.name,
        url: payload.url.trim_end_matches('/').to_string(),
        bearer_token: payload.bearer_token,
        github_repo: payload.github_repo,
        env_vars: HashMap::new(),
    };

    let mut instances = state.instances.write().await;
    instances.push(instance.clone());

    let config = Config {
        instances: instances.clone(),
    };
    if let Err(e) = save_config(&state.config_path, &config) {
        error!("Failed to save config: {}", e);
    }

    let redacted = Instance {
        bearer_token: "***".into(),
        ..instance
    };

    (StatusCode::CREATED, Json(redacted)).into_response()
}

async fn remove_instance(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }

    let mut instances = state.instances.write().await;
    let len_before = instances.len();
    instances.retain(|i| i.id != id);

    if instances.len() == len_before {
        return err_json(StatusCode::NOT_FOUND, "Instance not found");
    }

    let config = Config {
        instances: instances.clone(),
    };
    if let Err(e) = save_config(&state.config_path, &config) {
        error!("Failed to save config: {}", e);
    }

    StatusCode::NO_CONTENT.into_response()
}

async fn update_instance_env(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(env_vars): Json<HashMap<String, String>>,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }

    let mut instances = state.instances.write().await;
    let instance = match instances.iter_mut().find(|i| i.id == id) {
        Some(i) => i,
        None => return err_json(StatusCode::NOT_FOUND, "Instance not found"),
    };

    instance.env_vars = env_vars;

    let config = Config {
        instances: instances.clone(),
    };
    if let Err(e) = save_config(&state.config_path, &config) {
        error!("Failed to save config: {}", e);
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to save config");
    }

    Json(serde_json::json!({"status": "ok"})).into_response()
}

// --- Proxy handlers ---

async fn proxy_compose_up(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }
    proxy_stream(&state, &id, "compose/up", body).await
}

async fn proxy_compose_down(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }
    proxy_stream(&state, &id, "compose/down", body).await
}

async fn proxy_compose_logs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }
    proxy_json(&state, &id, reqwest::Method::POST, "compose/logs", Some(body), None).await
}

async fn proxy_docker_ps(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }
    proxy_json(&state, &id, reqwest::Method::GET, "docker/ps", None, None).await
}

async fn proxy_docker_restart(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }
    proxy_json(&state, &id, reqwest::Method::POST, "docker/restart", Some(body), None).await
}

async fn proxy_docker_clean(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }
    proxy_json(&state, &id, reqwest::Method::POST, "docker/clean", Some(body), None).await
}

async fn proxy_version(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }
    proxy_json(&state, &id, reqwest::Method::GET, "version", None, Some(5)).await
}

async fn proxy_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }
    proxy_json(&state, &id, reqwest::Method::GET, "status", None, Some(5)).await
}

// --- GitHub API ---

async fn github_tags(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<GithubQuery>,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }

    let (owner, name) = match parse_repo_param(&params.repo) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let cache_key = format!("tags:{}/{}", owner, name);
    let ttl = std::time::Duration::from_secs(GITHUB_CACHE_TTL_SECS);

    // Check cache
    {
        let cache = state.github_cache.read().await;
        if let Some(entry) = cache.get(&cache_key) {
            if entry.fetched_at.elapsed() < ttl {
                return Json(entry.value.clone()).into_response();
            }
        }
    }

    let url = format!(
        "https://api.github.com/repos/{}/{}/tags?per_page=100",
        owner, name
    );

    match state
        .http
        .get(&url)
        .header("User-Agent", "compose-manager-dashboard")
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                return err_json(
                    StatusCode::BAD_GATEWAY,
                    format!("GitHub API error: {}", resp.status()),
                );
            }
            match resp.json::<serde_json::Value>().await {
                Ok(tags) => {
                    let mut cache = state.github_cache.write().await;
                    cache.insert(cache_key, CacheEntry { value: tags.clone(), fetched_at: Instant::now() });
                    Json(tags).into_response()
                }
                Err(e) => err_json(StatusCode::BAD_GATEWAY, format!("Failed to parse GitHub response: {}", e)),
            }
        }
        Err(e) => err_json(StatusCode::BAD_GATEWAY, format!("GitHub API request failed: {}", e)),
    }
}

async fn github_files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<GithubFilesQuery>,
) -> Response {
    if let Err(e) = verify_bearer_token(&headers, &state.dashboard_token) {
        return e;
    }

    let (owner, name) = match parse_repo_param(&params.repo) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let cache_key = format!("files:{}/{}/{}", owner, name, params.tag);
    let ttl = std::time::Duration::from_secs(GITHUB_CACHE_TTL_SECS);

    // Check cache
    {
        let cache = state.github_cache.read().await;
        if let Some(entry) = cache.get(&cache_key) {
            if entry.fetched_at.elapsed() < ttl {
                return Json(entry.value.clone()).into_response();
            }
        }
    }

    let url = format!(
        "https://api.github.com/repos/{}/{}/git/trees/{}?recursive=1",
        owner, name, params.tag
    );

    match state
        .http
        .get(&url)
        .header("User-Agent", "compose-manager-dashboard")
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                return err_json(
                    StatusCode::BAD_GATEWAY,
                    format!("GitHub API error: {}", resp.status()),
                );
            }
            match resp.json::<GitTreeResponse>().await {
                Ok(tree) => {
                    let files: Vec<&str> = tree
                        .tree
                        .iter()
                        .filter(|e| e.type_ == "blob")
                        .map(|e| e.path.as_str())
                        .filter(|p| p.ends_with(".yml") || p.ends_with(".yaml"))
                        .collect();
                    let value = serde_json::to_value(&files).unwrap();
                    let mut cache = state.github_cache.write().await;
                    cache.insert(cache_key, CacheEntry { value: value.clone(), fetched_at: Instant::now() });
                    Json(value).into_response()
                }
                Err(e) => err_json(
                    StatusCode::BAD_GATEWAY,
                    format!("Failed to parse GitHub tree: {}", e),
                ),
            }
        }
        Err(e) => err_json(StatusCode::BAD_GATEWAY, format!("GitHub API request failed: {}", e)),
    }
}

// --- Main ---

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

    let dashboard_token = std::env::var("DASHBOARD_TOKEN")
        .context("DASHBOARD_TOKEN environment variable is required")?;
    let config_path = PathBuf::from(
        std::env::var("CONFIG_FILE").unwrap_or_else(|_| "dashboard.json".to_string()),
    );
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .context("PORT must be a valid number")?;

    let config = if config_path.exists() {
        load_config(&config_path)?
    } else {
        let config = Config {
            instances: vec![],
        };
        save_config(&config_path, &config)?;
        config
    };

    info!(
        instances = config.instances.len(),
        config = %config_path.display(),
        "Loaded configuration"
    );

    let state = Arc::new(AppState {
        dashboard_token,
        config_path,
        instances: RwLock::new(config.instances),
        http: reqwest::Client::new(),
        github_cache: RwLock::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/", get(serve_index))
        .route("/api/instances", get(list_instances))
        .route("/api/instances", post(add_instance))
        .route("/api/instances/:id", delete(remove_instance))
        .route("/api/instances/:id/env", axum::routing::put(update_instance_env))
        .route("/api/instances/:id/compose/up", post(proxy_compose_up))
        .route(
            "/api/instances/:id/compose/down",
            post(proxy_compose_down),
        )
        .route(
            "/api/instances/:id/compose/logs",
            post(proxy_compose_logs),
        )
        .route("/api/instances/:id/docker/ps", get(proxy_docker_ps))
        .route(
            "/api/instances/:id/docker/restart",
            post(proxy_docker_restart),
        )
        .route(
            "/api/instances/:id/docker/clean",
            post(proxy_docker_clean),
        )
        .route("/api/instances/:id/version", get(proxy_version))
        .route("/api/instances/:id/status", get(proxy_status))
        .route("/api/github/tags", get(github_tags))
        .route("/api/github/files", get(github_files))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    info!("Dashboard listening on port {}", port);
    axum::serve(listener, app).await?;

    Ok(())
}
