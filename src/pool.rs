//! Upstream key pool and client registry. Both live in memory (authoritative) and are
//! persisted through the store, so the admin UI can change them while the proxy runs.

use crate::config::{Config, Cooldowns, SeedClient, SeedKey};
use crate::now_ms;
use crate::store::{ClientRow, KeyRow, RequestRow, Store};
use anyhow::{bail, Result};
use parking_lot::Mutex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

struct KeyInfo {
    row: KeyRow,
    spend: f64,
    spend_since_balance: f64,
    requests: u64,
    last_used_ms: i64,
    last_error: Option<String>,
}

struct ClientInfo {
    row: ClientRow,
}

struct Inner {
    keys: BTreeMap<String, KeyInfo>,
    clients: BTreeMap<String, ClientInfo>,
    by_hash: HashMap<String, String>,
    pins: HashMap<String, String>,
    client_spend: HashMap<String, f64>,
    /// Keys with a background credit check in flight.
    checking: HashSet<String>,
}

pub struct Pool {
    inner: Mutex<Inner>,
    store: Arc<Store>,
    cooldowns: Cooldowns,
}

/// A key chosen for one upstream attempt.
#[derive(Clone, Debug)]
pub struct Picked {
    pub name: String,
    pub secret: String,
}

#[derive(Debug)]
pub struct Unavailable {
    pub retry_after_secs: u64,
    pub all_out_of_credit: bool,
    pub no_keys: bool,
}

/// An authenticated client.
pub struct ClientAuth {
    pub name: String,
    pub over_budget: bool,
}

pub fn round6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

pub fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn new_client_token() -> String {
    format!("sk-ckr-{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

pub fn hint(secret: &str) -> String {
    let tail: String = secret.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{tail}")
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        bail!("names must be 1-64 characters: letters, digits, '-', '_' or '.'");
    }
    Ok(())
}

impl Pool {
    pub fn new(cfg: &Config, seed_keys: Vec<SeedKey>, seed_clients: Vec<SeedClient>, store: Arc<Store>) -> Result<Self> {
        let loaded = store.load(cfg.routing.pin_retention_days)?;
        let now = now_ms();
        let mut known_keys: HashSet<String> = loaded.keys.iter().map(|k| k.name.clone()).collect();
        let known_clients: HashSet<String> = loaded.clients.iter().map(|c| c.name.clone()).collect();

        let mut key_rows = loaded.keys;
        for s in seed_keys {
            if known_keys.insert(s.name.clone()) {
                tracing::info!("importing key {} from config", s.name);
                let row = KeyRow {
                    name: s.name,
                    secret: s.secret,
                    enabled: true,
                    created_ms: now,
                    balance_usd: s.balance_usd,
                    balance_set_ms: now,
                    ..Default::default()
                };
                store.save_key(row.clone());
                key_rows.push(row);
            }
        }
        let mut client_rows = loaded.clients;
        for s in seed_clients {
            if !known_clients.contains(&s.name) {
                tracing::info!("importing client {} from config", s.name);
                let row = ClientRow {
                    name: s.name,
                    token_hash: hash_token(&s.token),
                    token_hint: hint(&s.token),
                    enabled: true,
                    budget_usd: s.budget_usd,
                    created_ms: now,
                    deleted_ms: None,
                };
                store.save_client(row.clone());
                client_rows.push(row);
            }
        }

        let mut keys = BTreeMap::new();
        for row in key_rows.into_iter().filter(|r| r.deleted_ms.is_none()) {
            let spend_since_balance = if row.balance_usd.is_some() { store.key_spend_since(&row.name, row.balance_set_ms)? } else { 0.0 };
            let spend = loaded.key_spend.get(&row.name).copied().unwrap_or(0.0);
            keys.insert(row.name.clone(), KeyInfo { row, spend, spend_since_balance, requests: 0, last_used_ms: 0, last_error: None });
        }
        let mut clients = BTreeMap::new();
        let mut by_hash = HashMap::new();
        for row in client_rows.into_iter().filter(|r| r.deleted_ms.is_none()) {
            by_hash.insert(row.token_hash.clone(), row.name.clone());
            clients.insert(row.name.clone(), ClientInfo { row });
        }
        Ok(Pool {
            inner: Mutex::new(Inner { keys, clients, by_hash, pins: loaded.pins, client_spend: loaded.client_spend, checking: HashSet::new() }),
            store,
            cooldowns: cfg.cooldowns.clone(),
        })
    }

    /// Usable for routing. An out-of-credits key stays out until a credit check
    /// (background or Refresh) succeeds; its timer only says when to check next.
    fn available(k: &KeyInfo, now: i64) -> bool {
        k.row.enabled && k.row.exhausted_ms.is_none() && k.row.cooldown_until_ms <= now
    }

    pub fn key_count(&self) -> usize {
        self.inner.lock().keys.len()
    }

    pub fn client_count(&self) -> usize {
        self.inner.lock().clients.len()
    }

    // ---------------------------------------------------------------- routing

    /// Rendezvous-hash selection: the same affinity always maps to the same healthy key,
    /// and when that key cools down only its share of traffic moves elsewhere.
    pub fn pick(&self, affinity: &[u8], exclude: &[String]) -> Result<Picked, Unavailable> {
        let now = now_ms();
        let mut inner = self.inner.lock();
        let best = inner
            .keys
            .values()
            .filter(|k| !exclude.contains(&k.row.name) && Self::available(k, now))
            .max_by_key(|k| {
                let mut h = Sha256::new();
                h.update(affinity);
                h.update([0u8]);
                h.update(k.row.name.as_bytes());
                u64::from_be_bytes(h.finalize()[..8].try_into().unwrap())
            })
            .map(|k| k.row.name.clone());
        match best {
            Some(name) => {
                let k = inner.keys.get_mut(&name).unwrap();
                k.requests += 1;
                k.last_used_ms = now;
                Ok(Picked { name, secret: k.row.secret.clone() })
            }
            None => Err(Self::unavailable(&inner, exclude, now)),
        }
    }

    /// A specific key (pinned resource or `x-proxy-key`), regardless of cooldown.
    pub fn get(&self, name: &str) -> Option<Picked> {
        let mut inner = self.inner.lock();
        let k = inner.keys.get_mut(name)?;
        k.requests += 1;
        k.last_used_ms = now_ms();
        Some(Picked { name: name.to_string(), secret: k.row.secret.clone() })
    }

    fn unavailable(inner: &Inner, exclude: &[String], now: i64) -> Unavailable {
        let mut soonest = i64::MAX;
        let mut all_credit = !inner.keys.is_empty();
        for k in inner.keys.values() {
            if exclude.contains(&k.row.name) && Self::available(k, now) {
                all_credit = false;
                continue;
            }
            if !k.row.enabled {
                continue;
            }
            soonest = soonest.min(k.row.cooldown_until_ms);
            if k.row.exhausted_ms.is_none() {
                all_credit = false;
            }
        }
        let retry_after_secs = if soonest == i64::MAX { 1 } else { ((soonest - now).max(1000) / 1000) as u64 };
        let no_keys = !inner.keys.values().any(|k| k.row.enabled);
        Unavailable { retry_after_secs, all_out_of_credit: all_credit && !no_keys, no_keys }
    }

    pub fn pinned(&self, resource_id: &str) -> Option<String> {
        self.inner.lock().pins.get(resource_id).cloned()
    }

    pub fn has_pins(&self) -> bool {
        !self.inner.lock().pins.is_empty()
    }

    pub fn pin(&self, resource_id: &str, key: &str) {
        let mut inner = self.inner.lock();
        if inner.pins.get(resource_id).map(String::as_str) == Some(key) {
            return;
        }
        inner.pins.insert(resource_id.to_string(), key.to_string());
        drop(inner);
        self.store.save_pin(resource_id.to_string(), key.to_string());
    }

    // ---------------------------------------------------------------- health

    fn update_key(&self, name: &str, f: impl FnOnce(&mut KeyInfo)) {
        let mut inner = self.inner.lock();
        let Some(k) = inner.keys.get_mut(name) else { return };
        f(k);
        let row = k.row.clone();
        drop(inner);
        self.store.save_key(row);
    }

    fn cooldown(&self, name: &str, secs: u64, reason: String) {
        let until = now_ms() + secs as i64 * 1000;
        self.update_key(name, |k| {
            k.last_error = Some(reason.clone());
            if until > k.row.cooldown_until_ms {
                k.row.cooldown_until_ms = until;
                k.row.reason = Some(reason);
            }
        });
    }

    /// Out of credits: no routing until a credit check succeeds; next check in
    /// `credit_exhausted_secs`.
    pub fn on_credit_exhausted(&self, name: &str, message: &str) {
        let now = now_ms();
        let next = now + self.cooldowns.credit_exhausted_secs as i64 * 1000;
        let mut first = false;
        self.update_key(name, |k| {
            first = k.row.exhausted_ms.is_none();
            k.row.exhausted_ms.get_or_insert(now);
            k.row.cooldown_until_ms = next;
            k.row.reason = Some(format!("out of credits: {message}"));
            k.last_error = k.row.reason.clone();
        });
        if first {
            tracing::warn!(key = %name, "out of credits; held out of rotation, re-checking every {}s", self.cooldowns.credit_exhausted_secs);
        }
    }

    /// Out-of-credit keys whose next check is due (marked in flight), up to `max` running at once.
    pub fn due_credit_checks(&self, max: usize) -> Vec<String> {
        let now = now_ms();
        let mut inner = self.inner.lock();
        let room = max.saturating_sub(inner.checking.len());
        let due: Vec<String> = inner
            .keys
            .values()
            .filter(|k| k.row.enabled && k.row.exhausted_ms.is_some() && k.row.cooldown_until_ms <= now)
            .map(|k| k.row.name.clone())
            .filter(|n| !inner.checking.contains(n))
            .take(room)
            .collect();
        inner.checking.extend(due.iter().cloned());
        due
    }

    /// After a credit check: if the key is still out of credits but the check didn't
    /// schedule the next one (e.g. the check hit a 5xx), schedule it now so it isn't retried in a loop.
    pub fn finish_credit_check(&self, name: &str) {
        let now = now_ms();
        let next = now + self.cooldowns.credit_exhausted_secs as i64 * 1000;
        self.inner.lock().checking.remove(name);
        let due = self.inner.lock().keys.get(name).map_or(false, |k| k.row.exhausted_ms.is_some() && k.row.cooldown_until_ms <= now);
        if due {
            self.update_key(name, |k| k.row.cooldown_until_ms = next);
        }
    }

    pub fn on_auth_failed(&self, name: &str, message: &str) {
        tracing::warn!(key = %name, "authentication failed: {message}");
        self.cooldown(name, self.cooldowns.auth_failure_secs, format!("authentication failed: {message}"));
    }

    pub fn on_rate_limited(&self, name: &str, retry_after: Option<u64>) {
        let secs = retry_after.unwrap_or(self.cooldowns.rate_limit_secs).max(1);
        tracing::info!(key = %name, "rate limited; cooling down for {secs}s");
        self.cooldown(name, secs, "rate limited".into());
    }

    pub fn on_overloaded(&self, name: &str, status: u16) {
        if self.cooldowns.overloaded_secs > 0 {
            self.cooldown(name, self.cooldowns.overloaded_secs, format!("upstream {status}"));
        }
    }

    /// A successful billable call proves the key has credit again.
    pub fn on_success(&self, name: &str) {
        let needs = self.inner.lock().keys.get(name).map_or(false, |k| k.row.exhausted_ms.is_some());
        if needs {
            tracing::info!(key = %name, "key has credit again");
            self.update_key(name, |k| {
                k.row.exhausted_ms = None;
                k.row.reason = None;
                k.row.cooldown_until_ms = 0;
            });
        }
    }

    /// Called by the store writer after a request row is durably inserted.
    pub fn on_insert(&self, row: &RequestRow) {
        let Some(cost) = row.cost_usd else { return };
        let mut inner = self.inner.lock();
        *inner.client_spend.entry(row.client.clone()).or_default() += cost;
        if let Some(k) = row.key_name.as_deref().and_then(|n| inner.keys.get_mut(n)) {
            k.spend += cost;
            if k.row.balance_usd.is_some() && row.ts_ms >= k.row.balance_set_ms {
                k.spend_since_balance += cost;
            }
        }
    }

    // ---------------------------------------------------------------- clients

    pub fn authenticate(&self, token: &str) -> Option<ClientAuth> {
        let hash = hash_token(token);
        let inner = self.inner.lock();
        let name = inner.by_hash.get(&hash)?;
        let c = inner.clients.get(name)?;
        if !c.row.enabled {
            return None;
        }
        let spend = inner.client_spend.get(name).copied().unwrap_or(0.0);
        Some(ClientAuth { name: name.clone(), over_budget: c.row.budget_usd.map_or(false, |b| spend >= b) })
    }

    pub fn client_spend(&self, client: &str) -> f64 {
        self.inner.lock().client_spend.get(client).copied().unwrap_or(0.0)
    }

    pub fn client_budget(&self, client: &str) -> Option<f64> {
        self.inner.lock().clients.get(client).and_then(|c| c.row.budget_usd)
    }

    /// Create a client and return its token (shown once; only the hash is stored).
    pub fn add_client(&self, name: &str, budget_usd: Option<f64>) -> Result<String> {
        validate_name(name)?;
        let token = new_client_token();
        let mut inner = self.inner.lock();
        if inner.clients.contains_key(name) {
            bail!("a client named {name} already exists");
        }
        let row = ClientRow {
            name: name.to_string(),
            token_hash: hash_token(&token),
            token_hint: hint(&token),
            enabled: true,
            budget_usd,
            created_ms: now_ms(),
            deleted_ms: None,
        };
        inner.by_hash.insert(row.token_hash.clone(), name.to_string());
        inner.clients.insert(name.to_string(), ClientInfo { row: row.clone() });
        drop(inner);
        self.store.save_client(row);
        Ok(token)
    }

    /// Replace a client's token; the old one stops working immediately.
    pub fn rotate_client(&self, name: &str) -> Result<String> {
        let token = new_client_token();
        let mut inner = self.inner.lock();
        let Some(c) = inner.clients.get_mut(name) else { bail!("unknown client {name}") };
        let old = std::mem::replace(&mut c.row.token_hash, hash_token(&token));
        c.row.token_hint = hint(&token);
        let row = c.row.clone();
        inner.by_hash.remove(&old);
        inner.by_hash.insert(row.token_hash.clone(), name.to_string());
        drop(inner);
        self.store.save_client(row);
        Ok(token)
    }

    pub fn update_client(&self, name: &str, enabled: Option<bool>, budget_usd: Option<Option<f64>>) -> Result<()> {
        let mut inner = self.inner.lock();
        let Some(c) = inner.clients.get_mut(name) else { bail!("unknown client {name}") };
        if let Some(e) = enabled {
            c.row.enabled = e;
        }
        if let Some(b) = budget_usd {
            c.row.budget_usd = b;
        }
        let row = c.row.clone();
        drop(inner);
        self.store.save_client(row);
        Ok(())
    }

    pub fn remove_client(&self, name: &str) -> Result<()> {
        let mut inner = self.inner.lock();
        let Some(c) = inner.clients.remove(name) else { bail!("unknown client {name}") };
        inner.by_hash.remove(&c.row.token_hash);
        drop(inner);
        let mut row = c.row;
        row.enabled = false;
        row.deleted_ms = Some(now_ms());
        self.store.save_client(row);
        Ok(())
    }

    pub fn clients_json(&self) -> Value {
        let inner = self.inner.lock();
        let list: Vec<Value> = inner
            .clients
            .values()
            .map(|c| {
                let spend = inner.client_spend.get(&c.row.name).copied().unwrap_or(0.0);
                json!({
                    "name": c.row.name,
                    "token_hint": c.row.token_hint,
                    "enabled": c.row.enabled,
                    "budget_usd": c.row.budget_usd,
                    "spend_usd": round6(spend),
                    "remaining_budget_usd": c.row.budget_usd.map(|b| round6(b - spend)),
                    "created_ms": c.row.created_ms,
                })
            })
            .collect();
        Value::Array(list)
    }

    // ---------------------------------------------------------------- key admin

    pub fn add_key(&self, name: &str, secret: &str, balance_usd: Option<f64>) -> Result<()> {
        validate_name(name)?;
        let secret = secret.trim();
        if secret.is_empty() {
            bail!("the API key is empty");
        }
        let now = now_ms();
        let mut inner = self.inner.lock();
        if inner.keys.contains_key(name) {
            bail!("a key named {name} already exists");
        }
        if inner.keys.values().any(|k| k.row.secret == secret) {
            bail!("this API key is already in the pool");
        }
        let row = KeyRow {
            name: name.to_string(),
            secret: secret.to_string(),
            enabled: true,
            created_ms: now,
            balance_usd,
            balance_set_ms: now,
            ..Default::default()
        };
        // Spend from an earlier key of the same name stays in history but not in this key's totals.
        inner.keys.insert(
            name.to_string(),
            KeyInfo { row: row.clone(), spend: 0.0, spend_since_balance: 0.0, requests: 0, last_used_ms: 0, last_error: None },
        );
        drop(inner);
        self.store.save_key(row);
        Ok(())
    }

    /// Change a key. `balance_usd: Some(x)` records a known balance now (x = None clears it).
    pub fn edit_key(&self, name: &str, secret: Option<&str>, enabled: Option<bool>, balance_usd: Option<Option<f64>>) -> Result<()> {
        let mut inner = self.inner.lock();
        if let Some(s) = secret.map(str::trim) {
            if s.is_empty() {
                bail!("the API key is empty");
            }
            if inner.keys.values().any(|k| k.row.secret == s && k.row.name != name) {
                bail!("this API key is already in the pool");
            }
        }
        let Some(k) = inner.keys.get_mut(name) else { bail!("unknown key {name}") };
        if let Some(s) = secret {
            // A new secret is a new credential: forget its predecessor's failures.
            k.row.secret = s.trim().to_string();
            k.row.cooldown_until_ms = 0;
            k.row.reason = None;
            k.row.exhausted_ms = None;
            k.last_error = None;
        }
        if let Some(e) = enabled {
            k.row.enabled = e;
        }
        if let Some(b) = balance_usd {
            k.row.balance_usd = b;
            k.row.balance_set_ms = now_ms();
            k.spend_since_balance = 0.0;
            if b.map_or(false, |b| b > 0.0) && k.row.exhausted_ms.is_some() {
                // A top-up: check the key now instead of waiting for the next scheduled check.
                k.row.cooldown_until_ms = 0;
            }
        }
        let row = k.row.clone();
        drop(inner);
        self.store.save_key(row);
        Ok(())
    }

    pub fn remove_key(&self, name: &str) -> Result<()> {
        let mut inner = self.inner.lock();
        let Some(k) = inner.keys.remove(name) else { bail!("unknown key {name}") };
        drop(inner);
        let mut row = k.row;
        row.enabled = false;
        row.deleted_ms = Some(now_ms());
        self.store.save_key(row);
        Ok(())
    }

    /// Clear cooldowns before a manual check. An out-of-credits key stays out of
    /// rotation until that check succeeds, so a failed or inconclusive check can't
    /// put an empty key back into traffic.
    pub fn reset_key(&self, name: &str) -> Result<()> {
        if !self.inner.lock().keys.contains_key(name) {
            bail!("unknown key {name}");
        }
        self.update_key(name, |k| {
            k.row.cooldown_until_ms = 0;
            if k.row.exhausted_ms.is_none() {
                k.row.reason = None;
            }
        });
        Ok(())
    }

    pub fn keys_json(&self) -> Value {
        let now = now_ms();
        let inner = self.inner.lock();
        let secs_left = |until: i64| if until > now { (until - now + 999) / 1000 } else { 0 };
        let list: Vec<Value> = inner
            .keys
            .values()
            .map(|k| {
                let cooling = k.row.cooldown_until_ms > now;
                let state = if !k.row.enabled {
                    "disabled"
                } else if k.row.exhausted_ms.is_some() {
                    "out_of_credits"
                } else if cooling {
                    "cooling_down"
                } else {
                    "available"
                };
                json!({
                    "name": k.row.name,
                    "key_hint": hint(&k.row.secret),
                    "enabled": k.row.enabled,
                    "state": state,
                    "available": Self::available(k, now),
                    "cooldown_secs": secs_left(k.row.cooldown_until_ms),
                    "reason": if cooling || k.row.exhausted_ms.is_some() { k.row.reason.clone() } else { None },
                    "last_error": k.last_error,
                    "out_of_credits_since_ms": k.row.exhausted_ms,
                    "balance_usd": k.row.balance_usd,
                    "balance_set_ms": k.row.balance_set_ms,
                    "spend_since_balance_usd": round6(k.spend_since_balance),
                    "estimated_remaining_usd": k.row.balance_usd.map(|b| round6(b - k.spend_since_balance)),
                    "spend_usd": round6(k.spend),
                    "requests_since_start": k.requests,
                    "last_used_ms": if k.last_used_ms > 0 { Some(k.last_used_ms) } else { None },
                    "created_ms": k.row.created_ms,
                })
            })
            .collect();
        Value::Array(list)
    }

    pub fn status(&self) -> Value {
        let pins = self.inner.lock().pins.len();
        json!({ "keys": self.keys_json(), "clients": self.clients_json(), "pins": pins })
    }
}
