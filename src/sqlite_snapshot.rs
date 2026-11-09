use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};

pub const HEADER: &[u8; 16] = b"SQLite format 3\0";
pub const COMPANIONS: [&str; 3] = ["-wal", "-shm", "-journal"];
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub fn is_sqlite(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() < 512 {
        return false;
    }
    let mut header = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .is_ok()
        && &header == HEADER
}

fn private_parent(dest: &Path) -> Result<()> {
    let parent = dest
        .parent()
        .context("snapshot destination has no parent")?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent)?;
    Ok(())
}

pub fn snapshot(source: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        bail!("snapshot destination already exists");
    }
    private_parent(dest)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let src = Connection::open_with_flags(source, flags)
        .with_context(|| format!("opening {} read-only", source.display()))?;
    src.busy_timeout(BUSY_TIMEOUT)?;
    let result = (|| -> Result<()> {
        let mut dst = Connection::open(dest)?;
        {
            let backup = Backup::new(&src, &mut dst)?;
            backup.run_to_completion(256, Duration::from_millis(25), None)?;
        }
        let check: String = dst.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if check != "ok" {
            bail!("integrity check failed: {check}");
        }
        let mode: String = dst.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("delete") {
            bail!("could not make the snapshot self-contained");
        }
        Ok(())
    })();
    if let Err(err) = result {
        let _ = std::fs::remove_file(dest);
        for suffix in COMPANIONS {
            let _ = std::fs::remove_file(format!("{}{suffix}", dest.display()));
        }
        return Err(err);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o600))?;
    }
    for suffix in COMPANIONS {
        let leftover = format!("{}{suffix}", dest.display());
        if Path::new(&leftover).exists() {
            bail!("snapshot left a {suffix} file behind");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database(path: &Path, wal: bool) -> Connection {
        let conn = Connection::open(path).unwrap();
        if wal {
            let mode: String = conn
                .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
        }
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);
             INSERT INTO t (v) VALUES ('a'), ('b'), ('c');",
        )
        .unwrap();
        conn
    }

    fn count(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn detects_sqlite_by_header_only() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("a.sqlite");
        drop(database(&db, false));
        assert!(is_sqlite(&db));
        let fake = tmp.path().join("fake.sqlite");
        std::fs::write(&fake, vec![b'x'; 1024]).unwrap();
        assert!(!is_sqlite(&fake));
        let tiny = tmp.path().join("tiny.db");
        std::fs::write(&tiny, HEADER).unwrap();
        assert!(!is_sqlite(&tiny));
        assert!(!is_sqlite(&tmp.path().join("missing.db")));
    }

    #[test]
    fn snapshot_of_a_live_wal_database_contains_uncheckpointed_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("live.sqlite");
        let writer = database(&db, true);
        writer
            .execute_batch("PRAGMA wal_autocheckpoint = 0; INSERT INTO t (v) VALUES ('wal-only');")
            .unwrap();
        assert!(tmp.path().join("live.sqlite-wal").metadata().unwrap().len() > 0);
        let dest = tmp.path().join("out/nested/live.sqlite");
        snapshot(&db, &dest).unwrap();
        drop(writer);
        assert_eq!(count(&dest), 4);
        for suffix in COMPANIONS {
            assert!(!Path::new(&format!("{}{suffix}", dest.display())).exists());
        }
        let mode: String = Connection::open(&dest)
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(dest.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn snapshot_refuses_non_databases_and_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake.sqlite");
        let mut bytes = HEADER.to_vec();
        bytes.extend(vec![0xffu8; 4096]);
        std::fs::write(&fake, bytes).unwrap();
        let dest = tmp.path().join("out/fake.sqlite");
        assert!(snapshot(&fake, &dest).is_err());
        assert!(!dest.exists());
        let existing = tmp.path().join("exists.sqlite");
        std::fs::write(&existing, b"keep").unwrap();
        let db = tmp.path().join("a.sqlite");
        drop(database(&db, false));
        assert!(snapshot(&db, &existing).is_err());
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep");
    }
}
