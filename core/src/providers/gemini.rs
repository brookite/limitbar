//! Reads Gemini CLI's existing Google OAuth login without refreshing it.
use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::creds::{home_dir, now, parse_reset, read_json_file};
use super::{
    classify_transport_error, discovery, response_json, AgentReading, LimitProvider, ProviderError,
};
use crate::model::Gauge;

pub const DEFAULT_BASE_URL: &str = "https://cloudcode-pa.googleapis.com";
pub const HOST: &str = "cloudcode-pa.googleapis.com";
pub struct GeminiProvider;

pub fn parse_quota(value: &Value) -> Vec<Gauge> {
    // Input/output token buckets can describe the same model. Keep its tightest quota.
    let mut models: BTreeMap<String, (f64, Option<i64>)> = BTreeMap::new();
    for bucket in value["buckets"].as_array().into_iter().flatten() {
        let (Some(model), Some(remaining)) = (
            bucket["modelId"].as_str(),
            bucket["remainingFraction"].as_f64(),
        ) else {
            continue;
        };
        if model.is_empty() || !remaining.is_finite() || !(0.0..=1.0).contains(&remaining) {
            continue;
        }
        let reset = parse_reset(bucket.get("resetTime"));
        if models
            .get(model)
            .is_none_or(|(existing, _)| remaining < *existing)
        {
            models.insert(model.to_owned(), (remaining, reset));
        }
    }
    models
        .into_iter()
        .map(|(model, (remaining, reset))| {
            Gauge::new(model.clone(), model, (1.0 - remaining) * 100.0, reset)
        })
        .collect()
}

async fn post(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    method: &str,
    body: &Value,
) -> Result<Value, ProviderError> {
    let response = http
        .post(format!(
            "{}/v1internal:{method}",
            base.trim_end_matches('/')
        ))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|e| classify_transport_error(&e, HOST))?;
    response_json(response, HOST).await
}

#[async_trait]
impl LimitProvider for GeminiProvider {
    fn id(&self) -> &'static str {
        "gemini"
    }
    fn display_name(&self) -> &'static str {
        "Gemini CLI"
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
        let home = home_dir()
            .ok_or(ProviderError::NotSignedIn)?
            .join(".gemini");
        // API-key / Vertex setups do not use the consumer OAuth quota endpoint.
        let settings_path = home.join("settings.json");
        if settings_path.is_file() {
            let settings = read_json_file(&settings_path)?;
            if settings
                .pointer("/security/auth/selectedType")
                .and_then(Value::as_str)
                .is_some_and(|t| t != "oauth-personal")
            {
                return Err(ProviderError::ApiKeyMode);
            }
        }
        let stored = read_json_file(&home.join("oauth_creds.json"))?;
        if stored["expiry_date"]
            .as_f64()
            .is_some_and(|t| t <= now() as f64 * 1000.0)
        {
            return Err(ProviderError::SessionExpired);
        }
        let token = stored["access_token"]
            .as_str()
            .filter(|t| !t.trim().is_empty())
            .ok_or(ProviderError::NotSignedIn)?;
        let configured_project = std::env::var("GOOGLE_CLOUD_PROJECT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("GOOGLE_CLOUD_PROJECT_ID")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            });
        let mut body = json!({"metadata":{"ideType":"GEMINI_CLI","pluginType":"GEMINI"}});
        if let Some(project) = &configured_project {
            body["cloudaicompanionProject"] = json!(project);
        }
        let status = post(http, base, token, "loadCodeAssist", &body).await?;
        let project = status["cloudaicompanionProject"]
            .as_str()
            .or_else(|| {
                status
                    .pointer("/cloudaicompanionProject/id")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                status
                    .pointer("/cloudaicompanionProject/projectId")
                    .and_then(Value::as_str)
            })
            .map(String::from)
            .or(configured_project);
        let quota_body = project
            .map(|p| json!({"project":p}))
            .unwrap_or_else(|| json!({}));
        let quota = post(http, base, token, "retrieveUserQuota", &quota_body).await?;
        let gauges = parse_quota(&quota);
        if gauges.is_empty() {
            return Err(ProviderError::NoLimitsReported);
        }
        let note = status
            .pointer("/paidTier/name")
            .or_else(|| status.pointer("/currentTier/name"))
            .or_else(|| status.pointer("/currentTier/id"))
            .and_then(Value::as_str)
            .map(String::from);
        Ok(AgentReading {
            gauges,
            note,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uses_tightest_model_bucket_and_preserves_exhaustion() {
        let gauges = parse_quota(&json!({"buckets":[
            {"modelId":"gemini-pro","remainingFraction":0.9},
            {"modelId":"gemini-pro","remainingFraction":0.25,"resetTime":"2099-01-01T00:00:00Z"},
            {"modelId":"gemini-flash","remainingFraction":0},
            {"modelId":"unknown"}, {"modelId":"invalid","remainingFraction":2}
        ]}));
        assert_eq!(gauges.len(), 2);
        assert_eq!(gauges[0].used_percent, 100.0);
        assert_eq!(gauges[1].used_percent, 75.0);
        assert!(gauges[1].resets_at.is_some());
    }
}
