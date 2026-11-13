//! Runtime header rules: the `[headers]` settings (editable from the web UI) plus a record of
//! which header names and beta flags clients have sent recently, so the UI can offer them.
//! Only names and beta values are recorded, never header values.

use crate::config::{pattern_matches, HeadersCfg, MINIMAL_HEADERS, PROTECTED_HEADERS};
use crate::store::Store;
use anyhow::Result;
use axum::http::HeaderMap;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::HashMap;

const SETTING: &str = "headers";
const MAX_SEEN: usize = 200;

pub struct HeaderRules {
    cfg: RwLock<HeadersCfg>,
    from_ui: RwLock<bool>,
    file_cfg: HeadersCfg,
    seen: Mutex<HashMap<String, i64>>,
    betas: Mutex<HashMap<String, i64>>,
}

impl HeaderRules {
    /// Settings saved from the UI win over the config file; the config file is the starting point.
    pub fn load(file_cfg: HeadersCfg, store: &Store) -> Self {
        let mut active = file_cfg.clone();
        let mut from_ui = false;
        match store.get_setting(SETTING) {
            Ok(Some(text)) => {
                let parsed = serde_json::from_str::<HeadersCfg>(&text).map_err(anyhow::Error::from).and_then(|mut c| {
                    c.validate()?;
                    Ok(c)
                });
                match parsed {
                    Ok(c) => {
                        tracing::info!("header rules: using the settings saved in the web UI (mode {:?})", c.mode);
                        active = c;
                        from_ui = true;
                    }
                    Err(e) => tracing::warn!("header rules saved in the web UI are invalid, using the config file: {e:#}"),
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("could not read saved header rules: {e:#}"),
        }
        Self { cfg: RwLock::new(active), from_ui: RwLock::new(from_ui), file_cfg, seen: Mutex::default(), betas: Mutex::default() }
    }

    pub fn current(&self) -> HeadersCfg {
        self.cfg.read().clone()
    }

    pub fn save(&self, mut new: HeadersCfg, store: &Store) -> Result<()> {
        new.validate()?;
        store.put_setting(SETTING, &serde_json::to_string(&new)?)?;
        *self.cfg.write() = new;
        *self.from_ui.write() = true;
        Ok(())
    }

    /// Forget the UI settings and go back to the config file.
    pub fn reset(&self, store: &Store) -> Result<()> {
        store.delete_setting(SETTING)?;
        *self.cfg.write() = self.file_cfg.clone();
        *self.from_ui.write() = false;
        Ok(())
    }

    /// Remember the header names and beta flags of a client request.
    pub fn observe(&self, headers: &HeaderMap, now: i64) {
        {
            let mut seen = self.seen.lock();
            for name in headers.keys() {
                remember(&mut seen, name.as_str(), now);
            }
        }
        let mut betas = self.betas.lock();
        for v in headers.get_all("anthropic-beta") {
            for b in v.to_str().unwrap_or("").split(',').map(str::trim).filter(|b| !b.is_empty()) {
                remember(&mut betas, &b.to_ascii_lowercase(), now);
            }
        }
    }

    /// Everything the UI shows: the settings, where they come from, and what happens to each
    /// recently seen header and beta flag under them.
    pub fn describe(&self) -> Value {
        let cfg = self.current();
        let mut seen: Vec<(String, i64)> = self.seen.lock().iter().map(|(k, v)| (k.clone(), *v)).collect();
        seen.sort();
        let mut betas: Vec<(String, i64)> = self.betas.lock().iter().map(|(k, v)| (k.clone(), *v)).collect();
        betas.sort();
        json!({
            "settings": cfg,
            "source": if *self.from_ui.read() { "ui" } else { "config" },
            "minimal_headers": MINIMAL_HEADERS,
            "protected_headers": PROTECTED_HEADERS,
            "seen": seen.iter().map(|(n, t)| json!({"name": n, "last_seen_ms": t, "status": header_status(&cfg, n)})).collect::<Vec<_>>(),
            "betas_seen": betas.iter().map(|(b, t)| json!({"value": b, "last_seen_ms": t, "status": beta_status(&cfg, b)})).collect::<Vec<_>>(),
        })
    }
}

fn remember(map: &mut HashMap<String, i64>, name: &str, now: i64) {
    if let Some(t) = map.get_mut(name) {
        *t = now;
        return;
    }
    if map.len() >= MAX_SEEN {
        if let Some(oldest) = map.iter().min_by_key(|(_, t)| **t).map(|(k, _)| k.clone()) {
            map.remove(&oldest);
        }
    }
    map.insert(name.to_string(), now);
}

/// What happens to a client header: `always_removed`, `protected`, `forwarded`,
/// `dropped_by_mode` (minimal mode) or `dropped_by_rule` (a `drop` pattern).
pub fn header_status(cfg: &HeadersCfg, name: &str) -> &'static str {
    if crate::proxy::skip_request_header(name) {
        "always_removed"
    } else if PROTECTED_HEADERS.contains(&name) {
        "protected"
    } else if cfg.drop.iter().any(|p| pattern_matches(p, name)) {
        "dropped_by_rule"
    } else if cfg.forwards(name) {
        "forwarded"
    } else {
        "dropped_by_mode"
    }
}

/// `always_removed` (oauth betas), `dropped_by_rule` or `forwarded`.
pub fn beta_status(cfg: &HeadersCfg, beta: &str) -> &'static str {
    if beta.starts_with("oauth-") {
        "always_removed"
    } else if cfg.drop_betas.iter().any(|p| pattern_matches(p, beta)) {
        "dropped_by_rule"
    } else {
        "forwarded"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HeaderMode;
    use axum::http::HeaderValue;

    #[test]
    fn statuses() {
        let cfg = HeadersCfg {
            mode: HeaderMode::Minimal,
            keep: vec!["x-keep".into()],
            drop: vec!["x-stainless-*".into()],
            drop_betas: vec!["b-*".into()],
        };
        assert_eq!(header_status(&cfg, "x-api-key"), "always_removed");
        assert_eq!(header_status(&cfg, "anthropic-version"), "protected");
        assert_eq!(header_status(&cfg, "x-stainless-os"), "dropped_by_rule");
        assert_eq!(header_status(&cfg, "x-keep"), "forwarded");
        assert_eq!(header_status(&cfg, "anthropic-beta"), "forwarded");
        assert_eq!(header_status(&cfg, "user-agent"), "dropped_by_mode");
        assert_eq!(header_status(&HeadersCfg::default(), "user-agent"), "forwarded");
        assert_eq!(beta_status(&cfg, "oauth-2025-04-20"), "always_removed");
        assert_eq!(beta_status(&cfg, "b-1"), "dropped_by_rule");
        assert_eq!(beta_status(&cfg, "a-1"), "forwarded");
    }

    #[test]
    fn observe_records_names_and_betas_but_no_values() {
        let store = Store::open(":memory:").unwrap();
        let r = HeaderRules::load(HeadersCfg::default(), &store);
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("secret-value"));
        h.insert("anthropic-beta", HeaderValue::from_static("A-1, b-2"));
        r.observe(&h, 5);
        let d = r.describe();
        let text = d.to_string();
        assert!(!text.contains("secret-value"));
        assert!(text.contains("\"x-api-key\""));
        assert_eq!(d["betas_seen"].as_array().unwrap().len(), 2);
        assert_eq!(d["source"], "config");
    }

    #[test]
    fn save_and_reset() {
        let store = Store::open(":memory:").unwrap();
        let r = HeaderRules::load(HeadersCfg::default(), &store);
        let bad = HeadersCfg { drop: vec!["content-type".into()], ..Default::default() };
        assert!(r.save(bad, &store).is_err());
        r.save(HeadersCfg { mode: HeaderMode::Minimal, ..Default::default() }, &store).unwrap();
        assert_eq!(HeaderRules::load(HeadersCfg::default(), &store).current().mode, HeaderMode::Minimal);
        r.reset(&store).unwrap();
        assert_eq!(r.current().mode, HeaderMode::All);
        assert_eq!(HeaderRules::load(HeadersCfg::default(), &store).current().mode, HeaderMode::All);
    }
}
