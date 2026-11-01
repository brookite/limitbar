//! Claude usage from the existing on-disk login. No token refresh or auth writes.
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use super::creds::{
    agent_home, is_window_live, non_empty, now, number, parse_reset, read_json_file,
    read_replacing_file, slug, UsageCredentials,
};
use super::{classify_transport_error, response_json, AgentReading, LimitProvider, ProviderError};
use crate::model::Gauge;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const HOST: &str = "api.anthropic.com";
const USAGE_PATH: &str = "/api/oauth/usage";

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

#[derive(Deserialize)]
#[serde(transparent)]
struct UsageResponse(Value);

fn subscription_note(oauth: &Value) -> Option<String> {
    let mut note = non_empty(oauth, "subscriptionType");
    if note.as_deref() == Some("max") {
        if let Some(tier) = non_empty(oauth, "rateLimitTier") {
            if let Some(multiplier) = tier
                .split('_')
                .find(|p| p.ends_with('x') && p.trim_end_matches('x').parse::<u32>().is_ok())
            {
                note = Some(format!("max {multiplier}"));
            }
        }
    }
    note
}

fn load_credentials() -> Result<UsageCredentials, ProviderError> {
    let home = agent_home("CLAUDE_CONFIG_DIR", ".claude").ok_or(ProviderError::NotSignedIn)?;
    let stored = read_json_file(&home.join(".credentials.json"))?;
    let oauth = &stored["claudeAiOauth"];
    let token = non_empty(oauth, "accessToken").ok_or(ProviderError::NotSignedIn)?;
    if number(oauth, "expiresAt").is_some_and(|t| t <= now() as f64 * 1000.0) {
        return Err(ProviderError::SessionExpired);
    }
    // Missing scope metadata is unknown, not proof of insufficient permissions.
    if let Some(scopes) = oauth["scopes"].as_array() {
        if scopes.iter().all(Value::is_string) && !scopes.iter().any(|s| s == "user:profile") {
            return Err(ProviderError::MissingScope);
        }
    }
    Ok(UsageCredentials::new(
        "claude",
        token,
        None,
        subscription_note(oauth),
    ))
}

fn usage_window(value: &Value, id: &str, label: &str) -> Option<Gauge> {
    let used = number(value, "utilization")?;
    let reset = parse_reset(value.get("resets_at"));
    is_window_live(reset).then(|| Gauge::new(id, label, used, reset))
}

fn to_gauges(response: &UsageResponse) -> Vec<Gauge> {
    let body = &response.0;
    let mut gauges: Vec<Gauge> = WINDOWS
        .iter()
        .filter_map(|(key, label)| usage_window(&body[*key], key, label))
        .collect();
    for entry in body["limits"].as_array().into_iter().flatten() {
        if entry["kind"] != "weekly_scoped" || entry["group"] != "weekly" {
            continue;
        }
        let model = &entry["scope"]["model"];
        let Some(name) = non_empty(model, "display_name") else {
            continue;
        };
        let identity = non_empty(model, "id").unwrap_or_else(|| name.clone());
        if slug(&name) == "all-models" || slug(&identity).ends_with("all-models") {
            continue;
        }
        let Some(used) = number(entry, "percent") else {
            continue;
        };
        let id = format!("weekly_scoped:{}", slug(&identity));
        if gauges.iter().any(|g| g.id == id) {
            continue;
        }
        let reset = parse_reset(entry.get("resets_at"));
        if is_window_live(reset) {
            gauges.push(Gauge::new(id, format!("{name} (7d)"), used, reset));
        }
    }
    for alias in [
        "seven_day_routines",
        "seven_day_claude_routines",
        "claude_routines",
        "routines",
        "routine",
        "seven_day_cowork",
        "cowork",
    ] {
        if let Some(gauge) = usage_window(&body[alias], "routines", "Routines / Cowork (7d)") {
            gauges.push(gauge);
            break;
        }
    }
    let extra = &body["extra_usage"];
    if extra["is_enabled"] == true {
        if let (Some(used), Some(limit)) = (
            number(extra, "used_credits"),
            number(extra, "monthly_limit").filter(|n| *n > 0.0),
        ) {
            if used >= 0.0 {
                gauges.push(Gauge::new(
                    "extra_usage",
                    "Extra usage (monthly)",
                    used / limit * 100.0,
                    None,
                ));
            }
        }
    }
    gauges
}

#[async_trait]
impl LimitProvider for ClaudeProvider {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn display_name(&self) -> &'static str {
        "Claude Code"
    }
    fn is_installed(&self) -> bool {
        super::discovery::installed(self.id())
    }
    fn host(&self) -> &'static str {
        HOST
    }
    fn default_base_url(&self) -> &'static str {
        DEFAULT_BASE_URL
    }
    async fn prepare(&self) -> Result<Option<UsageCredentials>, ProviderError> {
        read_replacing_file(load_credentials).await.map(Some)
    }
    async fn fetch_prepared(
        &self,
        http: &reqwest::Client,
        base: &str,
        credentials: Option<UsageCredentials>,
    ) -> Result<AgentReading, ProviderError> {
        let credentials = credentials.ok_or(ProviderError::NotSignedIn)?;
        fetch_usage(http, base, &credentials.access_token, credentials.note).await
    }
    async fn fetch(
        &self,
        http: &reqwest::Client,
        base: &str,
    ) -> Result<AgentReading, ProviderError> {
        self.fetch_prepared(http, base, self.prepare().await?).await
    }
}

/// Also used by OpenCode with its existing Anthropic OAuth credential.
pub async fn fetch_usage(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    note: Option<String>,
) -> Result<AgentReading, ProviderError> {
    let response = http
        .get(format!("{}{USAGE_PATH}", base.trim_end_matches('/')))
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-client-platform", "claude_code_cli")
        .send()
        .await
        .map_err(|e| classify_transport_error(&e, HOST))?;
    let body = response_json(response, HOST).await?;
    if !body.is_object() {
        return Err(ProviderError::Malformed("Expected usage object".into()));
    }
    let gauges = to_gauges(&UsageResponse(body));
    if gauges.is_empty() {
        return Err(ProviderError::NoLimitsReported);
    }
    Ok(AgentReading {
        gauges,
        note,
        ..Default::default()
    })
}

/// Both standard forms of Retry-After; the caller still enforces a polling floor.
pub fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    retry_after_at(headers, now())
}

fn retry_after_at(headers: &reqwest::header::HeaderMap, at: i64) -> Option<u64> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    let reset = chrono::DateTime::parse_from_rfc2822(value)
        .ok()?
        .timestamp();
    Some(reset.saturating_sub(at).max(0) as u64)
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
            retry_after_at(&headers, 1_792_567_680 - 120),
            Some(120),
            "HTTP dates are anchored to fetch time"
        );
    }

    #[test]
    fn scoped_and_routines_windows_survive_broken_siblings() {
        let at = future();
        let body = serde_json::json!({
            "five_hour":{"utilization":12,"resets_at":at},
            "seven_day":{"utilization":"broken"},
            "seven_day_opus":[],
            "seven_day_cowork":{"utilization":7},
            "extra_usage":{"is_enabled":true,"used_credits":150,"monthly_limit":1000},
            "limits":[null, {"kind":"weekly_scoped","group":"weekly","percent":42,"scope":{"model":{"id":"fixture-v1","display_name":"Fixture Model"}}},
                {"kind":"weekly_scoped","group":"weekly","percent":99,"scope":{"model":{"id":"fixture-v1","display_name":"Fixture Model"}}},
                {"kind":"weekly_scoped","group":"weekly","percent":8,"scope":{"model":{"display_name":"All models"}}}]
        });
        let gauges = to_gauges(&UsageResponse(body));
        assert_eq!(
            gauges.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(),
            [
                "five_hour",
                "weekly_scoped:fixture-v1",
                "routines",
                "extra_usage"
            ]
        );
        assert_eq!(gauges[1].used_percent, 42.0);
        assert_eq!(gauges[3].used_percent, 15.0);
        assert_eq!(gauges[3].resets_at, None);
    }

    #[test]
    fn invalid_or_unlimited_extra_spend_is_not_a_zero_gauge() {
        for extra in [
            serde_json::json!({"is_enabled":false,"monthly_limit":100,"used_credits":10}),
            serde_json::json!({"is_enabled":true,"monthly_limit":null,"used_credits":10}),
            serde_json::json!({"is_enabled":true,"monthly_limit":0,"used_credits":10}),
        ] {
            assert!(to_gauges(&UsageResponse(serde_json::json!({"extra_usage":extra}))).is_empty());
        }
    }

    #[test]
    fn retry_after_handles_past_dates_and_rejects_invalid_values() {
        let mut headers = reqwest::header::HeaderMap::new();
        for invalid in ["invalid", "-30", "NaN"] {
            headers.insert("Retry-After", invalid.parse().unwrap());
            assert_eq!(retry_after_at(&headers, 1_792_567_680), None);
        }
        headers.insert(
            "Retry-After",
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after_at(&headers, 1_792_567_681), Some(0));
    }
}
