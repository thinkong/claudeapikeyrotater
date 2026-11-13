use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address to listen on. Defaults to loopback so the proxy is not exposed by accident.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Upstream Claude API base URL.
    #[serde(default = "default_upstream")]
    pub upstream: String,
    /// Externally visible base URL of this proxy (e.g. `https://claude-proxy.internal`). Used to
    /// rewrite absolute URLs the API returns, such as a batch's `results_url`.
    /// Defaults to `http://<Host header>`.
    pub public_url: Option<String>,
    /// SQLite database for usage log, key state and resource pins.
    #[serde(default = "default_database")]
    pub database: String,
    /// Token for the web UI and /proxy/admin/* endpoints. When unset, one is generated on
    /// first start, stored in the database and printed to the log.
    pub admin_token: Option<String>,
    /// Accept requests that carry no (or an unknown) client token. Only for trusted networks.
    #[serde(default)]
    pub allow_unauthenticated: bool,
    /// Model used by the UI's key check (a 1-output-token request). Default: claude-haiku-5-5.
    #[serde(default = "default_check_model")]
    pub check_model: String,
    /// Optional pricing override file (same format as the built-in pricing.toml).
    pub pricing_file: Option<String>,
    /// Optional file with one upstream key per line (`sk-ant-...` or `name=sk-ant-...`).
    /// Like `[[keys]]`, these are imported into the database once; manage them in the UI after that.
    pub keys_file: Option<String>,

    #[serde(default)]
    pub clients: Vec<ClientCfg>,
    #[serde(default)]
    pub keys: Vec<KeyCfg>,
    #[serde(default)]
    pub cooldowns: Cooldowns,
    #[serde(default)]
    pub routing: RoutingCfg,
    #[serde(default)]
    pub headers: HeadersCfg,
}

/// Which client headers are forwarded upstream. Credentials, hop-by-hop headers, `x-proxy-*`,
/// `accept-encoding` and `oauth-*` betas are always removed, whatever the mode.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields, default)]
pub struct HeadersCfg {
    /// `all` (default): forward every other client header. `minimal`: forward only what the API
    /// reads (`anthropic-*`, `content-type`, `accept`) plus anything listed in `keep`.
    pub mode: HeaderMode,
    /// In `minimal` mode, extra header names to forward. A trailing `*` matches a prefix.
    pub keep: Vec<String>,
    /// Header names to drop (case-insensitive). A trailing `*` matches a prefix, e.g. `x-stainless-*`.
    pub drop: Vec<String>,
    /// `anthropic-beta` values to drop; the rest of the header is kept. A trailing `*` matches a
    /// prefix, and `*` alone drops the whole header.
    pub drop_betas: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum HeaderMode {
    #[default]
    All,
    Minimal,
}

/// Headers forwarded in `minimal` mode: everything the API itself reads.
pub const MINIMAL_HEADERS: [&str; 3] = ["anthropic-*", "content-type", "accept"];

impl HeadersCfg {
    /// Whether a client header (already lowercase, not a credential) goes upstream.
    pub fn forwards(&self, name: &str) -> bool {
        if self.drop.iter().any(|p| pattern_matches(p, name)) {
            return false;
        }
        match self.mode {
            HeaderMode::All => true,
            HeaderMode::Minimal => {
                MINIMAL_HEADERS.iter().any(|p| pattern_matches(p, name)) || self.keep.iter().any(|p| pattern_matches(p, name))
            }
        }
    }
}

/// Headers the API needs to accept or place a request. `headers.drop` may not match these.
pub const PROTECTED_HEADERS: [&str; 3] = ["anthropic-version", "content-type", "anthropic-workspace-id"];

impl HeadersCfg {
    /// Lowercase the patterns and reject invalid ones, including any that would drop a protected header.
    pub fn validate(&mut self) -> Result<()> {
        for p in self.drop.iter_mut().chain(self.drop_betas.iter_mut()).chain(self.keep.iter_mut()) {
            *p = p.trim().to_ascii_lowercase();
        }
        for p in &self.keep {
            let base = p.strip_suffix('*').unwrap_or(p);
            if base.is_empty() || base.contains('*') || axum::http::HeaderName::from_bytes(base.as_bytes()).is_err() {
                bail!("headers.keep: invalid header name pattern {p:?}");
            }
        }
        for p in &self.drop {
            let base = p.strip_suffix('*').unwrap_or(p);
            if base.is_empty() || base.contains('*') || axum::http::HeaderName::from_bytes(base.as_bytes()).is_err() {
                bail!("headers.drop: invalid header name pattern {p:?}");
            }
            if let Some(h) = PROTECTED_HEADERS.iter().find(|h| pattern_matches(p, h)) {
                bail!("headers.drop: {p:?} would drop {h}, which the API requires");
            }
        }
        for p in &self.drop_betas {
            let base = p.strip_suffix('*').unwrap_or(p);
            if (base.is_empty() && p != "*") || base.contains('*') || base.contains(',') || base.contains(char::is_whitespace) {
                bail!("headers.drop_betas: invalid beta pattern {p:?}");
            }
        }
        Ok(())
    }
}

/// A name pattern: exact match, or prefix match when written with a trailing `*`.
pub fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ClientCfg {
    pub name: String,
    pub token: Option<String>,
    /// Read the token from this environment variable instead of `token`.
    pub token_env: Option<String>,
    /// Requests are rejected once this client's tracked spend reaches the budget.
    pub budget_usd: Option<f64>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct KeyCfg {
    pub name: String,
    pub key: Option<String>,
    /// Read the key from this environment variable instead of `key`.
    pub key_env: Option<String>,
    /// Known credit balance, for the remaining-credit estimate.
    pub initial_balance_usd: Option<f64>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Cooldowns {
    /// How often an out-of-credits key is re-checked with a 1-token request. The key
    /// gets no traffic until a check succeeds.
    pub credit_exhausted_secs: u64,
    /// How long a key is skipped after a 401 (revoked / invalid key).
    pub auth_failure_secs: u64,
    /// Fallback for a 429 without a usable `retry-after` header.
    pub rate_limit_secs: u64,
    /// Skip a key for this long after a 529/5xx (0 = retry elsewhere without penalty).
    pub overloaded_secs: u64,
}

impl Default for Cooldowns {
    fn default() -> Self {
        Self { credit_exhausted_secs: 3600, auth_failure_secs: 86400, rate_limit_secs: 30, overloaded_secs: 0 }
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Affinity {
    /// Hash model + tools + system + first message: every turn of a conversation lands on
    /// the same key (cache hits), different conversations spread across keys.
    Conversation,
    /// Hash model + tools + system only: requests that share a system prompt share a key.
    Prefix,
    /// Hash the client name: each client sticks to one key.
    Client,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct RoutingCfg {
    pub affinity: Affinity,
    /// Total upstream attempts (across keys) for one client request.
    pub max_attempts: usize,
    /// Request bodies up to this size are buffered (needed for failover).
    /// Larger bodies (e.g. big file uploads) are streamed and cannot fail over.
    pub max_buffer_bytes: usize,
    pub connect_timeout_secs: u64,
    /// Max silence between bytes from upstream. There is deliberately no total timeout:
    /// long streaming turns can run for many minutes.
    pub read_timeout_secs: u64,
    /// Forget resource -> key pins (files, batches, agents...) older than this.
    pub pin_retention_days: u64,
}

impl Default for RoutingCfg {
    fn default() -> Self {
        Self {
            affinity: Affinity::Conversation,
            max_attempts: 4,
            max_buffer_bytes: 64 * 1024 * 1024,
            connect_timeout_secs: 10,
            read_timeout_secs: 900,
            pin_retention_days: 90,
        }
    }
}

/// An upstream key from the config file (seed for the database).
#[derive(Debug, Clone)]
pub struct SeedKey {
    pub name: String,
    pub secret: String,
    pub balance_usd: Option<f64>,
}

/// A client from the config file (seed for the database).
#[derive(Debug, Clone)]
pub struct SeedClient {
    pub name: String,
    pub token: String,
    pub budget_usd: Option<f64>,
}

fn default_listen() -> String {
    "127.0.0.1:8787".into()
}
fn default_upstream() -> String {
    "https://api.anthropic.com".into()
}
fn default_check_model() -> String {
    "claude-haiku-5-5".into()
}
fn default_database() -> String {
    "claude-key-router.db".into()
}

fn resolve_secret(value: &Option<String>, env: &Option<String>, what: &str) -> Result<String> {
    if let Some(var) = env {
        return std::env::var(var).with_context(|| format!("{what}: environment variable {var} is not set"));
    }
    match value {
        Some(v) if !v.is_empty() => Ok(v.clone()),
        _ => bail!("{what}: needs `key`/`token` or the matching `_env` field"),
    }
}

impl Config {
    /// Load the config file (if any), then apply `CKR_*` environment overrides.
    /// `path = None` means "use config.toml if present, else defaults" — so a container
    /// can run from environment variables alone.
    pub fn load(path: Option<&str>) -> Result<Self> {
        let text = match path {
            Some(p) => std::fs::read_to_string(p).with_context(|| format!("reading config {p}"))?,
            None => match std::fs::read_to_string("config.toml") {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => return Err(e).context("reading config.toml"),
            },
        };
        let name = path.unwrap_or("config.toml");
        let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parsing config {name}"))?;
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(v) = env("CKR_LISTEN") {
            cfg.listen = v;
        }
        if let Some(v) = env("CKR_UPSTREAM") {
            cfg.upstream = v;
        }
        if let Some(v) = env("CKR_DATABASE") {
            cfg.database = v;
        }
        if let Some(v) = env("CKR_ADMIN_TOKEN") {
            cfg.admin_token = Some(v);
        }
        if let Some(v) = env("CKR_PUBLIC_URL") {
            cfg.public_url = Some(v);
        }
        if let Some(v) = env("CKR_HEADERS") {
            cfg.headers.mode = match v.to_ascii_lowercase().as_str() {
                "all" => HeaderMode::All,
                "minimal" => HeaderMode::Minimal,
                _ => bail!("CKR_HEADERS must be \"all\" or \"minimal\", got {v:?}"),
            };
        }
        if cfg.routing.max_attempts == 0 {
            bail!("routing.max_attempts must be at least 1");
        }
        cfg.headers.validate()?;
        Ok(cfg)
    }

    pub fn seed_keys(&self) -> Result<Vec<SeedKey>> {
        let mut out = Vec::new();
        for k in &self.keys {
            let secret = resolve_secret(&k.key, &k.key_env, &format!("key {}", k.name))?;
            out.push(SeedKey { name: k.name.clone(), secret, balance_usd: k.initial_balance_usd });
        }
        if let Some(path) = &self.keys_file {
            let text = std::fs::read_to_string(path).with_context(|| format!("reading keys_file {path}"))?;
            for (lineno, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let (name, secret) = match line.split_once('=') {
                    Some((n, s)) => (n.trim().to_string(), s.trim().to_string()),
                    None => (format!("file-key-{}", lineno + 1), line.to_string()),
                };
                out.push(SeedKey { name, secret, balance_usd: None });
            }
        }
        let mut seen = HashSet::new();
        for k in &out {
            if !seen.insert(k.name.clone()) {
                bail!("duplicate key name {}", k.name);
            }
        }
        Ok(out)
    }

    pub fn seed_clients(&self) -> Result<Vec<SeedClient>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for c in &self.clients {
            let token = resolve_secret(&c.token, &c.token_env, &format!("client {}", c.name))?;
            if !seen.insert(token.clone()) {
                bail!("client {}: token is shared with another client", c.name);
            }
            out.push(SeedClient { name: c.name.clone(), token, budget_usd: c.budget_usd });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(drop: &[&str], betas: &[&str]) -> HeadersCfg {
        HeadersCfg {
            drop: drop.iter().map(|s| s.to_string()).collect(),
            drop_betas: betas.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn header_patterns_are_normalised() {
        let mut c = cfg(&[" X-Stainless-* ", "User-Agent"], &["Some-Beta-*", "*"]);
        c.validate().unwrap();
        assert_eq!(c.drop, ["x-stainless-*", "user-agent"]);
        assert_eq!(c.drop_betas, ["some-beta-*", "*"]);
    }

    #[test]
    fn protected_headers_cannot_be_dropped() {
        for p in ["anthropic-version", "Content-Type", "anthropic-workspace-id", "anthropic-*", "content-*", "anthropic-w*"] {
            let err = cfg(&[p], &[]).validate().unwrap_err().to_string();
            assert!(err.contains("which the API requires"), "{p}: {err}");
        }
        // Neighbouring names are still allowed.
        cfg(&["anthropic-dangerous-direct-browser-access", "content-md5"], &[]).validate().unwrap();
    }

    #[test]
    fn minimal_mode_forwards_only_api_headers() {
        let mut c = HeadersCfg { mode: HeaderMode::Minimal, keep: vec!["X-Request-Tag".into()], ..Default::default() };
        c.validate().unwrap();
        for h in ["anthropic-version", "anthropic-beta", "anthropic-workspace-id", "content-type", "accept", "x-request-tag"] {
            assert!(c.forwards(h), "{h} should be forwarded");
        }
        for h in ["user-agent", "x-stainless-os", "x-app", "referer"] {
            assert!(!c.forwards(h), "{h} should be dropped");
        }
        assert!(HeadersCfg::default().forwards("user-agent"));
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        assert!(cfg(&["*"], &[]).validate().is_err());
        assert!(cfg(&["bad name"], &[]).validate().is_err());
        assert!(cfg(&["a*b*"], &[]).validate().is_err());
        assert!(cfg(&[], &["a,b"]).validate().is_err());
        assert!(cfg(&[], &["a b"]).validate().is_err());
        assert!(HeadersCfg { keep: vec!["*".into()], ..Default::default() }.validate().is_err());
    }
}
