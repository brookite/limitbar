//! Cursor CLI JSON auth, with read-only IDE SQLite fallback. Grok shares this login.
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use base64::Engine;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

use super::creds::{home_dir, jwt_expiry, now, parse_reset};
use super::{
    classify_transport_error, discovery, response_json, AgentReading, LimitProvider, ProviderError,
};
use crate::model::Gauge;

pub const DEFAULT_BASE_URL: &str = "https://cursor.com";
pub const HOST: &str = "cursor.com";

pub struct CursorProvider;

fn cli_auth_path() -> Option<PathBuf> {
    if cfg!(windows) {
        discovery::config_dir().map(|p| p.join("Cursor/auth.json"))
    } else if cfg!(target_os = "macos") {
        home_dir().map(|p| p.join(".cursor/auth.json"))
    } else {
        discovery::config_dir().map(|p| p.join("cursor/auth.json"))
    }
}

fn ide_auth_path() -> Option<PathBuf> {
    discovery::config_dir().map(|p| p.join("Cursor/User/globalStorage/state.vscdb"))
}

fn sqlite_token(path: &Path) -> Result<Option<String>, ProviderError> {
    if !path.is_file() {
        return Ok(None);
    }
    let db = super::sqlite::open_readonly(path)?;
    let bytes: Option<Vec<u8>> = db
        .query_row(
            "SELECT CAST(value AS BLOB) FROM ItemTable WHERE key = ?1 LIMIT 1",
            ["cursorAuth/accessToken"],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| ProviderError::Malformed("Cannot read Cursor auth".into()))?;
    bytes
        .map(|bytes| {
            // Older Electron versions also stored ASCII tokens as UTF-16LE BLOBs.
            let (pairs, remainder) = bytes.as_chunks::<2>();
            if remainder.is_empty() && pairs.iter().all(|b| b[1] == 0) {
                String::from_utf16(
                    &pairs
                        .iter()
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .collect::<Vec<_>>(),
                )
                .map_err(|_| ProviderError::Malformed("Cursor token encoding".into()))
            } else {
                String::from_utf8(bytes)
                    .map_err(|_| ProviderError::Malformed("Cursor token encoding".into()))
            }
        })
        .transpose()
}

fn cookie(token: &str) -> Result<String, ProviderError> {
    if token.starts_with("cursor-safe-storage:") {
        return Err(ProviderError::SecureStorage);
    }
    if jwt_expiry(token).is_some_and(|exp| exp <= now()) {
        return Err(ProviderError::SessionExpired);
    }
    let payload = token.split('.').nth(1).ok_or(ProviderError::NotSignedIn)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| ProviderError::NotSignedIn)?;
    let claims: Value = serde_json::from_slice(&bytes).map_err(|_| ProviderError::NotSignedIn)?;
    let user = claims["sub"]
        .as_str()
        .and_then(|s| s.rsplit('|').next())
        .filter(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
        .ok_or(ProviderError::NotSignedIn)?;
    // Also validate the token before composing a Cookie header.
    if !token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-=".contains(&b))
    {
        return Err(ProviderError::NotSignedIn);
    }
    Ok(format!("WorkosCursorSessionToken={user}%3A%3A{token}"))
}

fn load_cookie() -> Result<String, ProviderError> {
    let mut failure = ProviderError::NotSignedIn;
    if let Some(path) = cli_auth_path().filter(|p| p.is_file()) {
        match super::creds::read_json_file(&path)
            .and_then(|v| cookie(v["accessToken"].as_str().unwrap_or_default()))
        {
            Ok(cookie) => return Ok(cookie),
            Err(err) => failure = err,
        }
    }
    if let Some(path) = ide_auth_path() {
        if let Some(token) = sqlite_token(&path)? {
            if !token.trim().is_empty() {
                return cookie(&token);
            }
        }
    }
    Err(failure)
}

fn percent(value: &Value, field: &str) -> Option<f64> {
    value[field].as_f64().filter(|v| v.is_finite() && *v >= 0.0)
}

fn budget(value: &Value) -> Option<f64> {
    let used = percent(value, "used")?;
    let limit = percent(value, "limit").filter(|v| *v > 0.0)?;
    Some(100.0 * used / limit)
}

/// Interpret only reported limits: a missing budget is never a zero-percent gauge.
pub fn parse_usage(
    summary: &Value,
    grok: Option<&Value>,
    unified: Option<&Value>,
    at: i64,
) -> AgentReading {
    let reset = parse_reset(summary.get("billingCycleEnd"));
    let mut gauges = Vec::new();
    let mut shared = false;
    if let Some(grok) = grok {
        let paid = grok["includedLimitZero"]
            .as_bool()
            .map(|b| !b)
            .or_else(|| grok["hasNonZeroIncludedLimit"].as_bool())
            .unwrap_or(false);
        let trial = !paid && parse_reset(grok.get("sandTrialExpiresAt")).is_some_and(|t| t > at);
        if let Some(used) = percent(grok, "usagePercent").filter(|_| paid || trial) {
            let grok_reset = if paid {
                parse_reset(grok.get("nextResetTimestampUtc"))
            } else {
                None
            };
            shared = paid
                && grok["isUnified"].as_bool() == Some(true)
                && grok_reset.is_some_and(|t| t > at);
            gauges.push(Gauge::new(
                "grok",
                if shared {
                    "Cursor + Grok (weekly)"
                } else if trial {
                    "Grok Bot (trial)"
                } else {
                    "Grok Bot"
                },
                used,
                grok_reset,
            ));
        }
    }
    let individual = &summary["individualUsage"];
    if !shared {
        let plan = &individual["plan"];
        let used = percent(plan, "totalPercentUsed")
            .or_else(|| budget(plan))
            .or_else(|| budget(&individual["overall"]))
            .or_else(|| budget(&summary["teamUsage"]["pooled"]));
        if let Some(used) = used {
            gauges.insert(0, Gauge::new("plan", "Plan (monthly)", used, reset));
        }
        for (field, id, label) in [
            ("autoPercentUsed", "auto", "Auto"),
            ("apiPercentUsed", "api", "API"),
        ] {
            if let Some(used) = percent(plan, field) {
                gauges.push(Gauge::new(id, label, used, reset));
            }
        }
    }
    let monthly = unified
        .filter(|v| v["availability"] == "available")
        .map(|v| &v["monthlyUsage"])
        .filter(|v| {
            parse_reset(v.get("periodStart")).is_some_and(|t| t <= at)
                && parse_reset(v.get("resetsAt")).is_some_and(|t| t > at)
        });
    let extra = monthly
        .and_then(|v| {
            Some((
                percent(v, "usedCents")?,
                percent(v, "limitCents").filter(|v| *v > 0.0)?,
                parse_reset(v.get("resetsAt")),
            ))
        })
        .map(|(used, limit, reset)| (100.0 * used / limit, reset))
        .or_else(|| budget(&individual["onDemand"]).map(|p| (p, reset)));
    if let Some((used, reset)) = extra {
        gauges.push(Gauge::new(
            "on_demand",
            "Additional spend (monthly)",
            used,
            reset,
        ));
    }
    AgentReading {
        gauges,
        note: summary["membershipType"].as_str().map(String::from),
        ..Default::default()
    }
}

async fn request(
    http: &reqwest::Client,
    base: &str,
    cookie: &str,
    path: &str,
    post: bool,
) -> Result<Value, ProviderError> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let request = if post {
        let origin = reqwest::Url::parse(base)
            .map_err(|_| ProviderError::Malformed("Cursor endpoint".into()))?
            .origin()
            .ascii_serialization();
        http.post(url).header("Origin", origin).json(&json!({}))
    } else {
        http.get(url)
    };
    response_json(
        request
            .header("Cookie", cookie)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| classify_transport_error(&e, HOST))?,
        HOST,
    )
    .await
}

#[async_trait]
impl LimitProvider for CursorProvider {
    fn id(&self) -> &'static str {
        "cursor"
    }
    fn display_name(&self) -> &'static str {
        "Cursor"
    }
    fn is_installed(&self) -> bool {
        discovery::installed(self.id())
    }
    fn host(&self) -> &'static str {
        HOST
    }
    fn default_base_url(&self) -> &'static str {
        DEFAULT_BASE_URL
    }
    async fn fetch(
        &self,
        http: &reqwest::Client,
        base: &str,
    ) -> Result<AgentReading, ProviderError> {
        let cookie = tokio::task::spawn_blocking(load_cookie)
            .await
            .map_err(|_| ProviderError::Malformed("Cursor credentials unavailable".into()))??;
        let (summary, grok) = tokio::join!(
            request(http, base, &cookie, "/api/usage-summary", false),
            request(
                http,
                base,
                &cookie,
                "/api/dashboard/get-sand-usage-status",
                true
            )
        );
        if let Err(ProviderError::RateLimited { retry_after }) = &grok {
            return Err(ProviderError::RateLimited {
                retry_after: *retry_after,
            });
        }
        let summary = summary?;
        let unified_account = summary["membershipType"]
            .as_str()
            .is_some_and(|s| s.starts_with("unified_"))
            && summary["limitType"] == "user";
        let unified = if unified_account {
            let result = request(
                http,
                base,
                &cookie,
                "/api/dashboard/v2/usage-status?scope=user",
                false,
            )
            .await;
            if let Err(ProviderError::RateLimited { retry_after }) = &result {
                return Err(ProviderError::RateLimited {
                    retry_after: *retry_after,
                });
            }
            result.ok()
        } else {
            None
        };
        let mut reading = parse_usage(&summary, grok.as_ref().ok(), unified.as_ref(), now());
        if reading.gauges.is_empty() {
            return Err(ProviderError::NoLimitsReported);
        }
        if grok.is_err() {
            reading.note = Some(format!(
                "{} · Grok: unavailable",
                reading.note.as_deref().unwrap_or("Cursor")
            ));
        }
        Ok(reading)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn shared_grok_pool_replaces_legacy_monthly_limits() {
        let summary =
            json!({"individualUsage":{"plan":{"totalPercentUsed":0,"autoPercentUsed":0}}});
        let grok = json!({"includedLimitZero":false,"isUnified":true,"usagePercent":83,"nextResetTimestampUtc":"2099-01-01T00:00:00Z"});
        let r = parse_usage(&summary, Some(&grok), None, now());
        assert_eq!(r.gauges.len(), 1);
        assert_eq!(r.gauges[0].label, "Cursor + Grok (weekly)");
        assert_eq!(r.gauges[0].used_percent, 83.0);
    }

    #[test]
    fn exhausted_grok_is_still_visible_and_missing_limits_are_not_zero() {
        let grok = json!({"includedLimitZero":false,"hasAvailableUsage":false,"usagePercent":100});
        assert_eq!(
            parse_usage(&json!({}), Some(&grok), None, now()).gauges[0].used_percent,
            100.0
        );
        assert!(parse_usage(&json!({}), None, None, now()).gauges.is_empty());
    }

    #[test]
    fn trial_expiry_is_not_a_quota_reset() {
        let mut grok = json!({"includedLimitZero":true,"usagePercent":42,"sandTrialExpiresAt":"2099-01-01T00:00:00Z","nextResetTimestampUtc":"2099-01-02T00:00:00Z"});
        let r = parse_usage(&json!({}), Some(&grok), None, now());
        assert_eq!(r.gauges[0].resets_at, None);
        grok["sandTrialExpiresAt"] = json!("2000-01-01T00:00:00Z");
        assert!(parse_usage(&json!({}), Some(&grok), None, now())
            .gauges
            .is_empty());
    }

    #[test]
    fn ide_sqlite_reads_live_wal_and_does_not_change_the_token() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.vscdb");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE ItemTable (key TEXT, value BLOB);")
            .unwrap();
        db.execute(
            "INSERT INTO ItemTable VALUES (?1, ?2)",
            ["cursorAuth/accessToken", "fixture-token"],
        )
        .unwrap();
        assert_eq!(
            sqlite_token(&path).unwrap().as_deref(),
            Some("fixture-token")
        );
        assert_eq!(
            sqlite_token(&path).unwrap().as_deref(),
            Some("fixture-token")
        );
        assert!(db.is_autocommit());
    }
}
