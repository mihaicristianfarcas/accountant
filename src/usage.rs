//! Usage meters: how much of each rate-limit window an account has used and
//! when it resets — the thing you actually want to know when picking the next
//! account.
//!
//! Best effort. We only query accounts whose short-lived access token is still
//! valid (never refreshing, so we never rotate a token behind a CLI's back),
//! only against the provider's own API host, and remember the last answer so
//! idle accounts still show when their window resets.

use crate::fsutil;
use crate::providers::{AccessToken, Provider};
use anyhow::{Result, bail};
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Window {
    /// "5h", "7d", "7d opus", …
    pub label: String,
    /// Percent used, 0–100.
    pub used: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub windows: Vec<Window>,
    pub fetched_at: DateTime<Utc>,
}

impl Usage {
    /// The window that limits the account the most right now. Windows whose
    /// reset time has passed count as empty.
    pub fn binding(&self) -> Option<Window> {
        let now = Utc::now();
        self.windows
            .iter()
            .map(|w| {
                let mut w = w.clone();
                if w.resets_at.is_some_and(|r| r <= now) {
                    w.used = 0.0;
                }
                w
            })
            .max_by(|a, b| a.used.total_cmp(&b.used))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Cache {
    #[serde(flatten)]
    pub by_profile: BTreeMap<String, Usage>,
}

impl Cache {
    pub fn load(path: &Path) -> Self {
        let mut cache: Cache = fsutil::read_optional(path)
            .ok()
            .flatten()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        // Answers cached before unknown fields were ignored.
        for u in cache.by_profile.values_mut() {
            u.windows.retain(|w| is_window_label(&w.label));
        }
        cache
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        fsutil::write_atomic(path, serde_json::to_string_pretty(self)?.as_bytes(), 0o600)
    }
}

pub fn fetch(provider: Provider, tok: &AccessToken) -> Result<Usage> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(8)))
        .http_status_as_error(false)
        .build()
        .into();
    let ua = concat!("accountant/", env!("CARGO_PKG_VERSION"));
    let mut resp = match provider {
        Provider::Claude => agent
            .get("https://api.anthropic.com/api/oauth/usage")
            .header("Authorization", &format!("Bearer {}", tok.token))
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("User-Agent", ua)
            .call()?,
        Provider::Codex => {
            let mut req = agent
                .get("https://chatgpt.com/backend-api/wham/usage")
                .header("Authorization", &format!("Bearer {}", tok.token))
                .header("User-Agent", ua);
            if let Some(acct) = &tok.account_id {
                req = req.header("ChatGPT-Account-Id", acct);
            }
            req.call()?
        }
        other => bail!("{} has no usage meter", other.label()),
    };
    let status = resp.status().as_u16();
    // We only ask with an access token that has not expired, so a 401 means
    // the provider revoked the whole session.
    if status == 401 {
        return Err(Revoked.into());
    }
    if status != 200 {
        bail!("usage endpoint answered HTTP {status}");
    }
    let v: Value = resp.body_mut().read_json()?;
    let windows = match provider {
        Provider::Claude => parse_claude(&v),
        Provider::Codex => parse_codex(&v),
        _ => vec![],
    };
    if windows.is_empty() {
        bail!("usage response had no windows");
    }
    Ok(Usage { windows, fetched_at: Utc::now() })
}

/// The provider no longer accepts the saved login: a browser sign-in is due.
#[derive(Debug)]
pub struct Revoked;

impl std::fmt::Display for Revoked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the provider no longer accepts this login — sign in again")
    }
}

impl std::error::Error for Revoked {}

/// Fetch several accounts in parallel; results arrive as they complete.
pub fn spawn_fetch(jobs: Vec<(String, Provider, AccessToken)>) -> Receiver<(String, Result<Usage>)> {
    let (tx, rx) = mpsc::channel();
    for (id, provider, tok) in jobs {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((id, fetch(provider, &tok)));
        });
    }
    rx
}

fn parse_time(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::String(s) => DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc)),
        Value::Number(n) => n.as_i64().and_then(|s| Utc.timestamp_opt(s, 0).single()),
        _ => None,
    }
}

/// "5h", "7d", "7d opus": a window length, then an optional qualifier.
fn is_window_label(label: &str) -> bool {
    let len = label.split(' ').next().unwrap_or("");
    len.len() >= 2 && len.ends_with(['h', 'd']) && len[..len.len() - 1].bytes().all(|b| b.is_ascii_digit())
}

/// The current shape, `{"limits": [{"kind": "session", "percent": 42, "resets_at": "…"},
/// {"kind": "weekly_scoped", "scope": {"model": {"display_name": "Opus"}}, …}]}`,
/// or the older `{"five_hour": {"utilization": 42.0, "resets_at": "…"}, "seven_day": {…}}`.
/// Only the rate-limit windows: the response also carries other objects that
/// have a `utilization` but are not limits you can run into.
fn parse_claude(v: &Value) -> Vec<Window> {
    let modern: Vec<Window> = v
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let label = match entry.get("kind")?.as_str()? {
                "session" => "5h".to_string(),
                "weekly_all" => "7d".to_string(),
                "weekly_scoped" => {
                    let model = entry
                        .pointer("/scope/model/display_name")
                        .and_then(Value::as_str)
                        .unwrap_or("model")
                        .to_lowercase();
                    format!("7d {model}")
                }
                _ => return None,
            };
            let used = entry.get("percent")?.as_f64().filter(|p| p.is_finite())?;
            Some(Window { label, used, resets_at: entry.get("resets_at").and_then(parse_time) })
        })
        .collect();
    if !modern.is_empty() {
        return modern;
    }
    let Some(obj) = v.as_object() else { return vec![] };
    let mut out: Vec<Window> = obj
        .iter()
        .filter(|(k, _)| matches!(k.as_str(), "five_hour" | "seven_day") || k.starts_with("seven_day_"))
        .filter_map(|(k, w)| {
            let used = w.get("utilization")?.as_f64()?;
            let label = match k.as_str() {
                "five_hour" => "5h".to_string(),
                "seven_day" => "7d".to_string(),
                other => other.replace("seven_day_", "7d ").replace('_', " "),
            };
            Some(Window { label, used, resets_at: w.get("resets_at").and_then(parse_time) })
        })
        .collect();
    out.sort_by_key(|w| (w.label != "5h", w.label != "7d", w.label.clone()));
    out
}

/// `{"rate_limit": {"primary_window": {"used_percent": 12, "limit_window_seconds": 18000,
///   "reset_at": 1759…}, "secondary_window": {…}}, "additional_rate_limits": [{"limit_name":
///   "GPT-5-Codex-Mini", "rate_limit": {…}}]}`. Model-specific limits get the model as qualifier.
fn parse_codex(v: &Value) -> Vec<Window> {
    let mut out = codex_windows(v.get("rate_limit").unwrap_or(v), None);
    for extra in v.get("additional_rate_limits").and_then(Value::as_array).into_iter().flatten() {
        let Some(rl) = extra.get("rate_limit") else { continue };
        let name = extra.get("limit_name").and_then(Value::as_str).map(str::to_lowercase);
        out.extend(codex_windows(rl, name.as_deref()));
    }
    out
}

fn codex_windows(rl: &Value, qualifier: Option<&str>) -> Vec<Window> {
    ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|k| {
            let w = rl.get(*k)?;
            let used = w.get("used_percent")?.as_f64()?;
            let secs = w.get("limit_window_seconds").and_then(Value::as_i64).unwrap_or(0);
            let mut label = match secs {
                0 => if *k == "primary_window" { "5h" } else { "7d" }.to_string(),
                s if s % 86400 == 0 => format!("{}d", s / 86400),
                s => format!("{}h", (s + 1800) / 3600),
            };
            if let Some(q) = qualifier.filter(|q| !q.is_empty()) {
                label = format!("{label} {q}");
            }
            let resets_at = w.get("reset_at").and_then(parse_time).or_else(|| {
                let after = w.get("reset_after_seconds")?.as_i64()?;
                Some(Utc::now() + chrono::Duration::seconds(after))
            });
            Some(Window { label, used, resets_at })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_claude_shape() {
        let v = json!({
            "seven_day": {"utilization": 30.0, "resets_at": "2026-10-10T00:00:00Z"},
            "five_hour": {"utilization": 100.0, "resets_at": "2026-10-06T15:00:00+00:00"},
            "seven_day_opus": {"utilization": 5.0, "resets_at": null},
            "iguana_necktie": {"utilization": 0.0, "resets_at": null},
            "extra": null
        });
        let w = parse_claude(&v);
        assert_eq!(w.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(), ["5h", "7d", "7d opus"]);
        assert_eq!(w[0].used, 100.0);
        assert!(w[0].resets_at.is_some());
    }

    #[test]
    fn parses_claudes_current_limits_list_the_same_as_the_old_shape() {
        let v = json!({"limits": [
            {"kind": "session", "percent": 37, "resets_at": "2026-10-06T15:00:00Z"},
            {"kind": "weekly_all", "percent": 12.5, "resets_at": "2026-10-10T00:00:00Z"},
            {"kind": "weekly_scoped", "percent": 80, "scope": {"model": {"display_name": "Opus"}}},
            {"kind": "something_new", "percent": 1}
        ]});
        let w = parse_claude(&v);
        assert_eq!(w.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(), ["5h", "7d", "7d opus"]);
        assert_eq!(w[0].used, 37.0);
        let old =
            parse_claude(&json!({"five_hour": {"utilization": 37.0, "resets_at": "2026-10-06T15:00:00Z"}}));
        assert_eq!(old[0], w[0]);
    }

    #[test]
    fn codex_model_specific_limits_are_kept_with_their_model() {
        let v = json!({
            "rate_limit": {"primary_window": {"used_percent": 10, "limit_window_seconds": 18000}},
            "additional_rate_limits": [{"limit_name": "GPT-5-Codex-Mini",
                "rate_limit": {"primary_window": {"used_percent": 90, "limit_window_seconds": 18000}}}]
        });
        let w = parse_codex(&v);
        assert_eq!(w.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(), ["5h", "5h gpt-5-codex-mini"]);
        assert!(w.iter().all(|w| is_window_label(&w.label)), "they survive the cache");
        assert_eq!(Usage { windows: w, fetched_at: Utc::now() }.binding().unwrap().used, 90.0);
    }

    #[test]
    fn parses_codex_shape() {
        let v = json!({"plan_type": "plus", "rate_limit": {
            "primary_window": {"used_percent": 64, "limit_window_seconds": 18000, "reset_after_seconds": 600},
            "secondary_window": {"used_percent": 20, "limit_window_seconds": 604800, "reset_at": 1791288000}
        }});
        let w = parse_codex(&v);
        assert_eq!(w[0].label, "5h");
        assert_eq!(w[1].label, "7d");
        assert_eq!(w[1].resets_at.unwrap().timestamp(), 1791288000);
    }

    #[test]
    fn cached_windows_with_unknown_labels_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let window = |label: &str| Window { label: label.into(), used: 1.0, resets_at: None };
        let labels = ["5h", "7d", "7d opus", "1d", "iguana necktie", "h", "7x"];
        let mut cache = Cache::default();
        cache.by_profile.insert(
            "p".into(),
            Usage { windows: labels.iter().map(|l| window(l)).collect(), fetched_at: Utc::now() },
        );
        cache.save(&path).unwrap();
        let kept: Vec<String> =
            Cache::load(&path).by_profile["p"].windows.iter().map(|w| w.label.clone()).collect();
        assert_eq!(kept, ["5h", "7d", "7d opus", "1d"]);
    }

    #[test]
    fn expired_windows_do_not_bind() {
        let u = Usage {
            windows: vec![
                Window {
                    label: "5h".into(),
                    used: 100.0,
                    resets_at: Some(Utc::now() - chrono::Duration::minutes(1)),
                },
                Window { label: "7d".into(), used: 40.0, resets_at: None },
            ],
            fetched_at: Utc::now(),
        };
        assert_eq!(u.binding().unwrap().label, "7d");
    }
}
