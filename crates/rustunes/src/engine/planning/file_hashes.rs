//! Cheap repeat checks for local files. A cached digest proves nothing about a
//! phone: callers must independently match a fresh, verified installed binding.
use crate::engine::state::cache_database;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
const SCHEMA: cache_database::Schema = cache_database::Schema {
    label: "local file hash cache",
    file: "file-hashes.sqlite",
    application_id: 0x464c4843,
    tables: &[(
        "hashes",
        "CREATE TABLE hashes(path BLOB PRIMARY KEY, stat BLOB NOT NULL CHECK(length(stat)=56), sha BLOB NOT NULL CHECK(length(sha)=32)) STRICT, WITHOUT ROWID",
    )],
};
static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
static DB: Mutex<Option<Connection>> = Mutex::new(None);
pub(crate) fn configure(path: PathBuf) {
    let _ = DIRECTORY.set(path);
}
fn stamp(path: &Path) -> Result<Vec<u8>> {
    stat(&fs::symlink_metadata(path)?)
}
fn stat(m: &fs::Metadata) -> Result<Vec<u8>> {
    ensure!(
        m.is_file() && !m.file_type().is_symlink(),
        "source is not a regular file"
    );
    Ok([
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime() as u64,
        m.mtime_nsec() as u64,
        m.ctime() as u64,
        m.ctime_nsec() as u64,
    ]
    .iter()
    .flat_map(|v| v.to_be_bytes())
    .collect())
}
fn with_db<T>(f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
    let dir = DIRECTORY.get()?;
    let mut db = DB.lock().ok()?;
    if db.is_none() {
        *db = cache_database::open(dir, &SCHEMA).ok();
    }
    f(db.as_ref()?).ok()
}
/// Only source inspection/link publication supplies this exact complete stamp.
pub(crate) fn remember(path: &Path, expected: &[u8], hash: [u8; 32]) {
    if stamp(path).is_ok_and(|s| s == expected) {
        with_db(|c| {
            c.execute(
                "INSERT OR REPLACE INTO hashes VALUES(?1,?2,?3)",
                params![path.as_os_str().as_bytes(), expected, &hash[..]],
            )
        });
    }
}
pub(crate) fn read(path: &Path, cancelled: &AtomicBool) -> Result<[u8; 32]> {
    ensure!(!cancelled.load(Ordering::Relaxed), "sync cancelled");
    let before = stamp(path)?;
    if let Some(Some(bytes)) = with_db(|c| {
        c.query_row(
            "SELECT sha FROM hashes WHERE path=?1 AND stat=?2",
            params![path.as_os_str().as_bytes(), before],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .optional()
    }) && let Ok(hash) = <[u8; 32]>::try_from(bytes)
    {
        ensure!(stamp(path)? == before, "source changed during hash lookup");
        return Ok(hash);
    }
    let mut file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        stat(&file.metadata()?)? == before,
        "source changed before hashing"
    );
    let mut hash = Sha256::new();
    let mut buf = [0; 256 * 1024];
    loop {
        ensure!(!cancelled.load(Ordering::Relaxed), "sync cancelled");
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    ensure!(
        stamp(path)? == before && stat(&file.metadata()?)? == before,
        "source changed while hashing"
    );
    let value: [u8; 32] = hash.finalize().into();
    with_db(|c| {
        c.execute(
            "INSERT OR REPLACE INTO hashes VALUES(?1,?2,?3)",
            params![path.as_os_str().as_bytes(), before, &value[..]],
        )
    });
    Ok(value)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changed_deleted_symlinked_and_cancelled_files_cannot_reuse_proof() {
        let root =
            std::env::temp_dir().join(format!("flurb-file-hash-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let p = root.join("source");
        let flag = AtomicBool::new(false);
        fs::write(&p, b"one").unwrap();
        let first = read(&p, &flag).unwrap();
        fs::write(&p, b"two").unwrap();
        assert_ne!(read(&p, &flag).unwrap(), first);
        flag.store(true, Ordering::Relaxed);
        assert!(read(&p, &flag).is_err());
        flag.store(false, Ordering::Relaxed);
        let link = root.join("link");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert!(read(&link, &flag).is_err());
        fs::remove_file(&p).unwrap();
        assert!(read(&p, &flag).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn persisted_hashes_require_the_complete_file_stamp() {
        let root =
            std::env::temp_dir().join(format!("flurb-file-hash-disk-test-{}", std::process::id()));
        let c = cache_database::open(&root, &SCHEMA).unwrap();
        let p = root.join("song");
        fs::write(&p, b"one").unwrap();
        let old = stamp(&p).unwrap();
        c.execute(
            "INSERT INTO hashes VALUES(?1,?2,?3)",
            params![p.as_os_str().as_bytes(), old, &[7u8; 32][..]],
        )
        .unwrap();
        drop(c);
        let c = cache_database::open(&root, &SCHEMA).unwrap();
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM hashes WHERE path=?1 AND stat=?2",
                params![p.as_os_str().as_bytes(), stamp(&p).unwrap()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        fs::write(&p, b"two").unwrap();
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM hashes WHERE path=?1 AND stat=?2",
                params![p.as_os_str().as_bytes(), stamp(&p).unwrap()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        drop(c);
        fs::remove_dir_all(root).unwrap();
    }
}
