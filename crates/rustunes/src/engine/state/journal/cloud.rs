//! Portable, verified binding hints. These are deliberately not executable
//! operation journals: a fresh device capture must confirm every binding.
use super::*;
use serde::{Deserialize, Serialize};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u8,
    bindings: Vec<Entry>,
    reserved: Vec<(i64, Vec<u8>)>,
    unfinished: Vec<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    content: Vec<u8>,
    source: Option<String>,
    #[serde(default)]
    conversion: Option<PortableConversion>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableConversion {
    path: String,
    bytes: u64,
    sha: [u8; 32],
    audio: [u8; 32],
    ffmpeg: String,
}
/// Export no pairing secrets, transfer files, or host file stamps. `scratch`
/// must be a new private directory and is owned by the caller.
pub fn export_cloud_snapshot(journal: &Path, scratch: &Path, root: &Path) -> Result<String> {
    let prior = read_prior(journal, scratch)?;
    let canonical = fs::canonicalize(root).unwrap_or(root.to_owned());
    let mut bindings = Vec::new();
    for binding in prior.bindings {
        let source = binding.source.and_then(|p| {
            let p = fs::canonicalize(&p).unwrap_or(p);
            p.strip_prefix(&canonical)
                .ok()
                .and_then(|p| p.to_str())
                .map(str::to_owned)
        });
        let conversion = binding.conversion.and_then(|c| {
            let path = fs::canonicalize(&c.source_path).unwrap_or(c.source_path);
            path.strip_prefix(&canonical)
                .ok()
                .and_then(|p| p.to_str())
                .map(|path| PortableConversion {
                    path: path.into(),
                    bytes: c.source_bytes,
                    sha: c.source_sha256,
                    audio: c.source_audio,
                    ffmpeg: c.ffmpeg,
                })
        });
        bindings.push(Entry {
            content: encode_content(&binding.content)?,
            source,
            conversion,
        });
    }
    Ok(serde_json::to_string(&Snapshot {
        version: 1,
        bindings,
        reserved: prior
            .reserved
            .into_iter()
            .map(|r| (r.namespace, r.value))
            .collect(),
        unfinished: prior.unfinished,
    })?)
}
/// Materialize a carry-only journal in a new directory, never over a live
/// journal. The pull reader treats its old schema as hints and verifies them
/// against the phone before establishing a usable local baseline.
pub fn import_cloud_snapshot(body: &str, directory: &Path, root: &Path) -> Result<()> {
    ensure!(
        body.len() <= 128 * 1024 * 1024,
        "cloud device snapshot exceeds bound"
    );
    let snapshot: Snapshot = serde_json::from_str(body)?;
    ensure!(
        snapshot.version == 1
            && snapshot.bindings.len() <= 50_000
            && snapshot.reserved.len() <= 100_000,
        "unsupported cloud device snapshot"
    );
    ensure!(
        snapshot.unfinished.is_empty(),
        "Another computer has unfinished device work; finish or recover it on that computer before switching"
    );
    ensure!(!directory.exists(), "cloud snapshot target already exists");
    fs::DirBuilder::new().mode(0o700).create(directory)?;
    let db = Connection::open(directory.join(DB_NAME))?;
    db.execute_batch("PRAGMA application_id=0x49534a31; CREATE TABLE binding_records(id INTEGER PRIMARY KEY,track BLOB,path TEXT);CREATE TABLE binding_versions(binding_id INTEGER,version INTEGER,content BLOB);CREATE TABLE active_bindings(binding_id INTEGER,version INTEGER);CREATE TABLE binding_sources(binding_id INTEGER,source TEXT);CREATE TABLE reservations(namespace INTEGER,value BLOB);CREATE TABLE binding_conversions(binding_id INTEGER,source_path TEXT,source_bytes INTEGER,source_sha BLOB,source_audio BLOB,ffmpeg TEXT);")?;
    let tx = db.unchecked_transaction()?;
    for (i, entry) in snapshot.bindings.into_iter().enumerate() {
        let content = decode_content_as(&entry.content, false)?;
        let id = i as i64 + 1;
        tx.execute(
            "INSERT INTO binding_records VALUES(?1,?2,?3)",
            params![id, content.track_pid.to_be_bytes().as_slice(), content.path],
        )?;
        tx.execute(
            "INSERT INTO binding_versions VALUES(?1,1,?2)",
            params![id, entry.content],
        )?;
        tx.execute("INSERT INTO active_bindings VALUES(?1,1)", [id])?;
        if let Some(c) = entry.conversion {
            let relative = Path::new(&c.path);
            ensure!(
                !relative.is_absolute()
                    && relative
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                "unsafe converted source path"
            );
            tx.execute(
                "INSERT INTO binding_conversions VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    id,
                    root.join(relative).to_string_lossy(),
                    i64::try_from(c.bytes)?,
                    c.sha.as_slice(),
                    c.audio.as_slice(),
                    c.ffmpeg
                ],
            )?;
        }
        if let Some(source) = entry.source {
            let relative = Path::new(&source);
            ensure!(
                !relative.is_absolute()
                    && relative
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                "unsafe cloud source path"
            );
            tx.execute(
                "INSERT INTO binding_sources VALUES(?1,?2)",
                params![id, root.join(relative).to_string_lossy()],
            )?;
        }
    }
    for (namespace, value) in snapshot.reserved {
        tx.execute(
            "INSERT INTO reservations VALUES(?1,?2)",
            params![namespace, value],
        )?;
    }
    tx.commit()?;
    File::open(directory.join(DB_NAME))?.sync_all()?;
    fs::write(directory.join("cloud-import"),b"1")?;
    File::open(directory.join("cloud-import"))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::state::journal::test_support as support;
    #[test]
    fn portable_snapshot_retains_verified_audio_but_requires_fresh_pull() {
        let a = support::Temp::new();
        let mut journal = Journal::create(&a.path(), &support::identity()).unwrap();
        let content = support::alac_content(1);
        support::insert_verified(&mut journal, content.clone());
        drop(journal);
        let exported = export_cloud_snapshot(
            &a.path(),
            &a.0.join("export"),
            Path::new("/machine-a/music"),
        )
        .unwrap();
        let b = support::Temp::new();
        let target = b.0.join("cloud");
        import_cloud_snapshot(&exported, &target, Path::new("/machine-b/music")).unwrap();
        assert!(Journal::open_read_only(&target, &support::identity()).is_err());
        let prior = read_prior(&target, &b.0.join("inspect")).unwrap();
        assert_eq!(prior.bindings.len(), 1);
        assert!(prior.bindings[0].content == content);
        assert!(prior.unfinished.is_empty());
    }
    #[test]
    fn converted_audio_keeps_its_original_identity_across_roots() {
        let a = support::Temp::new();
        let mut journal = Journal::create(&a.path(), &support::identity()).unwrap();
        let conversion = Conversion {
            source_path: "/machine-a/music/Album/Track.flac".into(),
            source_bytes: 123,
            source_sha256: [5; 32],
            source_audio: [6; 32],
            ffmpeg: "ffmpeg fixture".into(),
        };
        support::converted_insert_verified(&mut journal, support::alac_content(1), conversion, 10);
        drop(journal);
        let exported = export_cloud_snapshot(
            &a.path(),
            &a.0.join("export"),
            Path::new("/machine-a/music"),
        )
        .unwrap();
        let b = support::Temp::new();
        let target = b.0.join("cloud");
        import_cloud_snapshot(&exported, &target, Path::new("/machine-b/music")).unwrap();
        let prior = read_prior(&target, &b.0.join("inspect")).unwrap();
        let conversion = prior.bindings[0].conversion.as_ref().unwrap();
        assert_eq!(
            conversion.source_path,
            Path::new("/machine-b/music/Album/Track.flac")
        );
        assert_eq!(conversion.source_audio, [6; 32]);
    }
    #[test]
    fn unsafe_paths_and_unfinished_snapshots_cannot_authorize_a_device_write() {
        let a = support::Temp::new();
        let mut journal = Journal::create(&a.path(), &support::identity()).unwrap();
        support::insert_verified(&mut journal, support::alac_content(1));
        drop(journal);
        let exported =
            export_cloud_snapshot(&a.path(), &a.0.join("export"), Path::new("/music")).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&exported).unwrap();
        value["bindings"][0]["source"] = "../escape".into();
        let b = support::Temp::new();
        assert!(
            import_cloud_snapshot(&value.to_string(), &b.0.join("unsafe"), Path::new("/music"))
                .is_err()
        );
        value["unfinished"] = serde_json::json!(["unfinished insert"]);
        assert!(
            import_cloud_snapshot(
                &value.to_string(),
                &b.0.join("pending"),
                Path::new("/music")
            )
            .is_err()
        );
        assert!(!b.0.join("pending").exists());
    }
}
