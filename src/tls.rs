//! HTTPS support: a certificate from PEM files or from `tailscale cert`,
//! reloaded when it changes, and a listener that speaks both TLS and plain
//! HTTP on the same port. Plain HTTP from Tailscale addresses is redirected
//! to https:// (the certificate's name only resolves inside the tailnet);
//! anyone else, e.g. on the LAN, keeps getting plain HTTP.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use axum::Router;
use axum::extract::ConnectInfo;
use hyper_util::rt::{TokioIo, TokioTimer};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::crypto::ring;
use tower::ServiceExt;

/// How often to ask tailscale for a fresh certificate. It only contacts
/// Let's Encrypt when the cached one is close to expiry, so this is cheap.
const TAILSCALE_REFRESH: Duration = Duration::from_secs(12 * 3600);
/// How often to check the certificate files for changes.
const RELOAD_CHECK: Duration = Duration::from_secs(60);
/// Time a client gets to send its first byte / finish the TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Tls {
    config: RwLock<Arc<ServerConfig>>,
    cert: PathBuf,
    key: PathBuf,
    /// Set when the certificate comes from `tailscale cert`; plain-HTTP
    /// requests are redirected to this name so the certificate matches.
    pub domain: Option<String>,
}

pub struct TlsOptions {
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub tailscale: bool,
    pub wait: Duration,
}

impl Tls {
    pub async fn setup(opts: TlsOptions) -> Result<Arc<Tls>, String> {
        if !opts.tailscale {
            let (Some(cert), Some(key)) = (opts.cert, opts.key) else {
                return Err("--tls-cert and --tls-key must be given together".into());
            };
            let config = load(&cert, &key)?;
            return Ok(Arc::new(Tls { config: RwLock::new(config), cert, key, domain: None }));
        }

        let dir = state_dir();
        let cert = opts.cert.unwrap_or_else(|| dir.join("tls.crt"));
        let key = opts.key.unwrap_or_else(|| dir.join("tls.key"));
        for p in [&cert, &key] {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
        }

        // At boot tailscaled may not be up yet: keep retrying for a while.
        let deadline = Instant::now() + opts.wait;
        let domain = loop {
            match fetch(&cert, &key).await {
                Ok(d) => break d,
                Err(e) => {
                    if Instant::now() >= deadline {
                        // A previously fetched certificate is still better than nothing.
                        if let Ok(config) = load(&cert, &key) {
                            eprintln!("webtop: {e}; using existing {}", cert.display());
                            let domain = tailscale_domain().await.ok();
                            return Ok(Arc::new(Tls { config: RwLock::new(config), cert, key, domain }));
                        }
                        return Err(e);
                    }
                    eprintln!("webtop: {e}; retrying...");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        };
        let config = load(&cert, &key)?;
        eprintln!("webtop: using tailscale certificate for {domain}");
        Ok(Arc::new(Tls { config: RwLock::new(config), cert, key, domain: Some(domain) }))
    }

    fn acceptor(&self) -> TlsAcceptor {
        TlsAcceptor::from(self.config.read().unwrap().clone())
    }

    /// Re-fetch (tailscale) and reload the certificate whenever it changes.
    pub fn spawn_reloader(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut seen = mtimes(&self.cert, &self.key);
            let mut fetched = Instant::now();
            loop {
                tokio::time::sleep(RELOAD_CHECK).await;
                if self.domain.is_some() && fetched.elapsed() >= TAILSCALE_REFRESH {
                    if let Err(e) = fetch(&self.cert, &self.key).await {
                        eprintln!("webtop: certificate refresh failed: {e}");
                    }
                    fetched = Instant::now();
                }
                let now = mtimes(&self.cert, &self.key);
                if now == seen {
                    continue;
                }
                match load(&self.cert, &self.key) {
                    Ok(config) => {
                        *self.config.write().unwrap() = config;
                        seen = now;
                        eprintln!("webtop: reloaded certificate {}", self.cert.display());
                    }
                    // Probably caught mid-write; try again next round.
                    Err(e) => eprintln!("webtop: {e}"),
                }
            }
        });
    }

    /// Accept connections forever. TLS connections get `app`; plain HTTP
    /// ones get `redirect` if they come from a Tailscale address, else `app`.
    pub async fn serve(self: Arc<Self>, listener: TcpListener, app: Router, redirect: Router) {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    // e.g. EMFILE: back off instead of spinning.
                    eprintln!("webtop: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let tls = self.clone();
            let (app, redirect) = (app.clone(), redirect.clone());
            tokio::spawn(async move {
                // A TLS ClientHello always starts with record type 0x16.
                let mut first = [0u8; 1];
                let is_tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.peek(&mut first)).await {
                    Ok(Ok(1)) => first[0] == 0x16,
                    _ => return,
                };
                if is_tls {
                    let accept = tls.acceptor().accept(stream);
                    if let Ok(Ok(s)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, accept).await {
                        serve_conn(s, peer, app).await;
                    }
                } else if is_tailscale(peer.ip()) {
                    serve_conn(stream, peer, redirect).await;
                } else {
                    serve_conn(stream, peer, app).await;
                }
            });
        }
    }
}

fn is_tailscale(ip: IpAddr) -> bool {
    match crate::canonical(ip) {
        // 100.64.0.0/10 (CGNAT range Tailscale assigns from)
        IpAddr::V4(v4) => v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64,
        // fd7a:115c:a1e0::/48
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

async fn serve_conn<S>(io: S, peer: SocketAddr, router: Router)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
        req.extensions_mut().insert(ConnectInfo(peer));
        router.clone().oneshot(req)
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .serve_connection(TokioIo::new(io), svc)
        .await;
}

fn load(cert: &Path, key: &Path) -> Result<Arc<ServerConfig>, String> {
    let certs = CertificateDer::pem_file_iter(cert)
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("cannot read certificate {}: {e}", cert.display()))?;
    if certs.is_empty() {
        return Err(format!("no certificate found in {}", cert.display()));
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .map_err(|e| format!("cannot read private key {}: {e}", key.display()))?;
    let mut config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("invalid certificate/key: {e}"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

fn mtimes(cert: &Path, key: &Path) -> (Option<SystemTime>, Option<SystemTime>) {
    let m = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    (m(cert), m(key))
}

/// Where `--tailscale-cert` keeps its files: systemd's $STATE_DIRECTORY, else
/// $XDG_STATE_HOME/webtop, else ~/.local/state/webtop.
fn state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("STATE_DIRECTORY") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("webtop")
}

/// This node's MagicDNS name, e.g. `host.tailnet-name.ts.net`.
async fn tailscale_domain() -> Result<String, String> {
    let out = tokio::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .await
        .map_err(|e| format!("cannot run tailscale: {e}"))?;
    if !out.status.success() {
        return Err(format!("tailscale status failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("bad tailscale status output: {e}"))?;
    match v["Self"]["DNSName"].as_str().map(|s| s.trim_end_matches('.')) {
        Some(d) if !d.is_empty() => Ok(d.to_string()),
        _ => Err("tailscale has no MagicDNS name for this node (is it logged in?)".into()),
    }
}

/// Run `tailscale cert` for this node, writing to `cert` / `key`.
async fn fetch(cert: &Path, key: &Path) -> Result<String, String> {
    let domain = tailscale_domain().await?;
    let out = tokio::process::Command::new("tailscale")
        .arg("cert")
        .arg("--cert-file")
        .arg(cert)
        .arg("--key-file")
        .arg(key)
        .arg(&domain)
        .output()
        .await
        .map_err(|e| format!("cannot run tailscale: {e}"))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        let mut err = format!("tailscale cert failed: {}", msg.trim());
        if msg.contains("HTTPS") || msg.contains("not enabled") {
            err.push_str("\n(enable HTTPS certificates in the Tailscale admin console under DNS)");
        }
        return Err(err);
    }
    Ok(domain)
}
