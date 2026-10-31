//! Only rate-limit deadlines and SHA-256 credential-context keys are persisted.
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

#[derive(Default)]
pub struct Cooldowns {
    path: Option<PathBuf>,
    holds: BTreeMap<String, i64>,
}

impl Cooldowns {
    pub fn load(path: Option<PathBuf>) -> Self {
        let holds = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<BTreeMap<String, i64>>(&bytes).ok())
            .unwrap_or_default();
        let mut this = Self { path, holds };
        this.prune(super::creds::now());
        this
    }

    fn prune(&mut self, now: i64) {
        self.holds.retain(|key, until| {
            key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()) && *until > now
        });
        while self.holds.len() > 128 {
            self.holds.pop_first();
        }
    }

    pub fn get(&self, context: &str, now: i64) -> Option<i64> {
        self.holds
            .get(context)
            .copied()
            .filter(|until| *until > now)
    }

    pub fn record(&mut self, context: &str, until: Option<i64>, now: i64) {
        self.prune(now);
        let previous = self.holds.get(context).copied();
        if let Some(until) = until.filter(|t| *t > now) {
            self.holds.insert(context.to_owned(), until);
        } else {
            self.holds.remove(context);
        }
        if previous == self.holds.get(context).copied() {
            return;
        }
        self.prune(now);
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                if std::fs::create_dir_all(parent).is_err() {
                    return;
                }
            }
            if let Ok(bytes) = serde_json::to_vec(&self.holds) {
                // A failed save never changes the in-memory hold or fails a reading.
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let temporary = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                {
                    let written = file.write_all(&bytes).and_then(|_| file.sync_all());
                    drop(file);
                    if written.is_err() || std::fs::rename(&temporary, path).is_err() {
                        let _ = std::fs::remove_file(&temporary);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persists_only_active_hashed_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cooldowns.json");
        let context = "a".repeat(64);
        let now = super::super::creds::now();
        let mut store = Cooldowns::load(Some(path.clone()));
        store.record(&context, Some(now + 600), now);
        assert_eq!(
            Cooldowns::load(Some(path.clone())).get(&context, now),
            Some(now + 600)
        );
        assert_eq!(store.get(&"b".repeat(64), now), None);
        assert_eq!(store.get(&context, now + 601), None);
        store.record(&context, None, now);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        std::fs::write(&path, r#"{"not-a-hash":9999999999}"#).unwrap();
        assert!(Cooldowns::load(Some(path)).holds.is_empty());
    }
}
