mod gpu;
mod sampler;
mod topo;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use clap::Parser;
use ipnet::IpNet;
use serde::Deserialize;

use sampler::Shared;

const INDEX_HTML: &str = include_str!("../static/index.html");

/// Lightweight web resource monitor (CPU, RAM, NVIDIA GPU, processes).
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Network interface(s) to listen on, e.g. `tailscale0`. Binds to every
    /// address on that interface. Repeatable. Default: all interfaces.
    #[arg(short, long = "interface", value_name = "IFACE")]
    interfaces: Vec<String>,

    /// Explicit address(es) to bind to (in addition to --interface).
    #[arg(short, long = "bind", value_name = "IP")]
    binds: Vec<IpAddr>,

    /// TCP port.
    #[arg(short, long, default_value_t = 9898)]
    port: u16,

    /// Only allow clients from these IPs/CIDRs (comma separated or repeated),
    /// e.g. `100.64.0.0/10,fd7a:115c:a1e0::/48`. Default: allow everyone who
    /// can reach the listener.
    #[arg(short, long = "allow", value_name = "IP|CIDR", value_delimiter = ',')]
    allow: Vec<String>,

    /// Reject clients from these IPs/CIDRs (comma separated or repeated).
    /// Checked before --allow, so it can carve holes out of an allowed range.
    #[arg(short, long = "deny", value_name = "IP|CIDR", value_delimiter = ',')]
    deny: Vec<String>,

    /// Default sampling interval in milliseconds (viewers can change it from
    /// the UI, within 250..10000).
    #[arg(long, default_value_t = 1000)]
    interval: u64,

    /// Seconds to keep waiting for --interface to get an address (useful at
    /// boot when tailscale comes up after us). 0 = fail immediately.
    #[arg(long, default_value_t = 60)]
    wait_iface: u64,

    /// List network interfaces and their addresses, then exit.
    #[arg(long)]
    list_interfaces: bool,
}

struct App {
    shared: Arc<Shared>,
    allow: Vec<IpNet>,
    deny: Vec<IpNet>,
    interval_ms: u64,
}

fn parse_net(s: &str) -> Result<IpNet, String> {
    let s = s.trim();
    s.parse::<IpNet>()
        .or_else(|_| s.parse::<IpAddr>().map(IpNet::from))
        .map_err(|_| format!("invalid IP or CIDR: {s:?}"))
}

fn iface_addrs(name: &str) -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.name == name)
        .map(|i| i.ip())
        // IPv6 link-local needs a scope id and is never what you want here.
        .filter(|ip| !matches!(ip, IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80))
        .collect()
}

async fn resolve_binds(args: &Args) -> Result<Vec<IpAddr>, String> {
    let mut out = args.binds.clone();
    for name in &args.interfaces {
        let mut waited = 0;
        let addrs = loop {
            let a = iface_addrs(name);
            if !a.is_empty() {
                break a;
            }
            if waited >= args.wait_iface {
                return Err(format!(
                    "interface {name:?} has no usable addresses (see --list-interfaces)"
                ));
            }
            if waited == 0 {
                eprintln!("webtop: waiting for interface {name:?} to get an address...");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            waited += 1;
        };
        out.extend(addrs);
    }
    if out.is_empty() {
        // Dual-stack wildcard; on Linux `::` also accepts IPv4 unless bindv6only.
        out.push(IpAddr::from([0u8; 16]));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

async fn guard(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let ip = canonical(peer.ip());
    if app.deny.iter().any(|n| n.contains(&ip))
        || (!app.allow.is_empty() && !app.allow.iter().any(|n| n.contains(&ip)))
    {
        eprintln!("webtop: denied {ip}");
        return (StatusCode::FORBIDDEN, "forbidden\n").into_response();
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    h.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    h.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    resp
}

async fn index(State(app): State<Arc<App>>) -> impl IntoResponse {
    let html = INDEX_HTML.replace("__INTERVAL__", &app.interval_ms.to_string());
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html)
}

#[derive(Deserialize)]
struct StatsQuery {
    interval: Option<u64>,
}

async fn stats(State(app): State<Arc<App>>, Query(q): Query<StatsQuery>) -> impl IntoResponse {
    app.shared.touch();
    app.shared.request_interval(q.interval.unwrap_or(app.interval_ms));
    let body = app.shared.latest.read().unwrap().clone();
    ([(header::CONTENT_TYPE, "application/json")], (*body).clone())
}

#[derive(Deserialize)]
struct HistoryQuery {
    since: Option<u64>,
}

async fn history(State(app): State<Arc<App>>, Query(q): Query<HistoryQuery>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], app.shared.history_json(q.since.unwrap_or(0)))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();

    if args.list_interfaces {
        for i in if_addrs::get_if_addrs().unwrap_or_default() {
            println!("{:<16} {}", i.name, i.ip());
        }
        return;
    }

    let parse_list = |list: &[String]| -> Vec<IpNet> {
        match list.iter().filter(|s| !s.trim().is_empty()).map(|s| parse_net(s)).collect() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("webtop: {e}");
                std::process::exit(2);
            }
        }
    };
    let allow = parse_list(&args.allow);
    let deny = parse_list(&args.deny);
    let binds = match resolve_binds(&args).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("webtop: {e}");
            std::process::exit(2);
        }
    };

    let interval = Duration::from_millis(
        args.interval.clamp(sampler::MIN_INTERVAL_MS, sampler::MAX_INTERVAL_MS),
    );
    let shared = Shared::new(interval.as_millis() as u64);
    sampler::spawn(shared.clone());

    let app = Arc::new(App { shared, allow, deny, interval_ms: interval.as_millis() as u64 });
    let router = Router::new()
        .route("/", get(index))
        .route("/api/stats", get(stats))
        .route("/api/history", get(history))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app.clone());

    let mut tasks = tokio::task::JoinSet::new();
    for ip in binds {
        let addr = SocketAddr::new(ip, args.port);
        let mut res = tokio::net::TcpListener::bind(addr).await;
        if res.is_err() && ip.is_unspecified() && ip.is_ipv6() {
            // IPv6 disabled on this host: fall back to IPv4 wildcard.
            res = tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], args.port))).await;
        }
        let listener = match res {
            Ok(l) => l,
            Err(e) => {
                eprintln!("webtop: cannot bind {addr}: {e}");
                std::process::exit(1);
            }
        };
        eprintln!("webtop: listening on http://{addr}");
        let r = router.clone().into_make_service_with_connect_info::<SocketAddr>();
        tasks.spawn(async move { axum::serve(listener, r).await });
    }
    if app.allow.is_empty() {
        eprintln!("webtop: no --allow list; any client that can reach the listener is accepted");
    } else {
        eprintln!("webtop: allowing {:?}", app.allow.iter().map(|n| n.to_string()).collect::<Vec<_>>());
    }
    if !app.deny.is_empty() {
        eprintln!("webtop: denying {:?}", app.deny.iter().map(|n| n.to_string()).collect::<Vec<_>>());
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        Some(res) = tasks.join_next() => {
            if let Ok(Err(e)) = res { eprintln!("webtop: server error: {e}"); }
        }
    }
}
