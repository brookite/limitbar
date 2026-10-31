//! Credentials are synthetic; every network destination is a local mock server.
#![allow(clippy::await_holding_lock)]
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use base64::Engine;
use limitbar_core::providers::{
    cursor, gemini, http_client, opencode, LimitProvider, ProviderError, USER_AGENT,
};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct Env(Vec<(&'static str, Option<OsString>)>);
impl Env {
    fn home(root: &Path) -> Self {
        let mut env = Self(Vec::new());
        for key in [
            "HOME",
            "USERPROFILE",
            "APPDATA",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
        ] {
            env.set(key, Some(root.as_os_str()));
        }
        for key in [
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_PROJECT_ID",
            "OPENCODE_DB",
        ] {
            env.set(key, None);
        }
        env
    }
    fn set(&mut self, key: &'static str, value: Option<&std::ffi::OsStr>) {
        self.0.push((key, std::env::var_os(key)));
        if let Some(value) = value {
            std::env::set_var(key, value);
        } else {
            std::env::remove_var(key);
        }
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..).rev() {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

fn cursor_login(root: &Path, exp: i64) -> String {
    let dir = if cfg!(windows) {
        root.join("Cursor")
    } else if cfg!(target_os = "macos") {
        root.join(".cursor")
    } else {
        root.join("cursor")
    };
    std::fs::create_dir_all(&dir).unwrap();
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(json!({"sub":"auth0|fixture-user","exp":exp}).to_string());
    let token = format!("e30.{payload}.signature");
    std::fs::write(
        dir.join("auth.json"),
        json!({"accessToken":token}).to_string(),
    )
    .unwrap();
    format!("WorkosCursorSessionToken=fixture-user%3A%3A{token}")
}

#[tokio::test]
async fn cursor_reads_shared_grok_and_unified_billing_with_origin_and_cookie() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::home(tmp.path());
    let cookie = cursor_login(tmp.path(), 9_999_999_999);
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/api/usage-summary")).and(header("cookie", cookie.clone()))
        .and(header("user-agent", USER_AGENT)).respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "membershipType":"unified_pro","limitType":"user","individualUsage":{"plan":{"totalPercentUsed":0}}
        }))).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/api/dashboard/get-sand-usage-status"))
        .and(header("origin", server.uri())).and(header("cookie", cookie.clone())).and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"includedLimitZero":false,"isUnified":true,"usagePercent":100,"hasAvailableUsage":false,"nextResetTimestampUtc":"2099-01-01T00:00:00Z"})))
        .expect(1).mount(&server).await;
    Mock::given(method("GET")).and(path("/api/dashboard/v2/usage-status")).and(query_param("scope", "user"))
        .and(header("cookie", cookie)).respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "availability":"available","monthlyUsage":{"usedCents":93,"limitCents":1000,"periodStart":"2020-01-01T00:00:00Z","resetsAt":"2099-01-01T00:00:00Z"}
        }))).expect(1).mount(&server).await;
    let reading = cursor::CursorProvider
        .fetch(&http_client(), &server.uri())
        .await
        .unwrap();
    assert_eq!(reading.gauges.len(), 2);
    assert_eq!(reading.gauges[0].label, "Cursor + Grok (weekly)");
    assert_eq!(reading.gauges[0].used_percent, 100.0);
    assert!((reading.gauges[1].used_percent - 9.3).abs() < 0.001);
}

#[tokio::test]
async fn cursor_grok_failure_keeps_plan_but_signals_unavailable_grok() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::home(tmp.path());
    cursor_login(tmp.path(), 9_999_999_999);
    let server = MockServer::start().await;
    Mock::given(path("/api/usage-summary"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"individualUsage":{"plan":{"used":20,"limit":100}}})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/api/dashboard/get-sand-usage-status"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let reading = cursor::CursorProvider
        .fetch(&http_client(), &server.uri())
        .await
        .unwrap();
    assert_eq!(reading.gauges[0].used_percent, 20.0);
    assert!(reading.note.unwrap().contains("Grok: unavailable"));
}

#[tokio::test]
async fn expired_cursor_never_contacts_the_server() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::home(tmp.path());
    cursor_login(tmp.path(), 1);
    let server = MockServer::start().await;
    assert!(matches!(
        cursor::CursorProvider
            .fetch(&http_client(), &server.uri())
            .await,
        Err(ProviderError::SessionExpired)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

fn gemini_login(root: &Path, exp: i64) {
    let dir = root.join(".gemini");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("oauth_creds.json"),
        json!({"access_token":"gemini-fixture","expiry_date":exp}).to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn gemini_discovers_project_and_uses_it_for_quota_without_refreshing() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::home(tmp.path());
    gemini_login(tmp.path(), 9_999_999_999_000);
    let before = std::fs::read(tmp.path().join(".gemini/oauth_creds.json")).unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1internal:loadCodeAssist")).and(header("authorization", "Bearer gemini-fixture"))
        .and(header("user-agent", USER_AGENT)).and(body_json(json!({"metadata":{"ideType":"GEMINI_CLI","pluginType":"GEMINI"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"cloudaicompanionProject":{"id":"fixture-project"},"currentTier":{"name":"Pro"}})))
        .expect(1).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/v1internal:retrieveUserQuota"))
        .and(body_json(json!({"project":"fixture-project"})))
        .and(header("authorization", "Bearer gemini-fixture"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"buckets":[{"modelId":"gemini-pro","remainingFraction":0.25}]}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let reading = gemini::GeminiProvider
        .fetch(&http_client(), &server.uri())
        .await
        .unwrap();
    assert_eq!(reading.note.as_deref(), Some("Pro"));
    assert_eq!(reading.gauges[0].used_percent, 75.0);
    assert_eq!(
        std::fs::read(tmp.path().join(".gemini/oauth_creds.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn gemini_expired_or_api_key_login_sends_no_requests() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::home(tmp.path());
    gemini_login(tmp.path(), 1);
    let server = MockServer::start().await;
    assert!(matches!(
        gemini::GeminiProvider
            .fetch(&http_client(), &server.uri())
            .await,
        Err(ProviderError::SessionExpired)
    ));
    std::fs::write(
        tmp.path().join(".gemini/settings.json"),
        r#"{"security":{"auth":{"selectedType":"gemini-api-key"}}}"#,
    )
    .unwrap();
    assert!(matches!(
        gemini::GeminiProvider
            .fetch(&http_client(), &server.uri())
            .await,
        Err(ProviderError::ApiKeyMode)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

fn opencode_login(root: &Path, value: serde_json::Value) {
    std::fs::create_dir_all(root.join("opencode")).unwrap();
    std::fs::write(root.join("opencode/auth.json"), value.to_string()).unwrap();
}

#[tokio::test]
async fn opencode_go_reads_legacy_key_and_respects_retry_after() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::home(tmp.path());
    opencode_login(
        tmp.path(),
        json!({"opencode-go":{"type":"api","key":"go-fixture"}}),
    );
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/zen/go/v1/usage"))
        .and(header("authorization", "Bearer go-fixture"))
        .and(header("user-agent", USER_AGENT))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "180"))
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        opencode::OpenCodeProvider
            .fetch(&http_client(), &server.uri())
            .await,
        Err(ProviderError::RateLimited {
            retry_after: Some(180)
        })
    ));
}

#[tokio::test]
async fn opencode_codex_oauth_queries_only_openai_and_marks_shared_quota() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::home(tmp.path());
    opencode_login(
        tmp.path(),
        json!({"openai":{"type":"oauth","access":"openai-fixture","accountId":"account-fixture","expires":9_999_999_999_000_i64}}),
    );
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/codex/usage")).and(header("authorization", "Bearer openai-fixture"))
        .and(header("chatgpt-account-id", "account-fixture")).and(header("originator", "codex_cli_rs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"rate_limit":{"primary_window":{"used_percent":42,"limit_window_seconds":18000}}})))
        .expect(1).mount(&server).await;
    let reading = opencode::OpenCodeProvider
        .fetch(&http_client(), &server.uri())
        .await
        .unwrap();
    assert_eq!(reading.gauges[0].used_percent, 42.0);
    assert_eq!(reading.note.as_deref(), Some("Codex · shared limit"));
}

#[tokio::test]
async fn cursor_cookie_is_never_forwarded_on_redirect() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::home(tmp.path());
    cursor_login(tmp.path(), 9_999_999_999);
    let source = MockServer::start().await;
    let destination = MockServer::start().await;
    Mock::given(path("/api/usage-summary"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", destination.uri()))
        .mount(&source)
        .await;
    Mock::given(path("/api/dashboard/get-sand-usage-status"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&source)
        .await;
    assert!(cursor::CursorProvider
        .fetch(&http_client(), &source.uri())
        .await
        .is_err());
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn grok_throttling_slows_the_whole_provider_instead_of_hammering_optional_endpoint() {
    let _lock = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = Env::home(tmp.path());
    cursor_login(tmp.path(), 9_999_999_999);
    let server = MockServer::start().await;
    Mock::given(path("/api/usage-summary"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"individualUsage":{"plan":{"totalPercentUsed":30}}})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/api/dashboard/get-sand-usage-status"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "240"))
        .mount(&server)
        .await;
    assert!(matches!(
        cursor::CursorProvider
            .fetch(&http_client(), &server.uri())
            .await,
        Err(ProviderError::RateLimited {
            retry_after: Some(240)
        })
    ));
}
