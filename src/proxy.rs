//! The catch-all proxy handler: authenticates the client, picks an upstream key,
//! forwards the request (failing over between keys when that is safe), and meters cost.

use crate::config::Affinity;
use crate::pool::{round6, ClientAuth, Picked, Unavailable};
use crate::pricing::Usage;
use crate::store::RequestRow;
use crate::tee::{FrameHandler, Framing, Tee};
use crate::{now_ms, AppState};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Multipart bodies larger than this are streamed straight through (no failover).
const STREAM_MULTIPART_OVER: u64 = 8 * 1024 * 1024;
/// Cap on error bodies read from upstream.
const MAX_ERROR_BODY: usize = 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Messages,
    CountTokens,
    BatchCreate,
    BatchResults,
    Other,
}

fn classify(method: &Method, path: &str) -> Kind {
    let path = path.trim_end_matches('/');
    match (method, path) {
        (&Method::POST, "/v1/messages") => Kind::Messages,
        (&Method::POST, "/v1/messages/count_tokens") => Kind::CountTokens,
        (&Method::POST, "/v1/messages/batches") => Kind::BatchCreate,
        (&Method::GET, p) if p.starts_with("/v1/messages/batches/") && p.ends_with("/results") => Kind::BatchResults,
        _ => Kind::Other,
    }
}

pub fn api_error(status: StatusCode, etype: &str, message: &str) -> Response {
    let body = json!({"type": "error", "error": {"type": etype, "message": message}});
    (status, [("content-type", "application/json")], body.to_string()).into_response()
}

pub fn client_token(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(v.trim());
    }
    let auth = headers.get("authorization")?.to_str().ok()?;
    auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")).map(str::trim)
}

pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn find_client(st: &AppState, headers: &HeaderMap) -> Option<ClientAuth> {
    st.pool.authenticate(client_token(headers)?)
}

fn skip_request_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "content-length"
            | "x-api-key"
            | "authorization"
            | "connection"
            | "keep-alive"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "accept-encoding"
    ) || name.starts_with("x-proxy-")
}

fn skip_response_header(name: &str, keep_length: bool) -> bool {
    matches!(name, "connection" | "keep-alive" | "transfer-encoding" | "te" | "trailer" | "upgrade" | "proxy-connection")
        || (!keep_length && (name == "content-length" || name == "content-encoding"))
}

fn upstream_headers(incoming: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in incoming {
        let n = name.as_str();
        if skip_request_header(n) {
            continue;
        }
        if n == "anthropic-beta" {
            // OAuth betas only make sense with a bearer token; upstream gets an API key.
            let kept: Vec<&str> = value
                .to_str()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|b| !b.is_empty() && !b.starts_with("oauth-"))
                .collect();
            if !kept.is_empty() {
                if let Ok(v) = HeaderValue::from_str(&kept.join(",")) {
                    out.append(name.clone(), v);
                }
            }
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

fn affinity_key(mode: Affinity, client: &str, kind: Kind, body: Option<&Value>, headers: &HeaderMap) -> Vec<u8> {
    if let Some(v) = headers.get("x-proxy-affinity") {
        return v.as_bytes().to_vec();
    }
    let v = match (kind, body) {
        (Kind::Messages | Kind::CountTokens, Some(v)) if mode != Affinity::Client => v,
        _ => return client.as_bytes().to_vec(),
    };
    let mut h = Sha256::new();
    for part in ["model", "tools", "system"] {
        h.update(serde_json::to_vec(v.get(part).unwrap_or(&Value::Null)).unwrap_or_default());
        h.update([0u8]);
    }
    if mode == Affinity::Conversation {
        h.update(serde_json::to_vec(v.pointer("/messages/0").unwrap_or(&Value::Null)).unwrap_or_default());
    }
    h.finalize().to_vec()
}

/// Find a resource id (file, batch, agent, session, container...) that pins this request to a key.
fn find_pin_in_json(st: &AppState, v: &Value) -> Option<String> {
    match v {
        Value::String(s) if (8..=128).contains(&s.len()) => st.pool.pinned(s),
        Value::Array(a) => a.iter().find_map(|x| find_pin_in_json(st, x)),
        Value::Object(m) => m.values().find_map(|x| find_pin_in_json(st, x)),
        _ => None,
    }
}

fn public_base(st: &AppState, headers: &HeaderMap) -> String {
    if let Some(u) = &st.cfg.public_url {
        return u.trim_end_matches('/').to_string();
    }
    let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("127.0.0.1");
    format!("http://{host}")
}

/// Rewrite every absolute `results_url` (in a batch object or a list's `data`) to `base`.
fn rewrite_results_urls(v: &mut Value, base: &str) -> bool {
    let mut changed = false;
    let mut fix = |b: &mut Value| {
        if let Some(Value::String(u)) = b.get_mut("results_url") {
            if let Some(rest) = u.split_once("://").map(|(_, r)| r) {
                let path = rest.find('/').map_or("/", |i| &rest[i..]).to_string();
                *u = format!("{base}{path}");
                changed = true;
            }
        }
    };
    match v.get_mut("data").and_then(Value::as_array_mut) {
        Some(items) => items.iter_mut().for_each(&mut fix),
        None => fix(v),
    }
    changed
}

#[derive(Debug, PartialEq, Eq)]
pub enum ErrClass {
    /// Organization out of credits: cool the whole group down, try another group.
    Credit,
    /// Invalid/revoked key: cool the key down, try another key.
    Auth,
    /// 429: cool the key down for retry-after, try another key.
    RateLimit,
    /// 5xx/529 or a key-specific 403: try another key.
    Transient,
    /// The request itself is bad; return to the client.
    Final,
}

pub fn classify_error(status: u16, etype: &str, message: &str) -> ErrClass {
    let msg = message.to_ascii_lowercase();
    if status == 402 || etype == "billing_error" || msg.contains("credit balance") {
        return ErrClass::Credit;
    }
    match status {
        401 => ErrClass::Auth,
        429 => ErrClass::RateLimit,
        403 => ErrClass::Transient,
        500 | 502 | 503 | 504 | 529 => ErrClass::Transient,
        _ => ErrClass::Final,
    }
}

pub fn retry_after_secs(h: &HeaderMap) -> Option<u64> {
    let v = h.get("retry-after")?.to_str().ok()?;
    v.trim().parse::<f64>().ok().map(|s| s.ceil().max(1.0) as u64)
}

/// Everything needed to write the usage row once the response is complete.
#[derive(Clone)]
struct Meter {
    st: Arc<AppState>,
    row: RequestRow,
    key: String,
}

impl Meter {
    fn cost(&self, model: &str, usage: &Usage, batch: bool) -> Option<f64> {
        self.st.pricing.cost(model, usage, batch).map(round6)
    }

    fn record(&self, model: Option<String>, usage: Usage, cost: Option<f64>, error: Option<String>) {
        let mut row = self.row.clone();
        row.model = model;
        row.usage = usage;
        row.cost_usd = cost;
        if error.is_some() {
            row.error = error;
        }
        self.st.store.record_request(row);
    }

    fn pin_container(&self, v: Option<&Value>) {
        if let Some(id) = v.and_then(|c| c.get("id")).and_then(Value::as_str) {
            self.st.pool.pin(id, &self.key);
        }
    }
}

struct MessagesSse {
    meter: Meter,
    model: Option<String>,
    usage: Usage,
}

impl FrameHandler for MessagesSse {
    fn frame(&mut self, frame: &[u8]) -> Option<Bytes> {
        let first_line = frame.split(|&c| c == b'\n').next().unwrap_or(&[]);
        let is_start = first_line.ends_with(b"message_start") || first_line.ends_with(b"message_start\r");
        let is_delta = first_line.ends_with(b"message_delta") || first_line.ends_with(b"message_delta\r");
        if !is_start && !is_delta {
            return None;
        }
        let text = std::str::from_utf8(frame).ok()?;
        let data: String = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|d| d.strip_prefix(' ').unwrap_or(d))
            .collect::<Vec<_>>()
            .join("\n");
        let v: Value = serde_json::from_str(&data).ok()?;
        if is_start {
            let msg = v.get("message")?;
            self.model = msg.get("model").and_then(Value::as_str).map(String::from);
            if let Some(u) = msg.get("usage") {
                self.usage.merge(u);
            }
            self.meter.pin_container(msg.get("container"));
            return None;
        }
        if let Some(u) = v.get("usage") {
            self.usage.merge(u);
        }
        self.meter.pin_container(v.pointer("/delta/container"));
        None
    }

    fn finish(&mut self, complete: bool) {
        let cost = self.model.as_deref().and_then(|m| self.meter.cost(m, &self.usage, false));
        let err = (!complete).then(|| "stream ended early (client disconnect or upstream error)".to_string());
        self.meter.record(self.model.clone(), std::mem::take(&mut self.usage), cost, err);
    }
}

struct BatchResults {
    meter: Meter,
    batch_id: String,
}

impl FrameHandler for BatchResults {
    fn frame(&mut self, frame: &[u8]) -> Option<Bytes> {
        let v: Value = serde_json::from_slice(frame).ok()?;
        if v.pointer("/result/type").and_then(Value::as_str) != Some("succeeded") {
            return None;
        }
        let custom_id = v.get("custom_id").and_then(Value::as_str).unwrap_or("").to_string();
        let msg = v.pointer("/result/message")?;
        let model = msg.get("model").and_then(Value::as_str).map(String::from);
        let usage = Usage::from_value(msg.get("usage")?);
        let cost = model.as_deref().and_then(|m| self.meter.cost(m, &usage, true));
        let mut row = self.meter.row.clone();
        row.model = model;
        row.usage = usage;
        row.cost_usd = cost;
        row.batch_item = Some(format!("{}:{}", self.batch_id, custom_id));
        self.meter.st.store.record_request(row);
        None
    }

    fn finish(&mut self, _complete: bool) {}
}

fn add_proxy_headers(h: &mut HeaderMap, rid: &str, key: Option<&str>, attempts: u32) {
    h.insert("x-proxy-request-id", HeaderValue::from_str(rid).unwrap());
    if let Some(k) = key.and_then(|k| HeaderValue::from_str(k).ok()) {
        h.insert("x-proxy-upstream-key", k);
    }
    h.insert("x-proxy-attempts", HeaderValue::from(attempts));
}

fn response_with(status: StatusCode, up: &HeaderMap, keep_length: bool, body: Body) -> Response {
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    for (name, value) in up {
        if !skip_response_header(name.as_str(), keep_length) {
            h.append(name.clone(), value.clone());
        }
    }
    resp
}

fn unavailable_response(u: &Unavailable) -> Response {
    if u.no_keys {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "api_error", "No upstream API keys are configured (proxy). Add one in the web UI.");
    }
    let mut resp = if u.all_out_of_credit {
        api_error(
            StatusCode::PAYMENT_REQUIRED,
            "billing_error",
            &format!("All upstream API keys are out of credits (proxy). Next retry in {}s.", u.retry_after_secs),
        )
    } else {
        api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            &format!("No upstream API key is currently available (proxy). Retry in {}s.", u.retry_after_secs),
        )
    };
    resp.headers_mut().insert("retry-after", HeaderValue::from(u.retry_after_secs));
    resp
}

enum Body0 {
    Buffered(Bytes),
    Streaming(Option<Body>),
}

pub async fn handle(State(st): State<Arc<AppState>>, req: Request) -> Response {
    let rid = format!("ckr_{}", uuid::Uuid::new_v4().simple());
    let started = now_ms();
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let path_q = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| path.clone());
    let method = parts.method.clone();

    // ---- client authentication and budget
    let client_name = match find_client(&st, &parts.headers) {
        Some(c) => {
            if c.over_budget {
                return api_error(StatusCode::PAYMENT_REQUIRED, "billing_error", "Proxy budget for this client is exhausted.");
            }
            c.name
        }
        None if st.cfg.allow_unauthenticated => "anonymous".to_string(),
        None => return api_error(StatusCode::UNAUTHORIZED, "authentication_error", "invalid proxy x-api-key"),
    };

    // ---- request body
    let kind = classify(&method, &path);
    let ctype = parts.headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let clen: Option<u64> = parts.headers.get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
    let max_buf = st.cfg.routing.max_buffer_bytes;
    let stream_through = (ctype.starts_with("multipart/") && clen.map_or(true, |l| l > STREAM_MULTIPART_OVER))
        || clen.map_or(false, |l| l > max_buf as u64);

    let mut json_body: Option<Value> = None;
    let body = if stream_through {
        Body0::Streaming(Some(body))
    } else {
        let bytes = match axum::body::to_bytes(body, max_buf).await {
            Ok(b) => b,
            Err(_) => return api_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", "request body too large for proxy buffer"),
        };
        // Parsed only to read routing hints (model, stream, resource ids); the original
        // bytes are what gets forwarded.
        if ctype.contains("json") && !bytes.is_empty() {
            json_body = serde_json::from_slice(&bytes).ok();
        }
        Body0::Buffered(bytes)
    };
    let is_stream = kind == Kind::Messages && json_body.as_ref().and_then(|v| v.get("stream")).and_then(Value::as_bool) == Some(true);

    // ---- key selection
    let forced = parts.headers.get("x-proxy-key").and_then(|v| v.to_str().ok()).map(String::from);
    let pinned = forced
        .or_else(|| path.split('/').filter(|s| s.len() >= 8).find_map(|s| st.pool.pinned(s)))
        .or_else(|| if st.pool.has_pins() { json_body.as_ref().and_then(|v| find_pin_in_json(&st, v)) } else { None });
    let affinity = affinity_key(st.cfg.routing.affinity, &client_name, kind, json_body.as_ref(), &parts.headers);
    let can_failover = pinned.is_none() && matches!(body, Body0::Buffered(_));
    let max_attempts = if can_failover { st.cfg.routing.max_attempts } else { 1 };
    let base_headers = upstream_headers(&parts.headers);

    let mut row = RequestRow {
        request_id: rid.clone(),
        ts_ms: started,
        client: client_name.clone(),
        key_name: None,
        method: method.to_string(),
        path: path.clone(),
        model: json_body.as_ref().and_then(|v| v.get("model")).and_then(Value::as_str).map(String::from),
        status: 0,
        streamed: is_stream,
        usage: Usage::default(),
        cost_usd: None,
        upstream_request_id: None,
        batch_item: None,
        attempts: 0,
        error: None,
    };

    let mut body = body;
    let mut excluded: Vec<String> = Vec::new();
    let mut last_error: Option<(Response, RequestRow)> = None;
    let mut attempts = 0u32;

    let (key_name, upstream) = loop {
        attempts += 1;
        let key: Picked = match &pinned {
            Some(name) => match st.pool.get(name) {
                Some(k) => k,
                None => {
                    return api_error(
                        StatusCode::NOT_FOUND,
                        "not_found_error",
                        &format!("This resource belongs to upstream key {name}, which is no longer in the pool (proxy)."),
                    )
                }
            },
            None => match st.pool.pick(&affinity, &excluded) {
                Ok(k) => k,
                Err(u) => {
                    if let Some((resp, r)) = last_error {
                        st.store.record_request(r);
                        return resp;
                    }
                    row.status = if u.no_keys { 503 } else if u.all_out_of_credit { 402 } else { 429 };
                    row.error = Some("no upstream key available".into());
                    row.attempts = attempts;
                    st.store.record_request(row);
                    let mut resp = unavailable_response(&u);
                    add_proxy_headers(resp.headers_mut(), &rid, None, attempts);
                    return resp;
                }
            },
        };
        row.key_name = Some(key.name.clone());
        row.attempts = attempts;

        let mut headers = base_headers.clone();
        let Ok(secret) = HeaderValue::from_str(&key.secret) else {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, "api_error", &format!("upstream key {} contains invalid characters", key.name));
        };
        headers.insert(HeaderName::from_static("x-api-key"), secret);
        let req_body = match &mut body {
            Body0::Buffered(b) => reqwest::Body::from(b.clone()),
            Body0::Streaming(b) => reqwest::Body::wrap_stream(b.take().expect("streamed body used once").into_data_stream()),
        };
        let sent = st.http.request(method.clone(), format!("{}{}", st.upstream, path_q)).headers(headers).body(req_body).send().await;

        let resp = match sent {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(key = %key.name, "upstream request failed: {e}");
                let mut resp = api_error(StatusCode::BAD_GATEWAY, "api_error", &format!("proxy could not reach upstream: {e}"));
                add_proxy_headers(resp.headers_mut(), &rid, Some(&key.name), attempts);
                let mut r = row.clone();
                r.status = 502;
                r.error = Some(e.to_string());
                if attempts < max_attempts as u32 {
                    excluded.push(key.name.clone());
                    last_error = Some((resp, r));
                    continue;
                }
                st.store.record_request(r);
                return resp;
            }
        };

        let status = resp.status();
        if status.is_success() || status.is_redirection() || status.is_informational() {
            break (key.name, resp);
        }

        // ---- upstream error: classify, update key health, maybe fail over
        let up_headers = resp.headers().clone();
        let mut err_body = Vec::new();
        let mut s = resp.bytes_stream();
        while let Some(Ok(chunk)) = s.next().await {
            err_body.extend_from_slice(&chunk);
            if err_body.len() > MAX_ERROR_BODY {
                break;
            }
        }
        let ev: Value = serde_json::from_slice(&err_body).unwrap_or(Value::Null);
        let etype = ev.pointer("/error/type").and_then(Value::as_str).unwrap_or("");
        let emsg = ev.pointer("/error/message").and_then(Value::as_str).unwrap_or("");
        let class = classify_error(status.as_u16(), etype, emsg);
        match class {
            ErrClass::Credit => st.pool.on_credit_exhausted(&key.name, emsg),
            ErrClass::Auth => st.pool.on_auth_failed(&key.name, emsg),
            ErrClass::RateLimit => st.pool.on_rate_limited(&key.name, retry_after_secs(&up_headers)),
            ErrClass::Transient => st.pool.on_overloaded(&key.name, status.as_u16()),
            ErrClass::Final => {}
        }
        let mut out = response_with(status, &up_headers, false, Body::from(err_body));
        add_proxy_headers(out.headers_mut(), &rid, Some(&key.name), attempts);
        let mut r = row.clone();
        r.status = status.as_u16();
        r.upstream_request_id = up_headers.get("request-id").and_then(|v| v.to_str().ok()).map(String::from);
        r.error = Some(format!("{etype}: {emsg}"));
        if class != ErrClass::Final && attempts < max_attempts as u32 {
            tracing::info!(key = %key.name, status = status.as_u16(), "failing over to another key");
            excluded.push(key.name.clone());
            last_error = Some((out, r));
            continue;
        }
        st.store.record_request(r);
        return out;
    };

    // ---- success
    // Only a billable call proves the key has credit again; a pinned file download
    // or listing succeeds without credits and must not lift an out-of-credits cooldown.
    if matches!(kind, Kind::Messages | Kind::BatchCreate) {
        st.pool.on_success(&key_name);
    }
    let status = upstream.status();
    let up_headers = upstream.headers().clone();
    row.status = status.as_u16();
    row.upstream_request_id = up_headers.get("request-id").and_then(|v| v.to_str().ok()).map(String::from);
    let resp_ctype = up_headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let encoded = up_headers.contains_key("content-encoding");
    let meter = Meter { st: st.clone(), row, key: key_name.clone() };

    if !encoded && kind == Kind::Messages && resp_ctype.starts_with("text/event-stream") {
        let handler = MessagesSse { meter, model: None, usage: Usage::default() };
        let tee = Tee::new(Box::pin(upstream.bytes_stream()), Framing::Sse, Box::new(handler));
        let mut out = response_with(status, &up_headers, false, Body::from_stream(tee));
        add_proxy_headers(out.headers_mut(), &rid, Some(&key_name), attempts);
        return out;
    }

    if !encoded && kind == Kind::BatchResults {
        let batch_id = path.trim_end_matches('/').rsplit('/').nth(1).unwrap_or("").to_string();
        let handler = BatchResults { meter: meter.clone(), batch_id };
        let tee = Tee::new(Box::pin(upstream.bytes_stream()), Framing::Lines, Box::new(handler));
        meter.record(None, Usage::default(), None, None);
        let mut out = response_with(status, &up_headers, false, Body::from_stream(tee));
        add_proxy_headers(out.headers_mut(), &rid, Some(&key_name), attempts);
        return out;
    }

    let is_batch_api = path.starts_with("/v1/messages/batches") || path.starts_with("/v1/beta/messages/batches");
    let buffer_json = !encoded && resp_ctype.contains("json") && (kind == Kind::Messages || method == Method::POST || is_batch_api);
    if !buffer_json {
        // Plain pass-through (file downloads, listings, ...).
        meter.record(None, Usage::default(), None, None);
        let mut out = response_with(status, &up_headers, true, Body::from_stream(upstream.bytes_stream()));
        add_proxy_headers(out.headers_mut(), &rid, Some(&key_name), attempts);
        return out;
    }

    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(e) => {
            meter.record(None, Usage::default(), None, Some(format!("reading upstream body: {e}")));
            return api_error(StatusCode::BAD_GATEWAY, "api_error", "proxy failed reading upstream response");
        }
    };
    let mut v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let mut out_body = bytes;
    let mut rewritten = false;
    let mut cost_header = None;
    if kind == Kind::Messages && v.is_object() {
        let model = v.get("model").and_then(Value::as_str).map(String::from);
        let usage = v.get("usage").map(Usage::from_value).unwrap_or_default();
        let cost = model.as_deref().and_then(|m| meter.cost(m, &usage, false));
        meter.pin_container(v.get("container"));
        cost_header = Some(cost.map_or("unknown".to_string(), |c| format!("{c:.6}")));
        meter.record(model, usage, cost, None);
    } else {
        // A created resource (file, batch, agent, session, skill, vault...) lives in the
        // organization of the key that created it: pin its id to that key.
        if status.is_success() && method == Method::POST {
            if let Some(id) = v.get("id").and_then(Value::as_str) {
                st.pool.pin(id, &key_name);
            }
        }
        // SDKs download batch results from the absolute `results_url`; point it back at the
        // proxy so the download is authenticated with a pool key and metered.
        if is_batch_api && rewrite_results_urls(&mut v, &public_base(&st, &parts.headers)) {
            out_body = Bytes::from(serde_json::to_vec(&v).expect("serialize"));
            rewritten = true;
        }
        meter.record(None, Usage::default(), None, None);
    }
    // Unmodified bodies keep upstream's content-length.
    let mut out = response_with(status, &up_headers, !rewritten, Body::from(out_body));
    add_proxy_headers(out.headers_mut(), &rid, Some(&key_name), attempts);
    if let Some(c) = cost_header.and_then(|c| HeaderValue::from_str(&c).ok()) {
        out.headers_mut().insert("x-proxy-cost-usd", c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_errors_in_both_shapes() {
        assert_eq!(classify_error(402, "billing_error", "x"), ErrClass::Credit);
        assert_eq!(
            classify_error(400, "invalid_request_error", "Your credit balance is too low to access the Anthropic API."),
            ErrClass::Credit
        );
        assert_eq!(classify_error(400, "invalid_request_error", "max_tokens: field required"), ErrClass::Final);
        assert_eq!(classify_error(429, "rate_limit_error", ""), ErrClass::RateLimit);
        assert_eq!(classify_error(529, "overloaded_error", ""), ErrClass::Transient);
        assert_eq!(classify_error(401, "authentication_error", ""), ErrClass::Auth);
    }

    #[test]
    fn conversation_affinity_is_stable_across_turns() {
        let h = HeaderMap::new();
        let t1 = json!({"model": "m", "system": "s", "messages": [{"role": "user", "content": "a"}]});
        let t2 = json!({"model": "m", "system": "s", "messages": [{"role": "user", "content": "a"}, {"role": "assistant", "content": "b"}, {"role": "user", "content": "c"}]});
        let other = json!({"model": "m", "system": "s", "messages": [{"role": "user", "content": "z"}]});
        let k = |v: &Value| affinity_key(Affinity::Conversation, "c", Kind::Messages, Some(v), &h);
        assert_eq!(k(&t1), k(&t2));
        assert_ne!(k(&t1), k(&other));
    }

    #[test]
    fn results_url_rewrite() {
        let mut v = json!({"id": "b", "results_url": "https://api.anthropic.com/v1/messages/batches/b/results"});
        assert!(rewrite_results_urls(&mut v, "http://proxy:8787"));
        assert_eq!(v["results_url"], "http://proxy:8787/v1/messages/batches/b/results");
        let mut v = json!({"data": [{"results_url": null}, {"results_url": "https://x/y"}]});
        assert!(rewrite_results_urls(&mut v, "http://p"));
        assert_eq!(v["data"][1]["results_url"], "http://p/y");
    }

    #[test]
    fn oauth_beta_is_stripped() {
        let mut h = HeaderMap::new();
        h.insert("anthropic-beta", HeaderValue::from_static("oauth-2025-04-20, files-api-2025-04-14"));
        h.insert("x-api-key", HeaderValue::from_static("client"));
        h.insert("x-proxy-key", HeaderValue::from_static("a"));
        let out = upstream_headers(&h);
        assert_eq!(out.get("anthropic-beta").unwrap(), "files-api-2025-04-14");
        assert!(out.get("x-api-key").is_none());
        assert!(out.get("x-proxy-key").is_none());
    }
}
