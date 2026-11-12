use crate::{AppState, ProjectDeployed};
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone)]
struct InventoryState {
    app: Arc<AppState>,
    docker: reqwest::Client,
    scrape: Arc<Mutex<()>>,
}

#[derive(Deserialize)]
struct ComposeLabels {
    #[serde(rename = "com.docker.compose.project")]
    project: Option<String>,
    #[serde(rename = "com.docker.compose.service", default)]
    service: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Container {
    #[serde(default)]
    labels: Option<ComposeLabels>,
    #[serde(default)]
    names: Vec<String>,
    id: Option<String>,
    image: Option<String>,
    #[serde(rename = "ImageID")]
    image_id: Option<String>,
    state: Option<String>,
    #[serde(default)]
    status: String,
}

#[derive(Default)]
struct DockerInventory {
    containers: Vec<Container>,
    complete: bool,
}

pub(super) async fn start(app: Arc<AppState>) {
    let Ok(address) = std::env::var("COMPOSE_MANAGER_METRICS_ADDR") else {
        return;
    };
    if address.is_empty() {
        return;
    }
    let Ok(address) = address.parse::<SocketAddr>() else {
        tracing::warn!("Inventory metrics disabled because the listen address is invalid");
        return;
    };
    let docker_host = std::env::var("DOCKER_HOST").ok();
    let socket = docker_host
        .as_deref()
        .and_then(|host| host.strip_prefix("unix://"))
        .unwrap_or("/var/run/docker.sock");
    let Ok(docker) = docker_client(socket) else {
        tracing::warn!("Inventory metrics disabled because the Docker client could not start");
        return;
    };
    let Ok(listener) = tokio::net::TcpListener::bind(address).await else {
        tracing::warn!("Inventory metrics disabled because the listener could not start");
        return;
    };
    let router = Router::new()
        .route("/metrics", get(metrics))
        .with_state(InventoryState {
            app,
            docker,
            scrape: Arc::new(Mutex::new(())),
        });
    tokio::spawn(async move {
        if axum::serve(listener, router).await.is_err() {
            tracing::warn!("Inventory metrics listener stopped");
        }
    });
}

fn docker_client(socket: &str) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .unix_socket(socket)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(4))
        .build()
}

async fn metrics(State(state): State<InventoryState>) -> impl IntoResponse {
    let Ok(_scrape) = state.scrape.try_lock() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let docker = read_docker(&state.docker).await.unwrap_or_default();
    let projects = state
        .app
        .deployed_projects
        .read()
        .unwrap_or_else(|err| err.into_inner())
        .clone();
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        render(&projects, &docker),
    )
        .into_response()
}

async fn read_docker(client: &reqwest::Client) -> Option<DockerInventory> {
    let mut response = client
        .get("http://localhost/v1.44/containers/json?all=1")
        .send()
        .await
        .ok()?;
    if response.status() != StatusCode::OK {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > RESPONSE_LIMIT - body.len() {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    parse_inventory(&body)
}

fn parse_inventory(body: &[u8]) -> Option<DockerInventory> {
    let records: Vec<serde_json::Value> = serde_json::from_slice(body).ok()?;
    let mut inventory = DockerInventory {
        complete: true,
        ..Default::default()
    };
    for record in records {
        let Ok(container) = serde_json::from_value::<Container>(record) else {
            inventory.complete = false;
            continue;
        };
        if container
            .labels
            .as_ref()
            .and_then(|labels| labels.project.as_deref())
            .is_none()
        {
            continue;
        }
        if container.labels().is_some() {
            inventory.containers.push(container);
        } else {
            inventory.complete = false;
        }
    }
    Some(inventory)
}

impl Container {
    fn labels(&self) -> Option<[(&'static str, &str); 8]> {
        let compose = self.labels.as_ref()?;
        let health = match self.status.rsplit_once(" (").map(|(_, suffix)| suffix) {
            Some("healthy)") => "healthy",
            Some("unhealthy)") => "unhealthy",
            Some("health: starting)") => "starting",
            _ => "",
        };
        Some([
            ("compose_project", compose.project.as_deref()?),
            ("compose_service", &compose.service),
            (
                "container_name",
                self.names.first()?.trim_start_matches('/'),
            ),
            ("container_id", self.id.as_deref()?.get(..12)?),
            ("image", self.image.as_deref()?),
            ("image_id", self.image_id.as_deref()?),
            ("state", self.state.as_deref()?),
            ("health", health),
        ])
    }
}

fn render(deployments: &BTreeMap<String, ProjectDeployed>, docker: &DockerInventory) -> String {
    let mut output = String::from("# TYPE compose_manager_inventory_source_up gauge\n");
    sample(
        &mut output,
        "compose_manager_inventory_source_up",
        [("source", "Docker inventory")],
        docker.complete,
    );
    sample(
        &mut output,
        "compose_manager_inventory_source_up",
        [("source", "Deployment record")],
        !deployments.is_empty(),
    );

    let mut runtimes = BTreeMap::<&str, bool>::new();
    for container in &docker.containers {
        if let Some(project) = container
            .labels
            .as_ref()
            .and_then(|labels| labels.project.as_deref())
        {
            *runtimes.entry(project).or_default() |= container.state.as_deref() == Some("running");
        }
    }
    let projects: BTreeSet<_> = runtimes
        .keys()
        .copied()
        .chain(deployments.keys().map(String::as_str))
        .collect();
    output.push_str("# TYPE compose_manager_project_info gauge\n");
    for project in projects {
        let runtime = match runtimes.get(project) {
            Some(true) => "active",
            Some(false) if docker.complete => "inactive",
            None if docker.complete => "no containers",
            _ => "unavailable",
        };
        let deployment = deployments
            .get(project)
            .map(|deployment| &deployment.current);
        sample(
            &mut output,
            "compose_manager_project_info",
            [
                ("compose_project", project),
                (
                    "compose_file",
                    deployment.and_then(|d| d.file.as_deref()).unwrap_or(""),
                ),
                (
                    "compose_tag",
                    deployment.and_then(|d| d.tag.as_deref()).unwrap_or(""),
                ),
                (
                    "compose_commit",
                    deployment.and_then(|d| d.commit.as_deref()).unwrap_or(""),
                ),
                ("runtime", runtime),
            ],
            true,
        );
    }
    output.push_str("# TYPE compose_manager_container_info gauge\n");
    for container in &docker.containers {
        if let Some(labels) = container.labels() {
            sample(&mut output, "compose_manager_container_info", labels, true);
        }
    }
    output
}

fn sample<const N: usize>(output: &mut String, name: &str, labels: [(&str, &str); N], value: bool) {
    output.push_str(name);
    output.push('{');
    for (index, (key, label)) in labels.into_iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str(key);
        output.push_str("=\"");
        for character in label.chars() {
            match character {
                '\\' => output.push_str("\\\\"),
                '"' => output.push_str("\\\""),
                '\n' => output.push_str("\\n"),
                _ => output.push(character),
            }
        }
        output.push('"');
    }
    output.push_str(if value { "} 1\n" } else { "} 0\n" });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeployedRecord;
    use serde_json::json;

    #[test]
    fn keeps_good_container_rows_when_one_record_is_invalid() {
        let records = json!([
            {
                "Labels": {
                    "com.docker.compose.project": "application",
                    "com.docker.compose.service": "worker",
                    "unrelated": "omit-label",
                },
                "Names": ["/worker"],
                "Id": "a".repeat(64),
                "Image": "example/worker:1",
                "ImageID": "sha256:1234",
                "State": "running",
                "Status": "Up 2 seconds (healthy)",
                "Command": "omit-command",
            },
            {"Labels": {"com.docker.compose.project": "application"}},
            {"Labels": {}},
        ]);
        let docker = parse_inventory(&serde_json::to_vec(&records).unwrap()).unwrap();
        let deployments = BTreeMap::from([(
            "application".into(),
            ProjectDeployed {
                current: DeployedRecord {
                    tag: Some("v2".into()),
                    ..Default::default()
                },
                previous: Some(DeployedRecord {
                    tag: Some("v1".into()),
                    ..Default::default()
                }),
            },
        )]);
        let output = render(&deployments, &docker);
        assert!(output.contains(r#"source="Docker inventory"} 0"#));
        assert!(output.contains(r#"source="Deployment record"} 1"#));
        assert!(output.contains(r#"compose_tag="v2",compose_commit="",runtime="active""#));
        assert!(output.contains(r#"container_id="aaaaaaaaaaaa""#));
        assert!(output.contains(r#"image_id="sha256:1234",state="running",health="healthy""#));
        assert!(!output.contains("v1"));
        assert!(!output.contains("omit-"));
    }

    #[test]
    fn distinguishes_missing_deployments_from_empty_docker_inventory() {
        let docker = parse_inventory(b"[]").unwrap();
        let output = render(&BTreeMap::new(), &docker);
        assert!(output.contains(r#"source="Docker inventory"} 1"#));
        assert!(output.contains(r#"source="Deployment record"} 0"#));
    }

    #[tokio::test]
    async fn reads_all_container_summaries_over_the_docker_socket() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let socket =
            std::env::temp_dir().join(format!("inventory-{:016x}.sock", rand::random::<u64>()));
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut first_line = String::new();
            stream.read_line(&mut first_line).await.unwrap();
            assert_eq!(
                first_line.trim_end(),
                "GET /v1.44/containers/json?all=1 HTTP/1.1"
            );
            loop {
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                if line.trim_end().is_empty() {
                    break;
                }
            }
            stream
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]")
                .await
                .unwrap();
        });
        let client = docker_client(socket.to_str().unwrap()).unwrap();
        let result = read_docker(&client).await;
        std::fs::remove_file(socket).unwrap();
        server.await.unwrap();
        assert!(result.unwrap().complete);
    }

    #[test]
    fn escapes_prometheus_label_values() {
        let mut output = String::new();
        sample(&mut output, "info", [("name", "a\\b\"c\nd")], true);
        assert_eq!(output, "info{name=\"a\\\\b\\\"c\\nd\"} 1\n");
    }
}
