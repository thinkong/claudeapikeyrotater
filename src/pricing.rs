//! Model price table and per-request cost calculation from the API's `usage` object.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

const BUILTIN: &str = include_str!("../pricing.toml");

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct LongContext {
    pub threshold: u64,
    pub input: f64,
    pub output: f64,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    #[serde(default = "d_read")]
    pub cache_read_mult: f64,
    #[serde(default = "d_w5")]
    pub cache_write_5m_mult: f64,
    #[serde(default = "d_w1")]
    pub cache_write_1h_mult: f64,
    pub fast_input: Option<f64>,
    pub fast_output: Option<f64>,
    pub long_context: Option<LongContext>,
}

fn d_read() -> f64 {
    0.1
}
fn d_w5() -> f64 {
    1.25
}
fn d_w1() -> f64 {
    2.0
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PricingFile {
    web_search_per_1k_usd: Option<f64>,
    batch_multiplier: Option<f64>,
    us_geo_multiplier: Option<f64>,
    #[serde(default)]
    models: HashMap<String, ModelPrice>,
}

#[derive(Debug)]
pub struct Pricing {
    web_search_per_1k_usd: f64,
    batch_multiplier: f64,
    us_geo_multiplier: f64,
    models: HashMap<String, ModelPrice>,
}

/// Token counts for one response, merged across stream events.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// `cache_creation_input_tokens` (total of both TTLs).
    pub cache_creation_total: u64,
    /// `cache_creation.ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens`, when reported.
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
    pub web_search_requests: u64,
    pub fast: bool,
    pub us_geo: bool,
}

impl Usage {
    /// Merge a `usage` object into self. Later events win for every field they carry
    /// (stream `message_delta` usage is cumulative).
    pub fn merge(&mut self, u: &Value) {
        let get = |k: &str| u.get(k).and_then(Value::as_u64);
        if let Some(v) = get("input_tokens") {
            self.input = v;
        }
        if let Some(v) = get("output_tokens") {
            self.output = v;
        }
        if let Some(v) = get("cache_read_input_tokens") {
            self.cache_read = v;
        }
        if let Some(v) = get("cache_creation_input_tokens") {
            self.cache_creation_total = v;
        }
        if let Some(cc) = u.get("cache_creation").filter(|v| v.is_object()) {
            if let Some(v) = cc.get("ephemeral_5m_input_tokens").and_then(Value::as_u64) {
                self.cache_write_5m = Some(v);
            }
            if let Some(v) = cc.get("ephemeral_1h_input_tokens").and_then(Value::as_u64) {
                self.cache_write_1h = Some(v);
            }
        }
        if let Some(v) = u.pointer("/server_tool_use/web_search_requests").and_then(Value::as_u64) {
            self.web_search_requests = v;
        }
        if let Some(s) = u.get("speed").and_then(Value::as_str) {
            self.fast = s == "fast";
        }
        if let Some(s) = u.get("inference_geo").and_then(Value::as_str) {
            self.us_geo = s == "us";
        }
    }

    pub fn from_value(u: &Value) -> Self {
        let mut me = Self::default();
        me.merge(u);
        me
    }

    /// (5m writes, 1h writes). Without the per-TTL breakdown, all writes count as 5m.
    pub fn cache_writes(&self) -> (u64, u64) {
        match (self.cache_write_5m, self.cache_write_1h) {
            (None, None) => (self.cache_creation_total, 0),
            (a, b) => (a.unwrap_or(0), b.unwrap_or(0)),
        }
    }

    pub fn prompt_tokens(&self) -> u64 {
        let (w5, w1) = self.cache_writes();
        self.input + self.cache_read + w5 + w1
    }
}

impl Pricing {
    pub fn load(override_path: Option<&str>) -> Result<Self> {
        let base: PricingFile = toml::from_str(BUILTIN).context("built-in pricing.toml")?;
        let mut me = Pricing {
            web_search_per_1k_usd: base.web_search_per_1k_usd.unwrap_or(10.0),
            batch_multiplier: base.batch_multiplier.unwrap_or(0.5),
            us_geo_multiplier: base.us_geo_multiplier.unwrap_or(1.1),
            models: base.models,
        };
        if let Some(path) = override_path {
            let text = std::fs::read_to_string(path).with_context(|| format!("reading pricing_file {path}"))?;
            let ov: PricingFile = toml::from_str(&text).with_context(|| format!("parsing pricing_file {path}"))?;
            if let Some(v) = ov.web_search_per_1k_usd {
                me.web_search_per_1k_usd = v;
            }
            if let Some(v) = ov.batch_multiplier {
                me.batch_multiplier = v;
            }
            if let Some(v) = ov.us_geo_multiplier {
                me.us_geo_multiplier = v;
            }
            me.models.extend(ov.models);
        }
        Ok(me)
    }

    /// Find the price entry for a model id as reported in the response.
    pub fn lookup(&self, model: &str) -> Option<&ModelPrice> {
        let m = model.strip_prefix("anthropic.").unwrap_or(model);
        let m = m.split('@').next().unwrap_or(m);
        if let Some(p) = self.models.get(m) {
            return Some(p);
        }
        // `<id>-YYYYMMDD` snapshot or `<id>-latest`.
        if let Some((base, suffix)) = m.rsplit_once('-') {
            let is_date = suffix.len() == 8 && suffix.bytes().all(|b| b.is_ascii_digit());
            if is_date || suffix == "latest" {
                return self.models.get(base);
            }
        }
        None
    }

    /// Cost in USD, or None when the model is not in the price table.
    pub fn cost(&self, model: &str, usage: &Usage, batch: bool) -> Option<f64> {
        let p = self.lookup(model)?;
        let (mut base_in, mut base_out) = (p.input, p.output);
        if usage.fast {
            base_in = p.fast_input.unwrap_or(base_in);
            base_out = p.fast_output.unwrap_or(base_out);
        }
        if let Some(lc) = &p.long_context {
            if usage.prompt_tokens() > lc.threshold {
                base_in = lc.input;
                base_out = lc.output;
            }
        }
        let (w5, w1) = usage.cache_writes();
        let mut tokens = usage.input as f64 * base_in
            + w5 as f64 * base_in * p.cache_write_5m_mult
            + w1 as f64 * base_in * p.cache_write_1h_mult
            + usage.cache_read as f64 * base_in * p.cache_read_mult
            + usage.output as f64 * base_out;
        tokens /= 1_000_000.0;
        if usage.us_geo {
            tokens *= self.us_geo_multiplier;
        }
        if batch {
            tokens *= self.batch_multiplier;
        }
        Some(tokens + usage.web_search_requests as f64 * self.web_search_per_1k_usd / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn lookup_handles_snapshots() {
        let p = Pricing::load(None).unwrap();
        assert!(p.lookup("claude-opus-5-5").is_some());
        assert_eq!(p.lookup("claude-opus-4-5-20251101").unwrap().input, 5.0);
        // A different minor version must not silently match a shorter id.
        assert!(p.lookup("claude-opus-5-7").is_none());
        assert!(p.lookup("anthropic.claude-sonnet-5-5").is_some());
    }

    #[test]
    fn opus_5_5_full_breakdown() {
        let p = Pricing::load(None).unwrap();
        let u = Usage::from_value(&json!({
            "input_tokens": 1_000_000, "output_tokens": 1_000_000,
            "cache_read_input_tokens": 1_000_000, "cache_creation_input_tokens": 2_000_000,
            "cache_creation": {"ephemeral_5m_input_tokens": 1_000_000, "ephemeral_1h_input_tokens": 1_000_000},
            "server_tool_use": {"web_search_requests": 3}
        }));
        // 4 + 20 + 0.20 + 5 + 8 + 0.03
        approx(p.cost("claude-opus-5-5", &u, false).unwrap(), 37.23);
    }

    #[test]
    fn modifiers() {
        let p = Pricing::load(None).unwrap();
        let mut u = Usage::from_value(&json!({"input_tokens": 1_000_000, "output_tokens": 0, "speed": "fast"}));
        approx(p.cost("claude-opus-5-5", &u, false).unwrap(), 8.0);
        u.fast = false;
        u.us_geo = true;
        approx(p.cost("claude-opus-5-5", &u, false).unwrap(), 4.4);
        u.us_geo = false;
        approx(p.cost("claude-opus-5-5", &u, true).unwrap(), 2.0);
        // Without a TTL breakdown cache writes are priced as 5m.
        let u = Usage::from_value(&json!({"cache_creation_input_tokens": 1_000_000}));
        approx(p.cost("claude-sonnet-4-6", &u, false).unwrap(), 3.75);
    }

    #[test]
    fn haiku_long_context_counts_cache() {
        let p = Pricing::load(None).unwrap();
        let small = Usage::from_value(&json!({"input_tokens": 100_000, "output_tokens": 0}));
        approx(p.cost("claude-haiku-5-5", &small, false).unwrap(), 0.01);
        let big = Usage::from_value(&json!({"input_tokens": 50_000, "cache_read_input_tokens": 60_000}));
        // over threshold: 50k * 0.5 + 60k * 0.05
        approx(p.cost("claude-haiku-5-5", &big, false).unwrap(), 0.025 + 0.003);
    }

    #[test]
    fn unknown_model_is_none() {
        let p = Pricing::load(None).unwrap();
        assert!(p.cost("claude-future-9", &Usage::default(), false).is_none());
    }
}
