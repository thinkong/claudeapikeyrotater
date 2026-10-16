//! SQLite persistence. All writes go through one writer thread (batched in transactions)
//! so request handling never waits on disk; reads use a separate connection.

use crate::pricing::Usage;
use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::mpsc;
use std::thread::JoinHandle;

#[derive(Debug, Clone)]
pub struct RequestRow {
    pub request_id: String,
    pub ts_ms: i64,
    pub client: String,
    pub key_name: Option<String>,
    pub method: String,
    pub path: String,
    pub model: Option<String>,
    pub status: u16,
    pub streamed: bool,
    pub usage: Usage,
    pub cost_usd: Option<f64>,
    pub upstream_request_id: Option<String>,
    /// `batch_id:custom_id` for batch results; makes re-downloads idempotent.
    pub batch_item: Option<String>,
    pub attempts: u32,
    pub error: Option<String>,
}

/// An upstream key as persisted. `deleted_ms` keeps a tombstone so config seeds don't resurrect it.
#[derive(Debug, Clone, Default)]
pub struct KeyRow {
    pub name: String,
    pub secret: String,
    pub enabled: bool,
    pub created_ms: i64,
    pub balance_usd: Option<f64>,
    pub balance_set_ms: i64,
    pub cooldown_until_ms: i64,
    pub reason: Option<String>,
    pub exhausted_ms: Option<i64>,
    pub deleted_ms: Option<i64>,
}

/// A client as persisted. Only a SHA-256 of the token is stored.
#[derive(Debug, Clone, Default)]
pub struct ClientRow {
    pub name: String,
    pub token_hash: String,
    pub token_hint: String,
    pub enabled: bool,
    pub budget_usd: Option<f64>,
    pub created_ms: i64,
    pub deleted_ms: Option<i64>,
}

enum Op {
    Request(Box<RequestRow>),
    Pin(String, String, i64),
    Key(KeyRow),
    Client(ClientRow),
    Shutdown,
}

pub struct Loaded {
    pub keys: Vec<KeyRow>,
    pub clients: Vec<ClientRow>,
    pub pins: HashMap<String, String>,
    pub key_spend: HashMap<String, f64>,
    pub client_spend: HashMap<String, f64>,
}

pub type OnInsert = Box<dyn Fn(&RequestRow) + Send + 'static>;

pub struct Store {
    tx: mpsc::Sender<Op>,
    rx: Mutex<Option<mpsc::Receiver<Op>>>,
    writer: Mutex<Option<JoinHandle<()>>>,
    path: String,
    read: Mutex<Connection>,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS requests (
    id INTEGER PRIMARY KEY,
    request_id TEXT NOT NULL,
    ts_ms INTEGER NOT NULL,
    client TEXT NOT NULL,
    key_name TEXT,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    model TEXT,
    status INTEGER NOT NULL,
    streamed INTEGER NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cache_read_tokens INTEGER NOT NULL,
    cache_write_5m_tokens INTEGER NOT NULL,
    cache_write_1h_tokens INTEGER NOT NULL,
    web_search_requests INTEGER NOT NULL,
    fast INTEGER NOT NULL,
    us_geo INTEGER NOT NULL,
    cost_usd REAL,
    upstream_request_id TEXT,
    batch_item TEXT UNIQUE,
    attempts INTEGER NOT NULL,
    error TEXT
);
CREATE INDEX IF NOT EXISTS requests_request_id ON requests(request_id);
CREATE INDEX IF NOT EXISTS requests_ts ON requests(ts_ms);
CREATE INDEX IF NOT EXISTS requests_key_ts ON requests(key_name, ts_ms);
CREATE TABLE IF NOT EXISTS pins (
    resource_id TEXT PRIMARY KEY,
    key_name TEXT NOT NULL,
    ts_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS upstream_keys (
    name TEXT PRIMARY KEY,
    secret TEXT NOT NULL,
    enabled INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    balance_usd REAL,
    balance_set_ms INTEGER NOT NULL,
    cooldown_until_ms INTEGER NOT NULL,
    reason TEXT,
    exhausted_ms INTEGER,
    deleted_ms INTEGER
);
CREATE TABLE IF NOT EXISTS clients (
    name TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL,
    token_hint TEXT NOT NULL,
    enabled INTEGER NOT NULL,
    budget_usd REAL,
    created_ms INTEGER NOT NULL,
    deleted_ms INTEGER
);
CREATE TABLE IF NOT EXISTS settings (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
"#;

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening database {path}"))?;
        conn.execute_batch(SCHEMA).context("creating schema")?;
        let (tx, rx) = mpsc::channel();
        Ok(Store { tx, rx: Mutex::new(Some(rx)), writer: Mutex::new(None), path: path.to_string(), read: Mutex::new(conn) })
    }

    pub fn load(&self, pin_retention_days: u64) -> Result<Loaded> {
        let c = self.read.lock();
        let cutoff = crate::now_ms() - (pin_retention_days as i64) * 86_400_000;
        c.execute("DELETE FROM pins WHERE ts_ms < ?1", params![cutoff])?;

        let mut st = c.prepare(
            "SELECT name, secret, enabled, created_ms, balance_usd, balance_set_ms, cooldown_until_ms, reason, exhausted_ms, deleted_ms
             FROM upstream_keys ORDER BY created_ms, name",
        )?;
        let keys = st
            .query_map([], |r| {
                Ok(KeyRow {
                    name: r.get(0)?,
                    secret: r.get(1)?,
                    enabled: r.get(2)?,
                    created_ms: r.get(3)?,
                    balance_usd: r.get(4)?,
                    balance_set_ms: r.get(5)?,
                    cooldown_until_ms: r.get(6)?,
                    reason: r.get(7)?,
                    exhausted_ms: r.get(8)?,
                    deleted_ms: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut st = c.prepare(
            "SELECT name, token_hash, token_hint, enabled, budget_usd, created_ms, deleted_ms FROM clients ORDER BY created_ms, name",
        )?;
        let clients = st
            .query_map([], |r| {
                Ok(ClientRow {
                    name: r.get(0)?,
                    token_hash: r.get(1)?,
                    token_hint: r.get(2)?,
                    enabled: r.get(3)?,
                    budget_usd: r.get(4)?,
                    created_ms: r.get(5)?,
                    deleted_ms: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut pins = HashMap::new();
        let mut st = c.prepare("SELECT resource_id, key_name FROM pins")?;
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (k, v) = r?;
            pins.insert(k, v);
        }
        let sums = |sql: &str| -> Result<HashMap<String, f64>> {
            let mut m = HashMap::new();
            let mut st = c.prepare(sql)?;
            for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))? {
                let (k, v) = r?;
                m.insert(k, v);
            }
            Ok(m)
        };
        let key_spend = sums("SELECT key_name, SUM(cost_usd) FROM requests WHERE key_name IS NOT NULL AND cost_usd IS NOT NULL GROUP BY key_name")?;
        let client_spend = sums("SELECT client, SUM(cost_usd) FROM requests WHERE cost_usd IS NOT NULL GROUP BY client")?;
        Ok(Loaded { keys, clients, pins, key_spend, client_spend })
    }

    /// Spend recorded for a key since `since_ms`.
    pub fn key_spend_since(&self, key: &str, since_ms: i64) -> Result<f64> {
        let c = self.read.lock();
        Ok(c.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM requests WHERE key_name = ?1 AND ts_ms >= ?2",
            params![key, since_ms],
            |r| r.get(0),
        )?)
    }

    pub fn get_setting(&self, k: &str) -> Result<Option<String>> {
        let c = self.read.lock();
        Ok(c.query_row("SELECT v FROM settings WHERE k = ?1", params![k], |r| r.get(0)).optional()?)
    }

    pub fn put_setting(&self, k: &str, v: &str) -> Result<()> {
        let c = self.read.lock();
        c.execute("INSERT INTO settings (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v", params![k, v])?;
        Ok(())
    }

    pub fn start_writer(&self, on_insert: OnInsert) -> Result<()> {
        let rx = self.rx.lock().take().context("writer already started")?;
        let conn = Connection::open(&self.path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        let handle = std::thread::Builder::new().name("store-writer".into()).spawn(move || writer_loop(conn, rx, on_insert))?;
        *self.writer.lock() = Some(handle);
        Ok(())
    }

    pub fn record_request(&self, row: RequestRow) {
        let _ = self.tx.send(Op::Request(Box::new(row)));
    }
    pub fn save_pin(&self, resource_id: String, key_name: String) {
        let _ = self.tx.send(Op::Pin(resource_id, key_name, crate::now_ms()));
    }
    pub fn save_key(&self, k: KeyRow) {
        let _ = self.tx.send(Op::Key(k));
    }
    pub fn save_client(&self, c: ClientRow) {
        let _ = self.tx.send(Op::Client(c));
    }

    /// Flush pending writes and stop the writer thread.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Op::Shutdown);
        if let Some(h) = self.writer.lock().take() {
            let _ = h.join();
        }
    }

    pub fn get_request(&self, request_id: &str) -> Result<Option<Value>> {
        let c = self.read.lock();
        let mut st = c.prepare(
            "SELECT request_id, ts_ms, client, key_name, method, path, model, status, streamed,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens,
                    web_search_requests, fast, us_geo, cost_usd, upstream_request_id, attempts, error
             FROM requests WHERE request_id = ?1 ORDER BY id LIMIT 1",
        )?;
        Ok(st.query_row(params![request_id], request_json).optional()?)
    }

    /// Most recent requests, newest first.
    pub fn recent_requests(&self, limit: u32) -> Result<Value> {
        let c = self.read.lock();
        let mut st = c.prepare(
            "SELECT request_id, ts_ms, client, key_name, method, path, model, status, streamed,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens,
                    web_search_requests, fast, us_geo, cost_usd, upstream_request_id, attempts, error
             FROM requests WHERE batch_item IS NULL ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = st.query_map(params![limit], request_json)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Value::Array(rows))
    }

    /// Aggregated usage since `since_ms`, grouped by one of key|client|model|day.
    pub fn usage_summary(&self, since_ms: i64, group_by: &str, client: Option<&str>) -> Result<Value> {
        let col = match group_by {
            "key" => "COALESCE(key_name, '')",
            "client" => "client",
            "model" => "COALESCE(model, '')",
            "day" => "strftime('%Y-%m-%d', ts_ms / 1000, 'unixepoch')",
            _ => anyhow::bail!("group_by must be key, client, model or day"),
        };
        let sql = format!(
            "SELECT {col} AS k, COUNT(*), SUM(input_tokens), SUM(output_tokens), SUM(cache_read_tokens),
                    SUM(cache_write_5m_tokens), SUM(cache_write_1h_tokens), SUM(web_search_requests),
                    COALESCE(SUM(cost_usd), 0), SUM(CASE WHEN cost_usd IS NULL AND model IS NOT NULL AND status < 300 THEN 1 ELSE 0 END)
             FROM requests WHERE ts_ms >= ?1 AND (?2 IS NULL OR client = ?2) GROUP BY k ORDER BY k"
        );
        let c = self.read.lock();
        let mut st = c.prepare(&sql)?;
        let rows = st
            .query_map(params![since_ms, client], |r| {
                Ok(json!({
                    group_by: r.get::<_, String>(0)?,
                    "requests": r.get::<_, i64>(1)?,
                    "input_tokens": r.get::<_, i64>(2)?,
                    "output_tokens": r.get::<_, i64>(3)?,
                    "cache_read_input_tokens": r.get::<_, i64>(4)?,
                    "cache_write_5m_input_tokens": r.get::<_, i64>(5)?,
                    "cache_write_1h_input_tokens": r.get::<_, i64>(6)?,
                    "web_search_requests": r.get::<_, i64>(7)?,
                    "cost_usd": r.get::<_, f64>(8)?,
                    "unpriced_requests": r.get::<_, i64>(9)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Value::Array(rows))
    }
}

fn request_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "request_id": r.get::<_, String>(0)?,
        "ts_ms": r.get::<_, i64>(1)?,
        "client": r.get::<_, String>(2)?,
        "key": r.get::<_, Option<String>>(3)?,
        "method": r.get::<_, String>(4)?,
        "path": r.get::<_, String>(5)?,
        "model": r.get::<_, Option<String>>(6)?,
        "status": r.get::<_, i64>(7)?,
        "streamed": r.get::<_, bool>(8)?,
        "usage": {
            "input_tokens": r.get::<_, i64>(9)?,
            "output_tokens": r.get::<_, i64>(10)?,
            "cache_read_input_tokens": r.get::<_, i64>(11)?,
            "cache_write_5m_input_tokens": r.get::<_, i64>(12)?,
            "cache_write_1h_input_tokens": r.get::<_, i64>(13)?,
            "web_search_requests": r.get::<_, i64>(14)?,
            "fast": r.get::<_, bool>(15)?,
            "us_geo": r.get::<_, bool>(16)?,
        },
        "cost_usd": r.get::<_, Option<f64>>(17)?,
        "upstream_request_id": r.get::<_, Option<String>>(18)?,
        "attempts": r.get::<_, i64>(19)?,
        "error": r.get::<_, Option<String>>(20)?,
    }))
}

fn writer_loop(mut conn: Connection, rx: mpsc::Receiver<Op>, on_insert: OnInsert) {
    let mut done = false;
    while !done {
        let Ok(first) = rx.recv() else { break };
        let mut batch = vec![first];
        while batch.len() < 512 {
            match rx.try_recv() {
                Ok(op) => batch.push(op),
                Err(_) => break,
            }
        }
        let mut inserted = Vec::new();
        let res = (|| -> rusqlite::Result<()> {
            let tx = conn.transaction()?;
            for op in batch {
                match op {
                    Op::Request(row) => {
                        let (w5, w1) = row.usage.cache_writes();
                        let n = tx.execute(
                            "INSERT OR IGNORE INTO requests (request_id, ts_ms, client, key_name, method, path, model, status,
                                streamed, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens,
                                cache_write_1h_tokens, web_search_requests, fast, us_geo, cost_usd, upstream_request_id,
                                batch_item, attempts, error)
                             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
                            params![
                                row.request_id,
                                row.ts_ms,
                                row.client,
                                row.key_name,
                                row.method,
                                row.path,
                                row.model,
                                row.status,
                                row.streamed,
                                row.usage.input as i64,
                                row.usage.output as i64,
                                row.usage.cache_read as i64,
                                w5 as i64,
                                w1 as i64,
                                row.usage.web_search_requests as i64,
                                row.usage.fast,
                                row.usage.us_geo,
                                row.cost_usd,
                                row.upstream_request_id,
                                row.batch_item,
                                row.attempts,
                                row.error,
                            ],
                        )?;
                        if n == 1 {
                            inserted.push(row);
                        }
                    }
                    Op::Pin(id, key, ts) => {
                        tx.execute(
                            "INSERT INTO pins (resource_id, key_name, ts_ms) VALUES (?1, ?2, ?3)
                             ON CONFLICT(resource_id) DO UPDATE SET key_name = excluded.key_name",
                            params![id, key, ts],
                        )?;
                    }
                    Op::Key(k) => {
                        tx.execute(
                            "INSERT INTO upstream_keys (name, secret, enabled, created_ms, balance_usd, balance_set_ms,
                                cooldown_until_ms, reason, exhausted_ms, deleted_ms)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                             ON CONFLICT(name) DO UPDATE SET secret = excluded.secret, enabled = excluded.enabled,
                                created_ms = excluded.created_ms, balance_usd = excluded.balance_usd,
                                balance_set_ms = excluded.balance_set_ms, cooldown_until_ms = excluded.cooldown_until_ms,
                                reason = excluded.reason, exhausted_ms = excluded.exhausted_ms, deleted_ms = excluded.deleted_ms",
                            params![
                                k.name,
                                k.secret,
                                k.enabled,
                                k.created_ms,
                                k.balance_usd,
                                k.balance_set_ms,
                                k.cooldown_until_ms,
                                k.reason,
                                k.exhausted_ms,
                                k.deleted_ms
                            ],
                        )?;
                    }
                    Op::Client(c) => {
                        tx.execute(
                            "INSERT INTO clients (name, token_hash, token_hint, enabled, budget_usd, created_ms, deleted_ms)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                             ON CONFLICT(name) DO UPDATE SET token_hash = excluded.token_hash, token_hint = excluded.token_hint,
                                enabled = excluded.enabled, budget_usd = excluded.budget_usd, created_ms = excluded.created_ms,
                                deleted_ms = excluded.deleted_ms",
                            params![c.name, c.token_hash, c.token_hint, c.enabled, c.budget_usd, c.created_ms, c.deleted_ms],
                        )?;
                    }
                    Op::Shutdown => done = true,
                }
            }
            tx.commit()
        })();
        match res {
            Ok(()) => {
                for row in &inserted {
                    on_insert(row);
                }
            }
            Err(e) => tracing::error!("database write failed: {e}"),
        }
    }
}
