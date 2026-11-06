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
    collections::{BTreeMap, BTreeSet},
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
    state::{journal::{Binding, Identity, Journal}, layout},
};

pub(super) struct Options<'a> {
    pub(super) playlist: &'a Path,
    pub(super) name: &'a str,
    pub(super) state: &'a Path,
    pub(super) host_name: &'a str,
    pub(super) dry_run: bool,
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
    ensure!(!found.is_empty() && found.len() <= 499,
        "a playlist must contain 1..=499 distinct MP3 tracks");
    Ok(found)
}

/// The index represents MPEG-1 Layer III as a frame layout, but MPEG-2
/// and MPEG-2.5 Layer III as an EncodedAudio record. A byte-identical
/// file must also have the same independently decoded codec/layout.
fn same_mp3_codec(
    source: &music_protocol::media::mpeg::Source,
    codec: &crate::engine::state::audio_index::OtherCodec,
) -> bool {
    use crate::engine::state::audio_index::OtherCodec;
    match codec {
        OtherCodec::Encoded(encoded) => source
            .stream
            .as_ref()
            .is_some_and(|stream| stream.encoded == *encoded),
        OtherCodec::Mpeg1Layer3 {
            sample_rate,
            channels,
            frame_count,
            audio_offset,
            audio_bytes,
            trailing_tag_bytes,
        } => {
            let audio = &source.fingerprint.audio;
            source.stream.is_none()
                && *sample_rate == audio.sample_rate
                && *channels == audio.channels
                && *frame_count == audio.frame_count
                && *audio_offset == source.audio_offset
                && *audio_bytes == audio.audio_bytes
                && *trailing_tag_bytes == source.trailing_tag_bytes
        }
    }
}

/// Active journal bindings are the authoritative source-to-track associations,
/// not a temporary planning result or a guess based on title or filename.
struct JournalMatches {
    sources: BTreeMap<u64, PathBuf>,
    bindings: Vec<Binding>,
}

fn journal_matches(state: &Path, before: &Before, identity: &Identity) -> Result<Option<JournalMatches>> {
    let path = layout::journal_path(
        state,
        &before.snapshot.identity.device_identifier_sha256,
        &before.snapshot.identity.library_identity_sha256,
    )?;
    if !path.join("journal.sqlite").try_exists()? {
        return Ok(None);
    }
    let journal = Journal::open_read_only(&path, identity)
        .with_context(|| format!("reading active Rustunes journal {}", path.display()))?;
    journal.ensure_identity(identity)?;
    let sources = journal.recorded_sources()?;
    let bindings = journal.active_verified_bindings()?;
    journal.recheck_read_only_source()?;
    Ok(Some(JournalMatches { sources, bindings }))
}

/// Resolve a verified track ID from the journal. Prefer an explicitly recorded
/// source path; if it is absent, a unique audio identity may resolve the same
/// verified binding. Every candidate must still hold exactly the journaled
/// file on this particular phone. Do not trust historical IDs without readback.
fn journal_track(
    path: &Path,
    source: &music_protocol::media::fingerprint::TransferredSource,
    journal: &JournalMatches,
    before: &Before,
) -> Result<Option<(u64, &'static str)>> {
    let installed = &before.snapshot.index;
    let in_catalog = |id| before.current.catalog.tracks().iter().any(|t| t.sync_id == id);
    let bound: Vec<_> = journal.bindings.iter()
        .filter(|b| {
            b.content.source.audio.sha256 == source.audio.sha256
                && !b.content.source.is_alac()
                && installed.other_audio().contains_key(&b.content.track_pid)
                && in_catalog(b.content.track_pid)
                && installed.matches_transferred_source(
                    b.content.track_pid, &b.content.path, &b.content.source
                )
        })
        .collect();
    let path_candidates: Vec<u64> = journal.sources.iter()
        .filter(|(_, recorded)| recorded.as_path() == path)
        .map(|(id, _)| *id)
        .collect();
    ensure!(
        path_candidates.len() <= 1,
        "multiple journal track IDs are recorded for {}", path.display()
    );
    if let Some(id) = path_candidates.first() {
        ensure!(
            bound.iter().any(|b| b.content.track_pid == *id),
            "journal track {id:016x} for {} no longer holds its verified audio; refusing to use a stale track ID",
            path.display()
        );
        return Ok(Some((*id, "journal-source")));
    }
    ensure!(
        bound.len() <= 1,
        "multiple active journal track IDs match the audio of {}; cannot choose a member",
        path.display()
    );
    Ok(bound.first().map(|b| (b.content.track_pid, "journal-audio")))
}

/// Read each entry in M3U8 order. Prefer the exact active journal track ID;
/// only if no verified journal association exists, consider a unique exact
/// file match from the current device index. No stale plan is consulted.

fn resolve_members(
    playlist: &Path,
    paths: &[PathBuf],
    before: &Before,
    journal: Option<&JournalMatches>,
) -> Result<Vec<(u64, String)>> {
    let root = fs::canonicalize(playlist.parent().unwrap_or(Path::new(".")))
        .context("opening playlist folder")?;
    let installed = before.snapshot.index.other_audio();
    let mut used = BTreeSet::new();
    let mut members = Vec::with_capacity(paths.len());
    for relative in paths {
        let path = fs::canonicalize(root.join(relative))
            .with_context(|| format!("opening playlist track {}", relative.display()))?;
        ensure!(
            path.starts_with(&root),
            "{} resolves outside the playlist folder",
            relative.display()
        );
        let source = music_protocol::media::mpeg::read(&path)
            .with_context(|| format!("inspecting playlist MP3 {}", relative.display()))?;
        ensure!(
            matches!(
                source.format(),
                music_protocol::media::format::Format::Mpeg1Layer3
                    | music_protocol::media::format::Format::Mpeg2Layer3
                    | music_protocol::media::format::Format::Mpeg25Layer3
            ),
            "{} is not MPEG Layer III audio",
            relative.display()
        );
        let source_identity = source.transferred_identity()?;
        let binding = journal
            .map(|journal| journal_track(&path, &source_identity, journal, before))
            .transpose()?
            .flatten();
        let (id, basis) = match binding {
            Some(found) => found,
            None => {
                // No usable journal association: exact file+codec identity only.
                let matches: Vec<u64> = installed
                    .iter()
                    .filter(|(_, audio)| {
                        audio.file_bytes == source.fingerprint.file_bytes
                            && hex(&audio.full_file_sha256)
                                == source.fingerprint.full_file_sha256
                            && same_mp3_codec(&source, &audio.codec)
                    })
                    .map(|(id, _)| *id)
                    .collect();
                ensure!(
                    matches.len() == 1,
                    "{}: no verified journal track ID and {} exact on-device file matches. Push the missing audio first or resolve duplicates.",
                    relative.display(), matches.len()
                );
                (matches[0], "exact-file-fallback")
            }
        };
        let phone = installed.get(&id)
            .with_context(|| format!("resolved track {id:016x} has no installed MP3 audio"))?;
        let file_sha = hex(&phone.full_file_sha256);
        println!(
            "MATCH \"{}\" -> track {id:016x} ({basis})",
            relative.display()
        );
        ensure!(
            before.current.catalog.tracks().iter().any(|track| track.sync_id == id),
            "track {id:016x} has audio but no current catalogue registration"
        );
        ensure!(
            used.insert(id),
            "multiple M3U8 entries resolve to the same installed track {id:016x}"
        );
        members.push((id, file_sha));
    }
    Ok(members)
}

fn qualify(before: &Before, name: &str, members: &[(u64, String)]) -> Result<()> {
    let containers = before.sql.table("container")?;
    for (row, multiplicity) in &containers.rows {
        ensure!(*multiplicity == 1, "duplicate existing container row");
        if let Ok(existing) = containers.at(row, "name")?.text() {
            ensure!(!existing.eq_ignore_ascii_case(name),
                "playlist already exists: {name}; refusing duplicate");
        }
    }
    let index = &before.snapshot.index;
    for (id, sha) in members {
        let found = index.other_audio().get(id)
            .with_context(|| format!("track {id:016x} is no longer installed as MP3"))?;
        ensure!(hex(&found.full_file_sha256) == *sha, "phone track {id:016x} changed during matching");
        ensure!(before.current.catalog.tracks().iter().any(|t| t.sync_id == *id),
            "track {id:016x} is absent from current catalogue");
    }
    Ok(())
}

/// A previous Started trial might have created its playlist even if the
/// verification connection failed. Never queue a second creation on that
/// device/library until the first result has been investigated.
fn refuse_unsettled_trial(
    trials: &Path,
    device_sha: &str,
    library_sha: &str,
) -> Result<()> {
    for entry in fs::read_dir(trials)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        if !path.join("started.json").exists() || path.join("verified.json").exists() {
            continue;
        }
        let contents = fs::read(path.join("input.json"))
            .with_context(|| format!("reading unfinished trial {}", path.display()))?;
        let prior: Value = serde_json::from_slice(&contents)
            .with_context(|| format!("reading unfinished trial {}", path.display()))?;
        if prior["device_sha256"] == device_sha && prior["library_sha256"] == library_sha {
            bail!(
                "a previous playlist creation has not been verified: {}. Inspect the phone before any new playlist creation; never retry an uncertain write",
                path.display()
            );
        }
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
    ensure!(fs::metadata(o.playlist)?.len() <= 1024 * 1024, "M3U8 is too large");
    let playlist_text = fs::read_to_string(o.playlist).context("reading playlist M3U8")?;
    let selected = entries(&playlist_text)?;

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
    let journal = journal_matches(o.state, &before, &identity)?;
    let members = resolve_members(o.playlist, &selected, &before, journal.as_ref())?;
    qualify(&before, o.name, &members)?;

    if !o.dry_run {
        refuse_unsettled_trial(
            &tests,
            &hex(&identity.device_digest()),
            &hex(&identity.library_digest()),
        )?;
    }
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
        "device_sha256":hex(&identity.device_digest()),
        "library_sha256":hex(&identity.library_digest()),
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
    fn fixture_mp3_classification_matches_current_phone_index_shape() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/playlist-sync/generated/Flurb-Test-01.mp3");
        let source = music_protocol::media::mpeg::read(&path).unwrap();
        let (bytes, hash, codec) = music_protocol::media::other_audio::read(&path).unwrap();
        assert_eq!(bytes, source.fingerprint.file_bytes);
        assert_eq!(hex(&hash), source.fingerprint.full_file_sha256);
        assert!(same_mp3_codec(&source, &codec), "MP3 codecs differ for exact same fixture");
    }

    #[test]
    fn committed_playlist_has_ten_existing_mp3_files() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/playlist-sync");
        let playlist = root.join("playlist.m3u8");
        let list = entries(&fs::read_to_string(&playlist).unwrap()).unwrap();
        assert_eq!(list.len(), 10);
        let mut digests = BTreeSet::new();
        for entry in &list {
            let path = root.join(entry);
            assert!(path.is_file(), "missing fixture {}", path.display());
            assert!(digests.insert(sha_file(&path).unwrap()), "fixture files duplicate");
        }
    }

    #[test]
    fn allows_nonempty_playlist_of_up_to_499_entries() {
        assert!(entries("one.mp3\n").is_ok());
        assert!(entries("").is_err());
        let many = (1..=500)
            .map(|n| format!("track-{n}.mp3\n"))
            .collect::<String>();
        assert!(entries(&many).is_err());
        assert!(entries(&many.lines().take(499).collect::<Vec<_>>().join("\n")).is_ok());
    }

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
