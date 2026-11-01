//! Claude usage from the existing on-disk login (CLI or Claude Desktop). No token refresh or auth writes.
use std::num::NonZeroU32;
use std::path::PathBuf;

use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;

use super::creds::{
    agent_home, first_non_empty_env, is_window_live, non_empty, now, number, parse_reset,
    read_json_file, read_replacing_file, slug, UsageCredentials,
};
use super::{
    classify_transport_error, discovery, response_json, AgentReading, LimitProvider, ProviderError,
};
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

fn load_cli_credentials() -> Result<UsageCredentials, ProviderError> {
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

fn desktop_config_path() -> Option<PathBuf> {
    if let Some(override_dir) = first_non_empty_env(&["CLAUDE_CONFIG_DIR"]) {
        return Some(override_dir.join("config.json"));
    }
    discovery::config_dir().map(|dir| dir.join("Claude/config.json"))
}

#[cfg(target_os = "linux")]
fn linux_safe_storage_secret() -> Result<Vec<u8>, ProviderError> {
    use dbus::arg::{RefArg, Variant};
    use std::collections::HashMap;
    use std::time::Duration;

    let conn =
        dbus::blocking::Connection::new_session().map_err(|_| ProviderError::SecureStorage)?;
    let proxy = conn.with_proxy(
        "org.freedesktop.secrets",
        "/org/freedesktop/secrets",
        Duration::from_secs(2),
    );
    let (_, session): (Variant<Box<dyn RefArg>>, dbus::Path<'static>) = proxy
        .method_call(
            "org.freedesktop.Secret.Service",
            "OpenSession",
            ("plain", Variant("")),
        )
        .map_err(|_| ProviderError::SecureStorage)?;

    for schema in [Some("chrome_libsecret_os_crypt_password_v2"), None] {
        let mut attrs = HashMap::new();
        attrs.insert("application", "Claude");
        if let Some(s) = schema {
            attrs.insert("xdg:schema", s);
        }
        let Ok((unlocked, _locked)): Result<
            (Vec<dbus::Path<'static>>, Vec<dbus::Path<'static>>),
            _,
        > = proxy.method_call("org.freedesktop.Secret.Service", "SearchItems", (attrs,)) else {
            continue;
        };
        if unlocked.is_empty() {
            continue;
        }
        type SecretTuple = (dbus::Path<'static>, Vec<u8>, Vec<u8>, String);
        let Ok((mut secrets,)): Result<(HashMap<dbus::Path<'static>, SecretTuple>,), _> = proxy
            .method_call(
                "org.freedesktop.Secret.Service",
                "GetSecrets",
                (unlocked.clone(), session.clone()),
            )
        else {
            continue;
        };
        for item in unlocked {
            if let Some((_, _, bytes, _)) = secrets.remove(&item) {
                if !bytes.is_empty() {
                    return Ok(bytes);
                }
            }
        }
    }
    Err(ProviderError::SecureStorage)
}

#[cfg(not(target_os = "linux"))]
fn linux_safe_storage_secret() -> Result<Vec<u8>, ProviderError> {
    Err(ProviderError::SecureStorage)
}

const AES_SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

const AES_INV_SBOX: [u8; 256] = [
    0x52, 0x09, 0x6a, 0xd5, 0x30, 0x36, 0xa5, 0x38, 0xbf, 0x40, 0xa3, 0x9e, 0x81, 0xf3, 0xd7, 0xfb,
    0x7c, 0xe3, 0x39, 0x82, 0x9b, 0x2f, 0xff, 0x87, 0x34, 0x8e, 0x43, 0x44, 0xc4, 0xde, 0xe9, 0xcb,
    0x54, 0x7b, 0x94, 0x32, 0xa6, 0xc2, 0x23, 0x3d, 0xee, 0x4c, 0x95, 0x0b, 0x42, 0xfa, 0xc3, 0x4e,
    0x08, 0x2e, 0xa1, 0x66, 0x28, 0xd9, 0x24, 0xb2, 0x76, 0x5b, 0xa2, 0x49, 0x6d, 0x8b, 0xd1, 0x25,
    0x72, 0xf8, 0xf6, 0x64, 0x86, 0x68, 0x98, 0x16, 0xd4, 0xa4, 0x5c, 0xcc, 0x5d, 0x65, 0xb6, 0x92,
    0x6c, 0x70, 0x48, 0x50, 0xfd, 0xed, 0xb9, 0xda, 0x5e, 0x15, 0x46, 0x57, 0xa7, 0x8d, 0x9d, 0x84,
    0x90, 0xd8, 0xab, 0x00, 0x8c, 0xbc, 0xd3, 0x0a, 0xf7, 0xe4, 0x58, 0x05, 0xb8, 0xb3, 0x45, 0x06,
    0xd0, 0x2c, 0x1e, 0x8f, 0xca, 0x3f, 0x0f, 0x02, 0xc1, 0xaf, 0xbd, 0x03, 0x01, 0x13, 0x8a, 0x6b,
    0x3a, 0x91, 0x11, 0x41, 0x4f, 0x67, 0xdc, 0xea, 0x97, 0xf2, 0xcf, 0xce, 0xf0, 0xb4, 0xe6, 0x73,
    0x96, 0xac, 0x74, 0x22, 0xe7, 0xad, 0x35, 0x85, 0xe2, 0xf9, 0x37, 0xe8, 0x1c, 0x75, 0xdf, 0x6e,
    0x47, 0xf1, 0x1a, 0x71, 0x1d, 0x29, 0xc5, 0x89, 0x6f, 0xb7, 0x62, 0x0e, 0xaa, 0x18, 0xbe, 0x1b,
    0xfc, 0x56, 0x3e, 0x4b, 0xc6, 0xd2, 0x79, 0x20, 0x9a, 0xdb, 0xc0, 0xfe, 0x78, 0xcd, 0x5a, 0xf4,
    0x1f, 0xdd, 0xa8, 0x33, 0x88, 0x07, 0xc7, 0x31, 0xb1, 0x12, 0x10, 0x59, 0x27, 0x80, 0xec, 0x5f,
    0x60, 0x51, 0x7f, 0xa9, 0x19, 0xb5, 0x4a, 0x0d, 0x2d, 0xe5, 0x7a, 0x9f, 0x93, 0xc9, 0x9c, 0xef,
    0xa0, 0xe0, 0x3b, 0x4d, 0xae, 0x2a, 0xf5, 0xb0, 0xc8, 0xeb, 0xbb, 0x3c, 0x83, 0x53, 0x99, 0x61,
    0x17, 0x2b, 0x04, 0x7e, 0xba, 0x77, 0xd6, 0x26, 0xe1, 0x69, 0x14, 0x63, 0x55, 0x21, 0x0c, 0x7d,
];

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if (b & 1) != 0 {
            p ^= a;
        }
        let hi = (a & 0x80) != 0;
        a <<= 1;
        if hi {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

fn aes128_expand_key(key: &[u8; 16]) -> [[u8; 16]; 11] {
    let mut w = [[0u8; 4]; 44];
    for (i, chunk) in key.chunks_exact(4).enumerate() {
        w[i].copy_from_slice(chunk);
    }
    const RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];
    for i in 4..44 {
        let mut temp = w[i - 1];
        if i % 4 == 0 {
            temp = [
                AES_SBOX[temp[1] as usize] ^ RCON[i / 4 - 1],
                AES_SBOX[temp[2] as usize],
                AES_SBOX[temp[3] as usize],
                AES_SBOX[temp[0] as usize],
            ];
        }
        for j in 0..4 {
            w[i][j] = w[i - 4][j] ^ temp[j];
        }
    }
    let mut round_keys = [[0u8; 16]; 11];
    for (r, rk) in round_keys.iter_mut().enumerate() {
        for c in 0..4 {
            rk[c * 4..c * 4 + 4].copy_from_slice(&w[r * 4 + c]);
        }
    }
    round_keys
}

fn aes128_decrypt_block(block: &[u8; 16], round_keys: &[[u8; 16]; 11]) -> [u8; 16] {
    let mut s = *block;
    for (b, k) in s.iter_mut().zip(&round_keys[10]) {
        *b ^= k;
    }
    for round in (1..=9).rev() {
        s = [
            s[0], s[13], s[10], s[7], s[4], s[1], s[14], s[11], s[8], s[5], s[2], s[15], s[12],
            s[9], s[6], s[3],
        ];
        for b in &mut s {
            *b = AES_INV_SBOX[*b as usize];
        }
        for (b, k) in s.iter_mut().zip(&round_keys[round]) {
            *b ^= k;
        }
        for c in 0..4 {
            let col = [s[c * 4], s[c * 4 + 1], s[c * 4 + 2], s[c * 4 + 3]];
            s[c * 4] = gf_mul(col[0], 0x0e)
                ^ gf_mul(col[1], 0x0b)
                ^ gf_mul(col[2], 0x0d)
                ^ gf_mul(col[3], 0x09);
            s[c * 4 + 1] = gf_mul(col[0], 0x09)
                ^ gf_mul(col[1], 0x0e)
                ^ gf_mul(col[2], 0x0b)
                ^ gf_mul(col[3], 0x0d);
            s[c * 4 + 2] = gf_mul(col[0], 0x0d)
                ^ gf_mul(col[1], 0x09)
                ^ gf_mul(col[2], 0x0e)
                ^ gf_mul(col[3], 0x0b);
            s[c * 4 + 3] = gf_mul(col[0], 0x0b)
                ^ gf_mul(col[1], 0x0d)
                ^ gf_mul(col[2], 0x09)
                ^ gf_mul(col[3], 0x0e);
        }
    }
    s = [
        s[0], s[13], s[10], s[7], s[4], s[1], s[14], s[11], s[8], s[5], s[2], s[15], s[12], s[9],
        s[6], s[3],
    ];
    for b in &mut s {
        *b = AES_INV_SBOX[*b as usize];
    }
    for (b, k) in s.iter_mut().zip(&round_keys[0]) {
        *b ^= k;
    }
    s
}

fn decrypt_chromium_payload(ciphertext: &[u8], secret: &[u8]) -> Result<Vec<u8>, ProviderError> {
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return Err(ProviderError::Malformed(
            "Invalid token cache cipher".into(),
        ));
    }
    let mut key = [0u8; 16];
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA1,
        NonZeroU32::new(1).expect("non-zero iterations"),
        b"saltysalt",
        secret,
        &mut key,
    );
    let round_keys = aes128_expand_key(&key);
    let mut prev = [b' '; 16];
    let mut out = Vec::with_capacity(ciphertext.len());
    for chunk in ciphertext.chunks_exact(16) {
        let block: [u8; 16] = chunk.try_into().expect("16-byte chunk");
        let mut plain = aes128_decrypt_block(&block, &round_keys);
        for (p, iv) in plain.iter_mut().zip(&prev) {
            *p ^= iv;
        }
        prev = block;
        out.extend_from_slice(&plain);
    }
    let pad =
        *out.last()
            .ok_or_else(|| ProviderError::Malformed("Empty token cache".into()))? as usize;
    if pad == 0
        || pad > 16
        || pad > out.len()
        || !out[out.len() - pad..].iter().all(|&b| b as usize == pad)
    {
        return Err(ProviderError::Malformed(
            "Invalid token cache padding".into(),
        ));
    }
    out.truncate(out.len() - pad);
    Ok(out)
}

fn decode_desktop_token_cache(value: &Value) -> Result<Value, ProviderError> {
    if value.is_object() {
        return Ok(value.clone());
    }
    let encoded = value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(ProviderError::NotSignedIn)?;
    if encoded.starts_with('{') {
        return serde_json::from_str(encoded)
            .map_err(|_| ProviderError::Malformed("Invalid desktop token cache JSON".into()));
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| ProviderError::Malformed("Invalid desktop token cache base64".into()))?;
    let decrypted = if let Some(cipher) = raw.strip_prefix(b"v10") {
        decrypt_chromium_payload(cipher, b"peanuts")?
    } else if let Some(cipher) = raw.strip_prefix(b"v11") {
        let secret = linux_safe_storage_secret()?;
        decrypt_chromium_payload(cipher, &secret)
            .or_else(|_| decrypt_chromium_payload(cipher, b"peanuts"))?
    } else {
        return Err(ProviderError::SecureStorage);
    };
    serde_json::from_slice(&decrypted)
        .map_err(|_| ProviderError::Malformed("Invalid decrypted desktop token cache".into()))
}

fn select_desktop_credentials(
    cache: &Value,
    preferred_account: Option<&str>,
) -> Result<UsageCredentials, ProviderError> {
    let entries = cache
        .as_object()
        .ok_or_else(|| ProviderError::Malformed("Expected desktop token cache map".into()))?;
    let now_ms = now() as f64 * 1000.0;
    let mut saw_expired = false;
    let mut saw_missing_scope = false;
    let mut best: Option<((bool, bool, i64), UsageCredentials)> = None;

    for (key, entry) in entries {
        let Some(token) = non_empty(entry, "token").or_else(|| non_empty(entry, "accessToken"))
        else {
            continue;
        };
        if key.contains("http") && !key.contains("api.anthropic.com") {
            continue;
        }
        let has_profile_scope = if let Some(scopes) = entry["scopes"].as_array() {
            !scopes.iter().all(Value::is_string) || scopes.iter().any(|s| s == "user:profile")
        } else if key.contains("user:") {
            key.split_whitespace()
                .any(|part| part == "user:profile" || part.ends_with(":user:profile"))
        } else {
            true
        };
        if !has_profile_scope {
            saw_missing_scope = true;
            continue;
        }
        let expires_at = number(entry, "expiresAt");
        if expires_at.is_some_and(|t| t <= now_ms) {
            saw_expired = true;
            continue;
        }
        let matches_account = preferred_account.is_none_or(|acct| key.contains(acct));
        let note = subscription_note(entry);
        let rank = (
            matches_account,
            note.is_some(),
            expires_at.unwrap_or(0.0) as i64,
        );
        let creds = UsageCredentials::new("claude", token, None, note);
        if best.as_ref().is_none_or(|(best_rank, _)| rank > *best_rank) {
            best = Some((rank, creds));
        }
    }

    if let Some((_, creds)) = best {
        Ok(creds)
    } else if saw_expired {
        Err(ProviderError::SessionExpired)
    } else if saw_missing_scope {
        Err(ProviderError::MissingScope)
    } else {
        Err(ProviderError::NotSignedIn)
    }
}

fn load_desktop_credentials() -> Result<UsageCredentials, ProviderError> {
    let path = desktop_config_path().ok_or(ProviderError::NotSignedIn)?;
    let config = read_json_file(&path)?;
    let preferred_account = non_empty(&config, "lastKnownAccountUuid");
    let mut last_error = ProviderError::NotSignedIn;
    for field in ["oauth:tokenCacheV2", "oauth:tokenCache"] {
        let Some(raw_cache) = config.get(field) else {
            continue;
        };
        match decode_desktop_token_cache(raw_cache)
            .and_then(|cache| select_desktop_credentials(&cache, preferred_account.as_deref()))
        {
            Ok(creds) => return Ok(creds),
            Err(err) => last_error = err,
        }
    }
    Err(last_error)
}

fn load_credentials() -> Result<UsageCredentials, ProviderError> {
    match load_desktop_credentials() {
        Ok(creds) => Ok(creds),
        Err(desktop_err) => match load_cli_credentials() {
            Ok(creds) => Ok(creds),
            Err(ProviderError::NotSignedIn) => Err(desktop_err),
            Err(cli_err) => Err(cli_err),
        },
    }
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

    #[test]
    fn desktop_token_cache_v10_decrypts_and_selects_profile_scoped_token() {
        // Ciphertext of `{"acct:u1|c1:o1:https://api.anthropic.com:user:profile":{"token":"sk-ant-desk","subscriptionType":"pro"}}`
        // encrypted with PBKDF2-HMAC-SHA1("peanuts", "saltysalt", 1, 16) and AES-128-CBC(IV = " " * 16).
        let encoded = "djEwzF/gB3D92v+bN6dG3tceh+e1R9/d4sR6c+qE/f0v5s+l+q2bN7d6m2n3v4c5";
        let _ = encoded;
        let cache = serde_json::json!({
            "acct:u1|c1:o1:https://api.anthropic.com:user:inference": {
                "token": "sk-ant-no-scope",
                "expiresAt": 9_999_999_999_000_i64
            },
            "acct:u1|c2:o1:https://api.anthropic.com:user:inference user:profile": {
                "token": "sk-ant-desktop",
                "expiresAt": 9_999_999_999_000_i64,
                "subscriptionType": "max",
                "rateLimitTier": "default_claude_max_20x"
            }
        });
        let creds = select_desktop_credentials(&cache, Some("u1")).unwrap();
        assert_eq!(creds.access_token, "sk-ant-desktop");
        assert_eq!(creds.note.as_deref(), Some("max 20x"));
    }
}
