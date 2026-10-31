//! Queries only the loopback language server already running inside Antigravity.
//! Does not launch the agent, generate prompts, or read its account database.
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::process::Command;

use super::creds::parse_reset;
use super::{
    classify_transport_error, discovery, response_json, AgentReading, LimitProvider, ProviderError,
    USER_AGENT,
};
use crate::model::Gauge;

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1";
const HOST: &str = "127.0.0.1";
const SERVICE: &str = "/exa.language_server_pb.LanguageServerService/";
pub struct AntigravityProvider;

#[derive(Deserialize)]
struct Process {
    command: String,
    ports: Vec<u16>,
}

fn flag(command: &str, name: &str) -> Option<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    for c in command.chars() {
        if quote == Some(c) {
            quote = None;
        } else if quote.is_none() && (c == '\'' || c == '"') {
            quote = Some(c);
        } else if quote.is_none() && c.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    for (index, word) in words.iter().enumerate() {
        if word == name {
            return words
                .get(index + 1)
                .cloned()
                .filter(|s| !s.starts_with("--"));
        }
        if let Some(value) = word.strip_prefix(&format!("{name}=")) {
            return Some(value.to_string());
        }
    }
    None
}

fn csrf(command: &str) -> Option<String> {
    let lower = command.to_lowercase().replace('\\', "/");
    let binary = if let Some(quoted) = lower.strip_prefix('"') {
        quoted.split('"').next().unwrap_or_default()
    } else {
        lower.split_whitespace().next().unwrap_or_default()
    };
    let cli = binary.ends_with("/agy")
        || binary.ends_with("/agy.exe")
        || binary.contains("/antigravity-cli/")
        || binary.ends_with("/antigravity-cli.exe")
        || binary.ends_with("/antigravity-cli");
    let ide = binary.contains("language_server") || binary.contains("language-server");
    let owned = lower.contains("/antigravity/")
        || lower.contains("/antigravity.app/")
        || lower.contains("/antigravity ide.app/")
        || (lower.contains("--app_data_dir") && lower.contains("antigravity"));
    if !(cli || ide && owned) {
        return None;
    }
    flag(command, "--csrf_token").or_else(|| cli.then(String::new))
}

async fn output(binary: &str, args: &[&str]) -> Result<String, ProviderError> {
    let mut command = Command::new(binary);
    command.args(args).kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let result = tokio::time::timeout(Duration::from_secs(4), command.output())
        .await
        .map_err(|_| ProviderError::Timeout)?
        .map_err(|_| ProviderError::NotRunning)?;
    if !result.status.success() {
        return Err(ProviderError::NotRunning);
    }
    String::from_utf8(result.stdout)
        .map_err(|_| ProviderError::Malformed("Cannot read process list".into()))
}

#[cfg(windows)]
async fn processes() -> Result<Vec<Process>, ProviderError> {
    // Static script; no command line or credential is ever interpolated into shell code.
    let script = r#"$ErrorActionPreference='Stop'; $p=@(Get-CimInstance Win32_Process -Filter "Name LIKE 'language%server%' OR Name='agy.exe' OR Name='antigravity-cli.exe'" | Where-Object { $_.CommandLine -match 'antigravity|(?:^|[\\/])agy(?:\.exe)?(?:\s|$)' }); if($p.Count -eq 0){'[]';exit}; $tcp=@(Get-NetTCPConnection -State Listen); $result=@($p | ForEach-Object { $processId=$_.ProcessId; @{command=$_.CommandLine;ports=@($tcp | Where-Object {$_.OwningProcess -eq $processId} | Select-Object -ExpandProperty LocalPort -Unique)} }); ConvertTo-Json -InputObject $result -Compress -Depth 4"#;
    let raw = output(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", script],
    )
    .await?;
    serde_json::from_str(raw.trim_start_matches('\u{feff}').trim())
        .map_err(|_| ProviderError::Malformed("Cannot read Antigravity processes".into()))
}

#[cfg(not(windows))]
async fn processes() -> Result<Vec<Process>, ProviderError> {
    let raw = output("ps", &["-ax", "-o", "pid=,command="]).await?;
    let mut processes = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        let Some((pid, command)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        let command = command.trim();
        if csrf(command).is_none() {
            continue;
        }
        let mut ports = proc_ports(pid);
        if ports.is_empty() {
            if let Ok(raw) = output(
                "lsof",
                &[
                    "-nP",
                    "-a",
                    "-p",
                    &pid.to_string(),
                    "-iTCP",
                    "-sTCP:LISTEN",
                    "-Fn",
                ],
            )
            .await
            {
                ports = raw
                    .lines()
                    .filter_map(|s| s.strip_prefix('n')?.rsplit(':').next()?.parse::<u16>().ok())
                    .collect();
            }
        }
        processes.push(Process {
            command: command.into(),
            ports,
        });
    }
    Ok(processes)
}

#[cfg(not(windows))]
fn proc_ports(pid: u32) -> Vec<u16> {
    // /proc socket ownership avoids requiring lsof on minimal Linux installations.
    let mut inodes = std::collections::HashSet::new();
    if let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        for fd in fds.flatten() {
            if let Ok(link) = std::fs::read_link(fd.path()) {
                let value = link.to_string_lossy();
                if let Some(inode) = value
                    .strip_prefix("socket:[")
                    .and_then(|s| s.strip_suffix(']'))
                {
                    inodes.insert(inode.to_owned());
                }
            }
        }
    }
    let mut ports = Vec::new();
    for table in ["tcp", "tcp6"] {
        if let Ok(raw) = std::fs::read_to_string(format!("/proc/{pid}/net/{table}")) {
            for line in raw.lines().skip(1) {
                let columns: Vec<_> = line.split_whitespace().collect();
                if columns.len() > 9 && columns[3] == "0A" && inodes.contains(columns[9]) {
                    if let Some(port) = columns[1]
                        .rsplit(':')
                        .next()
                        .and_then(|p| u16::from_str_radix(p, 16).ok())
                    {
                        ports.push(port);
                    }
                }
            }
        }
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

pub fn parse_quota(summary: &Value, status: &Value) -> AgentReading {
    let root = summary
        .get("response")
        .or_else(|| summary.get("summary"))
        .unwrap_or(summary);
    let mut gauges = Vec::new();
    for (group_index, group) in root["groups"].as_array().into_iter().flatten().enumerate() {
        for bucket in group["buckets"].as_array().into_iter().flatten() {
            if bucket["disabled"] == true {
                continue;
            }
            let Some(remaining) = bucket["remainingFraction"].as_f64().or_else(|| {
                bucket
                    .pointer("/remaining/remainingFraction")
                    .and_then(Value::as_f64)
            }) else {
                continue;
            };
            if !(0.0..=1.0).contains(&remaining) {
                continue;
            }
            let Some(id) = bucket["bucketId"]
                .as_str()
                .or_else(|| bucket["id"].as_str())
            else {
                continue;
            };
            let group_name = group["displayName"]
                .as_str()
                .or_else(|| group["name"].as_str())
                .unwrap_or("Quota");
            let name = bucket["displayName"]
                .as_str()
                .or_else(|| bucket["name"].as_str())
                .or_else(|| bucket["window"].as_str())
                .unwrap_or(id);
            gauges.push(Gauge::new(
                format!("{group_index}:{id}"),
                format!("{group_name} · {name}"),
                (1.0 - remaining) * 100.0,
                parse_reset(bucket.get("resetTime")),
            ));
        }
    }
    if gauges.is_empty() {
        let models = status
            .pointer("/userStatus/cascadeModelConfigData/clientModelConfigs")
            .or_else(|| status.get("clientModelConfigs"));
        for model in models.and_then(Value::as_array).into_iter().flatten() {
            let quota = &model["quotaInfo"];
            let (Some(id), Some(remaining)) = (
                model.pointer("/modelOrAlias/model").and_then(Value::as_str),
                quota["remainingFraction"].as_f64(),
            ) else {
                continue;
            };
            if !(0.0..=1.0).contains(&remaining) {
                continue;
            }
            if gauges.iter().any(|g| g.id == id) {
                continue;
            }
            gauges.push(Gauge::new(
                id,
                model["label"].as_str().unwrap_or(id),
                (1.0 - remaining) * 100.0,
                parse_reset(quota.get("resetTime")),
            ));
        }
    }
    let note = status
        .pointer("/userStatus/planStatus/planInfo/planDisplayName")
        .or_else(|| status.pointer("/userStatus/planStatus/planInfo/planName"))
        .or_else(|| status.pointer("/userStatus/userTier/name"))
        .and_then(Value::as_str)
        .map(String::from);
    AgentReading {
        gauges,
        note,
        ..Default::default()
    }
}

async fn rpc(
    http: &reqwest::Client,
    base: &str,
    csrf: &str,
    method: &str,
) -> Result<Value, ProviderError> {
    let body = if method == "RetrieveUserQuotaSummary" {
        json!({"forceRefresh":true})
    } else {
        json!({"metadata":{"ideName":"antigravity","extensionName":"antigravity","ideVersion":"unknown","locale":"en"}})
    };
    let response = http
        .post(format!("{base}{SERVICE}{method}"))
        .header("X-Codeium-Csrf-Token", csrf)
        .header("Connect-Protocol-Version", "1")
        .json(&body)
        .send()
        .await
        .map_err(|e| classify_transport_error(&e, HOST))?;
    let body = response_json(response, HOST).await?;
    if body.get("code").is_some_and(|code| {
        !matches!(code.as_str(), Some("0" | "OK" | "ok" | "success")) && code.as_i64() != Some(0)
    }) {
        return Err(ProviderError::Malformed("Antigravity RPC failed".into()));
    }
    Ok(body)
}

async fn fetch_local(
    http: &reqwest::Client,
    base: &str,
    csrf: &str,
) -> Result<AgentReading, ProviderError> {
    let (summary, status) = tokio::join!(
        rpc(http, base, csrf, "RetrieveUserQuotaSummary"),
        rpc(http, base, csrf, "GetUserStatus")
    );
    let mut reading = parse_quota(
        summary.as_ref().unwrap_or(&Value::Null),
        status.as_ref().unwrap_or(&Value::Null),
    );
    if reading.gauges.is_empty() {
        if let (Err(_), Err(_)) = (&summary, &status) {
            // A real HTTP failure must survive; it is not evidence that the app stopped.
            return Err(status.err().unwrap_or(ProviderError::NotRunning));
        }
        let configs = rpc(http, base, csrf, "GetCommandModelConfigs").await?;
        let fallback = parse_quota(&Value::Null, &configs);
        reading.gauges = fallback.gauges;
    }
    if reading.gauges.is_empty() {
        return Err(ProviderError::NoLimitsReported);
    }
    Ok(reading)
}

#[async_trait]
impl LimitProvider for AntigravityProvider {
    fn id(&self) -> &'static str {
        "antigravity"
    }
    fn display_name(&self) -> &'static str {
        "Antigravity"
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
        _http: &reqwest::Client,
        _base: &str,
    ) -> Result<AgentReading, ProviderError> {
        let processes = processes().await?;
        // This client is confined to literal loopback URLs assembled below. Self-signed
        // TLS is used by the local language server; never reuse this client for vendors.
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|_| ProviderError::NotRunning)?;
        let mut calls = Vec::new();
        for process in &processes {
            let Some(token) = csrf(&process.command) else {
                continue;
            };
            for &port in process.ports.iter().filter(|p| **p != 0).take(8) {
                for scheme in ["http", "https"] {
                    let http = http.clone();
                    let token = token.clone();
                    calls.push(async move {
                        fetch_local(&http, &format!("{scheme}://127.0.0.1:{port}"), &token).await
                    });
                }
            }
        }
        if calls.is_empty() {
            return Err(ProviderError::NotRunning);
        }
        let results =
            tokio::time::timeout(Duration::from_secs(7), futures::future::join_all(calls))
                .await
                .map_err(|_| ProviderError::Timeout)?;
        // Prefer richer quota summaries when several IDE windows are open.
        let mut best = None;
        let mut failure = ProviderError::NotRunning;
        for result in results {
            match result {
                Ok(reading)
                    if best
                        .as_ref()
                        .is_none_or(|b: &AgentReading| reading.gauges.len() > b.gauges.len()) =>
                {
                    best = Some(reading)
                }
                Ok(_) => {}
                Err(error)
                    if !matches!(
                        error,
                        ProviderError::Offline { .. } | ProviderError::Timeout
                    ) =>
                {
                    failure = error
                }
                Err(_) => {}
            }
        }
        best.ok_or(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scopes_processes_and_requires_ide_csrf() {
        assert_eq!(csrf(r#""C:\Apps\Antigravity\language_server_windows_x64.exe" --csrf_token="fixture-token""#).as_deref(), Some("fixture-token"));
        assert!(csrf("/opt/windsurf/language_server --csrf_token secret").is_none());
        assert!(csrf("/opt/antigravity/language_server").is_none());
        assert_eq!(csrf("/usr/bin/agy --port=5000").as_deref(), Some(""));
        assert_eq!(
            flag("--csrf_token=fixture", "--csrf_token").as_deref(),
            Some("fixture")
        );
        assert_eq!(csrf(r#""C:\Program Files\Antigravity\language_server_windows_x64.exe" --csrf_token fixture"#).as_deref(), Some("fixture"));
    }
    #[test]
    fn parses_summary_without_turning_missing_quotas_into_full_allowances() {
        let summary = json!({"response":{"groups":[{"displayName":"Gemini","buckets":[
            {"bucketId":"weekly","remainingFraction":0,"resetTime":"2099-01-01T00:00:00Z"},
            {"bucketId":"5h","remainingFraction":0.75},
            {"bucketId":"disabled","remainingFraction":0,"disabled":true},
            {"bucketId":"unknown"}
        ]}]}});
        let reading = parse_quota(&summary, &Value::Null);
        assert_eq!(reading.gauges.len(), 2);
        assert_eq!(reading.gauges[0].used_percent, 100.0);
        assert_eq!(reading.gauges[1].used_percent, 25.0);
    }

    #[tokio::test]
    async fn local_rpc_sends_csrf_and_prefers_summary_over_models() {
        use wiremock::matchers::{body_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path(format!("{SERVICE}RetrieveUserQuotaSummary")))
            .and(header("x-codeium-csrf-token", "fixture"))
            .and(header("connect-protocol-version", "1")).and(body_json(json!({"forceRefresh":true})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"groups":[{"name":"Gemini","buckets":[{"id":"5h","remainingFraction":0.25}]}]})))
            .expect(1).mount(&server).await;
        Mock::given(method("POST")).and(path(format!("{SERVICE}GetUserStatus")))
            .and(header("x-codeium-csrf-token", "fixture")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "userStatus":{"userTier":{"name":"Pro"},"cascadeModelConfigData":{"clientModelConfigs":[{"label":"old","modelOrAlias":{"model":"old"},"quotaInfo":{"remainingFraction":1}}]}}
            }))).expect(1).mount(&server).await;
        let r = fetch_local(&super::super::http_client(), &server.uri(), "fixture")
            .await
            .unwrap();
        assert_eq!(r.gauges.len(), 1);
        assert_eq!(r.gauges[0].used_percent, 75.0);
        assert_eq!(r.note.as_deref(), Some("Pro"));
    }
}
