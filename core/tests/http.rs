//! End-to-end checks of what each provider puts on the wire and how it reads the
//! answer back. The request shape matters as much as the parsing: these assertions
//! are what keep us sending exactly what the real CLIs send.

// Credential discovery reads process-wide environment variables, so each test takes
// the lock for its whole body, awaits included. That is the point of the lock: two
// tests must not have conflicting `CODEX_HOME` values in flight at once.
#![allow(clippy::await_holding_lock)]

use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use async_trait::async_trait;
use base64::Engine;
use limitbar_core::model::AgentStatus;
use limitbar_core::providers::{
    claude, codex, AgentReading, LimitProvider, Monitor, ProviderError,
};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Monitor tests inject installation evidence; they must never query real agents
// installed on the developer's machine.
struct Installed<T>(T, bool);
#[async_trait]
impl<T: LimitProvider> LimitProvider for Installed<T> {
    fn id(&self) -> &'static str {
        self.0.id()
    }
    fn display_name(&self) -> &'static str {
        self.0.display_name()
    }
    fn is_installed(&self) -> bool {
        self.1
    }
    fn host(&self) -> &'static str {
        self.0.host()
    }
    fn default_base_url(&self) -> &'static str {
        self.0.default_base_url()
    }
    async fn fetch(
        &self,
        http: &reqwest::Client,
        base: &str,
    ) -> Result<AgentReading, ProviderError> {
        self.0.fetch(http, base).await
    }
}

fn test_providers() -> Vec<Box<dyn LimitProvider>> {
    vec![
        Box::new(Installed(claude::ClaudeProvider, true)),
        Box::new(Installed(codex::CodexProvider, false)),
    ]
}

/// Credential discovery goes through environment variables, which are per-process,
/// so these tests take turns rather than running concurrently.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_claude_home(dir: &Path, expires_at_ms: i64) {
    std::fs::write(
        dir.join(".credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-test","refreshToken":"sk-ant-refresh",
                 "expiresAt":{expires_at_ms},"subscriptionType":"pro"}}}}"#
        ),
    )
    .unwrap();
}

fn jwt_expiring_at(exp: i64) -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.signature",
        b64.encode(r#"{"alg":"none"}"#),
        b64.encode(format!(r#"{{"exp":{exp}}}"#))
    )
}

fn write_codex_home(dir: &Path, auth_mode: &str, exp: i64) {
    std::fs::write(
        dir.join("auth.json"),
        format!(
            r#"{{"auth_mode":"{auth_mode}","OPENAI_API_KEY":null,
                 "tokens":{{"id_token":"{token}","access_token":"{token}",
                            "refresh_token":"rt.test","account_id":"acct-123"}},
                 "last_refresh":"2026-09-16T20:27:00Z"}}"#,
            token = jwt_expiring_at(exp)
        ),
    )
    .unwrap();
}

fn in_one_hour() -> i64 {
    now() + 3_600
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Shaped after a real `/api/oauth/usage` reply: percentages, and RFC 3339 resets.
fn claude_body() -> String {
    let at = chrono::DateTime::<chrono::Utc>::from(
        std::time::SystemTime::now() + std::time::Duration::from_secs(3_600),
    )
    .to_rfc3339();
    format!(
        r#"{{"five_hour":{{"utilization":17.0,"resets_at":"{at}","limit_dollars":null}},
             "seven_day":{{"utilization":31.0,"resets_at":"{at}","limit_dollars":null}},
             "seven_day_opus":null,"amber_gauge":null,
             "limits":[{{"kind":"session","percent":17,"resets_at":"{at}"}}]}}"#
    )
}

/// Shaped after a real `/codex/usage` reply.
const CODEX_BODY: &str = r#"{"plan_type":"plus","rate_limit":{"allowed":true,"limit_reached":false,
    "primary_window":{"used_percent":37.5,"limit_window_seconds":18000,"reset_after_seconds":4200,"reset_at":1790027057},
    "secondary_window":{"used_percent":8.0,"limit_window_seconds":604800,"reset_after_seconds":300000,"reset_at":1790514238}}}"#;

// --- Claude -----------------------------------------------------------------

#[tokio::test]
async fn claude_sends_exactly_the_headers_the_cli_sends() {
    let _guard = env_lock();
    let home = tempfile::tempdir().unwrap();
    write_claude_home(home.path(), (in_one_hour()) * 1000);
    std::env::set_var("CLAUDE_CONFIG_DIR", home.path());

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oauth/usage"))
        .and(header("authorization", "Bearer sk-ant-test"))
        .and(header("content-type", "application/json"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("anthropic-beta", "oauth-2025-04-20"))
        .and(header("anthropic-client-platform", "claude_code_cli"))
        .and(header("user-agent", limitbar_core::providers::USER_AGENT))
        .respond_with(ResponseTemplate::new(200).set_body_string(claude_body()))
        .expect(1)
        .mount(&server)
        .await;

    let reading = claude::ClaudeProvider
        .fetch(&limitbar_core::providers::http_client(), &server.uri())
        .await
        .expect("fetch succeeds");

    assert_eq!(reading.note.as_deref(), Some("pro"));
    assert_eq!(reading.gauges.len(), 2);
    assert_eq!(reading.gauges[0].used_percent, 17.0);
    std::env::remove_var("CLAUDE_CONFIG_DIR");
}

#[tokio::test]
async fn claude_reports_an_expired_session_without_spending_a_request() {
    let _guard = env_lock();
    let home = tempfile::tempdir().unwrap();
    write_claude_home(home.path(), (now() - 60) * 1000);
    std::env::set_var("CLAUDE_CONFIG_DIR", home.path());

    let server = MockServer::start().await;
    // No mock is mounted: reaching the network here would be the bug.
    let err = claude::ClaudeProvider
        .fetch(&limitbar_core::providers::http_client(), &server.uri())
        .await
        .unwrap_err();

    assert!(matches!(err, ProviderError::SessionExpired));
    assert!(server.received_requests().await.unwrap().is_empty());
    std::env::remove_var("CLAUDE_CONFIG_DIR");
}

#[tokio::test]
async fn claude_maps_http_failures_onto_reasons_a_person_can_act_on() {
    /// HTTP status, optional `Retry-After`, and the reason it must produce.
    type Case = (u16, Option<&'static str>, fn(&ProviderError) -> bool);

    let cases: Vec<Case> = vec![
        (401, None, |e| matches!(e, ProviderError::Unauthorized)),
        (403, None, |e| matches!(e, ProviderError::Unauthorized)),
        (429, Some("120"), |e| {
            matches!(
                e,
                ProviderError::RateLimited {
                    retry_after: Some(120)
                }
            )
        }),
        (429, None, |e| {
            matches!(e, ProviderError::RateLimited { retry_after: None })
        }),
        (503, None, |e| {
            matches!(e, ProviderError::ServiceUnavailable(503))
        }),
        (500, None, |e| {
            matches!(e, ProviderError::ServiceUnavailable(500))
        }),
    ];

    for (status, retry_after, expected) in cases {
        let _guard = env_lock();
        let home = tempfile::tempdir().unwrap();
        write_claude_home(home.path(), in_one_hour() * 1000);
        std::env::set_var("CLAUDE_CONFIG_DIR", home.path());

        let server = MockServer::start().await;
        let mut response = ResponseTemplate::new(status);
        if let Some(value) = retry_after {
            response = response.insert_header("retry-after", value);
        }
        Mock::given(method("GET"))
            .respond_with(response)
            .mount(&server)
            .await;

        let err = claude::ClaudeProvider
            .fetch(&limitbar_core::providers::http_client(), &server.uri())
            .await
            .unwrap_err();
        assert!(expected(&err), "HTTP {status} produced {err:?}");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
    }
}

#[tokio::test]
async fn claude_flags_a_body_it_cannot_understand() {
    let _guard = env_lock();
    let home = tempfile::tempdir().unwrap();
    write_claude_home(home.path(), in_one_hour() * 1000);
    std::env::set_var("CLAUDE_CONFIG_DIR", home.path());

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>nope</html>"))
        .mount(&server)
        .await;

    let err = claude::ClaudeProvider
        .fetch(&limitbar_core::providers::http_client(), &server.uri())
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Malformed(_)));
    std::env::remove_var("CLAUDE_CONFIG_DIR");
}

#[tokio::test]
async fn claude_says_so_when_the_plan_reports_no_windows() {
    let _guard = env_lock();
    let home = tempfile::tempdir().unwrap();
    write_claude_home(home.path(), in_one_hour() * 1000);
    std::env::set_var("CLAUDE_CONFIG_DIR", home.path());

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let err = claude::ClaudeProvider
        .fetch(&limitbar_core::providers::http_client(), &server.uri())
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::NoLimitsReported));
    std::env::remove_var("CLAUDE_CONFIG_DIR");
}

// --- Codex ------------------------------------------------------------------

#[tokio::test]
async fn codex_sends_exactly_the_headers_the_cli_sends() {
    let _guard = env_lock();
    let home = tempfile::tempdir().unwrap();
    write_codex_home(home.path(), "chatgpt", in_one_hour());
    std::env::set_var("CODEX_HOME", home.path());

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/codex/usage"))
        .and(header("chatgpt-account-id", "acct-123"))
        .and(header("originator", "codex_cli_rs"))
        .and(header("accept", "application/json"))
        .and(header("user-agent", limitbar_core::providers::USER_AGENT))
        .respond_with(ResponseTemplate::new(200).set_body_string(CODEX_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let reading = codex::CodexProvider
        .fetch(&limitbar_core::providers::http_client(), &server.uri())
        .await
        .expect("fetch succeeds");

    assert_eq!(reading.note.as_deref(), Some("plus"));
    assert_eq!(reading.gauges.len(), 2);
    assert_eq!(reading.gauges[0].used_percent, 37.5);
    std::env::remove_var("CODEX_HOME");
}

#[tokio::test]
async fn codex_explains_an_api_key_setup_instead_of_drawing_nothing() {
    let _guard = env_lock();
    let home = tempfile::tempdir().unwrap();
    write_codex_home(home.path(), "apikey", in_one_hour());
    std::env::set_var("CODEX_HOME", home.path());

    let server = MockServer::start().await;
    let err = codex::CodexProvider
        .fetch(&limitbar_core::providers::http_client(), &server.uri())
        .await
        .unwrap_err();

    assert!(matches!(err, ProviderError::ApiKeyMode));
    assert!(server.received_requests().await.unwrap().is_empty());
    std::env::remove_var("CODEX_HOME");
}

#[tokio::test]
async fn codex_reads_expiry_out_of_its_own_token() {
    let _guard = env_lock();
    let home = tempfile::tempdir().unwrap();
    write_codex_home(home.path(), "chatgpt", now() - 1);
    std::env::set_var("CODEX_HOME", home.path());

    let server = MockServer::start().await;
    let err = codex::CodexProvider
        .fetch(&limitbar_core::providers::http_client(), &server.uri())
        .await
        .unwrap_err();

    assert!(matches!(err, ProviderError::SessionExpired));
    assert!(server.received_requests().await.unwrap().is_empty());
    std::env::remove_var("CODEX_HOME");
}

#[tokio::test]
async fn a_missing_codex_home_is_reported_as_not_installed() {
    let _guard = env_lock();
    std::env::set_var("CODEX_HOME", "/nonexistent/codex/home");

    let err = codex::CodexProvider
        .fetch(
            &limitbar_core::providers::http_client(),
            "http://127.0.0.1:1",
        )
        .await
        .unwrap_err();

    assert!(matches!(err, ProviderError::NotInstalled));
    std::env::remove_var("CODEX_HOME");
}

// --- The monitor as a whole --------------------------------------------------

#[tokio::test]
async fn one_broken_agent_does_not_take_the_other_down_with_it() {
    let _guard = env_lock();
    let claude_home = tempfile::tempdir().unwrap();
    write_claude_home(claude_home.path(), in_one_hour() * 1000);
    std::env::set_var("CLAUDE_CONFIG_DIR", claude_home.path());
    // Codex is not installed at all.
    std::env::set_var("CODEX_HOME", "/nonexistent/codex/home");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oauth/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_string(claude_body()))
        .mount(&server)
        .await;

    let mut monitor = Monitor::new(test_providers());
    monitor.set_base_url("claude", server.uri());
    monitor.set_base_url("codex", server.uri());

    let snapshot = monitor.refresh().await;
    let claude_agent = snapshot.agents.iter().find(|a| a.id == "claude").unwrap();

    assert_eq!(claude_agent.status, AgentStatus::Ok);
    assert_eq!(claude_agent.gauges.len(), 2);
    assert!(snapshot.agents.iter().all(|a| a.id != "codex"));

    std::env::remove_var("CLAUDE_CONFIG_DIR");
    std::env::remove_var("CODEX_HOME");
}

#[tokio::test]
async fn a_throttled_agent_is_not_polled_again_until_its_hold_expires() {
    let _guard = env_lock();
    let claude_home = tempfile::tempdir().unwrap();
    write_claude_home(claude_home.path(), in_one_hour() * 1000);
    std::env::set_var("CLAUDE_CONFIG_DIR", claude_home.path());
    std::env::set_var("CODEX_HOME", "/nonexistent/codex/home");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oauth/usage"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "300"))
        .expect(1) // the second refresh must not reach the server
        .mount(&server)
        .await;

    let mut monitor = Monitor::new(test_providers());
    monitor.set_base_url("claude", server.uri());

    let first = monitor.refresh().await;
    let claude_agent = first.agents.iter().find(|a| a.id == "claude").unwrap();
    assert!(matches!(
        claude_agent.status,
        AgentStatus::RateLimited { .. }
    ));

    let second = monitor.refresh().await;
    let claude_agent = second.agents.iter().find(|a| a.id == "claude").unwrap();
    assert!(matches!(
        claude_agent.status,
        AgentStatus::RateLimited { .. }
    ));

    std::env::remove_var("CLAUDE_CONFIG_DIR");
    std::env::remove_var("CODEX_HOME");
}

#[tokio::test]
async fn an_unreachable_host_is_reported_as_being_offline() {
    let _guard = env_lock();
    let claude_home = tempfile::tempdir().unwrap();
    write_claude_home(claude_home.path(), in_one_hour() * 1000);
    std::env::set_var("CLAUDE_CONFIG_DIR", claude_home.path());

    // Port 1 refuses connections immediately.
    let err = claude::ClaudeProvider
        .fetch(
            &limitbar_core::providers::http_client(),
            "http://127.0.0.1:1",
        )
        .await
        .unwrap_err();

    match err {
        ProviderError::Offline { host } => assert_eq!(host, claude::HOST),
        other => panic!("expected Offline, got {other:?}"),
    }
    std::env::remove_var("CLAUDE_CONFIG_DIR");
}
