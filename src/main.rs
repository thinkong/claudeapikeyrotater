mod admin;
mod config;
mod pool;
mod pricing;
mod proxy;
mod store;
mod tee;

use anyhow::{Context, Result};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::get;
use axum::Router;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

const UI_HTML: &str = include_str!("../ui/index.html");
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct AppState {
    pub cfg: config::Config,
    pub admin_token: String,
    pub pricing: pricing::Pricing,
    pub store: Arc<store::Store>,
    pub pool: Arc<pool::Pool>,
    pub http: reqwest::Client,
    pub upstream: String,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// The admin token from the config, or one generated on first start and kept in the database.
fn admin_token(cfg: &config::Config, store: &store::Store) -> Result<String> {
    if let Some(t) = cfg.admin_token.as_deref().filter(|t| !t.is_empty()) {
        return Ok(t.to_string());
    }
    if let Some(t) = store.get_setting("admin_token")? {
        tracing::info!("admin token: stored in the database (set admin_token in the config to choose your own)");
        return Ok(t);
    }
    let t = format!("ckr-admin-{}", uuid::Uuid::new_v4().simple());
    store.put_setting("admin_token", &t)?;
    tracing::warn!("generated admin token for the web UI (shown once, stored in the database): {t}");
    Ok(t)
}

/// `claude-key-router healthcheck`: exit 0 if `GET /health` on the local listener answers 200.
/// For container health checks in images that have no shell or curl.
fn healthcheck() -> i32 {
    use std::io::{Read, Write};
    let listen = std::env::var("CKR_LISTEN").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let port = listen.rsplit(':').next().unwrap_or("8787");
    let addr = format!("127.0.0.1:{port}");
    let timeout = Duration::from_secs(3);
    let probe = || -> std::io::Result<bool> {
        let sock = addr.parse().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let mut s = std::net::TcpStream::connect_timeout(&sock, timeout)?;
        s.set_read_timeout(Some(timeout))?;
        s.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf)?;
        Ok(buf[..n].starts_with(b"HTTP/1.1 200"))
    };
    match probe() {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            eprintln!("healthcheck: {addr}: {e}");
            1
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("healthcheck") => std::process::exit(healthcheck()),
        Some("--version" | "-V" | "version") => {
            println!("claude-key-router {VERSION}");
            return Ok(());
        }
        _ => {}
    }
    let path = arg.or_else(|| std::env::var("CKR_CONFIG").ok().filter(|v| !v.is_empty()));
    let cfg = config::Config::load(path.as_deref())?;
    let seed_keys = cfg.seed_keys()?;
    let seed_clients = cfg.seed_clients()?;
    let pricing = pricing::Pricing::load(cfg.pricing_file.as_deref())?;
    let store = Arc::new(store::Store::open(&cfg.database)?);
    let admin_token = admin_token(&cfg, &store)?;
    let pool = Arc::new(pool::Pool::new(&cfg, seed_keys, seed_clients, store.clone())?);
    {
        let pool = pool.clone();
        store.start_writer(Box::new(move |row| pool.on_insert(row)))?;
    }

    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(cfg.routing.connect_timeout_secs))
        .read_timeout(Duration::from_secs(cfg.routing.read_timeout_secs))
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building HTTP client")?;

    let listen = cfg.listen.clone();
    let upstream = cfg.upstream.trim_end_matches('/').to_string();
    tracing::info!(
        "claude-key-router {VERSION}: {} upstream keys, {} clients, upstream {upstream}",
        pool.key_count(),
        pool.client_count()
    );
    let state = Arc::new(AppState { cfg, admin_token, pricing, store: store.clone(), pool, http, upstream });
    tokio::spawn(admin::credit_checker(state.clone()));

    let app = Router::new()
        .route("/", get(|| async { Redirect::temporary("/ui") }))
        .route("/ui", get(|| async { ([("cache-control", "no-store")], Html(UI_HTML)).into_response() }))
        .route("/health", get(|| async { "ok" }))
        .nest("/proxy", admin::router())
        .fallback(proxy::handle)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen).await.with_context(|| format!("binding {listen}"))?;
    tracing::info!("listening on http://{listen}  (web UI: http://{listen}/ui)");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    store.shutdown();
    Ok(())
}
