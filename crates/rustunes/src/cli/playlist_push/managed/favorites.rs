//! Native liked_state sync. Audio and playlist membership are never written.
use super::*;
use std::os::unix::fs::MetadataExt;
mod state;
use state::Store;

#[derive(Clone, Debug)]
pub struct Track {
    pub id: String,
    pub path: Option<PathBuf>,
    pub favorite: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub id: String,
    pub wire: u64,
    pub audio_sha256: String,
    pub favorite: bool,
    stamp: Stamp,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
impl Stamp {
    fn read(path: &Path) -> Result<Self> {
        let m = fs::symlink_metadata(path)?;
        ensure!(
            m.is_file() && !m.file_type().is_symlink(),
            "favorite source is not a regular file"
        );
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        })
    }
}
pub struct Request<'a> {
    pub state: &'a Path,
    pub library_id: &'a str,
    pub tracks: &'a [Track],
    pub baselines: &'a [(String, Vec<Baseline>)],
    pub seconds: u64,
}
#[derive(Clone, Debug)]
pub struct ResultTrack {
    pub id: String,
    pub before: bool,
    pub favorite: bool,
}
pub struct Report {
    pub scope: String,
    pub tracks: Vec<ResultTrack>,
    pub baselines_json: String,
    pub receipt: String,
    pub skipped: usize,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    wire: u64,
    old: u32,
    new: u32,
    result: Baseline,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    scope: String,
    device: String,
    library: String,
    root: PathBuf,
    anchor: u32,
    files: String,
    changes: Vec<Change>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    scope: String,
    ids: Vec<i64>,
}

pub fn decode_baselines(text: &str) -> Result<Vec<Baseline>> {
    ensure!(
        text.len() <= 32 * 1024 * 1024,
        "favorite baseline exceeds bound"
    );
    let entries: Vec<Baseline> = serde_json::from_str(text)?;
    ensure!(entries.len() <= 50_000, "favorite baseline track limit");
    let mut ids = BTreeSet::new();
    let mut wires = BTreeSet::new();
    for b in &entries {
        ensure!(
            valid_id(&b.id)
                && b.wire != 0
                && ids.insert(&b.id)
                && wires.insert(b.wire)
                && b.audio_sha256.len() == 64
                && b.audio_sha256.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid favorite baseline"
        );
    }
    Ok(entries)
}
pub fn accept(state: &Path, receipt: &str) -> Result<()> {
    ensure!(
        receipt.len() <= 1024 * 1024,
        "favorite receipt exceeds bound"
    );
    let receipt: Receipt = serde_json::from_str(receipt)?;
    Store::open(state)?.accept(&receipt.scope, &receipt.ids)
}
/// Initially preserve favorites on both sides. Once a common state exists,
/// binary changes on either side have an unambiguous three-way result.
fn merge(base: Option<bool>, local: bool, phone: bool) -> bool {
    match base {
        None => local || phone,
        Some(base) if local != base => local,
        Some(_) => phone,
    }
}
fn phone_favorite(state: u32) -> Option<bool> {
    match state {
        0 | 1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}
fn favorite_write(state: u32, desired: bool) -> Option<u32> {
    let current = phone_favorite(state)?;
    (current != desired).then_some(if desired { 2 } else { 1 })
}
const FAVORITE_BATCH_SIZE: usize = music_protocol::protocol::operations::MAX_OPERATIONS - 1;
fn encode(before: &Before, changes: &[Change]) -> Result<Vec<u8>> {
    use music_protocol::catalogue::metadata_edit::Envelope;
    encode_changes(
        &Envelope {
            database_pid: before.current.catalog.database_pid(),
            primary_container_pid: before.current.catalog.primary_container_pid(),
            revision: before.anchor().checked_add(1).context("anchor exhausted")?,
            timestamp: SystemTime::now(),
        },
        changes,
    )
}
fn encode_changes(
    envelope: &music_protocol::catalogue::metadata_edit::Envelope,
    changes: &[Change],
) -> Result<Vec<u8>> {
    use music_protocol::catalogue::metadata_edit::TrackUpdate;
    ensure!(
        !changes.is_empty() && changes.len() <= FAVORITE_BATCH_SIZE,
        "favorite batch exceeds bound"
    );
    let seed: Vec<_> = changes
        .iter()
        .map(|c| TrackUpdate {
            sync_id: c.wire,
            item: BTreeMap::from([("comment".into(), plist::Value::String(String::new()))]),
        })
        .collect();
    let bytes = music_protocol::catalogue::metadata_edit::encode(envelope, &seed)?;
    let mut value = plist::Value::from_reader(std::io::Cursor::new(bytes))?;
    let operations = value
        .as_dictionary_mut()
        .unwrap()
        .get_mut("operations")
        .unwrap()
        .as_array_mut()
        .unwrap();
    for (op, change) in operations.iter_mut().skip(1).zip(changes) {
        ensure!(
            phone_favorite(change.old).is_some()
                && phone_favorite(change.new).is_some()
                && change.old != change.new,
            "invalid native favorite transition"
        );
        let op = op.as_dictionary_mut().unwrap();
        op.remove("item");
        let mut stats = plist::Dictionary::new();
        stats.insert("liked_state".into(), change.new.into());
        op.insert("item_stats".into(), plist::Value::Dictionary(stats));
    }
    let mut bytes = Vec::new();
    value.to_writer_binary(&mut bytes)?;
    Ok(bytes)
}
fn verify(before: &Before, after: &Before, intent: &Intent, later: bool) -> Result<()> {
    ensure!(
        before.snapshot.identity.device_identifier_sha256
            == after.snapshot.identity.device_identifier_sha256
            && before.snapshot.identity.library_identity_sha256
                == after.snapshot.identity.library_identity_sha256,
        "favorite device/library changed"
    );
    ensure!(
        files_digest(after)? == intent.files,
        "favorite session changed audio files"
    );
    let changes: Vec<_> = intent
        .changes
        .iter()
        .map(|c| (c.wire, c.old, c.new))
        .collect();
    crate::engine::verification::metadata_edit_sql::verify_favorites(
        &before.sql.tables,
        &after.sql.tables,
        &changes,
        intent.anchor,
        later,
    )?;
    Ok(())
}
fn path_key(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
fn source_stamp(track: &Track, binding: &Binding, baseline: Option<&Baseline>) -> Result<Stamp> {
    let path = track.path.as_deref().context("favorite source missing")?;
    let stamp = Stamp::read(path)?;
    let sha = hex(&binding.content.source.audio.sha256);
    if baseline.is_some_and(|b| b.stamp == stamp && b.audio_sha256 == sha) {
        return Ok(stamp);
    }
    // A byte-identical file already validated for this verified binding
    // needs no decoder, tag parsing or gapless analysis.
    if crate::engine::planning::file_hashes::read(path, &crate::engine::cancellation())?
        == binding.content.source.full_file_sha256
    {
        ensure!(
            Stamp::read(path)? == stamp,
            "favorite source changed while checking"
        );
        return Ok(stamp);
    }
    let audio = if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mp3"))
    {
        music_protocol::media::mpeg::read(path)?
            .transferred_identity()?
            .audio
    } else {
        music_protocol::media::fingerprint::TransferredSource::from(
            crate::engine::planning::source_reads::fingerprint(path)?,
        )
        .audio
    };
    ensure!(
        // The validated encoded-audio digest covers frame bytes, codec,
        // sample rate and frame count. Container offsets/trailing tag spans
        // may change after a tag edit without changing the music identity.
        audio.sha256 == binding.content.source.audio.sha256,
        "favorite source audio differs from installed track"
    );
    ensure!(
        Stamp::read(path)? == stamp,
        "favorite source changed while checking"
    );
    Ok(stamp)
}

pub(crate) async fn run(o: Request<'_>) -> Result<Report> {
    ensure!(
        valid_id(o.library_id) && o.tracks.len() <= 50_000 && (1..=1800).contains(&o.seconds),
        "invalid favorite sync request"
    );
    let mut locals = BTreeMap::new();
    for t in o.tracks {
        ensure!(
            valid_id(&t.id) && locals.insert(t.id.as_str(), t).is_none(),
            "ambiguous favorite track IDs"
        );
    }
    let mut store = Store::open(o.state)?;
    let evidence = o.state.join("favorites-sync-evidence");
    layout::private_directory(&evidence)?;
    let root = evidence.join(format!("{}-{}", now_ns()?, std::process::id()));
    create_trace(&root)?;
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(o.seconds),
        cancelled: crate::engine::cancellation(),
    };
    let (identity, mut before) =
        removal_batch::capture_fresh(&root.join("initial"), &control).await?;
    let device = hex(&identity.device_digest());
    let library = hex(&identity.library_digest());
    let scope = format!("{}/{device}/{library}", o.library_id);
    recover(&mut store, &mut before, &device, &library, &root, &control).await?;
    let mut baselines: BTreeMap<_, _> = o
        .baselines
        .iter()
        .find(|(s, _)| s == &scope)
        .map(|(_, v)| v.iter().cloned().map(|b| (b.id.clone(), b)).collect())
        .unwrap_or_default();
    for (_, entries) in store.verified(&scope)? {
        for b in entries {
            baselines.insert(b.id.clone(), b);
        }
    }
    let journal = journal_matches(o.state, &before, &identity)?;
    let mut paths: BTreeMap<PathBuf, Vec<&Track>> = BTreeMap::new();
    for t in o.tracks {
        if let Some(path) = &t.path {
            paths.entry(path_key(path)).or_default().push(t);
        }
    }
    let mut matched: Vec<(ResultTrack, u32, Baseline)> = Vec::new();
    let mut skipped = 0;
    if let Some(journal) = &journal {
        let mut used = BTreeSet::new();
        let mut ambiguous = BTreeSet::new();
        for binding in &journal.bindings {
            let wire = binding.content.track_pid;
            if !before.snapshot.index.matches_transferred_source(
                wire,
                &binding.content.path,
                &binding.content.source,
            ) {
                continue;
            }
            let prior = baselines.values().find(|b| b.wire == wire);
            let candidates = journal
                .sources
                .get(&wire)
                .and_then(|p| paths.get(&path_key(p)));
            let track = prior
                .and_then(|b| locals.get(b.id.as_str()).copied())
                .or_else(|| candidates.filter(|v| v.len() == 1).map(|v| v[0]));
            let Some(track) = track else {
                skipped += 1;
                continue;
            };
            if ambiguous.contains(track.id.as_str()) {
                skipped += 1;
                continue;
            }
            if baselines.get(&track.id).is_some_and(|b| {
                b.wire != wire
                    && before
                        .current
                        .catalog
                        .tracks()
                        .iter()
                        .any(|t| t.sync_id == b.wire)
            }) {
                skipped += 1;
                continue;
            }
            if !used.insert(track.id.as_str()) {
                matched.retain(|(r, _, _)| r.id != track.id);
                ambiguous.insert(track.id.as_str());
                skipped += 2;
                continue;
            }
            let Some(phone) = before
                .current
                .catalog
                .tracks()
                .iter()
                .find(|t| t.sync_id == wire)
            else {
                continue;
            };
            let Some(phone_hearted) = phone_favorite(phone.playback.liked_state) else {
                skipped += 1;
                continue;
            };
            let Ok(stamp) = source_stamp(track, binding, prior) else {
                skipped += 1;
                continue;
            };
            let base = prior
                .filter(|b| b.audio_sha256 == hex(&binding.content.source.audio.sha256))
                .map(|b| b.favorite);
            let desired = merge(base, track.favorite, phone_hearted);
            let baseline = Baseline {
                id: track.id.clone(),
                wire,
                audio_sha256: hex(&binding.content.source.audio.sha256),
                favorite: desired,
                stamp,
            };
            matched.push((
                ResultTrack {
                    id: track.id.clone(),
                    before: track.favorite,
                    favorite: desired,
                },
                phone.playback.liked_state,
                baseline,
            ));
        }
    }
    let writes: Vec<_> = matched
        .iter()
        .filter_map(|(_, phone, b)| {
            favorite_write(*phone, b.favorite).map(|new| Change {
                wire: b.wire,
                old: *phone,
                new,
                result: b.clone(),
            })
        })
        .collect();
    for (index, batch) in writes.chunks(FAVORITE_BATCH_SIZE).enumerate() {
        let batch_root = root.join(format!("batch-{index}"));
        create_trace(&batch_root)?;
        // Retain the initial/previous verified state for independent recovery.
        // The native Ready guard still checks these exact bytes before writing.
        before = removal_batch::retain_before(before, &batch_root.join("before"), &control)?;
        let intent = Intent {
            scope: scope.clone(),
            device: device.clone(),
            library: library.clone(),
            root: batch_root.clone(),
            anchor: before.anchor(),
            files: files_digest(&before)?,
            changes: batch.to_vec(),
        };
        let operation = encode(&before, batch)?;
        durable_write(&batch_root, "operation.plist", &operation)?;
        store.prepare(&intent)?;
        let copied = operation.clone();
        let sent = removal_batch::send_metadata_only(
            &before,
            &operation,
            Requests::None,
            Sha256::digest(&operation).into(),
            "Flurb",
            &batch_root.join("send"),
            &control,
            |_, signature| {
                store.started(&intent)?;
                Ok(Permit {
                    operation: copied,
                    signature,
                })
            },
        )
        .await;
        match sent {
            Sent::Unsent(e) => {
                store.clear_prepared(&intent)?;
                return Err(e);
            }
            Sent::Started(result) => {
                let (_, after) =
                    removal_batch::capture_fresh(&batch_root.join("after"), &control).await?;
                if let Err(error) = verify(&before, &after, &intent, false) {
                    durable_write(
                        &batch_root,
                        "verification-error.txt",
                        format!("{error:#}").as_bytes(),
                    )?;
                    return Err(error.context(format!(
                        "Favorite verification could not finish. Details saved in {}",
                        batch_root.display()
                    )));
                }
                store.finish(&intent)?;
                before = after;
                result?;
            }
        }
    }
    baselines.retain(|id, _| locals.contains_key(id.as_str()));
    let results: Vec<_> = matched
        .into_iter()
        .map(|(r, _, b)| {
            baselines.insert(b.id.clone(), b);
            r
        })
        .collect();
    let ids = store
        .verified(&scope)?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    Ok(Report {
        scope: scope.clone(),
        tracks: results,
        baselines_json: serde_json::to_string(&baselines.into_values().collect::<Vec<_>>())?,
        receipt: serde_json::to_string(&Receipt { scope, ids })?,
        skipped,
    })
}

async fn recover(
    store: &mut Store,
    current: &mut Before,
    device: &str,
    library: &str,
    root: &Path,
    control: &Control,
) -> Result<()> {
    let Some((intent, started)) = store.pending()? else {
        return Ok(());
    };
    ensure!(
        intent.device == device && intent.library == library,
        "Reconnect the iPhone with unfinished favorite changes before syncing favorites"
    );
    if !started {
        store.clear_prepared(&intent)?;
        return Ok(());
    }
    let snapshot = crate::engine::state::snapshot::open(&intent.root.join("before/snapshot"))?;
    let sql = removal_batch::materialize(&snapshot, &root.join("recover-before-sql"), control)?;
    // Same complete-table verifier as the normal path, with only the existing
    // later-run playback allowances. Uncertain Started work is never replayed.
    let changes: Vec<_> = intent
        .changes
        .iter()
        .map(|c| (c.wire, c.old, c.new))
        .collect();
    if crate::engine::verification::metadata_edit_sql::verify_favorites(
        &sql.tables,
        &current.sql.tables,
        &changes,
        intent.anchor,
        true,
    )
    .is_ok()
    {
        ensure!(
            files_digest(current)? == intent.files,
            "favorite recovery audio differs"
        );
        store.finish(&intent)?;
    } else if sql.tables == current.sql.tables
        && current.anchor() == intent.anchor
        && files_digest(current)? == intent.files
    {
        store.not_applied(&intent)?;
    } else {
        bail!(
            "Unfinished favorite changes need verification; no changes replayed. Evidence: {}",
            intent.root.display()
        );
    }
    Ok(())
}

/// Settle pending favorite writes before another native playlist/audio session
/// can advance the phone's anchor. This entry point sends no phone writes.
pub(crate) async fn recover_only(state: &Path, seconds: u64) -> Result<()> {
    ensure!(
        (1..=1800).contains(&seconds),
        "invalid favorite recovery deadline"
    );
    let mut store = Store::open(state)?;
    if store.pending()?.is_none() {
        return Ok(());
    }
    let parent = state.join("favorites-sync-evidence");
    layout::private_directory(&parent)?;
    let root = parent.join(format!("recover-{}-{}", now_ns()?, std::process::id()));
    create_trace(&root)?;
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(seconds),
        cancelled: crate::engine::cancellation(),
    };
    let (identity, mut current) =
        removal_batch::capture_fresh(&root.join("current"), &control).await?;
    recover(
        &mut store,
        &mut current,
        &hex(&identity.device_digest()),
        &hex(&identity.library_digest()),
        &root,
        &control,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_phone_unfavorite_imports_and_neutral_states_are_not_rewritten() {
        assert_eq!(phone_favorite(0), Some(false));
        assert_eq!(phone_favorite(1), Some(false));
        assert_eq!(phone_favorite(2), Some(true));
        assert!(!merge(Some(true), true, phone_favorite(1).unwrap()));
        assert!(merge(Some(false), false, phone_favorite(2).unwrap()));
        assert_eq!(favorite_write(2, false), Some(1));
        assert_eq!(favorite_write(1, true), Some(2));
        assert_eq!(favorite_write(0, false), None);
        assert_eq!(favorite_write(1, false), None);
        assert_eq!(favorite_write(2, true), None);
        assert_eq!(favorite_write(3, true), None);
        assert_eq!(phone_favorite(3), None);
    }
    #[test]
    fn tag_layout_changes_match_but_changed_music_does_not() {
        let root =
            std::env::temp_dir().join(format!("flurb-favorite-tag-layout-{}", now_ns().unwrap()));
        layout::private_directory(&root).unwrap();
        let path = root.join("track.mp3");
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
        let frames = frame.repeat(12);
        fs::write(&path, &frames).unwrap();
        let original = music_protocol::media::mpeg::read(&path)
            .unwrap()
            .transferred_identity()
            .unwrap();
        let binding = Binding {
            content: crate::engine::state::journal::Content {
                source: original,
                track_pid: 1,
                path: "iTunes_Control/Music/F00/TEST.mp3".into(),
                artwork: None,
                collection: None,
            },
            receipt_kind:
                crate::engine::state::journal::BindingReceiptKind::InsertBatchVerification,
            receipt_sha256: [1; 32],
            media_anchor: 1,
        };
        let mut tagged = b"ID3\x03\x00\x00\x00\x00\x01\x00".to_vec();
        tagged.resize(138, 0);
        tagged.extend_from_slice(&frames);
        fs::write(&path, &tagged).unwrap();
        let edited = music_protocol::media::mpeg::read(&path)
            .unwrap()
            .transferred_identity()
            .unwrap();
        assert!(edited.audio != binding.content.source.audio);
        assert_eq!(edited.audio.sha256, binding.content.source.audio.sha256);
        let track = Track {
            id: "one".into(),
            path: Some(path.clone()),
            favorite: true,
        };
        assert!(source_stamp(&track, &binding, None).is_ok());
        *tagged.last_mut().unwrap() = 1;
        fs::write(&path, &tagged).unwrap();
        assert!(source_stamp(&track, &binding, None).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "explicit saved snapshot, journal and local source; read-only diagnostic"]
    fn saved_source_matches_its_verified_phone_binding() {
        let snapshot = PathBuf::from(
            std::env::var_os("FLURB_FAVORITE_SOURCE_SNAPSHOT").expect("saved snapshot"),
        );
        let journal =
            PathBuf::from(std::env::var_os("FLURB_FAVORITE_SOURCE_JOURNAL").expect("journal path"));
        let path =
            PathBuf::from(std::env::var_os("FLURB_FAVORITE_SOURCE_FILE").expect("source file"));
        let wire =
            u64::from_str_radix(&std::env::var("FLURB_FAVORITE_SOURCE_WIRE").unwrap(), 16).unwrap();
        let expected = std::env::var("FLURB_FAVORITE_EXPECT_HEART")
            .ok()
            .map(|v| v.parse::<bool>().unwrap());
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                let snapshot = crate::engine::state::snapshot::open(&snapshot).unwrap();
                let library =
                    music_protocol::catalogue::identity::parse(&snapshot.raw_preferences).unwrap();
                let identity = Identity::for_saved_device(
                    snapshot.identity.device_identifier_sha256,
                    &library,
                )
                .unwrap();
                let journal = Journal::open_read_only(&journal, &identity).unwrap();
                let bindings = journal.active_verified_bindings().unwrap();
                let binding = bindings
                    .iter()
                    .find(|b| b.content.track_pid == wire)
                    .unwrap();
                assert!(snapshot.index.matches_transferred_source(
                    wire,
                    &binding.content.path,
                    &binding.content.source
                ));
                source_stamp(
                    &Track {
                        id: "diagnostic".into(),
                        path: Some(path),
                        favorite: true,
                    },
                    binding,
                    None,
                )
                .unwrap();
                if let Some(expected) = expected {
                    let phone = snapshot
                        .catalog
                        .tracks()
                        .iter()
                        .find(|t| t.sync_id == wire)
                        .unwrap()
                        .playback
                        .liked_state;
                    assert_eq!(
                        merge(Some(true), true, phone_favorite(phone).unwrap()),
                        expected
                    );
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }
    #[test]
    #[ignore = "explicit FLURB_FAVORITE_FAILURE intent file; saved evidence only"]
    fn saved_favorite_failure_verifies_without_phone_writes() {
        let input = std::env::var_os("FLURB_FAVORITE_FAILURE").expect("saved intent file");
        let intent: Intent = serde_json::from_slice(&fs::read(input).unwrap()).unwrap();
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                let root = std::env::temp_dir()
                    .join(format!("flurb-favorite-replay-{}", now_ns().unwrap()));
                layout::private_directory(&root).unwrap();
                let control = Control {
                    deadline: Instant::now() + Duration::from_secs(180),
                    cancelled: crate::engine::cancellation(),
                };
                let before =
                    crate::engine::state::snapshot::open(&intent.root.join("before/snapshot"))
                        .unwrap();
                let after =
                    crate::engine::state::snapshot::open(&intent.root.join("after/snapshot"))
                        .unwrap();
                let a =
                    removal_batch::materialize(&before, &root.join("before"), &control).unwrap();
                let b = removal_batch::materialize(&after, &root.join("after"), &control).unwrap();
                let changes: Vec<_> = intent
                    .changes
                    .iter()
                    .map(|c| (c.wire, c.old, c.new))
                    .collect();
                for later in [false, true] {
                    crate::engine::verification::metadata_edit_sql::verify_favorites(
                        &a.tables,
                        &b.tables,
                        &changes,
                        intent.anchor,
                        later,
                    )
                    .unwrap();
                }
                fs::remove_dir_all(root).unwrap();
            })
            .unwrap()
            .join()
            .unwrap();
    }
    #[test]
    #[ignore = "explicit FLURB_FAVORITES_RECOVER_STATE; phone read-only, local recovery writes"]
    fn recover_existing_favorite_batch_without_resending() {
        let state = PathBuf::from(
            std::env::var_os("FLURB_FAVORITES_RECOVER_STATE").expect("explicit state directory"),
        );
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let engine = crate::engine::Engine::new(
                    crate::engine::Config {
                        device: Default::default(),
                        deadline: None,
                        cancelled: crate::engine::cancellation(),
                    },
                    |_| Ok(()),
                )
                .with_fingerprint_cache(crate::engine::FingerprintCache::new(
                    state.join("fingerprint-cache"),
                    crate::engine::FingerprintCacheMode::Reuse,
                ));
                rt.block_on(engine.scope(recover_only(&state, 1800)))
                    .unwrap();
                assert!(Store::open(&state).unwrap().pending().unwrap().is_none());
            })
            .unwrap()
            .join()
            .unwrap();
    }
    #[test]
    fn favorites_reserve_the_database_operation_and_split_large_selections() {
        let changes: Vec<_> = (1000..2200)
            .map(|wire| Change {
                wire,
                old: 0,
                new: 2,
                result: Baseline {
                    id: format!("track-{wire}"),
                    wire,
                    audio_sha256: "00".repeat(32),
                    favorite: true,
                    stamp: Stamp {
                        dev: 0,
                        ino: 0,
                        size: 1,
                        mtime: (0, 0),
                        ctime: (0, 0),
                    },
                },
            })
            .collect();
        let envelope = music_protocol::catalogue::metadata_edit::Envelope {
            database_pid: 1,
            primary_container_pid: 2,
            revision: 3,
            timestamp: SystemTime::UNIX_EPOCH,
        };
        let mut seen = Vec::new();
        for batch in changes.chunks(FAVORITE_BATCH_SIZE) {
            let bytes = encode_changes(&envelope, batch).unwrap();
            let p = plist::Value::from_reader(std::io::Cursor::new(bytes)).unwrap();
            let ops = p.as_dictionary().unwrap()["operations"].as_array().unwrap();
            assert_eq!(ops.len(), batch.len() + 1);
            assert!(ops.len() <= music_protocol::protocol::operations::MAX_OPERATIONS);
            seen.extend(batch.iter().map(|c| c.wire));
        }
        assert_eq!(seen, changes.iter().map(|c| c.wire).collect::<Vec<_>>());
        assert!(encode_changes(&envelope, &changes[..500]).is_err());
    }

    #[test]
    fn first_merge_preserves_hearts_and_later_merges_propagate_unfavorites() {
        for local in [false, true] {
            for phone in [false, true] {
                assert_eq!(merge(None, local, phone), local || phone);
            }
        }
        assert!(!merge(Some(true), false, true));
        assert!(!merge(Some(true), true, false));
        assert!(merge(Some(false), true, false));
        assert!(merge(Some(false), false, true));
        assert_eq!(merge(Some(true), false, false), false);
        assert_eq!(merge(Some(false), true, true), true);
    }
}
