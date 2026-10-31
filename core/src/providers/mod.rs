//! Per-agent support lives behind [`LimitProvider`]. Adding an agent means adding a
//! module here and one line in [`default_providers`]; no widget code changes.

pub mod antigravity;
pub mod claude;
pub mod codex;
mod cooldowns;
pub mod creds;
pub mod cursor;
pub mod discovery;
pub mod gemini;
pub mod opencode;
mod sqlite;

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;

use crate::model::{AgentSnapshot, AgentStatus, Gauge, Snapshot, UsageDetail};
use creds::UsageCredentials;

/// limitbar identifies itself honestly. Every *functional* header is copied from the
/// agent CLIs verbatim (they are what authorises and routes the request), but the
/// User-Agent is ours: the bearer token is the gate here, not this string, and
/// impersonating an official CLI would be a disguise rather than compatibility.
pub const USER_AGENT: &str = concat!("limitbar/", env!("CARGO_PKG_VERSION"));

/// Per-request ceiling. The CLIs use 5s; we allow a little more headroom because a
/// slow answer is still better than an empty bar.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// Backoff ladder applied after a 429 or a server/network failure, in seconds.
const BACKOFF_LADDER: [u64; 4] = [60, 120, 300, 900];

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("agent is not installed")]
    NotInstalled,
    #[error("no credentials on disk")]
    NotSignedIn,
    #[error("stored token has expired")]
    SessionExpired,
    #[error("credentials rejected by the API")]
    Unauthorized,
    #[error("rate limited by the API")]
    RateLimited { retry_after: Option<u64> },
    #[error("service returned HTTP {0}")]
    ServiceUnavailable(u16),
    #[error("cannot reach {host}")]
    Offline { host: String },
    #[error("request timed out")]
    Timeout,
    #[error("unexpected response: {0}")]
    Malformed(String),
    #[error("configured with an API key rather than a subscription")]
    ApiKeyMode,
    #[error("no limit windows reported")]
    NoLimitsReported,
    #[error("start the agent to read its local usage API")]
    NotRunning,
    #[error("stored credentials require the agent's secure storage")]
    SecureStorage,
    #[error("token lacks user:profile usage scope")]
    MissingScope,
    #[error("unsupported stored authentication mode")]
    UnsupportedAuthMode,
    #[error("agent credentials changed during the request")]
    CredentialsChanged,
}

impl ProviderError {
    /// Whether this failure should slow our polling down.
    fn should_back_off(&self) -> bool {
        matches!(
            self,
            ProviderError::RateLimited { .. }
                | ProviderError::ServiceUnavailable(_)
                | ProviderError::Offline { .. }
                | ProviderError::Timeout
                // A body we cannot read is a reason to stop hammering the endpoint,
                // and the previous numbers are still the best thing to show.
                | ProviderError::Malformed(_)
        )
    }

    fn into_status(self, retry_at: i64) -> AgentStatus {
        match self {
            ProviderError::NotInstalled => AgentStatus::NotInstalled,
            ProviderError::NotSignedIn => AgentStatus::NotSignedIn,
            ProviderError::SessionExpired => AgentStatus::SessionExpired,
            ProviderError::Unauthorized => AgentStatus::Unauthorized,
            ProviderError::RateLimited { .. } => AgentStatus::RateLimited { retry_at },
            ProviderError::ServiceUnavailable(code) => {
                AgentStatus::ServiceUnavailable { http_status: code }
            }
            ProviderError::Offline { host } => AgentStatus::Offline { host },
            ProviderError::Timeout => AgentStatus::Timeout {
                after_secs: REQUEST_TIMEOUT.as_secs(),
            },
            ProviderError::Malformed(_) => AgentStatus::UnexpectedResponse,
            ProviderError::ApiKeyMode => AgentStatus::ApiKeyMode,
            ProviderError::NoLimitsReported => AgentStatus::NoLimitsReported,
            ProviderError::NotRunning => AgentStatus::NotRunning,
            ProviderError::SecureStorage => AgentStatus::SecureStorage,
            ProviderError::MissingScope => AgentStatus::MissingScope,
            ProviderError::UnsupportedAuthMode => AgentStatus::UnsupportedAuthMode,
            ProviderError::CredentialsChanged => AgentStatus::CredentialsChanged,
        }
    }
}

/// Translates a `reqwest` failure into the closest human-meaningful reason.
pub fn classify_transport_error(err: &reqwest::Error, host: &str) -> ProviderError {
    if err.is_timeout() {
        ProviderError::Timeout
    } else if err.is_connect() || err.is_request() {
        ProviderError::Offline {
            host: host.to_string(),
        }
    } else {
        ProviderError::Malformed(err.to_string())
    }
}

/// What a provider produced on a successful call.
#[derive(Debug, Default, PartialEq)]
pub struct AgentReading {
    pub gauges: Vec<Gauge>,
    /// Plan or tier, shown next to the section header when the API reports one.
    pub note: Option<String>,
    pub details: Vec<UsageDetail>,
    /// A best-effort resource returned 429; keep this reading but honor its hold.
    pub retry_after: Option<u64>,
}

#[async_trait]
pub trait LimitProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn is_installed(&self) -> bool {
        true
    }
    /// Local-only credential capture before cache/hold selection.
    async fn prepare(&self) -> Result<Option<UsageCredentials>, ProviderError> {
        Ok(None)
    }
    async fn fetch_prepared(
        &self,
        http: &reqwest::Client,
        base: &str,
        _credentials: Option<UsageCredentials>,
    ) -> Result<AgentReading, ProviderError> {
        self.fetch(http, base).await
    }
    /// Host used in "cannot reach …" messages.
    fn host(&self) -> &'static str;
    /// Reads credentials and calls the agent's usage endpoint.
    ///
    /// `base_url` is injected so tests can point a provider at a local server.
    async fn fetch(
        &self,
        http: &reqwest::Client,
        base_url: &str,
    ) -> Result<AgentReading, ProviderError>;
    /// Default endpoint root, used when the app runs for real.
    fn default_base_url(&self) -> &'static str;
}

pub fn default_providers() -> Vec<Box<dyn LimitProvider>> {
    vec![
        Box::new(claude::ClaudeProvider),
        Box::new(codex::CodexProvider),
        Box::new(cursor::CursorProvider),
        Box::new(gemini::GeminiProvider),
        Box::new(antigravity::AntigravityProvider),
        Box::new(opencode::OpenCodeProvider),
    ]
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("static reqwest configuration is valid")
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Backoff {
    consecutive_failures: u32,
}

impl Backoff {
    /// Seconds to wait before the next attempt, given how many times we have failed.
    fn penalty(&self) -> u64 {
        if self.consecutive_failures == 0 {
            return 0;
        }
        let index = (self.consecutive_failures as usize - 1).min(BACKOFF_LADDER.len() - 1);
        BACKOFF_LADDER[index]
    }

    fn record_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
    }

    fn reset(&mut self) {
        self.consecutive_failures = 0;
    }
}

#[derive(Debug, Default)]
struct AgentState {
    backoff: Backoff,
    /// Epoch seconds before which we must not send another request.
    hold_until: i64,
    last_ok: Option<LastGood>,
    context: Option<String>,
    hold_reason: Option<AgentStatus>,
}

#[derive(Debug, Clone)]
struct LastGood {
    at: i64,
    gauges: Vec<Gauge>,
    note: Option<String>,
    details: Vec<UsageDetail>,
}

/// Owns the providers, their backoff state and the last good numbers, so that a
/// failed refresh dims the previous reading instead of blanking the window.
pub struct Monitor {
    http: reqwest::Client,
    providers: Vec<Box<dyn LimitProvider>>,
    base_urls: HashMap<String, String>,
    state: HashMap<String, AgentState>,
    cooldowns: cooldowns::Cooldowns,
}

impl Monitor {
    pub fn new(providers: Vec<Box<dyn LimitProvider>>) -> Self {
        let mut monitor = Self::with_client(http_client(), providers);
        let path = crate::settings::settings_path().map(|p| p.with_file_name("cooldowns.json"));
        monitor.cooldowns = cooldowns::Cooldowns::load(path);
        monitor
    }

    pub fn with_client(http: reqwest::Client, providers: Vec<Box<dyn LimitProvider>>) -> Self {
        Self {
            http,
            providers,
            base_urls: HashMap::new(),
            state: HashMap::new(),
            cooldowns: cooldowns::Cooldowns::default(),
        }
    }

    /// Points one provider at a different endpoint root (used by tests).
    pub fn set_base_url(&mut self, provider_id: &str, base_url: impl Into<String>) {
        self.base_urls
            .insert(provider_id.to_string(), base_url.into());
    }

    fn base_url_for(&self, provider: &dyn LimitProvider) -> String {
        self.base_urls
            .get(provider.id())
            .cloned()
            .unwrap_or_else(|| provider.default_base_url().to_string())
    }

    /// Queries agents concurrently; failures remain isolated per provider.
    pub async fn refresh(&mut self) -> Snapshot {
        let now = creds::now();
        let http = self.http.clone();

        let calls = self.providers.iter().map(|provider| {
            let base_url = self.base_url_for(provider.as_ref());
            let previous = self.state.get(provider.id());
            let cooldowns = &self.cooldowns;
            let http = http.clone();
            async move {
                if !provider.is_installed() {
                    return (
                        provider.id(),
                        provider.display_name(),
                        None,
                        None,
                        Some(Err(ProviderError::NotInstalled)),
                    );
                }
                let prepared = match provider.prepare().await {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        return (
                            provider.id(),
                            provider.display_name(),
                            None,
                            None,
                            Some(Err(if matches!(error, ProviderError::NotInstalled) {
                                ProviderError::NotSignedIn
                            } else {
                                error
                            })),
                        )
                    }
                };
                let context = prepared.as_ref().map(|c| c.context.clone());
                let saved_hold = context.as_ref().and_then(|key| cooldowns.get(key, now));
                let held = previous.is_some_and(|s| s.context == context && s.hold_until > now)
                    || saved_hold.is_some();
                let outcome = if held {
                    None
                } else {
                    Some(
                        provider
                            .fetch_prepared(&http, &base_url, prepared)
                            .await
                            .map_err(|error| {
                                // Installation is already proven by the executable; a missing
                                // legacy config directory means the installed app has no login.
                                if matches!(error, ProviderError::NotInstalled) {
                                    ProviderError::NotSignedIn
                                } else {
                                    error
                                }
                            }),
                    )
                };
                if !held && context.is_some() {
                    match provider.prepare().await {
                        Ok(Some(current)) if Some(&current.context) != context.as_ref() => {
                            return (
                                provider.id(),
                                provider.display_name(),
                                Some(current.context),
                                None,
                                Some(Err(ProviderError::CredentialsChanged)),
                            );
                        }
                        Err(error) => {
                            return (
                                provider.id(),
                                provider.display_name(),
                                None,
                                None,
                                Some(Err(if matches!(error, ProviderError::NotInstalled) {
                                    ProviderError::NotSignedIn
                                } else {
                                    error
                                })),
                            )
                        }
                        _ => {}
                    }
                }
                (
                    provider.id(),
                    provider.display_name(),
                    context,
                    saved_hold,
                    outcome,
                )
            }
        });

        let results = futures::future::join_all(calls).await;
        let collected_at = creds::now();

        let mut agents = Vec::with_capacity(results.len());
        for (id, display_name, context, saved_hold, outcome) in results {
            if matches!(outcome, Some(Err(ProviderError::NotInstalled))) {
                self.state.remove(id);
                continue;
            }
            let state = self.state.entry(id.to_string()).or_default();
            if state.context != context {
                *state = AgentState {
                    context: context.clone(),
                    ..Default::default()
                };
            }
            if let Some(until) = saved_hold.filter(|t| *t > state.hold_until) {
                state.hold_until = until;
                state.hold_reason = Some(AgentStatus::RateLimited { retry_at: until });
            }
            let rate_limited = matches!(outcome, Some(Err(ProviderError::RateLimited { .. })))
                || matches!(&outcome, Some(Ok(reading)) if reading.retry_after.is_some());
            let clear_hold = matches!(&outcome, Some(Ok(reading)) if reading.retry_after.is_none());
            agents.push(reconcile(id, display_name, outcome, state, collected_at));
            if let Some(context) = &context {
                if rate_limited {
                    self.cooldowns
                        .record(context, Some(state.hold_until), collected_at);
                } else if clear_hold {
                    self.cooldowns.record(context, None, collected_at);
                }
            }
        }

        Snapshot {
            fetched_at: collected_at,
            agents,
        }
    }
}

/// Common response handling; response bodies and credentials never enter errors.
pub async fn response_json(
    response: reqwest::Response,
    host: &str,
) -> Result<serde_json::Value, ProviderError> {
    let status = response.status();
    match status.as_u16() {
        401 | 403 => return Err(ProviderError::Unauthorized),
        429 => {
            return Err(ProviderError::RateLimited {
                retry_after: claude::retry_after_secs(response.headers()),
            })
        }
        _ if !status.is_success() => {
            return Err(ProviderError::ServiceUnavailable(status.as_u16()))
        }
        _ => {}
    }
    response
        .json()
        .await
        .map_err(|e| classify_transport_error(&e, host))
}

/// Folds one provider's outcome into its stored state and the section the UI draws.
fn reconcile(
    id: &str,
    display_name: &str,
    outcome: Option<Result<AgentReading, ProviderError>>,
    state: &mut AgentState,
    now: i64,
) -> AgentSnapshot {
    let error = match outcome {
        // Skipped because we are holding off; re-report the reason we are waiting.
        None => {
            return stale_or_bare(
                id,
                display_name,
                state
                    .hold_reason
                    .clone()
                    .unwrap_or(AgentStatus::RateLimited {
                        retry_at: state.hold_until,
                    }),
                state,
                true,
            )
        }
        Some(Ok(reading)) => {
            state.backoff.reset();
            state.hold_until = 0;
            state.hold_reason = None;
            if let Some(seconds) = reading.retry_after {
                state.hold_until =
                    now.saturating_add(i64::try_from(seconds.max(15)).unwrap_or(i64::MAX));
                state.hold_reason = Some(AgentStatus::RateLimited {
                    retry_at: state.hold_until,
                });
            }
            state.last_ok = Some(LastGood {
                at: now,
                gauges: reading.gauges.clone(),
                note: reading.note.clone(),
                details: reading.details.clone(),
            });
            return AgentSnapshot {
                id: id.to_string(),
                display_name: display_name.to_string(),
                status: AgentStatus::Ok,
                gauges: reading.gauges,
                note: reading.note,
                details: reading.details,
            };
        }
        Some(Err(error)) => error,
    };

    let keep_previous = error.should_back_off();
    if keep_previous {
        state.backoff.record_failure();
        // A 429 that names its own cooling-off period wins over our ladder.
        let wait = match &error {
            ProviderError::RateLimited {
                retry_after: Some(secs),
            } => *secs,
            ProviderError::RateLimited { retry_after: None } if id == "claude" => {
                state.backoff.penalty().max(300)
            }
            _ => state.backoff.penalty(),
        };
        state.hold_until = now.saturating_add(i64::try_from(wait).unwrap_or(i64::MAX));
    } else {
        state.backoff.reset();
        state.hold_until = 0;
        state.hold_reason = None;
        // A configuration problem invalidates whatever we showed before.
        state.last_ok = None;
    }

    let status = error.into_status(state.hold_until);
    if keep_previous {
        state.hold_reason = Some(status.clone());
    }
    stale_or_bare(id, display_name, status, state, keep_previous)
}

/// Dresses a failure with the last good numbers when they are still worth showing.
fn stale_or_bare(
    id: &str,
    display_name: &str,
    status: AgentStatus,
    state: &AgentState,
    keep_previous: bool,
) -> AgentSnapshot {
    match (keep_previous, &state.last_ok) {
        (true, Some(last)) => AgentSnapshot {
            id: id.to_string(),
            display_name: display_name.to_string(),
            status: AgentStatus::Stale {
                last_ok_at: last.at,
                because: Box::new(status),
            },
            gauges: last.gauges.clone(),
            note: last.note.clone(),
            details: last.details.clone(),
        },
        _ => AgentSnapshot::failed(id, display_name, status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading() -> AgentReading {
        AgentReading {
            gauges: vec![Gauge::new(
                "five_hour",
                "Session (5h)",
                40.0,
                Some(9_999_999_999),
            )],
            note: Some("pro".into()),
            ..Default::default()
        }
    }

    #[test]
    fn backoff_climbs_the_ladder_then_holds_at_the_cap() {
        let mut b = Backoff::default();
        assert_eq!(b.penalty(), 0);
        let observed: Vec<u64> = (0..6)
            .map(|_| {
                b.record_failure();
                b.penalty()
            })
            .collect();
        assert_eq!(observed, vec![60, 120, 300, 900, 900, 900]);
    }

    #[test]
    fn a_success_clears_the_backoff() {
        let mut b = Backoff::default();
        b.record_failure();
        b.record_failure();
        assert_eq!(b.penalty(), 120);
        b.reset();
        assert_eq!(b.penalty(), 0);
    }

    #[test]
    fn success_is_remembered_as_the_last_good_reading() {
        let mut state = AgentState::default();
        let snap = reconcile(
            "claude",
            "Claude Code",
            Some(Ok(reading())),
            &mut state,
            1_000,
        );
        assert_eq!(snap.status, AgentStatus::Ok);
        assert_eq!(snap.gauges.len(), 1);
        assert_eq!(state.last_ok.as_ref().unwrap().at, 1_000);
        assert_eq!(state.hold_until, 0);
    }

    #[test]
    fn a_transient_failure_dims_the_previous_numbers_instead_of_clearing_them() {
        let mut state = AgentState::default();
        reconcile(
            "claude",
            "Claude Code",
            Some(Ok(reading())),
            &mut state,
            1_000,
        );

        let snap = reconcile(
            "claude",
            "Claude Code",
            Some(Err(ProviderError::Offline {
                host: "api.anthropic.com".into(),
            })),
            &mut state,
            1_100,
        );

        match snap.status {
            AgentStatus::Stale {
                last_ok_at,
                because,
            } => {
                assert_eq!(last_ok_at, 1_000);
                assert_eq!(
                    *because,
                    AgentStatus::Offline {
                        host: "api.anthropic.com".into()
                    }
                );
            }
            other => panic!("expected Stale, got {other:?}"),
        }
        assert_eq!(snap.gauges.len(), 1, "previous gauges stay on screen");
        assert_eq!(state.hold_until, 1_100 + 60);
    }

    #[test]
    fn a_configuration_failure_clears_the_previous_numbers() {
        let mut state = AgentState::default();
        reconcile("codex", "Codex", Some(Ok(reading())), &mut state, 1_000);

        let snap = reconcile(
            "codex",
            "Codex",
            Some(Err(ProviderError::SessionExpired)),
            &mut state,
            1_100,
        );

        assert_eq!(snap.status, AgentStatus::SessionExpired);
        assert!(snap.gauges.is_empty());
        assert!(state.last_ok.is_none());
        assert_eq!(
            state.hold_until, 0,
            "a stale login is not a reason to poll less"
        );
    }

    #[test]
    fn retry_after_overrides_our_own_ladder() {
        let mut state = AgentState::default();
        let snap = reconcile(
            "codex",
            "Codex",
            Some(Err(ProviderError::RateLimited {
                retry_after: Some(240),
            })),
            &mut state,
            5_000,
        );
        assert_eq!(snap.status, AgentStatus::RateLimited { retry_at: 5_240 });
        assert_eq!(state.hold_until, 5_240);
    }

    #[test]
    fn a_429_without_retry_after_uses_the_first_ladder_step() {
        let mut state = AgentState::default();
        let snap = reconcile(
            "codex",
            "Codex",
            Some(Err(ProviderError::RateLimited { retry_after: None })),
            &mut state,
            5_000,
        );
        assert_eq!(snap.status, AgentStatus::RateLimited { retry_at: 5_060 });
    }

    #[test]
    fn held_failures_keep_their_actual_reason_and_claude_defaults_to_five_minutes() {
        let mut state = AgentState::default();
        reconcile("codex", "Codex", Some(Ok(reading())), &mut state, 1000);
        let offline = AgentStatus::Offline {
            host: "chatgpt.com".into(),
        };
        reconcile(
            "codex",
            "Codex",
            Some(Err(ProviderError::Offline {
                host: "chatgpt.com".into(),
            })),
            &mut state,
            1001,
        );
        let held = reconcile("codex", "Codex", None, &mut state, 1002);
        assert_eq!(
            held.status,
            AgentStatus::Stale {
                last_ok_at: 1000,
                because: Box::new(offline)
            }
        );
        let mut claude = AgentState::default();
        reconcile(
            "claude",
            "Claude",
            Some(Err(ProviderError::RateLimited { retry_after: None })),
            &mut claude,
            1000,
        );
        assert_eq!(claude.hold_until, 1300);
    }

    #[test]
    fn a_skipped_poll_reports_why_it_is_waiting() {
        let mut state = AgentState::default();
        reconcile("codex", "Codex", Some(Ok(reading())), &mut state, 1_000);
        state.hold_until = 9_000;

        let snap = reconcile("codex", "Codex", None, &mut state, 1_100);
        match snap.status {
            AgentStatus::Stale { because, .. } => {
                assert_eq!(*because, AgentStatus::RateLimited { retry_at: 9_000 });
            }
            other => panic!("expected Stale, got {other:?}"),
        }
        assert_eq!(snap.gauges.len(), 1);
    }

    #[test]
    fn malformed_bodies_surface_as_an_unexpected_response() {
        let mut state = AgentState::default();
        let snap = reconcile(
            "claude",
            "Claude Code",
            Some(Err(ProviderError::Malformed("missing field".into()))),
            &mut state,
            1_000,
        );
        assert_eq!(snap.status, AgentStatus::UnexpectedResponse);
        assert_eq!(
            state.hold_until, 1_060,
            "a broken endpoint is not polled at full speed"
        );
    }

    #[tokio::test]
    async fn uninstall_removes_a_section_even_during_backoff_and_reinstall_retries() {
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        };
        struct Switchable {
            installed: Arc<AtomicBool>,
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl LimitProvider for Switchable {
            fn id(&self) -> &'static str {
                "test"
            }
            fn display_name(&self) -> &'static str {
                "Test"
            }
            fn host(&self) -> &'static str {
                "localhost"
            }
            fn default_base_url(&self) -> &'static str {
                "http://localhost"
            }
            fn is_installed(&self) -> bool {
                self.installed.load(Ordering::SeqCst)
            }
            async fn fetch(
                &self,
                _: &reqwest::Client,
                _: &str,
            ) -> Result<AgentReading, ProviderError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::RateLimited {
                    retry_after: Some(300),
                })
            }
        }
        let installed = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Switchable {
            installed: installed.clone(),
            calls: calls.clone(),
        };
        let mut monitor = Monitor::new(vec![Box::new(provider)]);
        assert!(monitor.refresh().await.agents.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        installed.store(true, Ordering::SeqCst);
        assert_eq!(monitor.refresh().await.agents.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        installed.store(false, Ordering::SeqCst);
        assert!(monitor.refresh().await.agents.is_empty());
        installed.store(true, Ordering::SeqCst);
        assert_eq!(monitor.refresh().await.agents.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn restart_restores_only_the_same_credential_hold_without_saving_tokens() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        struct Limited {
            token: &'static str,
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl LimitProvider for Limited {
            fn id(&self) -> &'static str {
                "codex"
            }
            fn display_name(&self) -> &'static str {
                "Codex"
            }
            fn host(&self) -> &'static str {
                "localhost"
            }
            fn default_base_url(&self) -> &'static str {
                "http://localhost"
            }
            async fn prepare(&self) -> Result<Option<UsageCredentials>, ProviderError> {
                Ok(Some(UsageCredentials::new(
                    "codex",
                    self.token.into(),
                    None,
                    None,
                )))
            }
            async fn fetch(
                &self,
                _: &reqwest::Client,
                _: &str,
            ) -> Result<AgentReading, ProviderError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::RateLimited {
                    retry_after: Some(300),
                })
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cooldowns.json");
        let calls = Arc::new(AtomicUsize::new(0));
        let build = |token| {
            let mut monitor = Monitor::with_client(
                http_client(),
                vec![Box::new(Limited {
                    token,
                    calls: calls.clone(),
                })],
            );
            monitor.cooldowns = cooldowns::Cooldowns::load(Some(path.clone()));
            monitor
        };
        let first = build("private-fixture-token").refresh().await;
        assert!(matches!(
            first.agents[0].status,
            AgentStatus::RateLimited { .. }
        ));
        let restored = build("private-fixture-token").refresh().await;
        assert_eq!(restored.agents[0].status, first.agents[0].status);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        build("different-private-token").refresh().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(!stored.contains("private") && !stored.contains("token"));
    }
}
