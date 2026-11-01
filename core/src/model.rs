//! Agent-agnostic domain model.
//!
//! Everything the UI renders is expressed here. Nothing in this module knows that
//! Anthropic or OpenAI exist; adding a third agent must not require touching it.

use serde::{Deserialize, Serialize};

/// A period of zero means manual: no timer runs and nothing is fetched until the
/// refresh button is pressed.
pub const MANUAL_INTERVAL_SECS: u64 = 0;

/// Refresh periods offered by the widget, in seconds, in menu order.
pub const INTERVAL_CHOICES: [u64; 8] = [MANUAL_INTERVAL_SECS, 15, 30, 45, 60, 90, 120, 180];
pub const DEFAULT_INTERVAL_SECS: u64 = 45;

/// The fastest *automatic* period the menu offers. Manual is not a faster period,
/// it is the absence of one, so it sits outside this floor.
pub const MIN_POLL_INTERVAL_SECS: u64 = 15;

/// Severity thresholds, in percent used.
const WARNING_AT: f64 = 75.0;
const CRITICAL_AT: f64 = 90.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// Unix epoch seconds at which this snapshot was assembled.
    pub fetched_at: i64,
    pub agents: Vec<AgentSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSnapshot {
    /// Stable provider id, e.g. `claude` / `codex`.
    pub id: String,
    pub display_name: String,
    pub status: AgentStatus,
    pub gauges: Vec<Gauge>,
    /// Free-form detail for the section header, e.g. the subscription plan.
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<UsageDetail>,
}

impl AgentSnapshot {
    pub fn failed(id: &str, display_name: &str, status: AgentStatus) -> Self {
        Self {
            id: id.to_string(),
            display_name: display_name.to_string(),
            status,
            gauges: Vec::new(),
            note: None,
            details: Vec::new(),
        }
    }
}

/// Why a section looks the way it does. The Rust side never formats prose: it
/// reports a structured reason plus the values the UI needs to fill in a sentence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AgentStatus {
    /// Fresh numbers.
    Ok,
    /// The last fetch failed, so these gauges are the previous good ones.
    Stale {
        last_ok_at: i64,
        /// The failure that made them stale.
        because: Box<AgentStatus>,
    },
    /// No config directory for this agent at all.
    NotInstalled,
    /// Agent is installed but there is no credentials file.
    NotSignedIn,
    /// Stored access token's expiry has passed.
    SessionExpired,
    /// The API rejected the token (401/403).
    Unauthorized,
    /// The API asked us to slow down (429).
    RateLimited {
        retry_at: i64,
    },
    ServiceUnavailable {
        http_status: u16,
    },
    Offline {
        host: String,
    },
    Timeout {
        after_secs: u64,
    },
    /// HTTP 200 but the body was not shaped the way we expect.
    UnexpectedResponse,
    /// Codex configured with an API key rather than a ChatGPT subscription.
    ApiKeyMode,
    /// The call succeeded but reported no active limit windows.
    NoLimitsReported,
    NotRunning,
    SecureStorage,
    MissingScope,
    UnsupportedAuthMode,
    CredentialsChanged,
}

/// Non-percentage metadata; expiry is distinct from a recurring quota reset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageDetail {
    pub label: String,
    pub value: String,
    pub expires_at: Option<i64>,
}

impl AgentStatus {
    /// Whether previously fetched gauges should be kept on screen (dimmed)
    /// instead of being cleared.
    pub fn keeps_previous_gauges(&self) -> bool {
        matches!(
            self,
            AgentStatus::RateLimited { .. }
                | AgentStatus::ServiceUnavailable { .. }
                | AgentStatus::Offline { .. }
                | AgentStatus::Timeout { .. }
                | AgentStatus::UnexpectedResponse
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Gauge {
    /// Stable within an agent, e.g. `five_hour` / `primary`.
    pub id: String,
    pub label: String,
    /// 0..=100.
    pub used_percent: f64,
    /// Unix epoch seconds when this window resets, when the API tells us.
    pub resets_at: Option<i64>,
    pub severity: Severity,
}

impl Gauge {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        used_percent: f64,
        resets_at: Option<i64>,
    ) -> Self {
        let used_percent = clamp_percent(used_percent);
        Self {
            id: id.into(),
            label: label.into(),
            used_percent,
            resets_at,
            severity: Severity::for_percent(used_percent),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    Normal,
    Warning,
    Critical,
}

impl Severity {
    pub fn for_percent(used_percent: f64) -> Self {
        if used_percent >= CRITICAL_AT {
            Severity::Critical
        } else if used_percent >= WARNING_AT {
            Severity::Warning
        } else {
            Severity::Normal
        }
    }
}

/// Both agents report their usage on a 0..100 scale: Claude as `utilization`,
/// Codex as `used_percent`. Neither is a fraction, which is worth stating because
/// it is easy to assume otherwise: confirmed against a live response, where
/// `five_hour.utilization` and the parallel `limits[0].percent` both read 17.
pub fn clamp_percent(percent: f64) -> f64 {
    if !percent.is_finite() {
        return 0.0;
    }
    percent.clamp(0.0, 100.0)
}

/// Whether this period means "only when asked".
pub fn is_manual(interval_secs: u64) -> bool {
    interval_secs == MANUAL_INTERVAL_SECS
}

/// Normalises a user-chosen refresh period, refusing anything off the menu — which
/// is also what stops a hand-edited settings file from polling faster than the floor.
pub fn sanitize_interval(secs: u64) -> u64 {
    if INTERVAL_CHOICES.contains(&secs) {
        secs
    } else {
        DEFAULT_INTERVAL_SECS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_follows_thresholds() {
        assert_eq!(Severity::for_percent(0.0), Severity::Normal);
        assert_eq!(Severity::for_percent(74.9), Severity::Normal);
        assert_eq!(Severity::for_percent(75.0), Severity::Warning);
        assert_eq!(Severity::for_percent(89.9), Severity::Warning);
        assert_eq!(Severity::for_percent(90.0), Severity::Critical);
        assert_eq!(Severity::for_percent(100.0), Severity::Critical);
    }

    #[test]
    fn percentages_are_clamped_and_nan_safe() {
        assert_eq!(clamp_percent(-5.0), 0.0);
        assert_eq!(clamp_percent(140.0), 100.0);
        assert_eq!(clamp_percent(f64::NAN), 0.0);
        assert_eq!(clamp_percent(17.0), 17.0);
    }

    #[test]
    fn gauge_derives_its_own_severity() {
        let g = Gauge::new("five_hour", "Session (5h)", 93.2, Some(1_800_000_000));
        assert_eq!(g.severity, Severity::Critical);
        assert_eq!(g.used_percent, 93.2);
    }

    #[test]
    fn interval_falls_back_to_default_when_off_menu() {
        assert_eq!(sanitize_interval(15), 15);
        assert_eq!(sanitize_interval(120), 120);
        assert_eq!(sanitize_interval(1), DEFAULT_INTERVAL_SECS);
        assert_eq!(sanitize_interval(3600), DEFAULT_INTERVAL_SECS);
    }

    #[test]
    fn manual_is_a_real_choice_and_survives_sanitising() {
        assert_eq!(
            sanitize_interval(MANUAL_INTERVAL_SECS),
            MANUAL_INTERVAL_SECS
        );
        assert!(is_manual(MANUAL_INTERVAL_SECS));
        assert!(!is_manual(15));
        assert!(!is_manual(DEFAULT_INTERVAL_SECS));
    }

    #[test]
    fn every_automatic_choice_respects_the_polling_floor() {
        for secs in INTERVAL_CHOICES.into_iter().filter(|s| !is_manual(*s)) {
            assert!(secs >= MIN_POLL_INTERVAL_SECS, "{secs}s is below the floor");
        }
    }

    #[test]
    fn transient_failures_keep_previous_numbers_on_screen() {
        assert!(AgentStatus::Offline { host: "x".into() }.keeps_previous_gauges());
        assert!(AgentStatus::RateLimited { retry_at: 0 }.keeps_previous_gauges());
        assert!(!AgentStatus::NotSignedIn.keeps_previous_gauges());
        assert!(!AgentStatus::SessionExpired.keeps_previous_gauges());
    }

    #[test]
    fn status_fields_serialize_as_camel_case_for_the_ui() {
        let json = serde_json::to_value(AgentStatus::Stale {
            last_ok_at: 100,
            because: Box::new(AgentStatus::RateLimited { retry_at: 200 }),
        })
        .unwrap();
        assert_eq!(json["lastOkAt"], 100);
        assert_eq!(json["because"]["retryAt"], 200);
    }
}
