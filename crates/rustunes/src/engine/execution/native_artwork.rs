//! One journaled JPEG update for an independently verified existing track.
//!
//! Source tags/cover may change without replacing the permanent audio container.
//! The journal retains that transferred container; this engine binds the desired
//! source separately. Success leaves Started until an independent fresh catalog
//! verifies artwork relationships and preservation and consumes the permit.
//! No retry, cleanup, deletion, or audio write occurs here.

use crate::engine::execution::device::DeviceProvider;
use anyhow::{Context, Result, ensure};
use plist::{Dictionary, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    future::Future,
    io::Cursor,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::time::{Instant, timeout_at};

use crate::{
    engine::execution::native_insert::{
        BoundSource, Control, FreshState, create_trace, durable_write, sync_trace,
    },
    engine::state::journal::{
        AttemptId, AttemptKind, AttemptState, Content, Identity, Journal, PreparedAttempt,
        StartEvidence, StartedPermit,
    },
};
use music_protocol::catalogue::artwork_update;
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::media::artwork::Cover;
use music_protocol::media::artwork_inventory;
use music_protocol::media::fingerprint::TransferredSource;
use music_protocol::protocol::session::{self, Session};
use music_protocol::protocol::templates;
use music_protocol::protocol::transfer::{
    Attributes, MAX_ASSET, MAX_AUDIO, MAX_BOUNDED_OPS, METADATA, SIGNATURE, Transfer, hex,
    regular_size,
};

const ARTWORK_DB: &str = "iTunes_Control/Artwork/ArtworkDB";
const CHUNK: u64 = 64 * 1024;

/// Exact caller-captured state. Saved ArtworkDB/CDB are hash-bound evidence,
/// not authoritative current catalogues or counters. SQL supplies current Media.
pub struct FreshArtworkState<'a> {
    catalog: &'a Catalog,
    common: FreshState,
    copied_cdb: Vec<u8>,
    artwork_bytes: u64,
    artwork_sha256: [u8; 32],
    artwork_inventory: artwork_inventory::Inventory,
}

impl<'a> FreshArtworkState<'a> {
    pub fn new(
        device: &str,
        catalog: &'a Catalog,
        raw_preferences: &[u8],
        plist_preferences: &[u8],
        artwork_db: &[u8],
        copied_cdb: &[u8],
        input_receipt_sha256: [u8; 32],
    ) -> Result<Self> {
        let artwork_inventory = artwork_inventory::parse(artwork_db)?;
        let mut common = FreshState::new(
            device,
            catalog,
            raw_preferences,
            plist_preferences,
            input_receipt_sha256,
        )?;
        common.bind_copied_cdb(copied_cdb)?;
        Ok(Self {
            catalog,
            common,
            copied_cdb: copied_cdb.to_vec(),
            artwork_bytes: artwork_db.len() as u64,
            artwork_sha256: Sha256::digest(artwork_db).into(),
            artwork_inventory,
        })
    }
}

/// Pure proposal, never reservation. Include all source300 SQL relationships,
/// saved image IDs/counter and all historical journal reservations. Deliberately
/// choose ABOVE the maximum, even if the saved host counter itself was unused.
pub fn allocate_cache_id(catalog: &Catalog, artwork_db: &[u8], journal: &Journal) -> Result<u32> {
    ensure!(
        journal.outstanding()?.is_none(),
        "reconcile or cancel outstanding attempt before allocation"
    );
    next_cache(
        &artwork_inventory::parse(artwork_db)?,
        catalog.artwork().occupied_cache_ids(),
        &journal.reserved_cache_ids()?,
    )
}

fn next_cache(
    saved: &artwork_inventory::Inventory,
    sql: &BTreeSet<u32>,
    reserved: &BTreeSet<u32>,
) -> Result<u32> {
    ensure!(
        saved.next_record_id >= 100,
        "unsupported saved artwork counter"
    );
    saved
        .images
        .keys()
        .chain(sql)
        .chain(reserved)
        .copied()
        .chain([saved.next_record_id])
        .max()
        .expect("counter present")
        .checked_add(1)
        .context("artwork cache namespace exhausted")
}

pub struct ArtworkUpdate<'a, 'catalog> {
    pub attempt: AttemptId,
    pub source: &'a Path,
    /// Current desired source, not the journal's transferred audio container.
    pub desired_source: &'a TransferredSource,
    pub state: &'a FreshArtworkState<'catalog>,
    pub host_name: &'a str,
    /// Must not exist; all partial evidence is retained on failure.
    pub trace: &'a Path,
}

pub struct ArtworkEvidence {
    pub content: Content,
    pub desired_source_sha256: [u8; 32],
    pub desired_source_bytes: u64,
    pub operation_sha256: [u8; 32],
    pub ready_receipt_sha256: [u8; 32],
    pub terminal_receipt_sha256: [u8; 32],
    /// Digest inherited from the verified audio binding, not reread for an image-only edit.
    pub permanent_audio_sha256: [u8; 32],
    /// Stable file identity held across the artwork-only session.
    pub audio_attributes: Attributes,
    /// Raw JPEG read back from Airlock before FileComplete. This does not verify
    /// a permanent rendered artwork cache file or its pixels.
    pub staged_cover_sha256: [u8; 32],
    pub media_anchor: u32,
    pub receipt_sha256: [u8; 32],
}

/// Caller must independently capture and check the fresh SQL artwork relations,
/// anchor, every pre-existing row, and independently read back the permanent
/// artwork cache before calling Journal::complete. The staged JPEG hash is not
/// that permanent-cache proof. Dropping
/// this permit leaves reconciliation required, not permission to replay.
pub struct PendingVerification {
    pub permit: StartedPermit,
    pub evidence: ArtworkEvidence,
}

pub async fn execute<F, Fut>(
    journal: &mut Journal,
    update: ArtworkUpdate<'_, '_>,
    control: &Control,
    connect: F,
) -> Result<PendingVerification>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    control.check()?;
    ensure!(
        control.deadline.saturating_duration_since(Instant::now()) <= Duration::from_secs(180),
        "artwork deadline exceeds 180-second bound"
    );
    let attempt = journal.attempt(update.attempt)?;
    ensure!(
        attempt.state == AttemptState::Prepared && attempt.prepared.kind == AttemptKind::Artwork,
        "engine requires one unstarted Artwork attempt"
    );
    let prepared = &attempt.prepared;
    let state = update.state;
    // Pin first, then inspect and validate the source once below.
    let source = BoundSource::open_for_upload(update.source, update.desired_source.file_bytes)?;
    let (current, cover) = super::cover::read_source(update.source)?;
    ensure!(
        current == *update.desired_source,
        "desired source fingerprint changed"
    );
    source.check()?;
    let cover = cover.context("desired source contains no supported front cover")?;
    validate_preparation(journal, prepared, state, &current, &cover)?;
    let asset_id = (prepared.content.track_pid as i64).to_string();
    let policy = state.common.ancillary().artwork(&asset_id)?;
    policy.ensure_session(
        state.common.library_identity().library_id(),
        state.common.media_anchor(),
    )?;
    let templates = templates::build_from_copied_cdb(
        &state.copied_cdb,
        state.catalog,
        state.common.library_identity(),
        update.host_name,
    )?;
    let session_prepared = session::prepare(
        templates.request,
        &templates.finished,
        state.common.library_identity().library_id(),
        state.common.media_anchor(),
    )?;
    let operations = operation_budget(
        state.common.state_read_chunks()?,
        state.artwork_bytes,
        prepared.content.source.file_bytes,
        cover.bytes.len() as u64,
    )?;
    create_trace(update.trace)?;
    durable_write(update.trace, "operation.plist", &prepared.operation)?;
    durable_write(update.trace, "source-cover.jpg", &cover.bytes)?;
    durable_write(
        update.trace,
        "artwork-input.plist",
        &input_receipt(prepared, state, &current)?,
    )?;

    let result = async {
        control.check()?;
        let provider = timeout_at(
            control
                .deadline
                .min(Instant::now() + Duration::from_secs(15)),
            connect(),
        )
        .await
        .context("artwork discovery deadline")??;
        ensure!(
            provider.udid == state.common.device_identifier(),
            "discovered device differs from prepared artwork state"
        );
        let mut afc = Transfer::connect_bounded(
            &provider,
            update.trace,
            control.deadline,
            operations,
            Arc::clone(&control.cancelled),
        )
        .await?;
        let stage = format!("Airlock/Media/Artwork/{asset_id}");
        let audio_before =
            audio_file_attributes(afc.stat(&prepared.content.path).await?, &prepared.content)?;
        control.check()?;
        let mut session =
            Session::connect(&provider, session_prepared, update.trace, control.deadline).await?;
        // Recheck the exact state in this Ready session, before ANY AFC write.
        let ready_audio = preflight(&mut afc, state, &prepared.content, &stage).await?;
        ensure!(
            audio_before == ready_audio,
            "permanent audio changed while opening artwork session"
        );
        source.check()?;
        control.check()?;
        let signature = session.sign_metadata(&prepared.operation)?;
        durable_write(update.trace, "operation.cig", &signature)?;
        sync_trace(update.trace)?;
        let ready = session.ready_receipt_sha256();
        let permit = journal.mark_started(
            update.attempt,
            StartEvidence {
                ready_media_anchor: state.common.media_anchor(),
                ready_receipt_sha256: ready,
                signature,
            },
        )?;
        // This durable transition is the sole gate to mutating operations.
        session.stage_metadata(&mut afc, &permit).await?;
        control.check()?;
        session.finish_metadata().await?;
        session.wait_for_artwork_with_policy(&policy).await?;
        source.check()?;
        crate::engine::execution::native_insert::staging_folders(&mut afc, true).await?;
        afc.require_absent(&stage).await?;
        let uploaded = afc
            .write_new(
                &stage,
                &mut Cursor::new(&cover.bytes),
                cover.bytes.len() as u64,
            )
            .await?;
        ensure!(
            uploaded == cover.sha256,
            "artwork upload digest differs; partial stage retained"
        );
        source.check()?;
        control.check()?;
        session.complete_artwork(&asset_id).await?;
        session.wait_finished().await?;
        let terminal = session.terminal_receipt_sha256()?;
        drop(session);
        let audio_after =
            audio_file_attributes(afc.stat(&prepared.content.path).await?, &prepared.content)?;
        let permanent = preserved_audio_digest(&prepared.content, &ready_audio, &audio_after)?;
        source.check()?;
        control.check()?;
        drop(afc);
        let mut evidence = ArtworkEvidence {
            content: prepared.content.clone(),
            desired_source_sha256: current.full_file_sha256,
            desired_source_bytes: current.file_bytes,
            operation_sha256: Sha256::digest(permit.operation()).into(),
            ready_receipt_sha256: ready,
            terminal_receipt_sha256: terminal,
            permanent_audio_sha256: permanent,
            audio_attributes: audio_after,
            // The write's replies and the stored size confirmed these bytes.
            staged_cover_sha256: uploaded,
            media_anchor: state
                .common
                .media_anchor()
                .checked_add(1)
                .context("Media anchor exhausted")?,
            receipt_sha256: [0; 32],
        };
        let receipt = evidence_plist(&evidence)?;
        evidence.receipt_sha256 = Sha256::digest(&receipt).into();
        durable_write(update.trace, "pending-verification.plist", &receipt)?;
        sync_trace(update.trace)?;
        Ok(PendingVerification { permit, evidence })
    }
    .await;
    if let Err(error) = &result {
        let _ = durable_write(
            update.trace,
            "error.txt",
            format!(
                "{error:#}\nNo retry or cleanup attempted. Reconcile any Started journal attempt.\n"
            )
            .as_bytes(),
        );
    }
    result
}

// Tag/artwork edits can move the audio payload and change trailing tags.
// Validated codec-tagged digests identify the frames, independently of layout.
pub(crate) fn same_encoded_audio(
    desired: &TransferredSource,
    transferred: &TransferredSource,
) -> Result<bool> {
    desired.validate()?;
    transferred.validate()?;
    Ok(
        desired.format() == transferred.format()
            && desired.audio.sha256 == transferred.audio.sha256,
    )
}

fn validate_preparation(
    journal: &Journal,
    prepared: &PreparedAttempt,
    state: &FreshArtworkState<'_>,
    desired: &TransferredSource,
    cover: &Cover,
) -> Result<()> {
    journal.ensure_identity(&Identity::new(
        state.common.device_identifier(),
        state.common.library_identity(),
    )?)?;
    state.common.ensure_catalog(state.catalog)?;
    ensure!(
        prepared.kind == AttemptKind::Artwork
            && prepared.new_album_pid.is_none()
            && prepared.new_artist_pid.is_none(),
        "invalid artwork attempt kind or collection allocation"
    );
    ensure!(
        prepared.current_media_anchor == state.common.media_anchor()
            && prepared.input_receipt_sha256 == state.common.input_receipt_sha256(),
        "artwork preparation differs from current state receipt"
    );
    let content = &prepared.content;
    ensure!(
        same_encoded_audio(desired, &content.source)?,
        "desired source encoded audio differs from transferred audio"
    );
    ensure!(
        content.source.file_bytes > 0 && content.source.file_bytes <= MAX_AUDIO,
        "transferred audio exceeds engine bound"
    );
    let mut prior = journal
        .lookup_active_verified(&content.source.audio)?
        .context("artwork requires verified journal content")?
        .content;
    prior.artwork = content.artwork.clone();
    ensure!(
        prior == *content,
        "artwork changes transferred content or identity"
    );
    let art = content
        .artwork
        .as_ref()
        .context("missing prepared artwork")?;
    ensure!(
        art.bytes == cover.bytes.len() as u64 && art.sha256 == cover.sha256,
        "desired JPEG differs from reserved artwork"
    );
    let mut reserved = journal.reserved_cache_ids()?;
    ensure!(
        reserved.remove(&art.cache_id),
        "prepared artwork cache is not reserved"
    );
    // The journal permits one outstanding attempt and unique reservations. Only
    // this attempt's own reservation is excluded; historical IDs remain occupied.
    ensure!(
        next_cache(
            &state.artwork_inventory,
            state.catalog.artwork().occupied_cache_ids(),
            &reserved
        )? == art.cache_id,
        "reserved cache differs from fresh union allocation"
    );
    let track = state
        .catalog
        .tracks()
        .iter()
        .find(|track| track.sync_id == content.track_pid)
        .context("artwork target absent from fresh catalog")?;
    ensure!(
        track.afc_path.as_deref() == Some(content.path.as_str())
            && track.metadata.file_size == content.source.file_bytes,
        "catalog audio path or size differs from transferred binding"
    );
    ensure!(
        state
            .catalog
            .tracks()
            .iter()
            .filter(|t| t
                .afc_path
                .as_ref()
                .is_some_and(|p| p.eq_ignore_ascii_case(&content.path)))
            .count()
            == 1,
        "artwork target path is aliased by another track"
    );
    let operation = Value::from_reader(Cursor::new(&prepared.operation))?;
    let timestamp: SystemTime = operation
        .as_dictionary()
        .and_then(|d| d.get("timestamp"))
        .and_then(Value::as_date)
        .context("prepared artwork timestamp missing")?
        .into();
    ensure!(
        artwork_update::prepare_for_format(
            state.catalog,
            content.track_pid,
            art.cache_id,
            cover,
            timestamp,
            content.source.format(),
        )? == prepared.operation,
        "prepared operation differs from exact artwork-only rebuild"
    );
    Ok(())
}

fn operation_budget(state_chunks: u64, artwork_db: u64, audio: u64, cover: u64) -> Result<usize> {
    ensure!(
        artwork_db <= MAX_ASSET && audio <= MAX_AUDIO && cover <= MAX_ASSET,
        "artwork operation inputs exceed bounds"
    );
    let operations = state_chunks
        .checked_add(artwork_db.div_ceil(CHUNK))
        .and_then(|n| n.checked_add(cover.div_ceil(CHUNK) * 2))
        .and_then(|n| n.checked_add(512))
        .context("artwork operation budget overflow")?;
    let operations = usize::try_from(operations)?;
    ensure!(
        operations <= MAX_BOUNDED_OPS,
        "derived artwork AFC allowance exceeds bound"
    );
    Ok(operations)
}

async fn preflight(
    afc: &mut Transfer,
    state: &FreshArtworkState<'_>,
    content: &Content,
    stage: &str,
) -> Result<Attributes> {
    // One full state guard in Ready, immediately before the durable send gate.
    // The operation is rebuilt as artwork-only; audio is never opened for
    // writing. Pin its attributes here and compare them after SyncFinished.
    // Its already-verified digest stays in the binding without USB audio reads.
    const NAMES: [&str; 7] = [
        "iTunes_Control/iTunes/iTunesPrefs",
        "iTunes_Control/iTunes/iTunesPrefs.plist",
        "iTunes_Control/iTunes/MediaLibrary.sqlitedb",
        "iTunes_Control/iTunes/MediaLibrary.sqlitedb-wal",
        "iTunes_Control/iTunes/MediaLibrary.sqlitedb-shm",
        "iTunes_Control/iTunes/iTunesCDB",
        ARTWORK_DB,
    ];
    let audio_before = audio_file_attributes(afc.stat(&content.path).await?, content)?;
    let mut before = Vec::new();
    for name in NAMES {
        before.push(afc.stat(name).await?);
    }
    state.common.verify_state(afc).await?;
    regular_size(
        before[6].as_ref().context("saved ArtworkDB missing")?,
        state.artwork_bytes,
    )?;
    ensure!(
        afc.read_hash(ARTWORK_DB, state.artwork_bytes, MAX_ASSET)
            .await?
            == state.artwork_sha256,
        "saved ArtworkDB changed since allocation"
    );

    afc.require_directory("iTunes_Control/Sync/Media").await?;
    afc.require_directory_or_absent(crate::engine::execution::native_insert::STAGING_ARTWORK)
        .await?;
    ensure!(
        afc.names("iTunes_Control/Sync/Media")
            .await?
            .iter()
            .all(|n| n == "." || n == ".."),
        "metadata namespace contains leftovers; nothing removed"
    );
    for path in [METADATA, SIGNATURE, stage] {
        afc.require_absent(path).await?;
    }
    let audio_after = audio_file_attributes(afc.stat(&content.path).await?, content)?;
    ensure!(
        audio_before == audio_after,
        "permanent audio attributes changed during artwork preflight"
    );
    for (name, previous) in NAMES.into_iter().zip(before) {
        ensure!(
            stable_attrs(previous) == stable_attrs(afc.stat(name).await?),
            "artwork preflight state changed across combined readback"
        );
    }
    Ok(audio_after)
}

fn stable_attrs(attrs: Option<Attributes>) -> Option<Attributes> {
    attrs.map(|mut attrs| {
        attrs.remove("st_atime");
        attrs
    })
}
fn audio_file_attributes(attributes: Option<Attributes>, content: &Content) -> Result<Attributes> {
    let attributes = attributes.context("permanent audio is missing before artwork update")?;
    regular_size(&attributes, content.source.file_bytes)?;
    attributes
        .get("st_mtime")
        .context("permanent audio modification time is missing")?
        .parse::<u64>()
        .context("invalid audio modification time")?;
    Ok(stable_attrs(Some(attributes)).expect("present attributes"))
}

/// An artwork operation has no audio mutation or rename. Reuse the already
/// verified container identity only when its complete stable stat is unchanged.
/// A missing file, new inode, changed length or write time refuses completion.
fn preserved_audio_digest(
    content: &Content,
    before: &Attributes,
    after: &Attributes,
) -> Result<[u8; 32]> {
    ensure!(
        before == after,
        "permanent audio changed during artwork update"
    );
    ensure!(
        content.source.full_file_sha256 != [0; 32],
        "verified audio digest is absent"
    );
    Ok(content.source.full_file_sha256)
}
fn binary(dictionary: Dictionary) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    Value::Dictionary(dictionary).to_writer_binary(&mut bytes)?;
    Ok(bytes)
}
fn input_receipt(
    prepared: &PreparedAttempt,
    state: &FreshArtworkState<'_>,
    desired: &TransferredSource,
) -> Result<Vec<u8>> {
    let art = prepared
        .content
        .artwork
        .as_ref()
        .context("prepared artwork absent")?;
    let mut d = Dictionary::new();
    d.insert("Format".into(), "snapshot-bound-artwork".into());
    d.insert(
        "InputReceiptSHA256".into(),
        hex(&prepared.input_receipt_sha256).into(),
    );
    d.insert(
        "AncillaryBindingSHA256".into(),
        hex(&state.common.ancillary().binding_sha256()).into(),
    );
    d.insert("ArtworkDBSHA256".into(), hex(&state.artwork_sha256).into());
    d.insert(
        "CopiedCDBSHA256".into(),
        hex(&Sha256::digest(&state.copied_cdb)).into(),
    );
    d.insert(
        "DesiredSourceSHA256".into(),
        hex(&desired.full_file_sha256).into(),
    );
    d.insert(
        "TransferredAudioSHA256".into(),
        hex(&prepared.content.source.full_file_sha256).into(),
    );
    d.insert(
        "EncodedAudioSHA256".into(),
        hex(&desired.audio.sha256).into(),
    );
    d.insert("DesiredSourceBytes".into(), desired.file_bytes.into());
    d.insert("ArtworkDBBytes".into(), state.artwork_bytes.into());
    d.insert("MediaAnchor".into(), state.common.media_anchor().into());
    d.insert(
        "TrackPID".into(),
        (prepared.content.track_pid as i64).into(),
    );
    d.insert("CacheID".into(), art.cache_id.into());
    d.insert(
        "AudioVerification".into(),
        "PreviouslyVerifiedContainerWithUnchangedAttributes".into(),
    );
    d.insert("CoverBytes".into(), art.bytes.into());
    d.insert("CoverSHA256".into(), hex(&art.sha256).into());
    d.insert("ForeignArtworkProvider".into(), "Unavailable".into());
    d.insert("GeniusProvider".into(), "Disabled".into());
    binary(d)
}
fn evidence_plist(e: &ArtworkEvidence) -> Result<Vec<u8>> {
    let art = e
        .content
        .artwork
        .as_ref()
        .context("artwork evidence missing cover")?;
    let mut d = Dictionary::new();
    d.insert(
        "Status".into(),
        "PendingIndependentCatalogVerification".into(),
    );
    d.insert("TrackPID".into(), (e.content.track_pid as i64).into());
    d.insert("PermanentPath".into(), e.content.path.clone().into());
    d.insert(
        "TransferredAudioBytes".into(),
        e.content.source.file_bytes.into(),
    );
    d.insert("DesiredSourceBytes".into(), e.desired_source_bytes.into());
    d.insert(
        "AudioVerification".into(),
        "PreviouslyVerifiedContainerWithUnchangedAttributes".into(),
    );
    d.insert(
        "AudioAttributes".into(),
        Value::Dictionary(
            e.audio_attributes
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        ),
    );
    d.insert("CoverBytes".into(), art.bytes.into());
    d.insert("CacheID".into(), art.cache_id.into());
    d.insert("MediaAnchor".into(), e.media_anchor.into());
    for (name, hash) in [
        ("OperationSHA256", e.operation_sha256),
        ("ReadyFrameSHA256", e.ready_receipt_sha256),
        ("TerminalFrameSHA256", e.terminal_receipt_sha256),
        ("PermanentAudioSHA256", e.permanent_audio_sha256),
        ("DesiredSourceSHA256", e.desired_source_sha256),
        ("StagedCoverSHA256", e.staged_cover_sha256),
    ] {
        d.insert(name.into(), hex(&hash).into());
    }
    binary(d)
}

#[cfg(test)]
mod tests;
