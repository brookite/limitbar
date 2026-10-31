//! Shared read-only access to agent-owned SQLite databases, including live WALs.
use std::path::Path;
use std::time::Duration;

use super::ProviderError;
use rusqlite::{Connection, OpenFlags};

pub fn open_readonly(path: &Path) -> Result<Connection, ProviderError> {
    let fail = || ProviderError::Malformed("Cannot read agent database".into());
    let mut has_sidecars = false;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        has_sidecars |= Path::new(&sidecar).try_exists().map_err(|_| fail())?;
    }
    let db = if has_sidecars {
        // Never use immutable mode here: it silently ignores uncheckpointed auth.
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
    } else {
        // An idle WAL-mode database can otherwise recreate its sidecars on a read.
        let mut uri = reqwest::Url::from_file_path(path).map_err(|_| fail())?;
        uri.set_query(Some("immutable=1"));
        Connection::open_with_flags(
            uri.as_str(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
    }
    .map_err(|_| fail())?;
    db.busy_timeout(Duration::from_millis(250))
        .map_err(|_| fail())?;
    Ok(db)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_database_is_never_created_and_writes_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");
        assert!(open_readonly(&path).is_err());
        assert!(!path.exists());
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE test (value TEXT);").unwrap();
        drop(db);
        let before = std::fs::read(&path).unwrap();
        let readonly = open_readonly(&path).unwrap();
        assert!(readonly
            .execute("INSERT INTO test VALUES ('should fail')", [])
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
