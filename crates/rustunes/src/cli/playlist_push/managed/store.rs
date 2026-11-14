//! Local playlist mappings and verified baselines. DELETE journaling plus FULL
//! synchronization keeps each baseline/pending transition atomic and durable.
use super::*;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

pub(super) struct Store {
    db: Connection,
}
impl Store {
    pub(super) fn open(state: &Path) -> Result<Self> {
        layout::private_directory(state)?;
        let path = state.join("playlist-sync.sqlite");
        let created = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => {
                file.sync_all()?;
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(e.into()),
        };
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.permissions().mode() & 0o077 == 0
                && metadata.uid() == unsafe { libc::geteuid() },
            "playlist state must be a private owned regular file"
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
            app == 0x46504c53 || app == 0 && tables == 0,
            "unrecognized playlist-state database"
        );
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        if app == 0 {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch("CREATE TABLE bindings(device TEXT NOT NULL,library TEXT NOT NULL,local_id TEXT NOT NULL,phone_id TEXT NOT NULL,body TEXT NOT NULL CHECK(length(body)<=2097152),PRIMARY KEY(device,library,local_id),UNIQUE(device,library,phone_id)) STRICT;
            CREATE TABLE pending(device TEXT NOT NULL,library TEXT NOT NULL,digest TEXT NOT NULL,stage TEXT NOT NULL CHECK(stage IN ('prepared','started')),body TEXT NOT NULL CHECK(length(body)<=2097152),PRIMARY KEY(device,library)) STRICT;
            PRAGMA application_id=0x46504c53; PRAGMA user_version=1;")?;
            tx.commit()?;
        }
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(version == 1, "unsupported playlist-state schema");
        if created {
            File::open(state)?.sync_all()?;
        }
        Ok(Self { db })
    }
    pub(super) fn get(&self, device: &str, library: &str, local: &str) -> Result<Option<Baseline>> {
        let body: Option<String> = self
            .db
            .query_row(
                "SELECT body FROM bindings WHERE device=?1 AND library=?2 AND local_id=?3",
                params![device, library, local],
                |r| r.get(0),
            )
            .optional()?;
        body.map(|b| {
            let b: Baseline = decode(&b)?;
            validate(&b)?;
            ensure!(
                b.device == device && b.library == library && b.local_id == local,
                "playlist mapping key differs from body"
            );
            Ok(b)
        })
        .transpose()
    }
    pub(super) fn list(&self, device: &str, library: &str) -> Result<Vec<Baseline>> {
        let mut query = self
            .db
            .prepare("SELECT local_id FROM bindings WHERE device=?1 AND library=?2 LIMIT 5001")?;
        let ids = query
            .query_map(params![device, library], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(ids.len() <= 5000, "too many playlist baselines");
        ids.into_iter()
            .map(|id| {
                self.get(device, library, &id)?
                    .context("playlist binding disappeared")
            })
            .collect()
    }
    pub(super) fn accept(&mut self, accepted: &[super::reconcile::Acceptance]) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut unique = BTreeSet::new();
        for receipt in accepted {
            let value = &receipt.observed;
            validate(value)?;
            if let Some(prior) = &receipt.prior {
                ensure!(
                    prior.device == value.device
                        && prior.library == value.library
                        && prior.local_id == value.local_id
                        && prior.phone_id == value.phone_id
                        && prior.created_sha256 == value.created_sha256,
                    "incoming acknowledgement changes playlist identity"
                );
            }
            ensure!(
                unique.insert((&value.device, &value.library, &value.local_id)),
                "duplicate incoming baseline"
            );
            let pending: i64 = tx.query_row(
                "SELECT count(*) FROM pending WHERE device=?1 AND library=?2",
                params![value.device, value.library],
                |r| r.get(0),
            )?;
            ensure!(pending == 0, "a phone write is pending; reconcile it first");
            let current: Option<String> = tx
                .query_row(
                    "SELECT body FROM bindings WHERE device=?1 AND library=?2 AND local_id=?3",
                    params![value.device, value.library, value.local_id],
                    |r| r.get(0),
                )
                .optional()?;
            if current.as_ref() == Some(&encode(value)?) {
                continue;
            }
            check_prior(&tx, receipt.prior.as_ref(), value)?;
            write_baseline(&tx, value)?;
        }
        tx.commit()?;
        Ok(())
    }
    pub(super) fn owner(&self, device: &str, library: &str, pid: u64) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row(
                "SELECT local_id FROM bindings WHERE device=?1 AND library=?2 AND phone_id=?3",
                params![device, library, format!("{pid:016x}")],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub(super) fn pending(&self) -> Result<Vec<Intent>> {
        let mut q = self
            .db
            .prepare("SELECT body,digest FROM pending ORDER BY device,library LIMIT 1001")?;
        let values = q
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(values.len() <= 1000, "too many pending playlist updates");
        values
            .into_iter()
            .map(|(body, digest)| {
                let intent: Intent = decode(&body)?;
                ensure!(
                    intent.version == 1 && intent.digest()? == digest,
                    "playlist intent digest/version differs"
                );
                validate_intent(&intent)?;
                Ok(intent)
            })
            .collect()
    }
    pub(super) fn was_started(&self, intent: &Intent) -> Result<bool> {
        let stage: String = self.db.query_row(
            "SELECT stage FROM pending WHERE device=?1 AND library=?2 AND digest=?3",
            params![
                intent.desired.device,
                intent.desired.library,
                intent.digest()?
            ],
            |r| r.get(0),
        )?;
        Ok(stage == "started")
    }
    pub(super) fn prepare(&mut self, prior: Option<&Baseline>, intent: &Intent) -> Result<()> {
        ensure!(
            intent.additional.is_empty(),
            "batch preparation needs all prior baselines"
        );
        self.prepare_many(&[prior.cloned()], intent)
    }
    pub(super) fn prepare_many(
        &mut self,
        priors: &[Option<Baseline>],
        intent: &Intent,
    ) -> Result<()> {
        validate_intent(intent)?;
        ensure!(
            priors.len() == 1 + intent.additional.len(),
            "batch prior count differs"
        );
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (prior, (desired, _, _)) in priors.iter().zip(intent.targets()) {
            check_prior(&tx, prior.as_ref(), desired)?;
        }
        tx.execute(
            "INSERT INTO pending(device,library,digest,stage,body) VALUES(?1,?2,?3,'prepared',?4)",
            params![
                intent.desired.device,
                intent.desired.library,
                intent.digest()?,
                encode(intent)?
            ],
        )
        .context("another playlist update is pending for this phone")?;
        tx.commit()?;
        Ok(())
    }
    pub(super) fn started(&mut self, intent: &Intent) -> Result<()> {
        ensure!(self.db.execute("UPDATE pending SET stage='started' WHERE device=?1 AND library=?2 AND digest=?3 AND stage='prepared'",params![intent.desired.device,intent.desired.library,intent.digest()?])?==1,"playlist intent changed before staging");
        Ok(())
    }
    /// Called only for typed Unsent or a SQL-stage Prepared intent.
    pub(super) fn abort(&mut self, intent: &Intent) -> Result<()> {
        ensure!(
            self.db.execute(
                "DELETE FROM pending WHERE device=?1 AND library=?2 AND digest=?3",
                params![
                    intent.desired.device,
                    intent.desired.library,
                    intent.digest()?
                ]
            )? == 1,
            "playlist intent changed before abort"
        );
        Ok(())
    }
    pub(super) fn commit(&mut self, intent: &Intent, baseline: &Baseline) -> Result<()> {
        ensure!(intent.additional.is_empty(), "use atomic batch completion");
        self.commit_many(intent, std::slice::from_ref(baseline))
    }
    pub(super) fn commit_many(&mut self, intent: &Intent, baselines: &[Baseline]) -> Result<()> {
        validate_intent(intent)?;
        ensure!(
            baselines.len() == 1 + intent.additional.len(),
            "verified batch target count differs"
        );
        for ((desired, prior, delete), baseline) in intent.targets().zip(baselines) {
            validate(baseline)?;
            let mut expected = desired.clone();
            expected.created_sha256 = baseline.created_sha256.clone();
            expected.verified_anchor = baseline.verified_anchor;
            expected.verified_ns = baseline.verified_ns.clone();
            ensure!(
                expected == *baseline
                    && delete == baseline.deleted
                    && baseline.verified_anchor
                        == prior
                            .base_anchor
                            .checked_add(1)
                            .context("anchor overflow")?
                    && valid_sha(&baseline.created_sha256),
                "verified baseline does not match playlist intent"
            );
        }
        let baseline = &intent.desired;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (digest, stage): (String, String) = tx.query_row(
            "SELECT digest,stage FROM pending WHERE device=?1 AND library=?2",
            params![baseline.device, baseline.library],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            digest == intent.digest()? && stage == "started",
            "no matching Started intent owns this playlist completion"
        );
        for baseline in baselines {
            write_baseline(&tx, baseline)?;
        }
        tx.execute(
            "DELETE FROM pending WHERE device=?1 AND library=?2 AND digest=?3",
            params![baseline.device, baseline.library, digest],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub(super) fn noop(&mut self, prior: Option<&Baseline>, baseline: &Baseline) -> Result<()> {
        validate(baseline)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending: i64 = tx.query_row(
            "SELECT count(*) FROM pending WHERE device=?1 AND library=?2",
            params![baseline.device, baseline.library],
            |r| r.get(0),
        )?;
        ensure!(
            pending == 0,
            "a playlist update is still pending for this phone"
        );
        check_prior(&tx, prior, baseline)?;
        write_baseline(&tx, baseline)?;
        tx.commit()?;
        Ok(())
    }
}
fn validate_intent(intent: &Intent) -> Result<()> {
    ensure!(
        intent.version == 1 && intent.additional.len() < 64,
        "invalid playlist batch size/version"
    );
    let mut locals = BTreeSet::new();
    let mut phones = BTreeSet::new();
    for (desired, before, delete) in intent.targets() {
        validate(desired)?;
        ensure!(
            desired.device == intent.desired.device
                && desired.library == intent.desired.library
                && locals.insert(&desired.local_id)
                && phones.insert(desired.phone_id)
                && desired.deleted == delete
                && before.base_anchor == intent.before.base_anchor,
            "invalid or repeated playlist batch identity"
        );
    }
    Ok(())
}
fn encode<T: Serialize>(value: &T) -> Result<String> {
    let body = serde_json::to_string(value)?;
    ensure!(
        body.len() <= 2 * 1024 * 1024,
        "playlist state exceeds bound"
    );
    Ok(body)
}
fn decode<T: serde::de::DeserializeOwned>(body: &str) -> Result<T> {
    ensure!(
        body.len() <= 2 * 1024 * 1024,
        "playlist state exceeds bound"
    );
    Ok(serde_json::from_str(body)?)
}
fn valid_sha(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn validate(b: &Baseline) -> Result<()> {
    ensure!(
        b.version == 1
            && valid_sha(&b.device)
            && valid_sha(&b.library)
            && valid_id(&b.local_id)
            && b.phone_id != 0
            && !b.name.trim().is_empty()
            && b.name.len() <= 255
            && b.members.len() <= 499,
        "invalid playlist baseline"
    );
    let mut local = BTreeSet::new();
    let mut phone = BTreeSet::new();
    for member in &b.members {
        ensure!(
            valid_id(&member.local_id)
                && local.insert(&member.local_id)
                && member.phone_id != 0
                && phone.insert(member.phone_id)
                && valid_sha(&member.file_sha256)
                && member.source.len() <= 4096,
            "invalid or duplicate baseline track"
        );
    }
    Ok(())
}
fn check_prior(
    tx: &rusqlite::Transaction<'_>,
    prior: Option<&Baseline>,
    new: &Baseline,
) -> Result<()> {
    let body: Option<String> = tx
        .query_row(
            "SELECT body FROM bindings WHERE device=?1 AND library=?2 AND local_id=?3",
            params![new.device, new.library, new.local_id],
            |r| r.get(0),
        )
        .optional()?;
    let expected = prior.map(encode).transpose()?;
    ensure!(
        body == expected,
        "playlist baseline changed during planning"
    );
    Ok(())
}
fn write_baseline(tx: &rusqlite::Transaction<'_>, b: &Baseline) -> Result<()> {
    ensure!(
        valid_sha(&b.created_sha256),
        "verified playlist creation identity is absent"
    );
    tx.execute("INSERT INTO bindings(device,library,local_id,phone_id,body) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(device,library,local_id) DO UPDATE SET phone_id=excluded.phone_id,body=excluded.body",params![b.device,b.library,b.local_id,format!("{:016x}",b.phone_id),encode(b)?])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "playlist-store-{}-{}",
                std::process::id(),
                now_ns().unwrap()
            ));
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn baseline() -> Baseline {
        Baseline {
            version: 1,
            device: "a".repeat(64),
            library: "b".repeat(64),
            local_id: "flurb:library:playlist".into(),
            phone_id: u64::MAX - 10,
            name: "Original".into(),
            members: vec![Member {
                local_id: "track-a".into(),
                phone_id: 101,
                file_sha256: "c".repeat(64),
                source: "Artist/A.mp3".into(),
            }],
            created_sha256: "d".repeat(64),
            verified_anchor: 10,
            verified_ns: "1000".into(),
            deleted: false,
            mutable: true,
        }
    }
    fn intent(b: &Baseline) -> Intent {
        Intent {
            version: 1,
            root: "/private/tmp/test-evidence".into(),
            desired: b.clone(),
            before: Proof {
                base_anchor: 10,
                files: "e".repeat(64),
                tracks: "f".repeat(64),
                other_playlists: "0".repeat(64),
                created: Some(b.created_sha256.clone()),
                ui: Some("1".repeat(64)),
            },
            operation_sha256: "2".repeat(64),
            delete: false,
            additional: vec![],
        }
    }
    fn completed(i: &Intent) -> Baseline {
        let mut b = i.desired.clone();
        b.verified_anchor = 11;
        b.verified_ns = "2000".into();
        b
    }
    #[test]
    fn batch_outbox_survives_restart_and_commits_every_baseline_atomically() {
        let temp = Temp::new();
        let original = baseline();
        let mut second = original.clone();
        second.local_id = "second".into();
        second.phone_id += 1;
        let mut intent = intent(&original);
        intent.additional.push(BatchTarget {
            desired: second.clone(),
            before: intent.before.clone(),
            delete: false,
        });
        let mut db = Store::open(&temp.0).unwrap();
        let mut stale = second.clone();
        stale.name = "Stale".into();
        assert!(db.prepare_many(&[None, Some(stale)], &intent).is_err());
        assert!(db.pending().unwrap().is_empty());
        db.prepare_many(&[None, None], &intent).unwrap();
        drop(db);
        let mut db = Store::open(&temp.0).unwrap();
        let saved = db.pending().unwrap().remove(0);
        assert_eq!(saved.digest().unwrap(), intent.digest().unwrap());
        assert!(!db.was_started(&saved).unwrap());
        db.started(&saved).unwrap();
        drop(db);
        let mut db = Store::open(&temp.0).unwrap();
        assert!(db.was_started(&saved).unwrap());
        let a = completed(&saved);
        let mut b = second.clone();
        b.verified_anchor = 11;
        b.verified_ns = "2000".into();
        let mut bad = b.clone();
        bad.name = "Wrong result".into();
        assert!(db.commit_many(&saved, &[a.clone(), bad]).is_err());
        assert!(
            db.get(&a.device, &a.library, &a.local_id)
                .unwrap()
                .is_none()
        );
        assert!(
            db.get(&b.device, &b.library, &b.local_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(db.pending().unwrap().len(), 1);
        db.commit_many(&saved, &[a.clone(), b.clone()]).unwrap();
        assert!(db.pending().unwrap().is_empty());
        assert_eq!(db.get(&a.device, &a.library, &a.local_id).unwrap(), Some(a));
        assert_eq!(db.get(&b.device, &b.library, &b.local_id).unwrap(), Some(b));
        let mut duplicate = intent.clone();
        duplicate.additional[0].desired.phone_id = duplicate.desired.phone_id;
        assert!(db.prepare_many(&[None, None], &duplicate).is_err());
    }

    #[test]
    fn incoming_acknowledgements_are_atomic_idempotent_and_keep_old_intent_digests_compatible() {
        use super::super::reconcile::Acceptance;
        let temp = Temp::new();
        let mut db = Store::open(&temp.0).unwrap();
        let original = baseline();
        db.noop(None, &original).unwrap();
        let mut changed = original.clone();
        changed.name = "Phone edit".into();
        let receipt = Acceptance {
            prior: Some(original.clone()),
            observed: changed.clone(),
        };
        db.accept(&[receipt]).unwrap();
        db.accept(&[Acceptance {
            prior: Some(original.clone()),
            observed: changed.clone(),
        }])
        .unwrap();
        let mut another = changed.clone();
        another.local_id = "second".into();
        another.phone_id += 1;
        let mut stale = original.clone();
        stale.name = "Stale".into();
        assert!(
            db.accept(&[
                Acceptance {
                    prior: None,
                    observed: another.clone()
                },
                Acceptance {
                    prior: Some(original.clone()),
                    observed: stale
                }
            ])
            .is_err()
        );
        assert!(
            db.get(&another.device, &another.library, &another.local_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.get(&original.device, &original.library, &original.local_id)
                .unwrap(),
            Some(changed)
        );
        let encoded = encode(&intent(&original)).unwrap();
        assert!(!encoded.contains("\"deleted\""));
        assert!(!encoded.contains("\"mutable\""));
        assert!(!encoded.contains("\"delete\""));
        let old: Intent = decode(&encoded).unwrap();
        assert_eq!(old.digest().unwrap(), intent(&original).digest().unwrap());
    }
    #[test]
    fn deletion_tombstones_commit_only_from_their_matching_started_intent() {
        let temp = Temp::new();
        let mut db = Store::open(&temp.0).unwrap();
        let original = baseline();
        db.noop(None, &original).unwrap();
        let mut deleted = original.clone();
        deleted.deleted = true;
        deleted.members.clear();
        let mut i = intent(&deleted);
        i.delete = true;
        db.prepare(Some(&original), &i).unwrap();
        assert!(db.commit(&i, &completed(&i)).is_err());
        db.started(&i).unwrap();
        db.commit(&i, &completed(&i)).unwrap();
        assert!(
            db.get(&original.device, &original.library, &original.local_id)
                .unwrap()
                .unwrap()
                .deleted
        );
    }

    #[test]
    fn prepared_and_started_keep_the_previous_verified_baseline_until_commit() {
        let temp = Temp::new();
        let mut db = Store::open(&temp.0).unwrap();
        let old = baseline();
        db.noop(None, &old).unwrap();
        let mut next = old.clone();
        next.name = "Renamed".into();
        next.members.clear();
        let i = intent(&next);
        db.prepare(Some(&old), &i).unwrap();
        assert_eq!(
            db.get(&old.device, &old.library, &old.local_id).unwrap(),
            Some(old.clone())
        );
        assert!(!db.was_started(&i).unwrap());
        assert!(db.commit(&i, &completed(&i)).is_err());
        assert!(db.noop(Some(&old), &old).is_err());
        drop(db);
        let mut db = Store::open(&temp.0).unwrap();
        assert_eq!(db.pending().unwrap().len(), 1);
        db.started(&i).unwrap();
        drop(db);
        let mut db = Store::open(&temp.0).unwrap();
        assert!(db.was_started(&i).unwrap());
        db.commit(&i, &completed(&i)).unwrap();
        assert!(db.pending().unwrap().is_empty());
        assert_eq!(
            db.get(&old.device, &old.library, &old.local_id).unwrap(),
            Some(completed(&i))
        );
    }
    #[test]
    fn commit_is_atomic_even_when_removing_pending_fails() {
        let temp = Temp::new();
        let mut db = Store::open(&temp.0).unwrap();
        let old = baseline();
        db.noop(None, &old).unwrap();
        let mut next = old.clone();
        next.name = "New".into();
        let i = intent(&next);
        db.prepare(Some(&old), &i).unwrap();
        db.started(&i).unwrap();
        db.db.execute_batch("CREATE TRIGGER fail_commit BEFORE DELETE ON pending BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
        assert!(db.commit(&i, &completed(&i)).is_err());
        assert_eq!(
            db.get(&old.device, &old.library, &old.local_id).unwrap(),
            Some(old)
        );
        assert!(db.was_started(&i).unwrap());
        db.db.execute_batch("DROP TRIGGER fail_commit").unwrap();
        db.commit(&i, &completed(&i)).unwrap();
    }
    #[test]
    fn stale_plans_wrong_receipts_and_duplicate_owners_cannot_advance_state() {
        let temp = Temp::new();
        let mut db = Store::open(&temp.0).unwrap();
        let old = baseline();
        db.noop(None, &old).unwrap();
        let mut next = old.clone();
        next.name = "New".into();
        let i = intent(&next);
        assert!(db.prepare(None, &i).is_err());
        db.prepare(Some(&old), &i).unwrap();
        db.started(&i).unwrap();
        let mut wrong = completed(&i);
        wrong.members.clear();
        assert!(db.commit(&i, &wrong).is_err());
        wrong = completed(&i);
        wrong.verified_anchor = 12;
        assert!(db.commit(&i, &wrong).is_err());
        let mut bad = i.clone();
        bad.operation_sha256 = "9".repeat(64);
        assert!(db.commit(&bad, &completed(&bad)).is_err());
        db.commit(&i, &completed(&i)).unwrap();
        next.local_id = "another-playlist".into();
        assert!(db.noop(None, &next).is_err());
        assert_eq!(
            db.owner(&old.device, &old.library, old.phone_id).unwrap(),
            Some(old.local_id.clone())
        );
        next.device = "3".repeat(64);
        db.noop(None, &next).unwrap();
    }
    #[test]
    fn unstarted_intent_can_be_aborted_without_changing_baseline() {
        let temp = Temp::new();
        let mut db = Store::open(&temp.0).unwrap();
        let b = baseline();
        let i = intent(&b);
        db.prepare(None, &i).unwrap();
        db.abort(&i).unwrap();
        assert!(db.pending().unwrap().is_empty());
        assert!(
            db.get(&b.device, &b.library, &b.local_id)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn wire_ids_roundtrip_losslessly_and_invalid_state_is_rejected() {
        let b = baseline();
        let json = encode(&b).unwrap();
        assert!(json.contains("fffffffffffffff5"));
        assert_eq!(decode::<Baseline>(&json).unwrap(), b);
        let mut bad = b.clone();
        bad.members.push(b.members[0].clone());
        assert!(validate(&bad).is_err());
        bad = b.clone();
        bad.members[0].local_id.clear();
        assert!(validate(&bad).is_err());
        assert!(decode::<Baseline>(&json.replace("fffffffffffffff5", "1")).is_err());
    }
    #[test]
    fn unrelated_databases_and_symlinks_are_refused() {
        let temp = Temp::new();
        layout::private_directory(&temp.0).unwrap();
        let path = temp.0.join("playlist-sync.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE unrelated(id INTEGER)")
            .unwrap();
        drop(db);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Store::open(&temp.0).is_err());
        fs::rename(&path, temp.0.join("other.sqlite")).unwrap();
        std::os::unix::fs::symlink("other.sqlite", &path).unwrap();
        assert!(Store::open(&temp.0).is_err());
    }
}
