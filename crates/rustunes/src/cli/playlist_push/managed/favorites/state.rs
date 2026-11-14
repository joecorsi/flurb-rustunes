//! Favorite write-ahead intents and verified outbox, separate from playlists.
use super::*;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
pub(super) struct Store {
    db: Connection,
    _lock: File,
}
impl Store {
    pub(super) fn open(state: &Path) -> Result<Self> {
        layout::private_directory(state)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(state.join("favorites-sync.lock"))?;
        ensure!(
            unsafe {
                libc::flock(
                    std::os::fd::AsRawFd::as_raw_fd(&lock),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            } == 0,
            "Another favorites sync is running"
        );
        let path = state.join("favorites-sync.sqlite");
        let created = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(f) => {
                f.sync_all()?;
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(e.into()),
        };
        let m = fs::symlink_metadata(&path)?;
        ensure!(
            m.is_file()
                && !m.file_type().is_symlink()
                && m.permissions().mode() & 0o077 == 0
                && m.uid() == unsafe { libc::geteuid() },
            "favorite state must be a private owned file"
        );
        let mut db = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        db.busy_timeout(Duration::from_secs(5))?;
        let app: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let tables: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            app == 0x46415653 || app == 0 && tables == 0,
            "unrecognized favorite state database"
        );
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        if app == 0 {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch("CREATE TABLE pending(id INTEGER PRIMARY KEY CHECK(id=1),body TEXT NOT NULL CHECK(length(body)<=1048576),started INTEGER NOT NULL CHECK(started IN (0,1))) STRICT; CREATE TABLE verified(id INTEGER PRIMARY KEY AUTOINCREMENT,scope TEXT NOT NULL,body TEXT NOT NULL CHECK(length(body)<=1048576)) STRICT; PRAGMA application_id=0x46415653; PRAGMA user_version=1;")?;
            tx.commit()?;
        }
        ensure!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? == 1,
            "unsupported favorite state version"
        );
        if created {
            File::open(state)?.sync_all()?;
        }
        Ok(Self { db, _lock: lock })
    }
    pub(super) fn pending(&self) -> Result<Option<(Intent, bool)>> {
        let row: Option<(String, bool)> = self
            .db
            .query_row("SELECT body,started FROM pending WHERE id=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        row.map(|(body, started)| Ok((serde_json::from_str(&body)?, started)))
            .transpose()
    }
    pub(super) fn prepare(&mut self, intent: &Intent) -> Result<()> {
        let body = serde_json::to_string(intent)?;
        self.db.execute(
            "INSERT INTO pending(id,body,started) VALUES(1,?1,0)",
            [body],
        )?;
        Ok(())
    }
    pub(super) fn started(&mut self, intent: &Intent) -> Result<()> {
        ensure!(
            self.db.execute(
                "UPDATE pending SET started=1 WHERE id=1 AND body=?1 AND started=0",
                [serde_json::to_string(intent)?]
            )? == 1,
            "favorite pending intent changed"
        );
        Ok(())
    }
    pub(super) fn clear_prepared(&mut self, intent: &Intent) -> Result<()> {
        ensure!(
            self.db.execute(
                "DELETE FROM pending WHERE id=1 AND body=?1 AND started=0",
                [serde_json::to_string(intent)?]
            )? == 1,
            "favorite prepared intent changed"
        );
        Ok(())
    }
    pub(super) fn not_applied(&mut self, intent: &Intent) -> Result<()> {
        ensure!(
            self.db.execute(
                "DELETE FROM pending WHERE id=1 AND body=?1",
                [serde_json::to_string(intent)?]
            )? == 1,
            "favorite intent changed"
        );
        Ok(())
    }
    pub(super) fn finish(&mut self, intent: &Intent) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            tx.execute(
                "DELETE FROM pending WHERE id=1 AND body=?1 AND started=1",
                [serde_json::to_string(intent)?]
            )? == 1,
            "favorite Started intent changed"
        );
        let baselines: Vec<_> = intent.changes.iter().map(|c| c.result.clone()).collect();
        tx.execute(
            "INSERT INTO verified(scope,body) VALUES(?1,?2)",
            params![intent.scope, serde_json::to_string(&baselines)?],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub(super) fn verified(&self, scope: &str) -> Result<Vec<(i64, Vec<Baseline>)>> {
        let mut q = self
            .db
            .prepare("SELECT id,body FROM verified WHERE scope=?1 ORDER BY id")?;
        let rows = q.query_map([scope], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.map(|r| {
            let (id, body) = r?;
            Ok((id, decode_baselines(&body)?))
        })
        .collect()
    }
    pub(super) fn accept(&mut self, scope: &str, ids: &[i64]) -> Result<()> {
        ensure!(
            scope.len() <= 512 && ids.len() <= 50_000 && ids.iter().all(|id| *id > 0),
            "invalid favorite receipt"
        );
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in ids {
            tx.execute(
                "DELETE FROM verified WHERE id=?1 AND scope=?2",
                params![id, scope],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "flurb-favorite-state-{}-{}",
                std::process::id(),
                now_ns().unwrap()
            ));
            layout::private_directory(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn intent(root: &Path) -> Intent {
        Intent {
            scope: "app/device/library".into(),
            device: "device".into(),
            library: "library".into(),
            root: root.to_owned(),
            anchor: 1,
            files: "a".repeat(64),
            changes: vec![Change {
                wire: 1,
                old: 0,
                new: 2,
                result: Baseline {
                    id: "one".into(),
                    wire: 1,
                    audio_sha256: "b".repeat(64),
                    favorite: true,
                    stamp: Stamp {
                        dev: 1,
                        ino: 1,
                        size: 1,
                        mtime: (1, 0),
                        ctime: (1, 0),
                    },
                },
            }],
        }
    }
    #[test]
    fn started_and_verified_work_survives_reopen_and_ack_is_scoped_and_idempotent() {
        let temp = Temp::new();
        let intent = intent(&temp.0);
        let mut store = Store::open(&temp.0).unwrap();
        assert!(Store::open(&temp.0).is_err());
        store.prepare(&intent).unwrap();
        assert!(store.finish(&intent).is_err());
        assert!(!store.pending().unwrap().unwrap().1);
        store.started(&intent).unwrap();
        drop(store);
        let mut store = Store::open(&temp.0).unwrap();
        assert!(store.pending().unwrap().unwrap().1);
        store.finish(&intent).unwrap();
        assert!(store.pending().unwrap().is_none());
        drop(store);
        let mut store = Store::open(&temp.0).unwrap();
        let rows = store.verified(&intent.scope).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1[0].favorite);
        let ids = vec![rows[0].0];
        store.accept("other-phone", &ids).unwrap();
        assert_eq!(store.verified(&intent.scope).unwrap().len(), 1);
        store.accept(&intent.scope, &ids).unwrap();
        store.accept(&intent.scope, &ids).unwrap();
        assert!(store.verified(&intent.scope).unwrap().is_empty());
    }
    #[test]
    fn prepared_work_can_be_discarded_but_started_work_cannot() {
        let temp = Temp::new();
        let intent = intent(&temp.0);
        let mut store = Store::open(&temp.0).unwrap();
        store.prepare(&intent).unwrap();
        store.clear_prepared(&intent).unwrap();
        store.prepare(&intent).unwrap();
        store.started(&intent).unwrap();
        assert!(store.clear_prepared(&intent).is_err());
        assert!(store.pending().unwrap().unwrap().1);
    }
}
