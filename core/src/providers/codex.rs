//! Codex quotas from the existing OAuth login. No login or refresh requests.
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

use super::creds::{
    agent_home, jwt_claims, jwt_expiry, non_empty, now, number, parse_reset, read_json_file,
    read_replacing_file, slug, UsageCredentials,
};
use super::{classify_transport_error, response_json, AgentReading, LimitProvider, ProviderError};
use crate::model::{Gauge, UsageDetail};

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
pub const HOST: &str = "chatgpt.com";
const USAGE_PATH: &str = "/codex/usage";
const RESET_CREDITS_PATH: &str = "/wham/rate-limit-reset-credits";
const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

#[derive(Default)]
pub struct CodexProvider;

#[derive(Deserialize)]
#[serde(transparent)]
struct UsageResponse(Value);

fn load_credentials() -> Result<UsageCredentials, ProviderError> {
    let home = agent_home("CODEX_HOME", ".codex")
        .filter(|p| p.is_dir())
        .ok_or(ProviderError::NotInstalled)?;
    let stored = read_json_file(&home.join("auth.json"))?;
    match stored.get("auth_mode").filter(|v| !v.is_null()) {
        Some(mode) if mode == "apikey" || mode == "api_key" => {
            return Err(ProviderError::ApiKeyMode)
        }
        Some(mode) if mode != "chatgpt" => return Err(ProviderError::UnsupportedAuthMode),
        None if non_empty(&stored, "OPENAI_API_KEY").is_some() => {
            return Err(ProviderError::ApiKeyMode)
        }
        _ => {}
    }
    let tokens = &stored["tokens"];
    let token = non_empty(tokens, "access_token")
        .or_else(|| non_empty(tokens, "accessToken"))
        .ok_or(ProviderError::NotSignedIn)?;
    if jwt_expiry(&token).is_some_and(|exp| exp <= now()) {
        return Err(ProviderError::SessionExpired);
    }
    let mut account = non_empty(tokens, "account_id").or_else(|| non_empty(tokens, "accountId"));
    if account.is_none() {
        for jwt in [
            non_empty(tokens, "id_token").or_else(|| non_empty(tokens, "idToken")),
            Some(token.clone()),
        ]
        .into_iter()
        .flatten()
        {
            let Some(claims) = jwt_claims(&jwt) else {
                continue;
            };
            account = non_empty(&claims, "chatgpt_account_id").or_else(|| {
                non_empty(&claims["https://api.openai.com/auth"], "chatgpt_account_id")
            });
            if account.is_some() {
                break;
            }
        }
    }
    Ok(UsageCredentials::new("codex", token, account, None))
}

fn label_for(seconds: Option<i64>, fallback: &str) -> String {
    match seconds {
        Some(s) if s > 0 && s % DAY == 0 => {
            if s / DAY == 7 {
                "Weekly".into()
            } else {
                format!("{}d window", s / DAY)
            }
        }
        Some(s) if s > 0 && s % HOUR == 0 => format!("Session ({}h)", s / HOUR),
        Some(s) if s > 0 => format!("{}m window", s.saturating_add(59) / 60),
        _ => fallback.into(),
    }
}

fn window(value: &Value, id: &str, label: &str, at: i64) -> Option<Gauge> {
    let used = number(value, "used_percent")?;
    let reset = value["reset_at"].as_i64().filter(|t| *t > 0).or_else(|| {
        value["reset_after_seconds"]
            .as_i64()
            .map(|s| at.saturating_add(s.max(0)))
    });
    Some(Gauge::new(id, label, used, reset))
}

fn to_gauges(response: &UsageResponse, at: i64) -> Vec<Gauge> {
    let body = &response.0;
    let mut gauges = Vec::new();
    for (key, id, fallback) in [
        ("primary_window", "primary", "Primary"),
        ("secondary_window", "secondary", "Secondary"),
    ] {
        let value = &body["rate_limit"][key];
        if let Some(gauge) = window(
            value,
            id,
            &label_for(value["limit_window_seconds"].as_i64(), fallback),
            at,
        ) {
            gauges.push(gauge);
        }
    }
    for entry in body["additional_rate_limits"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let Some(name) =
            non_empty(entry, "limit_name").or_else(|| non_empty(entry, "metered_feature"))
        else {
            continue;
        };
        let identity = non_empty(entry, "metered_feature").unwrap_or_else(|| name.clone());
        let identity = slug(&identity);
        if identity.is_empty() {
            continue;
        }
        for (key, lane, fallback) in [
            ("primary_window", "primary", "Primary"),
            ("secondary_window", "secondary", "Secondary"),
        ] {
            let id = format!("extra:{identity}:{lane}");
            if gauges.iter().any(|g: &Gauge| g.id == id) {
                continue;
            }
            let value = &entry["rate_limit"][key];
            let label = format!(
                "{name} · {}",
                label_for(value["limit_window_seconds"].as_i64(), fallback)
            );
            if let Some(gauge) = window(value, &id, &label, at) {
                gauges.push(gauge);
            }
        }
    }
    let budget = [
        &body["individual_limit"],
        &body["individualLimit"],
        &body["rate_limit"]["individual_limit"],
        &body["rate_limit"]["individualLimit"],
        &body["spend_control"]["individual_limit"],
        &body["spend_control"]["individualLimit"],
        &body["spendControl"]["individualLimit"],
        &body["spendControl"]["individual_limit"],
    ]
    .into_iter()
    .find(|v| v.is_object());
    if let Some(budget) = budget {
        let used = match (
            numeric(budget, "used"),
            numeric(budget, "limit").filter(|v| *v > 0.0),
        ) {
            (Some(used), Some(limit)) if used >= 0.0 => Some(used / limit * 100.0),
            _ => numeric(budget, "remaining_percent")
                .or_else(|| numeric(budget, "remainingPercent"))
                .filter(|p| (0.0..=100.0).contains(p))
                .map(|p| 100.0 - p),
        };
        if let Some(used) = used {
            let reset = parse_reset(
                budget
                    .get("reset_at")
                    .or_else(|| budget.get("resets_at"))
                    .or_else(|| budget.get("resetsAt")),
            );
            gauges.push(Gauge::new(
                "monthly_credits",
                "Monthly credits",
                used,
                reset,
            ));
        }
    }
    gauges
}

fn numeric(value: &Value, key: &str) -> Option<f64> {
    number(value, key).or_else(|| {
        value[key]
            .as_str()?
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
    })
}

fn usage_details(body: &Value) -> Vec<UsageDetail> {
    let credits = &body["credits"];
    let value = if credits["unlimited"] == true {
        Some("Unlimited".into())
    } else {
        numeric(credits, "balance")
            .filter(|n| *n >= 0.0)
            .map(|n| n.to_string())
    };
    value
        .map(|value| {
            vec![UsageDetail {
                label: "Credits".into(),
                value,
                expires_at: None,
            }]
        })
        .unwrap_or_default()
}

/// Keep only usable inventory; grant IDs, images and account metadata never reach the UI.
fn reset_details(body: &Value, at: i64) -> Option<Vec<UsageDetail>> {
    let entries = body["credits"].as_array()?;
    let mut seen = std::collections::HashSet::new();
    let mut available = 0;
    let mut expiries = std::collections::BTreeMap::<i64, usize>::new();
    for entry in entries {
        if entry["status"] != "available" || entry["reset_type"] != "codex_rate_limits" {
            continue;
        }
        let Some(id) = non_empty(entry, "id") else {
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        let reset = parse_reset(entry.get("expires_at"));
        if entry.get("expires_at").is_some_and(|v| !v.is_null()) && reset.is_none() {
            continue;
        }
        if reset.is_some_and(|t| t <= at) {
            continue;
        }
        available += 1;
        if let Some(expiry) = reset {
            *expiries.entry(expiry).or_default() += 1;
        }
    }
    let without_expiry = available - expiries.values().sum::<usize>();
    let mut details = Vec::new();
    if without_expiry > 0 || available == 0 {
        details.push(UsageDetail {
            label: "Limit resets".into(),
            value: without_expiry.to_string(),
            expires_at: None,
        });
    }
    for (expiry, count) in expiries {
        details.push(UsageDetail {
            label: "Limit resets".into(),
            value: count.to_string(),
            expires_at: Some(expiry),
        });
    }
    Some(details)
}

#[async_trait]
impl LimitProvider for CodexProvider {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn display_name(&self) -> &'static str {
        "Codex"
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
        fetch_usage(
            http,
            base,
            &credentials.access_token,
            credentials.account_id.as_deref(),
        )
        .await
    }
    async fn fetch(
        &self,
        http: &reqwest::Client,
        base: &str,
    ) -> Result<AgentReading, ProviderError> {
        self.fetch_prepared(http, base, self.prepare().await?).await
    }
}

/// Resource reads reuse the stored token. Never renew or replace it.
pub async fn fetch_usage(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    account: Option<&str>,
) -> Result<AgentReading, ProviderError> {
    if jwt_expiry(token).is_some_and(|exp| exp <= now()) {
        return Err(ProviderError::SessionExpired);
    }
    let mut request = http
        .get(format!("{}{USAGE_PATH}", base.trim_end_matches('/')))
        .bearer_auth(token)
        .header("Accept", "application/json")
        .header("originator", "codex_cli_rs");
    if let Some(account) = account {
        request = request.header("chatgpt-account-id", account);
    }
    let body = response_json(
        request
            .send()
            .await
            .map_err(|e| classify_transport_error(&e, HOST))?,
        HOST,
    )
    .await?;
    if !body.is_object() {
        return Err(ProviderError::Malformed("Expected usage object".into()));
    }
    let mut reading = AgentReading {
        gauges: to_gauges(&UsageResponse(body.clone()), now()),
        note: non_empty(&body, "plan_type"),
        details: usage_details(&body),
        ..Default::default()
    };
    let mut request = http
        .get(format!(
            "{}{RESET_CREDITS_PATH}",
            base.trim_end_matches('/')
        ))
        .bearer_auth(token)
        .header("Accept", "application/json")
        .header("OpenAI-Beta", "codex-1")
        .header("originator", "Codex Desktop")
        .timeout(Duration::from_secs(4));
    if let Some(account) = account {
        request = request.header("chatgpt-account-id", account);
    }
    let inventory = match request.send().await {
        Ok(response) => response_json(response, HOST).await,
        Err(error) => Err(classify_transport_error(&error, HOST)),
    };
    match inventory {
        Ok(inventory) => {
            if let Some(details) = reset_details(&inventory, now()) {
                reading.details.extend(details);
            }
        }
        Err(error) => {
            if let ProviderError::RateLimited { retry_after } = error {
                reading.retry_after = Some(retry_after.unwrap_or(300));
            }
            reading.details.push(UsageDetail {
                label: "Limit resets".into(),
                value: "Unavailable".into(),
                expires_at: None,
            });
        }
    }
    if reading.gauges.is_empty() && reading.details.iter().all(|d| d.value == "Unavailable") {
        return Err(ProviderError::NoLimitsReported);
    }
    Ok(reading)
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

    #[test]
    fn additional_windows_and_monthly_budget_survive_invalid_primary() {
        let body = serde_json::json!({"rate_limit": {
            "primary_window":{"used_percent":"bad"},
            "secondary_window":{"used_percent":15,"limit_window_seconds":604800}
        },"additional_rate_limits":[false, {"limit_name":"Fixture Spark","metered_feature":"fixture-spark","rate_limit":{
            "primary_window":{"used_percent":25,"limit_window_seconds":18000,"reset_at":0,"reset_after_seconds":60},
            "secondary_window":{"used_percent":35,"limit_window_seconds":604800}
        }}],"spend_control":{"individual_limit":{"used":"125","limit":"500","reset_at":12345}}});
        let gauges = to_gauges(&UsageResponse(body), 1000);
        assert_eq!(gauges.len(), 4);
        assert_eq!(gauges[0].id, "secondary");
        assert_eq!(gauges[1].id, "extra:fixture-spark:primary");
        assert_eq!(gauges[1].resets_at, Some(1060));
        assert_eq!(gauges[2].used_percent, 35.0);
        assert_eq!(gauges[3].used_percent, 25.0);
        assert_eq!(gauges[3].resets_at, Some(12345));
    }

    #[test]
    fn credit_balances_are_details_and_reset_inventory_filters_expiry_and_status() {
        let body = serde_json::json!({"credits":{"balance":"123.5"}});
        assert_eq!(usage_details(&body)[0].value, "123.5");
        assert!(to_gauges(&UsageResponse(body), 0).is_empty());
        let inventory = serde_json::json!({"available_count":99,"credits":[
            {"id":"a","status":"available","reset_type":"codex_rate_limits","expires_at":"2099-01-01T00:00:00Z"},
            {"id":"a","status":"available","reset_type":"codex_rate_limits","expires_at":"2099-01-01T00:00:00Z"},
            {"id":"b","status":"available","reset_type":"codex_rate_limits","expires_at":"2000-01-01T00:00:00Z"},
            {"id":"c","status":"used","reset_type":"codex_rate_limits"},
            {"id":"d","status":"available","reset_type":"different"},
            {"id":"e","status":"available","reset_type":"codex_rate_limits","expires_at":"invalid"},
            {"status":"available","reset_type":"codex_rate_limits"},
            {"id":"f","status":"available","reset_type":"codex_rate_limits"}
        ]});
        let details = reset_details(&inventory, now()).unwrap();
        assert_eq!(details.len(), 2);
        assert_eq!(
            details
                .iter()
                .map(|d| d.value.parse::<usize>().unwrap())
                .sum::<usize>(),
            2
        );
        assert!(details[1].expires_at.is_some());
        let serialized = serde_json::to_string(&details).unwrap();
        assert!(!serialized.contains("2099-") && !serialized.contains("reset_type"));
    }
}
