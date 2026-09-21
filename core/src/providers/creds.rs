//! Shared helpers for locating and reading agent credential files.
//!
//! limitbar only ever *reads* these files. It never refreshes or rewrites them:
//! OpenAI rotates refresh tokens, so writing one back would invalidate the copy a
//! running `codex` holds, and concurrent writes would race the live CLI.

use std::path::{Path, PathBuf};

use base64::Engine;
use serde::de::DeserializeOwned;

use super::ProviderError;

/// The first of these variables that is set to something non-empty.
///
/// An empty value counts as unset: an exported-but-blank variable should fall
/// through to the next candidate rather than resolve to the filesystem root.
pub fn first_non_empty_env(names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(|name| {
        let value = std::env::var_os(name)?;
        (!value.is_empty()).then(|| PathBuf::from(value))
    })
}

/// The user's home directory, resolved the way the agent CLIs resolve it.
///
/// On Windows that is `%USERPROFILE%`, and it deliberately outranks `HOME`: a
/// POSIX-ish shell such as Git Bash or MSYS sets `HOME` to its own private home,
/// which is *not* where `claude` and `codex` keep their credentials.
pub fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        first_non_empty_env(&["USERPROFILE", "HOME"])
    } else {
        first_non_empty_env(&["HOME"])
    }
}

/// Where an agent keeps its configuration.
///
/// `env_var` wins over `<home>/<dir_name>` so tests (and users with relocated
/// config) can point us elsewhere, matching how the CLIs resolve their own homes.
pub fn agent_home(env_var: &str, dir_name: &str) -> Option<PathBuf> {
    if let Some(dir) = first_non_empty_env(&[env_var]) {
        return Some(dir);
    }
    home_dir().map(|home| home.join(dir_name))
}

/// Reads and parses a credentials file, distinguishing "agent absent" from
/// "agent present but not signed in" so the UI can say the right thing.
pub fn read_credentials<T: DeserializeOwned>(
    home: Option<&Path>,
    file_name: &str,
) -> Result<T, ProviderError> {
    let Some(home) = home else {
        return Err(ProviderError::NotInstalled);
    };
    if !home.is_dir() {
        return Err(ProviderError::NotInstalled);
    }
    let path = home.join(file_name);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProviderError::NotSignedIn)
        }
        Err(err) => return Err(ProviderError::Malformed(err.to_string())),
    };
    serde_json::from_str(&raw).map_err(|err| ProviderError::Malformed(err.to_string()))
}

/// Extracts the `exp` claim from a JWT without verifying its signature.
///
/// Codex stores no explicit expiry alongside its access token, so the only way to
/// tell a stale token from a live one before spending a request is to look inside.
/// We are not authenticating anything here, just reading a hint, so skipping
/// signature verification is safe: the server remains the authority.
pub fn jwt_expiry(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims.get("exp")?.as_i64()
}

/// Parses a reset moment that the API may express either as epoch seconds or, as
/// Claude's usage endpoint does, as an RFC 3339 timestamp.
pub fn parse_reset(value: Option<&serde_json::Value>) -> Option<i64> {
    match value? {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        serde_json::Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.timestamp()),
        _ => None,
    }
}

/// Unix epoch seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A window whose reset moment has already passed is no longer interesting;
/// Claude Code hides those too.
pub fn is_window_live(resets_at: Option<i64>) -> bool {
    match resets_at {
        Some(at) => at > now(),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize, Debug, PartialEq)]
    struct Sample {
        hello: String,
    }

    fn scoped_env<T>(key: &str, value: Option<&str>, body: impl FnOnce() -> T) -> T {
        let previous = std::env::var_os(key);
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let out = body();
        match previous {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        out
    }

    #[test]
    fn env_override_beats_home() {
        scoped_env("HOME", Some("/home/someone"), || {
            scoped_env("LIMITBAR_TEST_HOME", Some("/elsewhere/cfg"), || {
                assert_eq!(
                    agent_home("LIMITBAR_TEST_HOME", ".claude"),
                    Some(PathBuf::from("/elsewhere/cfg"))
                );
            });
            scoped_env("LIMITBAR_TEST_HOME", None, || {
                assert_eq!(
                    agent_home("LIMITBAR_TEST_HOME", ".claude"),
                    Some(PathBuf::from("/home/someone/.claude"))
                );
            });
            // An empty override is treated as unset rather than as the root directory.
            scoped_env("LIMITBAR_TEST_HOME", Some(""), || {
                assert_eq!(
                    agent_home("LIMITBAR_TEST_HOME", ".claude"),
                    Some(PathBuf::from("/home/someone/.claude"))
                );
            });
        });
    }

    #[test]
    fn an_exported_but_blank_variable_counts_as_unset() {
        scoped_env("LIMITBAR_TEST_A", Some(""), || {
            scoped_env("LIMITBAR_TEST_B", Some("/second"), || {
                assert_eq!(
                    first_non_empty_env(&["LIMITBAR_TEST_A", "LIMITBAR_TEST_B"]),
                    Some(PathBuf::from("/second"))
                );
            });
        });
    }

    #[test]
    fn env_candidates_are_tried_in_order() {
        scoped_env("LIMITBAR_TEST_A", Some("/first"), || {
            scoped_env("LIMITBAR_TEST_B", Some("/second"), || {
                assert_eq!(
                    first_non_empty_env(&["LIMITBAR_TEST_A", "LIMITBAR_TEST_B"]),
                    Some(PathBuf::from("/first"))
                );
            });
        });
        assert_eq!(first_non_empty_env(&["LIMITBAR_TEST_MISSING"]), None);
    }

    #[test]
    fn the_home_directory_follows_the_platform() {
        scoped_env("HOME", Some("/posix/home"), || {
            scoped_env("USERPROFILE", Some(r"C:\Users\someone"), || {
                let expected = if cfg!(windows) {
                    // %USERPROFILE% outranks the HOME that Git Bash also exports.
                    PathBuf::from(r"C:\Users\someone")
                } else {
                    PathBuf::from("/posix/home")
                };
                assert_eq!(home_dir(), Some(expected));
            });
        });
    }

    #[test]
    fn missing_directory_means_not_installed() {
        let err =
            read_credentials::<Sample>(Some(Path::new("/nope/definitely/not/here")), "x.json")
                .unwrap_err();
        assert!(matches!(err, ProviderError::NotInstalled));
        let err = read_credentials::<Sample>(None, "x.json").unwrap_err();
        assert!(matches!(err, ProviderError::NotInstalled));
    }

    #[test]
    fn present_directory_without_file_means_not_signed_in() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_credentials::<Sample>(Some(dir.path()), "auth.json").unwrap_err();
        assert!(matches!(err, ProviderError::NotSignedIn));
    }

    #[test]
    fn unreadable_json_is_reported_as_malformed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("auth.json"), "{not json").unwrap();
        let err = read_credentials::<Sample>(Some(dir.path()), "auth.json").unwrap_err();
        assert!(matches!(err, ProviderError::Malformed(_)));
    }

    #[test]
    fn well_formed_json_parses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("auth.json"), r#"{"hello":"world"}"#).unwrap();
        let parsed: Sample = read_credentials(Some(dir.path()), "auth.json").unwrap();
        assert_eq!(parsed.hello, "world");
    }

    fn fake_jwt(payload: &str) -> String {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!("{}.{}.{}", b64.encode("{}"), b64.encode(payload), "sig")
    }

    #[test]
    fn jwt_expiry_reads_the_exp_claim() {
        assert_eq!(
            jwt_expiry(&fake_jwt(r#"{"exp":1790037057}"#)),
            Some(1790037057)
        );
    }

    #[test]
    fn jwt_expiry_tolerates_tokens_it_cannot_read() {
        assert_eq!(jwt_expiry(&fake_jwt(r#"{"sub":"abc"}"#)), None);
        assert_eq!(jwt_expiry("not-a-jwt"), None);
        assert_eq!(jwt_expiry(""), None);
        assert_eq!(jwt_expiry("a.!!!not-base64!!!.c"), None);
    }

    #[test]
    fn reset_moments_parse_from_either_shape() {
        use serde_json::json;
        // Claude's usage endpoint sends RFC 3339 with a fractional second and offset.
        assert_eq!(
            parse_reset(Some(&json!("2026-09-21T21:40:00.959411+00:00"))),
            Some(1_790_026_800)
        );
        assert_eq!(
            parse_reset(Some(&json!("2026-09-21T23:40:00+02:00"))),
            Some(1_790_026_800)
        );
        // Codex sends plain epoch seconds.
        assert_eq!(
            parse_reset(Some(&json!(1_790_026_800i64))),
            Some(1_790_026_800)
        );
        assert_eq!(parse_reset(Some(&json!(null))), None);
        assert_eq!(parse_reset(Some(&json!("not a date"))), None);
        assert_eq!(parse_reset(None), None);
    }

    #[test]
    fn windows_without_a_reset_are_treated_as_live() {
        assert!(is_window_live(None));
        assert!(is_window_live(Some(now() + 600)));
        assert!(!is_window_live(Some(now() - 1)));
    }
}
