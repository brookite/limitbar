//! OpenCode Go credentials: v2 SQLite, or v1 auth.json. BYOK limits belong to its vendor.
use std::path::Path;

use async_trait::async_trait;
use serde_json::Value;

use super::creds::{now, parse_reset, read_json_file};
use super::{
    classify_transport_error, discovery, response_json, AgentReading, LimitProvider, ProviderError,
};
use crate::model::Gauge;

pub const DEFAULT_BASE_URL: &str = "https://opencode.ai";
pub const HOST: &str = "opencode.ai";
pub struct OpenCodeProvider;

fn active_credentials(directory: &Path, database: &Path) -> Result<Value, ProviderError> {
    if database.is_file() {
        let db = super::sqlite::open_readonly(database)?;
        let has_table: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='credential')", [], |r| r.get(0))
            .map_err(|_| ProviderError::Malformed("Cannot read OpenCode schema".into()))?;
        if has_table {
            let mut statement = db.prepare("SELECT integration_id, value FROM credential WHERE active = 1 AND integration_id IN ('opencode-go', 'opencode', 'openai', 'anthropic')")
                .map_err(|_| ProviderError::Malformed("Cannot read OpenCode credentials".into()))?;
            let rows = statement
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map_err(|_| ProviderError::Malformed("Cannot read OpenCode credentials".into()))?;
            let mut entries = serde_json::Map::new();
            for row in rows {
                let (id, raw) = row.map_err(|_| {
                    ProviderError::Malformed("Cannot read OpenCode credential".into())
                })?;
                let entry = serde_json::from_str(&raw)
                    .map_err(|_| ProviderError::Malformed("Invalid OpenCode credential".into()))?;
                if entries.insert(id, entry).is_some() {
                    return Err(ProviderError::Malformed(
                        "Ambiguous active OpenCode accounts".into(),
                    ));
                }
            }
            // Once migrated, the active database is authoritative; old JSON may contain another account.
            return Ok(Value::Object(entries));
        }
    }
    read_json_file(&directory.join("auth.json"))
}

fn access(entry: &Value) -> Result<&str, ProviderError> {
    if entry["type"] == "oauth" {
        if entry["expires"]
            .as_f64()
            .is_some_and(|exp| exp <= now() as f64 * 1000.0)
        {
            return Err(ProviderError::SessionExpired);
        }
        entry["access"].as_str()
    } else if entry["type"] == "api" || entry["type"] == "key" {
        entry["key"].as_str()
    } else {
        None
    }
    .filter(|s| !s.is_empty() && !s.chars().any(char::is_whitespace))
    .ok_or(ProviderError::NotSignedIn)
}

pub fn parse_usage(body: &Value, at: i64) -> Vec<Gauge> {
    let mut gauges = Vec::new();
    for (key, label) in [
        ("rolling", "Session (5h)"),
        ("weekly", "Weekly"),
        ("monthly", "Monthly"),
    ] {
        let window = &body["usage"][key];
        let Some(used) = window["usagePercent"]
            .as_f64()
            .filter(|v| v.is_finite() && *v >= 0.0)
        else {
            continue;
        };
        let reset =
            parse_reset(window.get("resetAt").or_else(|| window.get("resetsAt"))).or_else(|| {
                window["resetInSec"]
                    .as_i64()
                    .map(|s| at.saturating_add(s.max(0)))
            });
        gauges.push(Gauge::new(key, label, used, reset));
    }
    gauges
}

#[async_trait]
impl LimitProvider for OpenCodeProvider {
    fn id(&self) -> &'static str {
        "opencode"
    }
    fn display_name(&self) -> &'static str {
        "OpenCode"
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
        let directory = discovery::data_dir()
            .ok_or(ProviderError::NotSignedIn)?
            .join("opencode");
        let database = std::env::var_os("OPENCODE_DB")
            .filter(|p| !p.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "opencode.db".into());
        let database = directory.join(database);
        let entries =
            tokio::task::spawn_blocking(move || active_credentials(&directory, &database))
                .await
                .map_err(|_| {
                    ProviderError::Malformed("OpenCode credentials unavailable".into())
                })??;
        if let Some(go) = entries
            .get("opencode-go")
            .or_else(|| entries.get("opencode"))
        {
            let token = access(go)?;
            let response = http
                .get(format!("{}/zen/go/v1/usage", base.trim_end_matches('/')))
                .bearer_auth(token)
                .header("Accept", "application/json")
                .send()
                .await
                .map_err(|e| classify_transport_error(&e, HOST))?;
            let body = response_json(response, HOST).await?;
            let gauges = parse_usage(&body, now());
            if gauges.is_empty() {
                return Err(ProviderError::NoLimitsReported);
            }
            return Ok(AgentReading {
                gauges,
                note: Some("Go".into()),
                ..Default::default()
            });
        }
        // Reuse the vendor's exact request/parsing path with OpenCode's stored login.
        // These limits are shared with the original agent, not an extra allowance.
        if let Some(openai) = entries.get("openai").filter(|v| v["type"] == "oauth") {
            let token = access(openai)?;
            let vendor_base = if base == DEFAULT_BASE_URL {
                super::codex::DEFAULT_BASE_URL
            } else {
                base
            };
            let mut reading =
                super::codex::fetch_usage(http, vendor_base, token, openai["accountId"].as_str())
                    .await?;
            reading.note = Some("Codex · общий лимит".into());
            return Ok(reading);
        }
        if let Some(anthropic) = entries.get("anthropic").filter(|v| v["type"] == "oauth") {
            let token = access(anthropic)?;
            let vendor_base = if base == DEFAULT_BASE_URL {
                super::claude::DEFAULT_BASE_URL
            } else {
                base
            };
            let mut reading = super::claude::fetch_usage(http, vendor_base, token, None).await?;
            reading.note = Some("Claude · общий лимит".into());
            return Ok(reading);
        }
        if entries.as_object().is_some_and(|m| !m.is_empty()) {
            Err(ProviderError::NoLimitsReported)
        } else {
            Err(ProviderError::NotSignedIn)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::json;
    #[test]
    fn api_percent_is_not_a_fraction_and_optional_windows_stay_absent() {
        let gauges = parse_usage(
            &json!({"usage":{"rolling":{"usagePercent":0.5,"resetInSec":60},"weekly":{"usagePercent":100}}}),
            1000,
        );
        assert_eq!(gauges.len(), 2);
        assert_eq!(gauges[0].used_percent, 0.5);
        assert_eq!(gauges[0].resets_at, Some(1060));
        assert_eq!(gauges[1].used_percent, 100.0);
    }
    #[test]
    fn migrated_database_does_not_fall_back_to_stale_json() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("auth.json"),
            r#"{"opencode-go":{"type":"api","key":"stale"}}"#,
        )
        .unwrap();
        let path = tmp.path().join("opencode.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE credential(integration_id TEXT, value TEXT, active INTEGER);",
        )
        .unwrap();
        assert_eq!(active_credentials(tmp.path(), &path).unwrap(), json!({}));
        db.execute(
            "INSERT INTO credential VALUES (?1, ?2, 1)",
            ["opencode-go", r#"{"type":"key","key":"current"}"#],
        )
        .unwrap();
        let entries = active_credentials(tmp.path(), &path).unwrap();
        assert_eq!(access(&entries["opencode-go"]).unwrap(), "current");
        db.execute(
            "INSERT INTO credential VALUES (?1, ?2, 1)",
            ["opencode-go", r#"{"type":"key","key":"ambiguous"}"#],
        )
        .unwrap();
        assert!(active_credentials(tmp.path(), &path).is_err());
    }
}
