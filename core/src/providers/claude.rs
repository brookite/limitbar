//! Claude Code support.
//!
//! The request is formed the way the agent's own CLI forms it — the endpoint and
//! the headers below are what authorise and route the call — with one deliberate
//! difference: the User-Agent is ours.

use async_trait::async_trait;
use serde::Deserialize;

use crate::model::{clamp_percent, Gauge};

use super::creds::{agent_home, is_window_live, now, parse_reset, read_credentials};
use super::{classify_transport_error, AgentReading, LimitProvider, ProviderError};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const HOST: &str = "api.anthropic.com";
const USAGE_PATH: &str = "/api/oauth/usage";

/// Required by the endpoint; these values are not ours to choose.
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA: &str = "oauth-2025-04-20";
const CLIENT_PLATFORM: &str = "claude_code_cli";

/// The windows we render, in the order they appear in the widget, with the names
/// Claude Code uses for them in its own limit messages.
const WINDOWS: [(&str, &str); 6] = [
    ("five_hour", "Session (5h)"),
    ("seven_day", "Weekly"),
    ("seven_day_opus", "Opus (7d)"),
    ("seven_day_sonnet", "Sonnet (7d)"),
    ("seven_day_oauth_apps", "Apps (7d)"),
    ("cinder_cove", "Extra (7d)"),
];

#[derive(Default)]
pub struct ClaudeProvider;

#[derive(Debug, Deserialize)]
struct StoredCredentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuthCredentials>,
}

#[derive(Debug, Deserialize)]
struct OAuthCredentials {
    #[serde(rename = "accessToken")]
    access_token: String,
    /// Milliseconds since the epoch, as Claude Code stores it.
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
}

/// One limit window.
///
/// `utilization` is already a percentage (0..100) — cross-checked against the
/// sibling `limits[].percent`, which carries the same number. `resets_at` arrives
/// as an RFC 3339 string, so it is kept raw and parsed by [`parse_reset`].
#[derive(Debug, Default, Deserialize)]
struct UsageWindow {
    utilization: Option<f64>,
    resets_at: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
    seven_day_opus: Option<UsageWindow>,
    seven_day_sonnet: Option<UsageWindow>,
    seven_day_oauth_apps: Option<UsageWindow>,
    cinder_cove: Option<UsageWindow>,
}

impl UsageResponse {
    fn window(&self, key: &str) -> Option<&UsageWindow> {
        match key {
            "five_hour" => self.five_hour.as_ref(),
            "seven_day" => self.seven_day.as_ref(),
            "seven_day_opus" => self.seven_day_opus.as_ref(),
            "seven_day_sonnet" => self.seven_day_sonnet.as_ref(),
            "seven_day_oauth_apps" => self.seven_day_oauth_apps.as_ref(),
            "cinder_cove" => self.cinder_cove.as_ref(),
            _ => None,
        }
    }
}

fn to_gauges(response: &UsageResponse) -> Vec<Gauge> {
    WINDOWS
        .iter()
        .filter_map(|(key, label)| {
            let window = response.window(key)?;
            // A window the API reports as empty, or one whose reset has already
            // passed, is not a live limit; Claude Code hides those too.
            let utilization = window.utilization?;
            let resets_at = parse_reset(window.resets_at.as_ref());
            if !is_window_live(resets_at) {
                return None;
            }
            Some(Gauge::new(
                *key,
                *label,
                clamp_percent(utilization),
                resets_at,
            ))
        })
        .collect()
}

#[async_trait]
impl LimitProvider for ClaudeProvider {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
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
        let home = agent_home("CLAUDE_CONFIG_DIR", ".claude");
        let stored: StoredCredentials = read_credentials(home.as_deref(), ".credentials.json")?;
        let oauth = stored.oauth.ok_or(ProviderError::NotSignedIn)?;

        // `expiresAt` is in milliseconds. We check it before spending a request so a
        // logged-out user gets a useful instruction instead of a bare 401.
        if let Some(expires_at) = oauth.expires_at {
            if expires_at <= now().saturating_mul(1000) {
                return Err(ProviderError::SessionExpired);
            }
        }

        let response = http
            .get(format!("{}{}", base_url.trim_end_matches('/'), USAGE_PATH))
            .header("Authorization", format!("Bearer {}", oauth.access_token))
            .header("Content-Type", "application/json")
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("anthropic-beta", ANTHROPIC_BETA)
            .header("anthropic-client-platform", CLIENT_PLATFORM)
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

        let gauges = to_gauges(&parsed);
        if gauges.is_empty() {
            return Err(ProviderError::NoLimitsReported);
        }

        Ok(AgentReading {
            gauges,
            note: oauth.subscription_type,
        })
    }
}

/// Honours the server's own cooling-off period when it states one.
pub fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> UsageResponse {
        serde_json::from_str(json).expect("fixture parses")
    }

    /// The endpoint formats reset moments as RFC 3339, so the fixtures do too.
    fn future() -> String {
        let at = std::time::SystemTime::now() + std::time::Duration::from_secs(3_600);
        chrono::DateTime::<chrono::Utc>::from(at).to_rfc3339()
    }

    fn past() -> String {
        let at = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        chrono::DateTime::<chrono::Utc>::from(at).to_rfc3339()
    }

    #[test]
    fn maps_windows_to_gauges_in_display_order() {
        let at = future();
        let parsed = parse(&format!(
            r#"{{"five_hour":{{"utilization":17.0,"resets_at":"{at}"}},
                 "seven_day":{{"utilization":31.0,"resets_at":"{at}"}},
                 "seven_day_opus":{{"utilization":90.0,"resets_at":"{at}"}}}}"#
        ));
        let gauges = to_gauges(&parsed);
        let ids: Vec<&str> = gauges.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, vec!["five_hour", "seven_day", "seven_day_opus"]);
        assert_eq!(gauges[0].label, "Session (5h)");
        // `utilization` is already a percentage, so the bar reads 17%.
        assert_eq!(gauges[0].used_percent, 17.0);
        assert_eq!(gauges[2].used_percent, 90.0);
        assert!(
            gauges[0].resets_at.is_some(),
            "the RFC 3339 reset is understood"
        );
    }

    #[test]
    fn windows_the_api_leaves_empty_are_not_drawn() {
        let at = future();
        let parsed = parse(&format!(
            r#"{{"five_hour":{{"utilization":null,"resets_at":"{at}"}},
                 "seven_day":{{"utilization":50.0,"resets_at":"{at}"}},
                 "seven_day_opus":null}}"#
        ));
        let ids: Vec<String> = to_gauges(&parsed).into_iter().map(|g| g.id).collect();
        assert_eq!(ids, vec!["seven_day"]);
    }

    #[test]
    fn windows_that_have_already_reset_are_dropped() {
        let parsed = parse(&format!(
            r#"{{"five_hour":{{"utilization":80.0,"resets_at":"{}"}},
                 "seven_day":{{"utilization":30.0,"resets_at":"{}"}}}}"#,
            past(),
            future()
        ));
        let ids: Vec<String> = to_gauges(&parsed).into_iter().map(|g| g.id).collect();
        assert_eq!(ids, vec!["seven_day"]);
    }

    #[test]
    fn unknown_fields_do_not_break_the_parse() {
        // The endpoint is private and may grow fields at any time.
        let at = future();
        // Shaped after a real response, which carries a crowd of keys we have no
        // use for, plus a parallel `limits` array saying the same thing twice.
        let parsed = parse(&format!(
            r#"{{"five_hour":{{"utilization":17.0,"resets_at":"{at}","limit_dollars":null,
                               "locked_reason":null,"remaining_dollars":null,"used_dollars":null}},
                 "amber_gauge":null,"juniper_tide":null,"tangelo":null,
                 "extra_usage":{{"is_enabled":false,"currency":"usd"}},
                 "seven_day_breakdown":{{"as_of":"{at}","rows":[]}},
                 "spend":{{"balance":0,"percent":0}},
                 "limits":[{{"kind":"session","group":"session","percent":17,
                             "resets_at":"{at}","severity":"normal","is_active":false,"scope":null}}],
                 "member_dashboard_available":false,
                 "something_new":42}}"#
        ));
        assert_eq!(to_gauges(&parsed).len(), 1);
        assert_eq!(to_gauges(&parsed)[0].used_percent, 17.0);
    }

    #[test]
    fn an_empty_body_yields_no_gauges() {
        assert!(to_gauges(&parse("{}")).is_empty());
    }

    #[test]
    fn a_window_without_a_reset_still_draws_a_bar() {
        let parsed = parse(r#"{"five_hour":{"utilization":17.0}}"#);
        let gauges = to_gauges(&parsed);
        assert_eq!(gauges.len(), 1);
        assert_eq!(gauges[0].resets_at, None);
    }

    #[test]
    fn retry_after_is_read_when_present() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_secs(&headers), None);
        headers.insert(reqwest::header::RETRY_AFTER, "120".parse().unwrap());
        assert_eq!(retry_after_secs(&headers), Some(120));
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(
            retry_after_secs(&headers),
            None,
            "http-date form is left to our own ladder"
        );
    }
}
