//! TLS on the API port.
//!
//! compose-manager keeps listening on a single port and serves both TLS and
//! plain HTTP there: the first byte of each connection decides (a TLS
//! ClientHello always starts with 0x16, an HTTP request never does). Plain
//! HTTP stays available because in-CVM callers that cannot be reconfigured
//! without a CVM redeploy use it (the launcher's `LAUNCHER_HEALTH_URL`, the
//! attestation fetch from the model proxies). Remote clients should use TLS.
//!
//! The certificate is the one the CVM already maintains for itself (certbot
//! in the app compose writes it to the `certs` volume). By default it is
//! copied out of the certbot container with `docker cp -L`, so no extra
//! volume mount is needed, and re-read periodically to pick up renewals. If no
//! certificate can be loaded, the port serves plain HTTP only and loading is
//! retried.
//!
//! Configuration:
//! - `TLS_MODE`: `container` (default), `files`, or `off`.
//! - `container` mode: `TLS_CERT_SERVICE` (`<compose project>/<service>`,
//!   default `dstack/certbot`) and `TLS_CERT_DIR` (default
//!   `/etc/letsencrypt/live/completions.near.ai`, which must contain
//!   `fullchain.pem` and `privkey.pem`).
//! - `files` mode: `TLS_CERT_FILE` and `TLS_KEY_FILE` (PEM).

use anyhow::{anyhow, bail, Context, Result};
use axum::Router;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto,
};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;
use tracing::{error, info, warn};

const TLS_HANDSHAKE_RECORD: u8 = 0x16;
/// How long a new connection may stay silent before its first byte arrives.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(10);
/// Re-read interval once a certificate is loaded (certbot renews ~30 days
/// before expiry, so a few hours is plenty).
const RELOAD_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Retry interval while no certificate could be loaded yet (on a fresh CVM,
/// certbot issues the certificate shortly after compose-manager starts).
const RETRY_INTERVAL: Duration = Duration::from_secs(60);

const DEFAULT_CERT_SERVICE: &str = "dstack/certbot";
const DEFAULT_CERT_DIR: &str = "/etc/letsencrypt/live/completions.near.ai";

/// Where the certificate comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// Copy `fullchain.pem` / `privkey.pem` out of a compose service's container.
    Container {
        project: String,
        service: String,
        dir: String,
    },
    /// Read PEM files from this container's filesystem.
    Files { cert: PathBuf, key: PathBuf },
    /// Plain HTTP only.
    Off,
}

impl Source {
    pub fn from_env() -> Result<Self> {
        Self::from_vars(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
    }

    fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mode = var("TLS_MODE").unwrap_or_else(|| "container".into());
        match mode.trim() {
            "container" => {
                let spec = var("TLS_CERT_SERVICE").unwrap_or_else(|| DEFAULT_CERT_SERVICE.into());
                let (project, service) = spec
                    .trim()
                    .split_once('/')
                    .filter(|(p, s)| !p.is_empty() && !s.is_empty())
                    .ok_or_else(|| {
                        anyhow!("TLS_CERT_SERVICE must be <project>/<service>, got {spec:?}")
                    })?;
                Ok(Source::Container {
                    project: project.to_string(),
                    service: service.to_string(),
                    dir: var("TLS_CERT_DIR")
                        .unwrap_or_else(|| DEFAULT_CERT_DIR.into())
                        .trim_end_matches('/')
                        .to_string(),
                })
            }
            "files" => Ok(Source::Files {
                cert: var("TLS_CERT_FILE")
                    .context("TLS_MODE=files requires TLS_CERT_FILE")?
                    .into(),
                key: var("TLS_KEY_FILE")
                    .context("TLS_MODE=files requires TLS_KEY_FILE")?
                    .into(),
            }),
            "off" => Ok(Source::Off),
            other => bail!("TLS_MODE must be container, files or off, got {other:?}"),
        }
    }
}

/// Build a server config from a PEM certificate chain and private key.
pub fn server_config_from_pem(
    chain_pem: &[u8],
    key_pem: &[u8],
) -> Result<Arc<rustls::ServerConfig>> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow!("invalid certificate PEM: {e:?}"))?;
    if chain.is_empty() {
        bail!("certificate PEM contains no certificate");
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|e| anyhow!("invalid private key PEM: {e:?}"))?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(chain, key)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// The acceptor currently in use; `None` means plain HTTP only.
#[derive(Clone, Default)]
pub struct SharedAcceptor(Arc<RwLock<Option<TlsAcceptor>>>);

impl SharedAcceptor {
    pub fn set(&self, config: Arc<rustls::ServerConfig>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Some(TlsAcceptor::from(config));
    }

    pub fn get(&self) -> Option<TlsAcceptor> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn is_loaded(&self) -> bool {
        self.get().is_some()
    }
}

async fn load(source: &Source) -> Result<Arc<rustls::ServerConfig>> {
    let (chain, key) = match source {
        Source::Off => bail!("TLS is off"),
        Source::Files { cert, key } => (
            tokio::fs::read(cert)
                .await
                .with_context(|| format!("reading {}", cert.display()))?,
            tokio::fs::read(key)
                .await
                .with_context(|| format!("reading {}", key.display()))?,
        ),
        Source::Container {
            project,
            service,
            dir,
        } => copy_from_container(project, service, dir).await?,
    };
    server_config_from_pem(&chain, &key)
}

/// Copy `fullchain.pem` and `privkey.pem` out of the container of a compose
/// service. `-L` matters: certbot's `live/` entries are relative symlinks into
/// `archive/`, and without it `docker cp` copies a dangling link.
async fn copy_from_container(
    project: &str,
    service: &str,
    dir: &str,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let ps = run_docker(&[
        "ps",
        "-aq",
        "--filter",
        &format!("label=com.docker.compose.project={project}"),
        "--filter",
        &format!("label=com.docker.compose.service={service}"),
    ])
    .await?;
    let id = first_container_id(&ps)
        .ok_or_else(|| anyhow!("no container for compose service {project}/{service}"))?;

    let tmp = scratch_dir()?;
    let result = async {
        let mut out = Vec::with_capacity(2);
        for name in ["fullchain.pem", "privkey.pem"] {
            let dest = tmp.join(name);
            let dest_str = dest.to_str().context("non-UTF-8 temp path")?;
            run_docker(&["cp", "-L", &format!("{id}:{dir}/{name}"), dest_str]).await?;
            out.push(tokio::fs::read(&dest).await?);
        }
        let key = out.pop().unwrap_or_default();
        let chain = out.pop().unwrap_or_default();
        Ok((chain, key))
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    result
}

fn first_container_id(ps_output: &str) -> Option<String> {
    ps_output
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// A fresh private (0700) directory for the copied key material.
fn scratch_dir() -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = std::env::temp_dir().join(format!(
        "compose-manager-tls-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

async fn run_docker(args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new("docker")
        .args(args)
        .output()
        .await
        .context("running docker")?;
    if !out.status.success() {
        bail!(
            "docker {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Load the certificate once, then keep re-reading it in the background.
/// Never fails: without a certificate the port serves plain HTTP only.
pub async fn start(source: Source) -> SharedAcceptor {
    let shared = SharedAcceptor::default();
    if source == Source::Off {
        info!("TLS disabled (TLS_MODE=off); serving plain HTTP only");
        return shared;
    }
    match load(&source).await {
        Ok(config) => {
            shared.set(config);
            info!("TLS certificate loaded");
        }
        Err(e) => {
            error!(error = %format!("{e:#}"), "TLS certificate not loaded; serving plain HTTP only until it is")
        }
    }
    let bg = shared.clone();
    tokio::spawn(async move {
        loop {
            let wait = if bg.is_loaded() {
                RELOAD_INTERVAL
            } else {
                RETRY_INTERVAL
            };
            tokio::time::sleep(wait).await;
            match load(&source).await {
                Ok(config) => {
                    let first = !bg.is_loaded();
                    bg.set(config);
                    if first {
                        info!("TLS certificate loaded");
                    }
                }
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "TLS certificate reload failed; keeping the current one")
                }
            }
        }
    });
    shared
}

/// Serve `app` on `listener`, TLS or plain HTTP per connection.
pub async fn serve(listener: TcpListener, app: Router, acceptor: SharedAcceptor) -> Result<()> {
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                // Transient (e.g. EMFILE): back off briefly instead of exiting.
                warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let app = app.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(tcp, peer, app, acceptor).await {
                tracing::debug!(%peer, error = %e, "connection ended with error");
            }
        });
    }
}

async fn handle(
    tcp: TcpStream,
    peer: std::net::SocketAddr,
    app: Router,
    acceptor: SharedAcceptor,
) -> Result<()> {
    let mut first = [0u8; 1];
    match tokio::time::timeout(FIRST_BYTE_TIMEOUT, tcp.peek(&mut first)).await {
        Ok(Ok(n)) if n > 0 => {}
        _ => return Ok(()),
    }
    let tls_acceptor = (first[0] == TLS_HANDSHAKE_RECORD)
        .then(|| acceptor.get())
        .flatten();
    let is_tls = tls_acceptor.is_some();

    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        if !is_tls && req.headers().contains_key(hyper::header::AUTHORIZATION) {
            warn!(%peer, path = %req.uri().path(), "plain-http authenticated request");
        }
        app.clone().oneshot(req)
    });
    let builder = auto::Builder::new(TokioExecutor::new());
    match tls_acceptor {
        Some(acceptor) => {
            let stream = acceptor.accept(tcp).await?;
            builder
                .serve_connection_with_upgrades(TokioIo::new(stream), service)
                .await
                .map_err(|e| anyhow!(e))
        }
        None => builder
            .serve_connection_with_upgrades(TokioIo::new(tcp), service)
            .await
            .map_err(|e| anyhow!(e)),
    }
}

pub fn describe(source: &Source) -> String {
    match source {
        Source::Container {
            project,
            service,
            dir,
        } => format!("container {project}/{service}:{dir}"),
        Source::Files { cert, key } => format!("files {} {}", path_str(cert), path_str(key)),
        Source::Off => "off".into(),
    }
}

fn path_str(p: &Path) -> String {
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use rustls::pki_types::ServerName;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct TestCert {
        chain_pem: String,
        key_pem: String,
        der: CertificateDer<'static>,
    }

    fn test_cert() -> TestCert {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        TestCert {
            chain_pem: ck.cert.pem(),
            key_pem: ck.key_pair.serialize_pem(),
            der: ck.cert.der().clone(),
        }
    }

    fn app() -> Router {
        Router::new().route("/ping", get(|| async { "pong" }))
    }

    async fn start_server(acceptor: SharedAcceptor) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, app(), acceptor));
        addr
    }

    const REQUEST: &[u8] = b"GET /ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

    async fn plain_get(addr: std::net::SocketAddr) -> String {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(REQUEST).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    async fn tls_get(
        addr: std::net::SocketAddr,
        trust: &CertificateDer<'static>,
    ) -> Result<String> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(trust.clone())?;
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let tcp = TcpStream::connect(addr).await?;
        let mut s = connector
            .connect(ServerName::try_from("localhost")?, tcp)
            .await?;
        s.write_all(REQUEST).await?;
        let mut out = Vec::new();
        // The server may close without close_notify; keep what was read.
        let _ = s.read_to_end(&mut out).await;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn loaded(cert: &TestCert) -> SharedAcceptor {
        let acceptor = SharedAcceptor::default();
        acceptor.set(
            server_config_from_pem(cert.chain_pem.as_bytes(), cert.key_pem.as_bytes()).unwrap(),
        );
        acceptor
    }

    #[tokio::test]
    async fn plain_and_tls_share_one_port() {
        let cert = test_cert();
        let addr = start_server(loaded(&cert)).await;

        let plain = plain_get(addr).await;
        assert!(plain.starts_with("HTTP/1.1 200"), "{plain}");
        assert!(plain.ends_with("pong"), "{plain}");

        let tls = tls_get(addr, &cert.der).await.unwrap();
        assert!(tls.starts_with("HTTP/1.1 200"), "{tls}");
        assert!(tls.ends_with("pong"), "{tls}");
    }

    #[tokio::test]
    async fn without_certificate_only_plain_http_is_served() {
        let cert = test_cert();
        let addr = start_server(SharedAcceptor::default()).await;

        assert!(plain_get(addr).await.starts_with("HTTP/1.1 200"));
        assert!(tls_get(addr, &cert.der).await.is_err());
    }

    #[tokio::test]
    async fn swapped_certificate_applies_to_new_connections() {
        let (old, new) = (test_cert(), test_cert());
        let acceptor = loaded(&old);
        let addr = start_server(acceptor.clone()).await;
        assert!(tls_get(addr, &old.der).await.unwrap().ends_with("pong"));

        acceptor
            .set(server_config_from_pem(new.chain_pem.as_bytes(), new.key_pem.as_bytes()).unwrap());
        assert!(tls_get(addr, &new.der).await.unwrap().ends_with("pong"));
        assert!(tls_get(addr, &old.der).await.is_err());
    }

    #[tokio::test]
    async fn files_source_loads_pem_files() {
        let cert = test_cert();
        let dir = scratch_dir().unwrap();
        let (c, k) = (dir.join("fullchain.pem"), dir.join("privkey.pem"));
        std::fs::write(&c, &cert.chain_pem).unwrap();
        std::fs::write(&k, &cert.key_pem).unwrap();
        let acceptor = start(Source::Files { cert: c, key: k }).await;
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(acceptor.is_loaded());
    }

    #[tokio::test]
    async fn unreadable_source_starts_without_tls() {
        let acceptor = start(Source::Files {
            cert: "/nonexistent/fullchain.pem".into(),
            key: "/nonexistent/privkey.pem".into(),
        })
        .await;
        assert!(!acceptor.is_loaded());
    }

    /// Mirrors certbot's layout (`live/` symlinks into `archive/`) in a real
    /// container found by its compose labels.
    #[tokio::test]
    #[ignore = "requires Docker; creates one synthetic container"]
    async fn container_source_follows_certbot_symlinks_real_docker() {
        let cert = test_cert();
        let project = format!("cm-tls-test-{}", rand::random::<u32>());
        let script = "set -e; a=/etc/letsencrypt/archive/x; l=/etc/letsencrypt/live/x; \
            mkdir -p $a $l; printf '%s' \"$CHAIN\" > $a/fullchain3.pem; \
            printf '%s' \"$KEY\" > $a/privkey3.pem; \
            ln -s ../../archive/x/fullchain3.pem $l/fullchain.pem; \
            ln -s ../../archive/x/privkey3.pem $l/privkey.pem; touch /ready; sleep 120";
        let id = run_docker(&[
            "run",
            "-d",
            "--rm",
            "--label",
            &format!("com.docker.compose.project={project}"),
            "--label",
            "com.docker.compose.service=certbot",
            "-e",
            &format!("CHAIN={}", cert.chain_pem),
            "-e",
            &format!("KEY={}", cert.key_pem),
            "alpine:3.21",
            "sh",
            "-c",
            script,
        ])
        .await
        .unwrap();
        let id = id.trim();
        for _ in 0..50 {
            if run_docker(&["exec", id, "test", "-f", "/ready"])
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let acceptor = start(Source::Container {
            project,
            service: "certbot".into(),
            dir: "/etc/letsencrypt/live/x".into(),
        })
        .await;
        let _ = run_docker(&["rm", "-f", id]).await;
        assert!(acceptor.is_loaded());
    }

    #[test]
    fn rejects_bad_pem() {
        let cert = test_cert();
        assert!(server_config_from_pem(b"", cert.key_pem.as_bytes()).is_err());
        assert!(server_config_from_pem(cert.chain_pem.as_bytes(), b"").is_err());
        // Key that does not match the certificate.
        let other = test_cert();
        assert!(
            server_config_from_pem(cert.chain_pem.as_bytes(), other.key_pem.as_bytes()).is_err()
        );
    }

    #[test]
    fn source_defaults_and_overrides() {
        let none = |_: &str| None;
        assert_eq!(
            Source::from_vars(none).unwrap(),
            Source::Container {
                project: "dstack".into(),
                service: "certbot".into(),
                dir: DEFAULT_CERT_DIR.into(),
            }
        );
        let vars = |k: &str| match k {
            "TLS_MODE" => Some("container".to_string()),
            "TLS_CERT_SERVICE" => Some("proj/svc".to_string()),
            "TLS_CERT_DIR" => Some("/certs/live/x/".to_string()),
            _ => None,
        };
        assert_eq!(
            Source::from_vars(vars).unwrap(),
            Source::Container {
                project: "proj".into(),
                service: "svc".into(),
                dir: "/certs/live/x".into(),
            }
        );
        let off = |k: &str| (k == "TLS_MODE").then(|| "off".to_string());
        assert_eq!(Source::from_vars(off).unwrap(), Source::Off);
        let files_missing_key = |k: &str| match k {
            "TLS_MODE" => Some("files".to_string()),
            "TLS_CERT_FILE" => Some("/c.pem".to_string()),
            _ => None,
        };
        assert!(Source::from_vars(files_missing_key).is_err());
        let bad_service = |k: &str| (k == "TLS_CERT_SERVICE").then(|| "certbot".to_string());
        assert!(Source::from_vars(bad_service).is_err());
        let bad_mode = |k: &str| (k == "TLS_MODE").then(|| "on".to_string());
        assert!(Source::from_vars(bad_mode).is_err());
    }

    #[test]
    fn first_container_id_skips_blank_lines() {
        assert_eq!(
            first_container_id("\n abc123 \ndef456\n"),
            Some("abc123".into())
        );
        assert_eq!(first_container_id("\n\n"), None);
    }
}
