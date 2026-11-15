//! Complete MP3 analysis, keyed by the exact path and file stamp. This cache
//! grants no write permission: the upload still binds and hashes actual bytes.
use super::*;
use music_protocol::media::mpeg::{
    Fingerprint, FrameEvidence, Picture, Source, Tags, gapless, timing,
};
use serde::{Deserialize, Serialize};
const REVISION: u32 = 1;
const MAX_BODY: usize = 1024 * 1024;
const SCHEMA: cache_database::Schema = cache_database::Schema {
    label: "MP3 inspection cache",
    file: "mpeg-inspections.sqlite",
    application_id: 0x4d504743,
    tables: &[(
        "inspections",
        "CREATE TABLE inspections(path BLOB PRIMARY KEY, stat BLOB NOT NULL CHECK(length(stat)=56), revision INTEGER NOT NULL, body BLOB NOT NULL CHECK(length(body) BETWEEN 1 AND 1048576), sha BLOB NOT NULL CHECK(length(sha)=32)) STRICT, WITHOUT ROWID",
    )],
};
struct Disk {
    directory: PathBuf,
    reuse: bool,
    db: Mutex<Option<Connection>>,
    failed: AtomicBool,
}
static DISK: OnceLock<Disk> = OnceLock::new();
static MEMORY: Mutex<Option<HashMap<Key, Source>>> = Mutex::new(None);
pub(super) fn configure(directory: PathBuf, reuse: bool) {
    let _ = DISK.set(Disk {
        directory,
        reuse,
        db: Mutex::new(None),
        failed: AtomicBool::new(false),
    });
}
impl Disk {
    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Option<T> {
        if self.failed.load(Ordering::Relaxed) {
            return None;
        }
        let mut db = self.db.lock().ok()?;
        if db.is_none() {
            match cache_database::open(&self.directory, &SCHEMA) {
                Ok(c) => *db = Some(c),
                Err(_) => {
                    self.failed.store(true, Ordering::Relaxed);
                    return None;
                }
            }
        }
        f(db.as_ref()?).ok()
    }
    fn load(&self, k: &Key) -> Option<Source> {
        if !self.reuse {
            return None;
        }
        self.with(|c| load(c, k))?
    }
    fn store(&self, k: &Key, s: &Source) {
        let _ = self.with(|c| store(c, k, s));
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    source: Cached,
    declared: Vec<bool>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cached {
    codec: String,
    fingerprint: Fingerprint,
    timing: Option<timing::TimingEvidence>,
    stream: Option<serde_json::Value>,
    apple_timing: Option<gapless::AppleTiming>,
    metadata: Tags,
    fallback_title: Option<String>,
    id3_version: Option<u8>,
    frames: Vec<FrameEvidence>,
    artwork: Vec<Picture>,
    audio_offset: u64,
    trailing_tag_bytes: u32,
}
fn encode(s: &Source) -> Result<Vec<u8>> {
    ensure!(s.stream.is_none(), "only MPEG-1 source inspections persist");
    let value = serde_json::json!({"source":s,"declared":s.artwork.iter().map(|p|p.declared_type).collect::<Vec<_>>()});
    let body = serde_json::to_vec(&value)?;
    ensure!(body.len() <= MAX_BODY, "MP3 inspection exceeds cache bound");
    Ok(body)
}
fn decode(body: &[u8]) -> Result<Source> {
    ensure!(body.len() <= MAX_BODY, "MP3 cache body exceeds bound");
    let mut e: Envelope = serde_json::from_slice(body)?;
    ensure!(
        e.source.codec == "mpeg1-layer3"
            && e.source.stream.is_none()
            && e.source.artwork.len() == e.declared.len()
            && e.source.artwork.len() <= 8
            && e.source.frames.len() <= 4096,
        "invalid MP3 cache shape"
    );
    for (picture, declared) in e.source.artwork.iter_mut().zip(e.declared) {
        picture.declared_type = declared;
    }
    let c = e.source;
    let s = Source {
        codec: "mpeg1-layer3",
        fingerprint: c.fingerprint,
        timing: c.timing,
        stream: None,
        apple_timing: c.apple_timing,
        metadata: c.metadata,
        fallback_title: c.fallback_title,
        id3_version: c.id3_version,
        frames: c.frames,
        artwork: c.artwork,
        audio_offset: c.audio_offset,
        trailing_tag_bytes: c.trailing_tag_bytes,
    };
    s.transferred_identity()?;
    ensure!(
        s.timing.is_some() && s.apple_timing.is_some(),
        "MP3 cache lacks qualified timing"
    );
    Ok(s)
}
fn load(c: &Connection, k: &Key) -> Result<Option<Source>> {
    let row = c
        .query_row(
            "SELECT body,sha FROM inspections WHERE path=?1 AND stat=?2 AND revision=?3",
            params![k.path(), k.stat(), REVISION],
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    let Some((body, sha)) = row else {
        return Ok(None);
    };
    ensure!(
        Sha256::digest(&body).as_slice() == sha,
        "MP3 cache checksum mismatch"
    );
    let source = decode(&body)?;
    ensure!(
        source.fingerprint.file_bytes == k.size,
        "MP3 cache size mismatch"
    );
    Ok(Some(source))
}
fn store(c: &Connection, k: &Key, s: &Source) -> Result<()> {
    let body = encode(s)?;
    ensure!(
        s.fingerprint.file_bytes == k.size,
        "MP3 cache size mismatch"
    );
    c.execute(
        "INSERT OR REPLACE INTO inspections VALUES(?1,?2,?3,?4,?5)",
        params![
            k.path(),
            k.stat(),
            REVISION,
            &body,
            &Sha256::digest(&body)[..]
        ],
    )?;
    Ok(())
}
pub(crate) fn read(path: &Path) -> Result<Source> {
    let before = key(path);
    if let Some(k) = &before {
        let cached = MEMORY
            .lock()
            .ok()
            .and_then(|m| m.as_ref().and_then(|m| m.get(k).cloned()))
            .or_else(|| DISK.get().and_then(|d| d.load(k)));
        if let Some(s) = cached
            && key(path).as_ref() == Some(k)
        {
            if let Ok(mut memory) = MEMORY.lock() {
                let memory = memory.get_or_insert_default();
                if memory.len() >= 4096 {
                    memory.clear();
                }
                memory.insert(k.clone(), s.clone());
            }
            return Ok(s);
        }
    }
    let s = music_protocol::media::mpeg::read(path)?;
    if let Some(k) = before
        && key(path).as_ref() == Some(&k)
    {
        if let Some(d) = DISK.get() {
            d.store(&k, &s);
        }
        if let Ok(mut m) = MEMORY.lock() {
            let m = m.get_or_insert_default();
            if m.len() >= 4096 {
                m.clear();
            }
            m.insert(k, s.clone());
        }
    }
    Ok(s)
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new(label: &str) -> Self {
            let p = std::env::temp_dir().join(format!("mpeg-cache-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn roundtrip_and_file_stamp_preserve_the_entire_inspection() {
        let root = Temp::new("mpeg-cache");
        let path = root.0.join("song.mp3");
        std::fs::write(&path, music_protocol::test_support::mpeg::audio()).unwrap();
        let s = music_protocol::media::mpeg::read(&path).unwrap();
        assert_eq!(decode(&encode(&s).unwrap()).unwrap(), s);
        let k = key(&path).unwrap();
        let c = cache_database::open(&root.0.join("db"), &SCHEMA).unwrap();
        store(&c, &k, &s).unwrap();
        drop(c);
        let c = cache_database::open(&root.0.join("db"), &SCHEMA).unwrap();
        assert_eq!(load(&c, &k).unwrap().unwrap(), s);
        std::fs::write(
            &path,
            [music_protocol::test_support::mpeg::audio(), vec![0; 10]].concat(),
        )
        .unwrap();
        assert!(load(&c, &key(&path).unwrap()).unwrap().is_none());
        c.execute("UPDATE inspections SET sha=zeroblob(32)", [])
            .unwrap();
        assert!(load(&c, &k).is_err());
    }
    #[test]
    fn in_memory_reads_invalidate_when_source_changes() {
        let root = Temp::new("mpeg-memory");
        let p = root.0.join("song.mp3");
        std::fs::write(&p, music_protocol::test_support::mpeg::audio()).unwrap();
        let first = read(&p).unwrap();
        assert_eq!(read(&p).unwrap(), first);
        std::fs::write(&p, b"damaged").unwrap();
        assert!(read(&p).is_err());
    }
}

/// An inspection bound to the exact regular source observed before our own
/// link/copy operation. Fields stay private: callers cannot invent cache facts.
pub(crate) struct Inspection {
    path: PathBuf,
    key: Key,
    source: Source,
}
impl Inspection {
    pub(crate) fn source(&self) -> &Source {
        &self.source
    }
}
pub(crate) fn inspect_for_transfer(path: &Path) -> Result<Inspection> {
    let before = key(path).ok_or_else(|| anyhow::anyhow!("source is not a regular file"))?;
    let source = read(path)?;
    ensure!(
        key(path).as_ref() == Some(&before),
        "source changed during transfer inspection"
    );
    Ok(Inspection {
        path: path.to_owned(),
        key: before,
        source,
    })
}
fn same_content_stamp(a: &Key, b: &Key) -> bool {
    a.dev == b.dev && a.ino == b.ino && a.size == b.size && a.mtime == b.mtime
}
fn remember(path: &Path, source: &Source) -> Result<()> {
    let k = key(path).ok_or_else(|| anyhow::anyhow!("transfer alias is not a regular file"))?;
    ensure!(
        k.size == source.fingerprint.file_bytes,
        "transfer alias size differs"
    );
    let mut source = source.clone();
    if source.fallback_title.is_some() {
        source.fallback_title = path.file_stem().and_then(|v| v.to_str()).map(str::to_owned);
    }
    if let Some(d) = DISK.get() {
        d.store(&k, &source);
    }
    if let Ok(mut memory) = MEMORY.lock() {
        let memory = memory.get_or_insert_default();
        if memory.len() >= 4096 {
            memory.clear();
        }
        memory.insert(k.clone(), source.clone());
    }
    crate::engine::planning::file_hashes::remember(
        path,
        &k.stat(),
        source.transferred_identity()?.full_file_sha256,
    );
    ensure!(
        key(path).as_ref() == Some(&k),
        "transfer alias changed while retaining inspection"
    );
    Ok(())
}
pub(crate) fn link_transfer(inspection: &Inspection, target: &Path) -> Result<()> {
    ensure!(
        key(&inspection.path).as_ref() == Some(&inspection.key),
        "source changed before transfer link"
    );
    ensure!(
        matches!(std::fs::symlink_metadata(target),Err(e) if e.kind()==std::io::ErrorKind::NotFound),
        "transfer staging path already exists"
    );
    let linked = std::fs::hard_link(&inspection.path, target).is_ok();
    if !linked {
        let input = std::fs::File::open(&inspection.path)?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)?;
        let mut input = std::io::Read::take(input, inspection.key.size + 1);
        ensure!(
            std::io::copy(&mut input, &mut output)? == inspection.key.size,
            "transfer copy size differs"
        );
    }
    let after = key(&inspection.path)
        .ok_or_else(|| anyhow::anyhow!("source disappeared during transfer link"))?;
    ensure!(
        if linked {
            same_content_stamp(&inspection.key, &after)
        } else {
            after == inspection.key
        },
        "source changed during transfer link"
    );
    let target_key = key(target).ok_or_else(|| anyhow::anyhow!("transfer link disappeared"))?;
    ensure!(
        target_key.size == after.size && (!linked || same_content_stamp(&after, &target_key)),
        "transfer link identity differs"
    );
    // This is only inspection reuse. The actual upload still hashes every byte
    // against the prepared full-file digest before completing the native asset.
    remember(&inspection.path, &inspection.source)?;
    remember(target, &inspection.source)
}
pub(crate) fn publish_transfer(staging: &Path, target: &Path) -> Result<()> {
    let inspection = inspect_for_transfer(staging)?;
    std::fs::File::open(staging)?.sync_all()?;
    ensure!(
        key(staging).as_ref() == Some(&inspection.key),
        "staging changed before publication"
    );
    std::fs::rename(staging, target)?;
    let after = key(target).ok_or_else(|| anyhow::anyhow!("published transfer disappeared"))?;
    ensure!(
        same_content_stamp(&inspection.key, &after),
        "published transfer identity differs"
    );
    remember(target, &inspection.source)
}

#[cfg(test)]
mod alias_tests {
    use super::*;
    #[test]
    fn transfer_link_and_publish_reuse_facts_and_invalidate_on_external_changes() {
        let root = std::env::temp_dir().join(format!("mpeg-transfer-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let original = root.join("original.mp3");
        let staging = root.join("staging.mp3");
        let target = root.join("published.mp3");
        std::fs::write(&original, music_protocol::test_support::mpeg::audio()).unwrap();
        let inspection = inspect_for_transfer(&original).unwrap();
        let bytes = std::fs::read(&original).unwrap();
        link_transfer(&inspection, &staging).unwrap();
        publish_transfer(&staging, &target).unwrap();
        let k = key(&target).unwrap();
        assert!(MEMORY.lock().unwrap().as_ref().unwrap().contains_key(&k));
        assert_eq!(std::fs::read(&original).unwrap(), bytes);
        let observed = read(&target).unwrap();
        assert_eq!(observed.fingerprint, inspection.source.fingerprint);
        assert_eq!(observed.fallback_title.as_deref(), Some("published"));
        std::fs::write(&target, b"changed").unwrap();
        assert!(read(&target).is_err());
        assert!(link_transfer(&inspection, &root.join("stale.mp3")).is_err());
        let broken = root.join("broken.mp3");
        std::os::unix::fs::symlink(root.join("absent"), &broken).unwrap();
        // A fresh source must not overwrite or follow an existing staging link.
        std::fs::write(&original, &bytes).unwrap();
        let fresh = inspect_for_transfer(&original).unwrap();
        assert!(link_transfer(&fresh, &broken).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
