//! `/proxy/*` endpoints: the admin API behind the web UI (keys, clients, status, usage)
//! plus the client-facing `/proxy/me` and `/proxy/requests/{id}`.

use crate::pool::validate_name;
use crate::pricing::Usage;
use crate::proxy::{api_error, classify_error, client_token, ct_eq, find_client, retry_after_secs, ErrClass};
use crate::store::RequestRow;
use crate::{now_ms, AppState};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/status", get(status))
        .route("/usage", get(usage))
        .route("/me", get(me))
        .route("/requests/{id}", get(request))
        .route("/admin/keys", get(list_keys).post(add_key))
        .route("/admin/keys/{name}", axum::routing::patch(edit_key).delete(remove_key))
        .route("/admin/keys/{name}/refresh", post(refresh_key))
        .route("/admin/clients", get(list_clients).post(add_client))
        .route("/admin/clients/{name}", axum::routing::patch(edit_client).delete(remove_client))
        .route("/admin/clients/{name}/rotate", post(rotate_client))
        .route("/admin/requests", get(recent_requests))
}

enum Caller {
    Admin,
    Client(String),
}

fn caller(st: &AppState, h: &HeaderMap) -> Option<Caller> {
    let tok = client_token(h)?;
    if ct_eq(&st.admin_token, tok) {
        return Some(Caller::Admin);
    }
    find_client(st, h).map(|c| Caller::Client(c.name))
}

fn require_admin(st: &AppState, h: &HeaderMap) -> Result<(), Response> {
    match caller(st, h) {
        Some(Caller::Admin) => Ok(()),
        _ => Err(api_error(StatusCode::UNAUTHORIZED, "authentication_error", "admin token required")),
    }
}

fn bad_request(e: impl std::fmt::Display) -> Response {
    api_error(StatusCode::BAD_REQUEST, "invalid_request_error", &e.to_string())
}

fn internal(e: impl std::fmt::Display) -> Response {
    api_error(StatusCode::INTERNAL_SERVER_ERROR, "api_error", &e.to_string())
}

/// `absent` -> None, `null` -> Some(None), number -> Some(Some(x)).
fn tri_f64(v: &Value, field: &str) -> Result<Option<Option<f64>>, Response> {
    match v.get(field) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(x) => match x.as_f64() {
            Some(f) if f.is_finite() && f >= 0.0 => Ok(Some(Some(f))),
            _ => Err(bad_request(format!("{field} must be a non-negative number or null"))),
        },
    }
}

macro_rules! admin {
    ($st:expr, $h:expr) => {
        if let Err(r) = require_admin(&$st, &$h) {
            return r;
        }
    };
}

// ------------------------------------------------------------------- status & usage

async fn status(State(st): State<Arc<AppState>>, h: HeaderMap) -> Response {
    admin!(st, h);
    let mut v = st.pool.status();
    v["check_model"] = json!(st.cfg.check_model);
    v["version"] = json!(crate::VERSION);
    v["cooldowns"] = json!({
        "credit_exhausted_secs": st.cfg.cooldowns.credit_exhausted_secs,
        "auth_failure_secs": st.cfg.cooldowns.auth_failure_secs,
    });
    Json(v).into_response()
}

#[derive(Deserialize)]
struct UsageQuery {
    /// Unix seconds; default: last 30 days.
    since: Option<i64>,
    /// key | client | model | day (default model).
    group_by: Option<String>,
    client: Option<String>,
}

async fn usage(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<UsageQuery>) -> Response {
    let client_filter = match caller(&st, &h) {
        Some(Caller::Admin) => q.client.clone(),
        Some(Caller::Client(name)) => Some(name),
        None => return api_error(StatusCode::UNAUTHORIZED, "authentication_error", "token required"),
    };
    let since_ms = q.since.map(|s| s * 1000).unwrap_or_else(|| now_ms() - 30 * 86_400_000);
    let group_by = q.group_by.unwrap_or_else(|| "model".into());
    let st2 = st.clone();
    let res = tokio::task::spawn_blocking(move || st2.store.usage_summary(since_ms, &group_by, client_filter.as_deref())).await;
    match res {
        Ok(Ok(v)) => Json(json!({"since_ms": since_ms, "rows": v})).into_response(),
        Ok(Err(e)) => bad_request(e),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct RecentQuery {
    limit: Option<u32>,
}

async fn recent_requests(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<RecentQuery>) -> Response {
    admin!(st, h);
    let limit = q.limit.unwrap_or(50).min(500);
    let st2 = st.clone();
    match tokio::task::spawn_blocking(move || st2.store.recent_requests(limit)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

async fn me(State(st): State<Arc<AppState>>, h: HeaderMap) -> Response {
    let Some(c) = find_client(&st, &h) else {
        return api_error(StatusCode::UNAUTHORIZED, "authentication_error", "client token required");
    };
    let spend = st.pool.client_spend(&c.name);
    let budget = st.pool.client_budget(&c.name);
    Json(json!({
        "name": c.name,
        "spend_usd": crate::pool::round6(spend),
        "budget_usd": budget,
        "remaining_budget_usd": budget.map(|b| crate::pool::round6(b - spend)),
    }))
    .into_response()
}

async fn request(State(st): State<Arc<AppState>>, h: HeaderMap, Path(id): Path<String>) -> Response {
    let who = caller(&st, &h);
    if who.is_none() {
        return api_error(StatusCode::UNAUTHORIZED, "authentication_error", "token required");
    }
    let st2 = st.clone();
    let row = match tokio::task::spawn_blocking(move || st2.store.get_request(&id)).await {
        Ok(Ok(Some(row))) => row,
        Ok(Ok(None)) => return api_error(StatusCode::NOT_FOUND, "not_found_error", "unknown request id (rows are written within a few ms)"),
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    if let Some(Caller::Client(name)) = who {
        if row["client"].as_str() != Some(name.as_str()) {
            return api_error(StatusCode::NOT_FOUND, "not_found_error", "unknown request id");
        }
    }
    Json(row).into_response()
}

// ------------------------------------------------------------------- upstream keys

async fn list_keys(State(st): State<Arc<AppState>>, h: HeaderMap) -> Response {
    admin!(st, h);
    Json(st.pool.keys_json()).into_response()
}

#[derive(Deserialize)]
struct AddKey {
    name: String,
    key: String,
    balance_usd: Option<f64>,
    /// Send a 1-token request to verify the key (default true).
    check: Option<bool>,
}

async fn add_key(State(st): State<Arc<AppState>>, h: HeaderMap, Json(b): Json<AddKey>) -> Response {
    admin!(st, h);
    let name = b.name.trim().to_string();
    if let Err(e) = st.pool.add_key(&name, &b.key, b.balance_usd) {
        return bad_request(e);
    }
    let check = if b.check.unwrap_or(true) { check_key(&st, &name).await } else { Value::Null };
    (StatusCode::CREATED, Json(json!({"ok": true, "name": name, "check": check}))).into_response()
}

async fn edit_key(State(st): State<Arc<AppState>>, h: HeaderMap, Path(name): Path<String>, Json(b): Json<Value>) -> Response {
    admin!(st, h);
    let balance = match tri_f64(&b, "balance_usd") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let secret = b.get("key").and_then(Value::as_str);
    let enabled = b.get("enabled").and_then(Value::as_bool);
    if let Err(e) = st.pool.edit_key(&name, secret, enabled, balance) {
        return bad_request(e);
    }
    // A replaced secret gets checked straight away.
    let check = if secret.is_some() { check_key(&st, &name).await } else { Value::Null };
    Json(json!({"ok": true, "name": name, "check": check})).into_response()
}

async fn remove_key(State(st): State<Arc<AppState>>, h: HeaderMap, Path(name): Path<String>) -> Response {
    admin!(st, h);
    match st.pool.remove_key(&name) {
        Ok(()) => Json(json!({"ok": true, "name": name})).into_response(),
        Err(e) => api_error(StatusCode::NOT_FOUND, "not_found_error", &e.to_string()),
    }
}

/// Clear the key's cooldown and check it right away.
async fn refresh_key(State(st): State<Arc<AppState>>, h: HeaderMap, Path(name): Path<String>) -> Response {
    admin!(st, h);
    if let Err(e) = st.pool.reset_key(&name) {
        return api_error(StatusCode::NOT_FOUND, "not_found_error", &e.to_string());
    }
    let check = check_key(&st, &name).await;
    Json(json!({"ok": true, "name": name, "check": check})).into_response()
}

/// Send a minimal billable request (1 output token on the cheapest model) with this key.
/// Only a billable call can tell whether the key's organization has credit; a model
/// listing would succeed with an empty balance. Updates the key's state like real traffic.
async fn check_key(st: &Arc<AppState>, name: &str) -> Value {
    let Some(key) = st.pool.get(name) else { return json!({"ok": false, "message": "unknown key"}) };
    let model = st.cfg.check_model.clone();
    let body = json!({"model": model, "max_tokens": 1, "messages": [{"role": "user", "content": "ping"}]});
    let sent = st
        .http
        .post(format!("{}/v1/messages", st.upstream))
        .header("x-api-key", &key.secret)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await;
    let resp = match sent {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "message": format!("could not reach upstream: {e}")}),
    };
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let v: Value = resp.bytes().await.ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let mut row = RequestRow {
        request_id: format!("ckr_{}", uuid::Uuid::new_v4().simple()),
        ts_ms: now_ms(),
        client: "_key_check".into(),
        key_name: Some(name.to_string()),
        method: "POST".into(),
        path: "/v1/messages".into(),
        model: Some(model.clone()),
        status,
        streamed: false,
        usage: Usage::default(),
        cost_usd: None,
        upstream_request_id: headers.get("request-id").and_then(|v| v.to_str().ok()).map(String::from),
        batch_item: None,
        attempts: 1,
        error: None,
    };
    if (200..300).contains(&status) {
        let usage = v.get("usage").map(Usage::from_value).unwrap_or_default();
        let m = v.get("model").and_then(Value::as_str).unwrap_or(&model).to_string();
        row.cost_usd = st.pricing.cost(&m, &usage, false).map(crate::pool::round6);
        row.usage = usage;
        let cost = row.cost_usd;
        st.store.record_request(row);
        st.pool.on_success(name);
        return json!({"ok": true, "status": status, "message": "Key works and has credit.", "cost_usd": cost});
    }
    let etype = v.pointer("/error/type").and_then(Value::as_str).unwrap_or("").to_string();
    let emsg = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("").to_string();
    let (ok, message) = match classify_error(status, &etype, &emsg) {
        ErrClass::Credit => {
            st.pool.on_credit_exhausted(name, &emsg);
            (false, "Out of credits.".to_string())
        }
        ErrClass::Auth => {
            st.pool.on_auth_failed(name, &emsg);
            (false, "Invalid or revoked key.".to_string())
        }
        ErrClass::RateLimit => {
            st.pool.on_rate_limited(name, retry_after_secs(&headers));
            // The key itself is valid; it is just busy.
            (true, "Key is valid but currently rate limited.".to_string())
        }
        _ => (false, format!("Upstream answered {status}.")),
    };
    row.error = Some(format!("{etype}: {emsg}"));
    st.store.record_request(row);
    json!({"ok": ok, "status": status, "message": message, "error_type": etype, "error_message": emsg})
}

// ------------------------------------------------------------------- clients

async fn list_clients(State(st): State<Arc<AppState>>, h: HeaderMap) -> Response {
    admin!(st, h);
    Json(st.pool.clients_json()).into_response()
}

#[derive(Deserialize)]
struct AddClient {
    name: String,
    budget_usd: Option<f64>,
}

async fn add_client(State(st): State<Arc<AppState>>, h: HeaderMap, Json(b): Json<AddClient>) -> Response {
    admin!(st, h);
    let name = b.name.trim().to_string();
    if let Err(e) = validate_name(&name) {
        return bad_request(e);
    }
    if b.budget_usd.map_or(false, |x| !x.is_finite() || x < 0.0) {
        return bad_request("budget_usd must be a non-negative number");
    }
    match st.pool.add_client(&name, b.budget_usd) {
        Ok(token) => (StatusCode::CREATED, Json(json!({"ok": true, "name": name, "token": token}))).into_response(),
        Err(e) => bad_request(e),
    }
}

async fn edit_client(State(st): State<Arc<AppState>>, h: HeaderMap, Path(name): Path<String>, Json(b): Json<Value>) -> Response {
    admin!(st, h);
    let budget = match tri_f64(&b, "budget_usd") {
        Ok(v) => v,
        Err(r) => return r,
    };
    match st.pool.update_client(&name, b.get("enabled").and_then(Value::as_bool), budget) {
        Ok(()) => Json(json!({"ok": true, "name": name})).into_response(),
        Err(e) => bad_request(e),
    }
}

async fn remove_client(State(st): State<Arc<AppState>>, h: HeaderMap, Path(name): Path<String>) -> Response {
    admin!(st, h);
    match st.pool.remove_client(&name) {
        Ok(()) => Json(json!({"ok": true, "name": name})).into_response(),
        Err(e) => api_error(StatusCode::NOT_FOUND, "not_found_error", &e.to_string()),
    }
}

async fn rotate_client(State(st): State<Arc<AppState>>, h: HeaderMap, Path(name): Path<String>) -> Response {
    admin!(st, h);
    match st.pool.rotate_client(&name) {
        Ok(token) => Json(json!({"ok": true, "name": name, "token": token})).into_response(),
        Err(e) => api_error(StatusCode::NOT_FOUND, "not_found_error", &e.to_string()),
    }
}
