//! Apply one source cover to its already verified journal track. Audio is not
//! retransferred. Completion requires independent SQL and permanent-image proof.
use std::{
    fs::{self, File},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::Path,
    sync::Arc,
    time::SystemTime,
};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::{
    engine::execution::device,
    engine::execution::native_artwork::{self, ArtworkUpdate, FreshArtworkState},
    engine::state::current::{self, hex},
    engine::state::journal::{
        Artwork, AttemptKind, AttemptState, CompletionEvidence, Identity, Journal, PreparedAttempt,
    },
    engine::state::snapshot,
    engine::{CoverResult, Engine, Event},
};
use music_protocol::catalogue::artwork_update;
use music_protocol::catalogue::artwork_verification;
use music_protocol::catalogue::identity;
use music_protocol::media::artwork;
use music_protocol::media::fingerprint;
use music_protocol::protocol::trace::private_write;

#[derive(Clone, Copy)]
pub struct Options<'a> {
    pub file: &'a Path,
    pub snapshot: &'a Path,
    pub journal: &'a Path,
    pub trace: &'a Path,
    pub host_name: &'a str,
    pub dry_run: bool,
    /// When invoked by sync, bind the cover to the exact reviewed source file.
    pub expected_source: Option<&'a fingerprint::TransferredSource>,
    /// An album present before an add-only sync must keep its existing cover bytes.
    pub preserve_album: Option<u64>,
    /// Add-only may finish only an owned first cover, never replace a cover
    /// introduced after the saved plan.
    pub pending_cover_only: bool,
}

pub(crate) async fn run(options: Options<'_>, engine: &Engine) -> Result<CoverResult> {
    run_with_verified_current(options, engine, None)
        .await
        .map(|(result, _)| result)
}

/// Constructed only after SQL and permanent-image verification.
/// Audio identity is inherited from the prior verified binding, with its
/// stable file attributes held across the artwork-only session.
/// The next operation still rechecks all captured state before any write.
struct VerifiedCurrent {
    current: current::Current,
    directory: std::path::PathBuf,
}
async fn run_with_verified_current(
    options: Options<'_>,
    engine: &Engine,
    prior: Option<VerifiedCurrent>,
) -> Result<(CoverResult, Option<VerifiedCurrent>)> {
    engine.check()?;
    let started = std::time::Instant::now();
    ensure!(
        !options.trace.try_exists()?,
        "artwork trace directory must be new"
    );
    let saved = snapshot::open(options.snapshot)?;
    let library = identity::parse(&saved.raw_preferences)?;
    let identity = Identity::for_saved_device(saved.identity.device_identifier_sha256, &library)?;
    let mut journal = Journal::open(options.journal, &identity)?;
    if let Some(attempt) = journal.outstanding()? {
        if attempt.state == AttemptState::Prepared && attempt.prepared.kind == AttemptKind::Artwork
        {
            journal.cancel_prepared(attempt.id)?;
        }
    }
    ensure!(
        journal.outstanding()?.is_none(),
        "unfinished journal operation requires reconciliation before an artwork update"
    );
    let (desired, cover) = read_source(options.file)?;
    if let Some(expected) = options.expected_source {
        ensure!(
            &desired == expected,
            "source changed after sync planning; cover update refused"
        );
    }
    let binding = journal
        .lookup_active_verified(&desired.audio)?
        .context("source has no active verified journal binding; verify or sync its audio first")?;
    let cover = cover.context("source contains no supported embedded front cover")?;
    require_decoder()?;
    fs::DirBuilder::new().mode(0o700).create(options.trace)?;
    File::open(
        options
            .trace
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()?;
    let before_started = std::time::Instant::now();
    let current = if let Some(prior) = prior {
        ensure!(
            prior.current.identity.device_digest() == identity.device_digest()
                && prior.current.identity.library_digest() == identity.library_digest(),
            "verified artwork capture belongs to another device or library"
        );
        super::removal_batch::copy_snapshot(&prior.directory, &options.trace.join("before"))?;
        prior.current
    } else {
        let capture_control = engine.control(120);
        current::capture_with_artwork_controlled(
            &options.trace.join("before"),
            &identity,
            capture_control.deadline,
            Arc::clone(&capture_control.cancelled),
        )
        .await?
    };
    let before_ms = before_started.elapsed().as_millis();
    let artwork_db = current
        .artwork_db
        .as_deref()
        .context("artwork database capture absent")?;
    let target = current
        .catalog
        .tracks()
        .iter()
        .find(|t| t.sync_id == binding.content.track_pid)
        .context("verified track is absent from current catalogue")?;
    ensure!(
        target.afc_path.as_deref() == Some(binding.content.path.as_str())
            && target.metadata.file_size == binding.content.source.file_bytes,
        "verified target path or audio size changed"
    );
    if options.pending_cover_only {
        ensure!(
            crate::engine::planning::additive::owes_cover(
                &current.catalog,
                target.sync_id,
                &desired.require_alac()?,
                &binding,
                false
            ),
            "add-only pending cover changed after planning; existing artwork is preserved"
        );
    }
    if let Some(existing) = binding
        .content
        .artwork
        .as_ref()
        .filter(|art| art.sha256 == cover.sha256 && art.bytes == cover.bytes.len() as u64)
        && current
            .catalog
            .artwork()
            .track_cache_ids()
            .get(&target.sync_id)
            == Some(&existing.cache_id)
    {
        let proof = crate::engine::verification::artwork::verify_track(
            &current,
            target.sync_id,
            existing.cache_id,
            &cover,
            &options.trace.join("existing-artwork"),
            &engine.control(120),
            device::provider,
        )
        .await?;
        private_write(
            &options.trace.join("unchanged.json"),
            &serde_json::to_vec_pretty(&proof)?,
        )?;
        File::open(options.trace)?.sync_all()?;
        engine.emit(Event::CoverFinished(CoverResult::Unchanged))?;
        return Ok((
            CoverResult::Unchanged,
            Some(VerifiedCurrent {
                current,
                directory: options.trace.join("before"),
            }),
        ));
    }
    let preserved_album = if let Some(album) = options.preserve_album {
        ensure!(
            target.album_sync_id == album,
            "protected album identity changed after planning"
        );
        Some(crate::engine::verification::artwork::verify_album_original(
            &current,album,&cover,&options.trace.join("preserved-album"),
            &engine.control(120),
            device::provider,
        ).await.context("add-only cover refused: existing album image could not be proven identical; verified audio is retained")?)
    } else {
        None
    };
    let cache = native_artwork::allocate_cache_id(&current.catalog, artwork_db, &journal)?;
    let operation = artwork_update::prepare_for_format(
        &current.catalog,
        target.sync_id,
        cache,
        &cover,
        SystemTime::now(),
        binding.content.source.format(),
    )?;
    let possible_recent_resets =
        artwork_verification::possible_recent_play_count_resets(&current.catalog);
    let mut input = serde_json::json!({"schema":"rustunes.artwork-input","device_sha256":hex(&identity.device_digest()),
        "library_sha256":hex(&identity.library_digest()),"current_state_receipt_sha256":hex(&current.receipt_sha256),
        "track_id":format!("{:016x}",target.sync_id),"album_id":format!("{:016x}",target.album_sync_id),"cache_id":cache,
        "desired_source_sha256":hex(&desired.full_file_sha256),"transferred_audio_sha256":hex(&binding.content.source.full_file_sha256),
        "cover_sha256":hex(&cover.sha256),"cover_bytes":cover.bytes.len(),"operation_sha256":hex(&Sha256::digest(&operation)),
        "media_revision":current.catalog.persisted_media_anchor()+1,"audio_retransfer":false,"phone_deletions":0,
        "artwork_verification":"exact_track_source_and_decoded_derivative",
        "album_policy":if preserved_album.is_some(){"preserve_existing_original_bytes"}else{"native_representative_new_or_prior_cache"},
        "preserved_album_original":preserved_album,
        "possible_recent_play_count_import_resets":possible_recent_resets,
        "recent_play_count_import_rule":artwork_verification::RECENT_PLAY_IMPORT_RULE});
    let possible_skip_resets =
        artwork_verification::possible_recent_skip_count_resets(&current.catalog);
    if !possible_skip_resets.is_empty() {
        input["possible_recent_skip_count_import_resets"] =
            serde_json::to_value(&possible_skip_resets)?;
        input["recent_count_import_rule"] =
            serde_json::json!(artwork_verification::RECENT_COUNT_IMPORT_RULE);
    }
    let bytes = serde_json::to_vec_pretty(&input)?;
    private_write(&options.trace.join("input.json"), &bytes)?;
    File::open(options.trace)?.sync_all()?;
    if !possible_skip_resets.is_empty() || !possible_recent_resets.is_empty() {
        engine.emit(Event::ArtworkRecentCountResets {
            plays: &possible_recent_resets,
            skips: &possible_skip_resets,
        })?;
    }
    if options.dry_run {
        let result = CoverResult::Planned {
            track_id: target.sync_id,
            cache_id: cache,
            bytes: cover.bytes.len(),
        };
        engine.emit(Event::CoverFinished(result))?;
        return Ok((result, None));
    }
    let input_hash = Sha256::digest(&bytes).into();
    let state = FreshArtworkState::new(
        &current.device,
        &current.catalog,
        &current.raw_preferences,
        &current.plist_preferences,
        artwork_db,
        &current.cdb,
        input_hash,
    )?;
    let mut content = binding.content.clone();
    content.artwork = Some(Artwork {
        cache_id: cache,
        bytes: cover.bytes.len() as u64,
        sha256: cover.sha256,
    });
    let track_id = content.track_pid;
    let attempt = journal.prepare(PreparedAttempt {
        kind: AttemptKind::Artwork,
        content,
        new_album_pid: None,
        new_artist_pid: None,
        current_media_anchor: current.catalog.persisted_media_anchor(),
        operation,
        input_receipt_sha256: input_hash,
    })?;
    engine.emit(Event::ArtworkStarted { track_id })?;
    let session_started = std::time::Instant::now();
    let pending = match native_artwork::execute(
        &mut journal,
        ArtworkUpdate {
            attempt,
            source: options.file,
            desired_source: &desired,
            state: &state,
            host_name: options.host_name,
            trace: &options.trace.join("transfer"),
        },
        &engine.control(180),
        device::provider,
    )
    .await
    {
        Ok(pending) => pending,
        Err(error) => {
            // Prepared means the durable send gate was never crossed. Started
            // attempts always retain their evidence for independent recovery.
            if journal.attempt(attempt)?.state == AttemptState::Prepared {
                journal.cancel_prepared(attempt)?;
            }
            return Err(error).context("artwork update did not reach independent verification");
        }
    };
    let session_ms = session_started.elapsed().as_millis();
    let after_started = std::time::Instant::now();
    let after_control = engine.control(120);
    let after = current::capture_with_artwork_controlled(
        &options.trace.join("after"),
        &identity,
        after_control.deadline,
        Arc::clone(&after_control.cancelled),
    )
    .await?;
    let after_ms = after_started.elapsed().as_millis();
    let artwork_started = std::time::Instant::now();
    let catalog_proof = artwork_verification::verify_independent_representative(
        &current.catalog,
        &after.catalog,
        track_id,
        cache,
    )?;
    let file_proof = crate::engine::verification::artwork::verify_track_after_update(
        &after,
        track_id,
        cache,
        &cover,
        &options.trace.join("permanent-artwork"),
        &engine.control(120),
        device::provider,
    )
    .await?;
    let protected_album_after = if let Some(album) = options.preserve_album {
        Some(
            crate::engine::verification::artwork::verify_album_original(
                &after,
                album,
                &cover,
                &options.trace.join("preserved-album-after"),
                &engine.control(120),
                device::provider,
            )
            .await?,
        )
    } else {
        None
    };
    let receipt = serde_json::json!({"schema":"rustunes.verified-artwork","catalogue":catalog_proof,"permanent_files":file_proof,
        "preserved_album_original_after":protected_album_after,
        "transfer_receipt_sha256":hex(&pending.evidence.receipt_sha256),"after_state_receipt_sha256":hex(&after.receipt_sha256),
        "preserved_audio_sha256":hex(&pending.evidence.permanent_audio_sha256),
        "audio_verification":"previously_verified_container_with_unchanged_attributes",
        "audio_attributes":pending.evidence.audio_attributes,"staged_jpeg_sha256":hex(&pending.evidence.staged_cover_sha256),
        "timing_ms":{"before_capture":before_ms,"native_session":session_ms,"after_capture":after_ms,
            "artwork_readback":artwork_started.elapsed().as_millis(),"total":started.elapsed().as_millis()},
        "phone_deletions":0});
    let bytes = serde_json::to_vec_pretty(&receipt)?;
    private_write(&options.trace.join("verified.json"), &bytes)?;
    File::open(options.trace)?.sync_all()?;
    journal.complete(
        pending.permit,
        CompletionEvidence {
            track_pid: track_id,
            path: binding.content.path,
            audio_bytes: binding.content.source.file_bytes,
            audio_file_sha256: pending.evidence.permanent_audio_sha256,
            artwork_sha256: Some(cover.sha256),
            media_anchor: after.catalog.persisted_media_anchor(),
            receipt_sha256: Sha256::digest(&bytes).into(),
        },
    )?;
    engine.emit(Event::CoverFinished(CoverResult::Verified))?;
    Ok((
        CoverResult::Verified,
        Some(VerifiedCurrent {
            current: after,
            directory: options.trace.join("after"),
        }),
    ))
}

/// Fail before any transfer when the required image decoder is unavailable.
pub fn require_decoder() -> Result<()> {
    let available = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| {
            fs::metadata(directory.join("ffmpeg"))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    });
    ensure!(
        available,
        "FFmpeg is required for permanent artwork verification; install ffmpeg before syncing covers"
    );
    Ok(())
}

/// Read the exact desired container and its normalized native front cover.
/// Audio identity remains codec-specific; tags and cover bytes never establish it.
pub(crate) fn read_source(
    path: &Path,
) -> Result<(fingerprint::TransferredSource, Option<artwork::Cover>)> {
    if let Ok(source) = fingerprint::read(path) {
        let art = artwork::read(path)?;
        ensure!(
            art.source_size == source.file_bytes && art.source_sha256 == source.full_file_sha256,
            "source changed during artwork inspection"
        );
        return Ok((source.into(), art.cover));
    }
    let source = crate::engine::planning::source_reads::mpeg(path)?;
    let identity = source.transferred_identity()?;
    let cover = music_protocol::media::mpeg::read_front_cover(path, &source)?
        .map(|picture| {
            artwork::sent_cover(picture.bytes(), artwork::checksum_header(source.format()))
        })
        .transpose()?;
    Ok((identity, cover))
}

/// Only changed covers of exact, verified managed audio are sent. The normal
/// cover operation rechecks live state and journals/verifies each write.
pub(crate) async fn changed(
    snapshot_path: &Path,
    journal_path: &Path,
    files: &[std::path::PathBuf],
    trace: &Path,
    host_name: &str,
    engine: &Engine,
) -> Result<usize> {
    if files.is_empty() {
        return Ok(0);
    }
    let saved = snapshot::open(snapshot_path)?;
    let library = identity::parse(&saved.raw_preferences)?;
    let identity = Identity::for_saved_device(saved.identity.device_identifier_sha256, &library)?;
    let mut changed = 0;
    let mut seen = std::collections::BTreeSet::new();
    let mut previous = None;
    for file in files {
        engine.check()?;
        let (desired, cover) = read_source(file)?;
        let Some(cover) = cover else {
            continue;
        };
        let journal = Journal::open_read_only(journal_path, &identity)?;
        let Some(binding) = journal.lookup_active_verified(&desired.audio)? else {
            continue;
        };
        if !cover_changed(&binding.content, &cover) || !seen.insert(binding.content.track_pid) {
            continue;
        }
        drop(journal);
        crate::engine::state::layout::private_directory(trace)?;
        engine.emit(Event::CheckingCover {
            number: changed + 1,
            total: files.len(),
        })?;
        let directory = trace.join(format!("cover-{:04}", changed + 1));
        let options = Options {
            file,
            snapshot: snapshot_path,
            journal: journal_path,
            trace: &directory,
            host_name,
            dry_run: false,
            expected_source: Some(&desired),
            preserve_album: None,
            pending_cover_only: false,
        };
        let reused = previous.is_some();
        let outcome = run_with_verified_current(options, engine, previous.take()).await;
        let refreshed = trace.join(format!("cover-{:04}-refresh", changed + 1));
        let (_, next) = match outcome {
            Err(error)
                if reused
                    && super::native_insert::catalog_read_drift(&error)
                    && Journal::open_read_only(journal_path, &identity)?
                        .outstanding()?
                        .is_none() =>
            {
                // The unsent Prepared attempt was durably cancelled. A changed
                // phone gets one genuinely fresh capture, never a guard bypass.
                run_with_verified_current(
                    Options {
                        trace: &refreshed,
                        ..options
                    },
                    engine,
                    None,
                )
                .await
            }
            result => result,
        }
        .with_context(|| {
            format!(
                "updating artwork for {}",
                file.file_name().unwrap_or_default().to_string_lossy()
            )
        })?;
        previous = next;
        changed += 1;
    }
    Ok(changed)
}

fn cover_changed(content: &crate::engine::state::journal::Content, cover: &artwork::Cover) -> bool {
    content
        .artwork
        .as_ref()
        .is_none_or(|old| old.sha256 != cover.sha256 || old.bytes != cover.bytes.len() as u64)
}

#[cfg(test)]
mod changed_tests {
    use super::*;
    use crate::engine::state::journal::{Artwork, Content};
    fn mp3(path: &Path, jpeg: &[u8]) {
        let picture = [b"\0image/jpeg\0\x03\0".as_slice(), jpeg].concat();
        let frame = [
            b"APIC".as_slice(),
            &(picture.len() as u32).to_be_bytes(),
            &[0, 0],
            &picture,
        ]
        .concat();
        let n = frame.len();
        let size = [
            ((n >> 21) & 127) as u8,
            ((n >> 14) & 127) as u8,
            ((n >> 7) & 127) as u8,
            (n & 127) as u8,
        ];
        fs::write(
            path,
            [
                b"ID3\x03\0\0".as_slice(),
                &size,
                &frame,
                &music_protocol::test_support::mpeg::audio(),
            ]
            .concat(),
        )
        .unwrap();
    }
    #[test]
    fn changed_mp3_cover_keeps_audio_identity_and_skips_unchanged_jpeg() {
        let root = std::env::temp_dir().join(format!(
            "cover-change-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let first = root.join("first.mp3");
        let second = root.join("second.mp3");
        let jpeg = |color: &str| {
            let output = std::process::Command::new("ffmpeg")
                .env_remove("FFREPORT")
                .args([
                    "-nostdin",
                    "-hide_banner",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    color,
                    "-frames:v",
                    "1",
                    "-threads",
                    "1",
                    "-c:v",
                    "mjpeg",
                    "-f",
                    "image2pipe",
                    "pipe:1",
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        mp3(&first, &jpeg("color=c=red:s=8x8"));
        mp3(&second, &jpeg("color=c=blue:s=8x8"));
        let mut bytes = fs::read(&second).unwrap();
        let size = bytes[6..10]
            .iter()
            .fold(0usize, |n, &b| (n << 7) | usize::from(b));
        bytes.splice(10 + size..10 + size, [0u8; 512]);
        let size = size + 512;
        bytes[6..10].copy_from_slice(&[
            ((size >> 21) & 127) as u8,
            ((size >> 14) & 127) as u8,
            ((size >> 7) & 127) as u8,
            (size & 127) as u8,
        ]);
        fs::write(&second, bytes).unwrap();
        let (old, original) = read_source(&first).unwrap();
        let (desired, replacement) = read_source(&second).unwrap();
        assert!(old.audio != desired.audio); // includes the shifted ID3/audio offset
        assert!(native_artwork::same_encoded_audio(&desired, &old).unwrap());
        let mut different = desired.clone();
        different.audio.sha256[0] ^= 1;
        assert!(native_artwork::same_encoded_audio(&different, &old).is_err());
        assert_ne!(old.full_file_sha256, desired.full_file_sha256);
        let original = original.unwrap();
        let replacement = replacement.unwrap();
        let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
        // Exercise the real builder too: an MP3 must not be subjected to the
        // old ALAC-only checksum check after its cover has been read correctly.
        artwork_update::prepare_for_format(
            &catalog,
            catalog.tracks()[0].sync_id,
            100,
            &replacement,
            SystemTime::UNIX_EPOCH,
            desired.format(),
        )
        .unwrap();
        let mut content = Content {
            source: old.clone(),
            track_pid: 1001,
            path: "iTunes_Control/Music/F00/TEST.mp3".into(),
            artwork: Some(Artwork {
                cache_id: 1,
                bytes: original.bytes.len() as u64,
                sha256: original.sha256,
            }),
            collection: None,
        };
        assert!(!cover_changed(&content, &original));
        assert!(cover_changed(&content, &replacement));
        content.artwork = Some(Artwork {
            cache_id: 2,
            bytes: replacement.bytes.len() as u64,
            sha256: replacement.sha256,
        });
        assert!(!cover_changed(&content, &replacement));
        assert!(content.source == old); // artwork never substitutes the phone audio container
        fs::remove_dir_all(root).unwrap();
    }
}
