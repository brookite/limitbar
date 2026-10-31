//! Synthetic logins and local resource servers. No login or token-exchange endpoint.
#![allow(clippy::await_holding_lock)]
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use base64::Engine;
use limitbar_core::model::AgentStatus;
use limitbar_core::providers::creds::UsageCredentials;
use limitbar_core::providers::{
    claude, codex, http_client, AgentReading, LimitProvider, Monitor, ProviderError,
};
use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct Env(&'static str, Option<OsString>);
impl Env {
    fn set(key: &'static str, path: &Path) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, path);
        Self(key, previous)
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        if let Some(value) = &self.1 {
            std::env::set_var(self.0, value);
        } else {
            std::env::remove_var(self.0);
        }
    }
}

struct Installed<T>(T);
#[async_trait]
impl<T: LimitProvider> LimitProvider for Installed<T> {
    fn id(&self) -> &'static str {
        self.0.id()
    }
    fn display_name(&self) -> &'static str {
        self.0.display_name()
    }
    fn host(&self) -> &'static str {
        self.0.host()
    }
    fn default_base_url(&self) -> &'static str {
        self.0.default_base_url()
    }
    async fn prepare(&self) -> Result<Option<UsageCredentials>, ProviderError> {
        self.0.prepare().await
    }
    async fn fetch_prepared(
        &self,
        http: &reqwest::Client,
        base: &str,
        credentials: Option<UsageCredentials>,
    ) -> Result<AgentReading, ProviderError> {
        self.0.fetch_prepared(http, base, credentials).await
    }
    async fn fetch(
        &self,
        http: &reqwest::Client,
        base: &str,
    ) -> Result<AgentReading, ProviderError> {
        self.0.fetch(http, base).await
    }
}

fn codex_login(home: &Path, token: &str) {
    std::fs::write(home.join("auth.json"), json!({"auth_mode":"chatgpt","tokens":{"access_token":token,"account_id":"fixture-account"}}).to_string()).unwrap();
}

fn usage(percent: f64) -> Value {
    json!({"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":percent,"limit_window_seconds":18000,"reset_after_seconds":60}}})
}

async fn reset_endpoint(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/wham/rate-limit-reset-credits"))
        .respond_with(response)
        .mount(server)
        .await;
}

#[tokio::test]
async fn legacy_codex_jwt_identity_and_reset_resource_reuse_the_exact_login() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::set("CODEX_HOME", tmp.path());
    let claims = json!({"exp":9_999_999_999_i64,"https://api.openai.com/auth":{"chatgpt_account_id":"jwt-account"}});
    let token = format!(
        "e30.{}.signature",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    std::fs::write(
        tmp.path().join("auth.json"),
        json!({"tokens":{"accessToken":token}}).to_string(),
    )
    .unwrap();
    let before = std::fs::read(tmp.path().join("auth.json")).unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/codex/usage"))
        .and(header("authorization", format!("Bearer {token}")))
        .and(header("chatgpt-account-id", "jwt-account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(usage(42.0)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/wham/rate-limit-reset-credits"))
        .and(header("authorization", format!("Bearer {token}"))).and(header("chatgpt-account-id", "jwt-account"))
        .and(header("openai-beta", "codex-1")).and(header("originator", "Codex Desktop"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"credits":[{"id":"private-grant-id","reset_type":"codex_rate_limits","status":"available","expires_at":"2099-01-01T00:00:00Z"}]})))
        .expect(1).mount(&server).await;
    let result = codex::CodexProvider
        .fetch(&http_client(), &server.uri())
        .await
        .unwrap();
    assert_eq!(result.gauges[0].used_percent, 42.0);
    assert_eq!(result.details[0].value, "1");
    assert!(result.details[0].expires_at.is_some());
    assert!(!format!("{result:?}").contains(&token));
    assert!(!format!("{result:?}").contains("private-grant-id"));
    assert_eq!(std::fs::read(tmp.path().join("auth.json")).unwrap(), before);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|r| r.method == "GET" && r.body.is_empty()));
}

#[tokio::test]
async fn unsupported_or_expired_codex_credentials_never_start_authorization() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::set("CODEX_HOME", tmp.path());
    let server = MockServer::start().await;
    for (mode, expected_api_key) in [("apikey", true), ("future-auth", false)] {
        std::fs::write(
            tmp.path().join("auth.json"),
            json!({"auth_mode":mode,"tokens":{"access_token":"fixture"}}).to_string(),
        )
        .unwrap();
        let result = codex::CodexProvider
            .fetch(&http_client(), &server.uri())
            .await;
        if expected_api_key {
            assert!(matches!(result, Err(ProviderError::ApiKeyMode)));
        } else {
            assert!(matches!(result, Err(ProviderError::UnsupportedAuthMode)));
        }
    }
    let token = format!(
        "e30.{}.signature",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"exp":1}"#)
    );
    codex_login(tmp.path(), &token);
    assert!(matches!(
        codex::CodexProvider
            .fetch(&http_client(), &server.uri())
            .await,
        Err(ProviderError::SessionExpired)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn optional_reset_errors_preserve_usage_and_throttling_is_honored() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::set("CODEX_HOME", tmp.path());
    for status in [401, 403, 429, 500] {
        codex_login(tmp.path(), &format!("fixture-{status}"));
        let server = MockServer::start().await;
        Mock::given(path("/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(usage(25.0)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/wham/rate-limit-reset-credits"))
            .respond_with(ResponseTemplate::new(status).insert_header("Retry-After", "240"))
            .expect(1)
            .mount(&server)
            .await;
        let mut monitor = Monitor::with_client(
            http_client(),
            vec![Box::new(Installed(codex::CodexProvider))],
        );
        monitor.set_base_url("codex", server.uri());
        let result = monitor.refresh().await;
        assert_eq!(result.agents[0].status, AgentStatus::Ok);
        assert_eq!(result.agents[0].gauges[0].used_percent, 25.0);
        assert_eq!(result.agents[0].details[0].value, "Unavailable");
        if status == 429 {
            let held = monitor.refresh().await;
            assert!(matches!(held.agents[0].status, AgentStatus::Stale { .. }));
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }
    }
}

#[tokio::test]
async fn account_changes_during_a_hold_do_not_reuse_old_numbers_or_deadlines() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::set("CODEX_HOME", tmp.path());
    codex_login(tmp.path(), "account-a-token");
    let server = MockServer::start().await;
    Mock::given(path("/codex/usage"))
        .and(header("authorization", "Bearer account-a-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(usage(91.0)))
        .expect(1)
        .mount(&server)
        .await;
    reset_endpoint(
        &server,
        ResponseTemplate::new(429).insert_header("Retry-After", "300"),
    )
    .await;
    Mock::given(path("/codex/usage"))
        .and(header("authorization", "Bearer account-b-token"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let mut monitor = Monitor::with_client(
        http_client(),
        vec![Box::new(Installed(codex::CodexProvider))],
    );
    monitor.set_base_url("codex", server.uri());
    assert_eq!(
        monitor.refresh().await.agents[0].gauges[0].used_percent,
        91.0
    );
    codex_login(tmp.path(), "account-b-token");
    let changed = monitor.refresh().await;
    assert!(changed.agents[0].gauges.is_empty());
    assert_eq!(
        changed.agents[0].status,
        AgentStatus::ServiceUnavailable { http_status: 503 }
    );
    let held = monitor.refresh().await;
    assert_eq!(
        held.agents[0].status,
        AgentStatus::ServiceUnavailable { http_status: 503 }
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn responses_from_a_superseded_login_are_discarded_without_retrying_http() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::set("CODEX_HOME", tmp.path());
    codex_login(tmp.path(), "account-a-token");
    let home = tmp.path().to_path_buf();
    let server = MockServer::start().await;
    Mock::given(path("/codex/usage"))
        .respond_with(move |_: &wiremock::Request| {
            codex_login(&home, "account-b-token");
            ResponseTemplate::new(200).set_body_json(usage(91.0))
        })
        .expect(1)
        .mount(&server)
        .await;
    reset_endpoint(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"credits":[]})),
    )
    .await;
    let mut monitor = Monitor::with_client(
        http_client(),
        vec![Box::new(Installed(codex::CodexProvider))],
    );
    monitor.set_base_url("codex", server.uri());
    let result = monitor.refresh().await;
    assert_eq!(result.agents[0].status, AgentStatus::CredentialsChanged);
    assert!(result.agents[0].gauges.is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn claude_partial_usage_and_scope_diagnostics_use_no_extra_requests() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::set("CLAUDE_CONFIG_DIR", tmp.path());
    let file = tmp.path().join(".credentials.json");
    std::fs::write(&file, json!({"claudeAiOauth":{"accessToken":"fixture","expiresAt":9_999_999_999_000_i64,"scopes":["user:inference"]}}).to_string()).unwrap();
    let server = MockServer::start().await;
    assert!(matches!(
        claude::ClaudeProvider
            .fetch(&http_client(), &server.uri())
            .await,
        Err(ProviderError::MissingScope)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
    std::fs::write(&file, json!({"claudeAiOauth":{"accessToken":"fixture","scopes":["user:profile"],"subscriptionType":"max","rateLimitTier":"default_claude_max_5x"}}).to_string()).unwrap();
    Mock::given(method("GET")).and(path("/api/oauth/usage")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "five_hour":{"utilization":"bad"},"seven_day":{"utilization":30},
        "limits":[{"kind":"weekly_scoped","group":"weekly","percent":42,"scope":{"model":{"display_name":"Fixture"}}}]
    }))).expect(1).mount(&server).await;
    let reading = claude::ClaudeProvider
        .fetch(&http_client(), &server.uri())
        .await
        .unwrap();
    assert_eq!(reading.gauges.len(), 2);
    assert_eq!(reading.note.as_deref(), Some("max 5x"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn optional_resource_redirect_never_forwards_credentials_to_a_login_target() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::set("CODEX_HOME", tmp.path());
    codex_login(tmp.path(), "fixture-private-access-token");
    let source = MockServer::start().await;
    let destination = MockServer::start().await;
    Mock::given(path("/codex/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(usage(12.0)))
        .mount(&source)
        .await;
    reset_endpoint(
        &source,
        ResponseTemplate::new(302)
            .insert_header("Location", format!("{}/oauth/authorize", destination.uri())),
    )
    .await;
    let reading = codex::CodexProvider
        .fetch(&http_client(), &source.uri())
        .await
        .unwrap();
    assert_eq!(reading.gauges[0].used_percent, 12.0);
    assert_eq!(reading.details[0].value, "Unavailable");
    assert!(destination.received_requests().await.unwrap().is_empty());
}
