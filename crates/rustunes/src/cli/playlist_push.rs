//! One deliberately narrow M3U8 playlist creation trial. The audio was already
//! pushed. No track bytes, metadata or artwork are written. A durable Started
//! receipt is recorded before the first write and is NEVER replayed blindly.
//!
//! This is not the production sync/playlist journal or a general M3U8 importer.
use anyhow::{Context, Result, bail, ensure};
use music_protocol::catalogue::mutation;
use music_protocol::protocol::session::StartedOperation;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;

use crate::engine::{
    execution::{
        native_insert::{Control, create_trace, durable_write},
        removal_batch::{self, Before, Requests, Sent},
    },
    state::layout,
    verification::sql_tables::reader::Cell,
};

pub(super) struct Options<'a> {
    pub(super) playlist: &'a Path,
    pub(super) saved_plan: &'a Path,
    pub(super) name: &'a str,
    pub(super) state: &'a Path,
    pub(super) host_name: &'a str,
    pub(super) dry_run: bool,
    pub(super) yes: bool,
    pub(super) seconds: u64,
}

struct Permit {
    operation: Vec<u8>,
    signature: [u8; 21],
}
impl StartedOperation for Permit {
    fn operation(&self) -> &[u8] {
        &self.operation
    }
    fn signature(&self) -> &[u8; 21] {
        &self.signature
    }
}

fn hex(bytes: &[u8]) -> String {
    music_protocol::protocol::transfer::hex(bytes)
}

fn sha_file(path: &Path) -> Result<String> {
    ensure!(fs::metadata(path)?.len() <= 64 * 1024 * 1024, "source exceeds 64 MiB fixture limit");
    let mut reader = File::open(path)?;
    let mut buf = [0u8; 65536];
    let mut hash = Sha256::new();
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hex(&hash.finalize()))
}

fn entries(text: &str) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let path = PathBuf::from(line);
        ensure!(
            path.components().all(|component| matches!(component, Component::Normal(_))),
            "playlist entry must be a relative path without parent traversal: {line}"
        );
        ensure!(path.extension().and_then(|ext| ext.to_str()) == Some("mp3"),
            "this first playlist trial accepts only MP3 entries");
        ensure!(seen.insert(path.clone()), "duplicate playlist entry: {line}");
        found.push(path);
    }
    ensure!(found.len() == 10, "the first playlist trial requires exactly 10 unique tracks");
    Ok(found)
}

fn plan_rows(plan: &Value, playlist: &Path, paths: &[PathBuf]) -> Result<Vec<(u64, String)>> {
    ensure!(plan["phone_deletions"] == 0, "saved plan is not additive");
    ensure!(plan["execution_authorized_by_plan"] == false, "unexpected saved plan authorization");
    ensure!(plan["journal_blockers"].as_array().is_some_and(Vec::is_empty),
        "saved plan has journal blockers");
    let source_root = plan["source_root"].as_str().context("saved plan has no source_root")?;
    let root = fs::canonicalize(source_root).context("saved plan source directory has moved")?;
    let parent = playlist.parent().context("playlist has no directory")?;
    let sources = plan["sources"].as_array().context("saved plan has no source inventory")?;
    ensure!(sources.len() == paths.len(), "saved plan must cover exactly these 10 playlist tracks");

    let mut used_ids = BTreeSet::new();
    let mut ids = Vec::new();
    for entry in paths {
        let selected = fs::canonicalize(parent.join(entry))
            .with_context(|| format!("opening playlist entry {}", entry.display()))?;
        ensure!(selected.starts_with(&root), "playlist entry lies outside saved source root");
        let candidates: Vec<_> = sources.iter().filter(|source| {
            let Some(name) = source["relative_path"].as_str() else {
                return false;
            };
            fs::canonicalize(root.join(name)).is_ok_and(|p| p == selected)
        }).collect();
        ensure!(candidates.len() == 1, "playlist entry has no unique saved match: {}", entry.display());
        let source = candidates[0];
        ensure!(source["decision"] == "keep", "{} is not KEEP in saved plan", entry.display());
        let expected_file = source["source_file_sha256"].as_str().context("saved source hash missing")?;
        ensure!(sha_file(&selected)? == expected_file, "{} changed since dry-run", entry.display());
        let matches = source["matches"].as_array().context("saved matching evidence absent")?;
        ensure!(matches.len() == 1, "{} has ambiguous device matches", entry.display());
        let matched = &matches[0];
        ensure!(matched["ownership"] == "inserted_by_us"
            && matched["encoded_audio_equal"] == true
            && matched["match_basis"] == "owned_mpeg_frame_identity",
            "{} is not an unchanged Rustunes-owned MP3", entry.display());
        let id = u64::from_str_radix(
            matched["track_id"].as_str().context("track ID absent")?, 16
        )?;
        ensure!(used_ids.insert(id), "two playlist entries refer to one device track");
        let phone_sha = matched["device_file_sha256"].as_str()
            .context("saved plan lacks actual phone file SHA-256")?;
        ids.push((id, phone_sha.to_owned()));
    }
    Ok(ids)
}

fn qualify(before: &Before, name: &str, members: &[(u64, String)]) -> Result<()> {
    let containers = before.sql.table("container")?;
    for (row, multiplicity) in &containers.rows {
        ensure!(*multiplicity == 1, "duplicate existing container row");
        let existing = containers.at(row, "name")?.text()?;
        ensure!(!existing.eq_ignore_ascii_case(name), "playlist already exists: {name}; refusing duplicate");
    }
    let index = &before.snapshot.index;
    for (id, sha) in members {
        let found = index.other_audio().get(id)
            .with_context(|| format!("track {id:016x} is no longer installed as MP3"))?;
        ensure!(hex(&found.full_file_sha256) == *sha, "phone track {id:016x} changed since dry-run");
        ensure!(before.current.catalog.tracks().iter().any(|t| t.sync_id == *id),
            "track {id:016x} is absent from current catalogue");
    }
    Ok(())
}

fn fresh_pid(before: &Before) -> Result<u64> {
    let mut random = File::open("/dev/urandom")?;
    for _ in 0..64 {
        let mut buf = [0u8; 8];
        random.read_exact(&mut buf)?;
        let pid = u64::from_ne_bytes(buf) | (1u64 << 63);
        if pid != before.current.catalog.database_pid()
            && pid != before.current.catalog.primary_container_pid()
            && !before.current.catalog.represented_sync_ids().contains(&pid)
        {
            let containers = before.sql.table("container")?;
            if !containers.rows.keys().any(|row| {
                containers.at(row, "container_pid")
                    .and_then(|cell| cell.integer()).ok() == Some(pid as i64)
            }) {
                return Ok(pid);
            }
        }
    }
    bail!("could not allocate an unused playlist identity")
}

fn verify(after: &Before, before: &Before, pid: u64, name: &str, members: &[(u64, String)]) -> Result<()> {
    ensure!(
        before.snapshot.index.same_files(&after.snapshot.index),
        "the iPhone audio inventory changed during playlist creation"
    );
    ensure!(
        before.current.catalog.tracks().len() == after.current.catalog.tracks().len(),
        "track count changed during playlist creation"
    );
    ensure!(
        after.anchor() == before.anchor().checked_add(1).context("anchor overflow")?,
        "the iPhone media anchor did not advance exactly once"
    );
    let container = after.sql.table("container")?;
    let row = container.one("container_pid", pid as i64)?;
    ensure!(container.at(row, "name")?.text()? == name
        && container.at(row, "sync_id")?.integer()? == pid as i64,
        "created playlist name or identity differs");

    let member_table = after.sql.table("container_item")?;
    let mut observed = Vec::new();
    for (row, n) in &member_table.rows {
        if member_table.at(row, "container_pid")?.integer()? == pid as i64 {
            ensure!(*n == 1, "duplicate playlist member");
            observed.push((
                member_table.at(row, "position")?.integer()?,
                member_table.at(row, "item_pid")?.integer()?,
            ));
        }
    }
    observed.sort_unstable();
    let expected: Vec<_> = members.iter().enumerate()
        .map(|(position, (id, _))| Ok((position as i64, before.item_pid(*id)?)))
        .collect::<Result<_>>()?;
    ensure!(observed == expected, "playlist membership or ordering differs from M3U8");
    Ok(())
}

pub(super) async fn run(o: Options<'_>) -> Result<()> {
    ensure!(!o.name.trim().is_empty() && o.name.len() <= 255
        && !o.name.chars().any(char::is_control), "invalid playlist name");
    ensure!(o.dry_run || o.yes,
        "playlist creation writes to the iPhone; use --dry-run or explicitly pass --yes");
    ensure!(fs::metadata(o.playlist)?.len() <= 1024 * 1024, "M3U8 is too large");
    ensure!(fs::metadata(o.saved_plan)?.len() <= 16 * 1024 * 1024, "saved plan is too large");
    let playlist_text = fs::read_to_string(o.playlist).context("reading playlist M3U8")?;
    let selected = entries(&playlist_text)?;
    let plan: Value = serde_json::from_slice(&fs::read(o.saved_plan)?)
        .context("reading saved dry-run plan")?;
    let members = plan_rows(&plan, o.playlist, &selected)?;

    layout::private_directory(o.state)?;
    let tests = o.state.join("playlist-trials");
    layout::private_directory(&tests)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let root = tests.join(format!("{now}-{}", std::process::id()));
    create_trace(&root)?;

    let control = Control {
        deadline: Instant::now() + Duration::from_secs(o.seconds),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let (identity, before) = removal_batch::capture_fresh(&root.join("before"), &control).await?;
    ensure!(hex(&identity.device_digest()) == plan["device_sha256"].as_str().unwrap_or(""),
        "the saved plan belongs to another device");
    ensure!(hex(&identity.library_digest()) == plan["library_sha256"].as_str().unwrap_or(""),
        "the saved plan belongs to another music library");
    qualify(&before, o.name, &members)?;

    if o.dry_run {
        println!("Playlist dry-run: {:?} → {} existing tracks, correct M3U8 order; 0 audio transfers", o.name, members.len());
        println!("No phone writes. Evidence: {}", root.display());
        return Ok(());
    }

    let pid = fresh_pid(&before)?;
    let operation = mutation::insert_playlist(
        &before.current.catalog,
        pid,
        o.name,
        &members.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        SystemTime::now(),
    )?;
    let input = serde_json::to_vec_pretty(&json!({
        "schema":"rustunes.playlist-trial.v1",
        "name":o.name,
        "pid":format!("{pid:016x}"),
        "base_anchor":before.anchor(),
        "source":o.playlist,
        "saved_plan":o.saved_plan,
        "members":members.iter().map(|(id, _)| format!("{id:016x}")).collect::<Vec<_>>(),
        "operation_sha256":hex(&Sha256::digest(&operation)),
        "before_audio_index_sha256":hex(&before.snapshot.index_receipt_sha256),
    }))?;
    durable_write(&root, "input.json", &input)?;
    durable_write(&root, "operation.plist", &operation)?;
    let receipt: [u8;32] = Sha256::digest(&input).into();
    println!("Creating playlist {:?} ({} already-installed tracks).",o.name,members.len());
    let send = root.join("send");
    let operation_copy = operation.clone();
    let result = removal_batch::send_metadata_only(
        &before,
        &operation,
        Requests::None,
        receipt,
        o.host_name,
        &send,
        &control,
        |ready, signature| {
            durable_write(&root, "started.json", &serde_json::to_vec_pretty(&json!({
                "status":"started_never_replay",
                "pid":format!("{pid:016x}"),
                "ready_sha256":hex(&ready),
                "operation_sha256":hex(&Sha256::digest(&operation_copy)),
                "signature_sha256":hex(&Sha256::digest(signature)),
            }))?)?;
            Ok(Permit { operation:operation_copy, signature })
        },
    ).await;
    match result {
        Sent::Unsent(error) => return Err(error.context(format!(
            "nothing was written to the phone; evidence: {}", root.display()
        ))),
        Sent::Started(Err(error)) => bail!(
            "playlist session started but outcome uncertain ({error:#}); DO NOT retry. Inspect with rustunes list. Evidence: {}",
            root.display()
        ),
        Sent::Started(Ok(_)) => {}
    }
    let (after_identity, after) = removal_batch::capture_fresh(&root.join("after"), &control)
        .await.context("playlist session finished but verification capture failed; do not retry blindly")?;
    ensure!(after_identity.device_digest() == identity.device_digest()
        && after_identity.library_digest() == identity.library_digest(),
        "verification saw another device/library");
    verify(&after, &before, pid, o.name, &members)
        .with_context(|| format!("playlist outcome requires review; never retry blindly. Evidence: {}", root.display()))?;
    durable_write(&root, "verified.json", &serde_json::to_vec_pretty(&json!({
        "status":"verified",
        "playlist_id":format!("{pid:016x}"),
        "name":o.name,
        "tracks":members.len(),
        "anchor":after.anchor(),
        "audio_unchanged":true,
        "ordered_membership_verified":true,
    }))?)?;
    println!("Verified playlist {:?}: {} tracks in M3U8 order. No audio retransferred.",o.name,members.len());
    println!("Evidence: {}", root.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_only_safe_relative_mp3_entries() {
        let text = "#EXTM3U\n".to_string() +
            &(1..=10).map(|n| format!("#EXTINF:5,Track {n}\ngenerated/Flurb-Test-{n:02}.mp3\n"))
                .collect::<String>();
        assert_eq!(entries(&text).unwrap().len(), 10);
        assert!(entries(&text.replace("generated/Flurb-Test-01.mp3", "../escape.mp3")).is_err());
        assert!(entries(&text.replace("generated/Flurb-Test-01.mp3", "/tmp/escape.mp3")).is_err());
        assert!(entries(&text.replace("generated/Flurb-Test-10.mp3", "generated/Flurb-Test-01.mp3")).is_err());
    }
}
