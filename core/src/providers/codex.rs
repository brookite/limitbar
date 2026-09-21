//! Codex support.
//!
//! The request is formed the way the agent's own CLI forms it — the endpoint and
//! the headers below are what authorise and route the call — with one deliberate
//! difference: the User-Agent is ours.

use async_trait::async_trait;
use serde::Deserialize;

use crate::model::{clamp_percent, Gauge};

use super::claude::retry_after_secs;
use super::creds::{agent_home, jwt_expiry, now, read_credentials};
use super::{classify_transport_error, AgentReading, LimitProvider, ProviderError};

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
pub const HOST: &str = "chatgpt.com";
const USAGE_PATH: &str = "/codex/usage";

/// Functional header: the backend routes and authorises Codex traffic by it.
const ORIGINATOR: &str = "codex_cli_rs";

const HOUR: i64 = 3_600;
const DAY: i64 = 24 * HOUR;

#[derive(Default)]
pub struct CodexProvider;

#[derive(Debug, Deserialize)]
struct StoredCredentials {
    auth_mode: Option<String>,
    tokens: Option<Tokens>,
}

#[derive(Debug, Deserialize)]
struct Tokens {
    access_token: String,
    account_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct UsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<RateLimit>,
}

#[derive(Debug, Default, Deserialize)]
struct RateLimit {
    primary_window: Option<Window>,
    secondary_window: Option<Window>,
}

/// Codex reports a window as a percentage, its length, and the reset both as an
/// absolute epoch second and as a countdown. We prefer the absolute value and fall
/// back to the countdown anchored at fetch time.
#[derive(Debug, Default, Deserialize)]
struct Window {
    used_percent: Option<f64>,
    limit_window_seconds: Option<i64>,
    reset_after_seconds: Option<i64>,
    reset_at: Option<i64>,
}

/// Names the window by its length, because the API labels it only by duration.
fn label_for(window_seconds: Option<i64>, fallback: &str) -> String {
    match window_seconds {
        Some(secs) if secs > 0 && secs % DAY == 0 => {
            let days = secs / DAY;
            if days == 7 {
                "Weekly".to_string()
            } else {
                format!("{days}d window")
            }
        }
        Some(secs) if secs > 0 && secs % HOUR == 0 => format!("Session ({}h)", secs / HOUR),
        Some(secs) if secs > 0 => format!("{}m window", (secs + 59) / 60),
        _ => fallback.to_string(),
    }
}

fn to_gauges(response: &UsageResponse, at: i64) -> Vec<Gauge> {
    let Some(rate_limit) = response.rate_limit.as_ref() else {
        return Vec::new();
    };
    [
        ("primary", rate_limit.primary_window.as_ref(), "Primary"),
        (
            "secondary",
            rate_limit.secondary_window.as_ref(),
            "Secondary",
        ),
    ]
    .into_iter()
    .filter_map(|(id, window, fallback)| {
        let window = window?;
        let used_percent = window.used_percent?;
        // `reset_at` is absolute; `reset_after_seconds` is relative to the reply, so
        // it only means anything once anchored to the moment we asked.
        let resets_at = window
            .reset_at
            .or_else(|| window.reset_after_seconds.map(|secs| at + secs.max(0)));
        Some(Gauge::new(
            id,
            label_for(window.limit_window_seconds, fallback),
            clamp_percent(used_percent),
            resets_at,
        ))
    })
    .collect()
}

#[async_trait]
impl LimitProvider for CodexProvider {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex"
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
        base_url: &str,
    ) -> Result<AgentReading, ProviderError> {
        let home = agent_home("CODEX_HOME", ".codex");
        let stored: StoredCredentials = read_credentials(home.as_deref(), "auth.json")?;

        // Subscription limits only exist for a ChatGPT login; an API-key setup is
        // billed per request and has nothing for us to draw.
        if stored.auth_mode.as_deref() != Some("chatgpt") {
            return Err(ProviderError::ApiKeyMode);
        }
        let tokens = stored.tokens.ok_or(ProviderError::NotSignedIn)?;

        // Codex stores no expiry next to the token, so we read the JWT's own `exp`.
        if let Some(exp) = jwt_expiry(&tokens.access_token) {
            if exp <= now() {
                return Err(ProviderError::SessionExpired);
            }
        }

        let mut request = http
            .get(format!("{}{}", base_url.trim_end_matches('/'), USAGE_PATH))
            .header("Authorization", format!("Bearer {}", tokens.access_token))
            .header("Accept", "application/json")
            .header("originator", ORIGINATOR);
        if let Some(account_id) = tokens.account_id.as_deref() {
            request = request.header("chatgpt-account-id", account_id);
        }

        let response = request
            .send()
            .await
            .map_err(|err| classify_transport_error(&err, HOST))?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ProviderError::Unauthorized);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::RateLimited {
                retry_after: retry_after_secs(response.headers()),
            });
        }
        if !status.is_success() {
            return Err(ProviderError::ServiceUnavailable(status.as_u16()));
        }

        let body = response
            .text()
            .await
            .map_err(|err| classify_transport_error(&err, HOST))?;
        let parsed: UsageResponse =
            serde_json::from_str(&body).map_err(|err| ProviderError::Malformed(err.to_string()))?;

        let gauges = to_gauges(&parsed, now());
        if gauges.is_empty() {
            return Err(ProviderError::NoLimitsReported);
        }

        Ok(AgentReading {
            gauges,
            note: parsed.plan_type,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> UsageResponse {
        serde_json::from_str(json).expect("fixture parses")
    }

    #[test]
    fn maps_both_windows_and_anchors_their_reset() {
        let parsed = parse(
            r#"{"plan_type":"plus",
                "rate_limit":{
                  "primary_window":{"used_percent":37.5,"limit_window_seconds":18000,"reset_after_seconds":4200},
                  "secondary_window":{"used_percent":8.0,"limit_window_seconds":604800,"reset_after_seconds":300000}}}"#,
        );
        let gauges = to_gauges(&parsed, 1_000_000);
        assert_eq!(gauges.len(), 2);
        assert_eq!(gauges[0].id, "primary");
        assert_eq!(gauges[0].label, "Session (5h)");
        assert_eq!(gauges[0].used_percent, 37.5);
        assert_eq!(gauges[0].resets_at, Some(1_004_200));
        assert_eq!(gauges[1].label, "Weekly");
        assert_eq!(gauges[1].resets_at, Some(1_300_000));
    }

    #[test]
    fn a_countdown_is_anchored_to_the_moment_we_asked() {
        let parsed = parse(
            r#"{"rate_limit":{"primary_window":{"used_percent":12.0,"limit_window_seconds":18000,
                                                "reset_after_seconds":4200}}}"#,
        );
        assert_eq!(to_gauges(&parsed, 1_000_000)[0].resets_at, Some(1_004_200));
    }

    #[test]
    fn a_missing_secondary_window_is_simply_not_drawn() {
        let parsed = parse(
            r#"{"rate_limit":{"primary_window":{"used_percent":12.0,"limit_window_seconds":18000,"reset_after_seconds":60}}}"#,
        );
        let ids: Vec<String> = to_gauges(&parsed, 0).into_iter().map(|g| g.id).collect();
        assert_eq!(ids, vec!["primary"]);
    }

    #[test]
    fn an_empty_rate_limit_block_yields_nothing() {
        assert!(to_gauges(&parse(r#"{"rate_limit":{}}"#), 0).is_empty());
        assert!(to_gauges(&parse(r#"{"plan_type":"pro"}"#), 0).is_empty());
        assert!(to_gauges(&parse("{}"), 0).is_empty());
    }

    #[test]
    fn a_window_without_a_percentage_is_skipped() {
        let parsed = parse(r#"{"rate_limit":{"primary_window":{"limit_window_seconds":18000}}}"#);
        assert!(to_gauges(&parsed, 0).is_empty());
    }

    #[test]
    fn a_window_without_a_reset_still_draws_a_bar() {
        let parsed = parse(
            r#"{"rate_limit":{"primary_window":{"used_percent":50.0,"limit_window_seconds":18000}}}"#,
        );
        let gauges = to_gauges(&parsed, 0);
        assert_eq!(gauges.len(), 1);
        assert_eq!(gauges[0].resets_at, None);
    }

    #[test]
    fn unknown_fields_do_not_break_the_parse() {
        let parsed = parse(
            r#"{"plan_type":"pro","normal_model_slug":"gpt-5","credits":{"balance":0},
                "additional_rate_limits":[{"limit_name":"x"}],
                "rate_limit":{"allowed":true,"limit_reached":false,
                              "primary_window":{"used_percent":5.0,"limit_window_seconds":18000,
                                                "reset_after_seconds":10,"remaining":95,"remaining_percent":95.0}},
                "brand_new_field":true}"#,
        );
        assert_eq!(to_gauges(&parsed, 0).len(), 1);
    }

    #[test]
    fn window_labels_come_from_their_length() {
        assert_eq!(label_for(Some(5 * 3600), "x"), "Session (5h)");
        assert_eq!(label_for(Some(7 * 86400), "x"), "Weekly");
        assert_eq!(label_for(Some(30 * 86400), "x"), "30d window");
        assert_eq!(label_for(Some(1800), "x"), "30m window");
        assert_eq!(label_for(None, "Primary"), "Primary");
        assert_eq!(label_for(Some(0), "Primary"), "Primary");
    }

    #[test]
    fn percentages_out_of_range_are_clamped() {
        let parsed = parse(
            r#"{"rate_limit":{"primary_window":{"used_percent":142.0,"limit_window_seconds":18000}}}"#,
        );
        assert_eq!(to_gauges(&parsed, 0)[0].used_percent, 100.0);
    }
}
