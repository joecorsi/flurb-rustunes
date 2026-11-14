//! Opt-in receiver qualification. One installed track is toggled and restored;
//! no audio or playlist writes are requested.
use super::*;

fn operation(before: &Before, pid: u64, field: &str, liked: u32) -> Result<Vec<u8>> {
    use music_protocol::catalogue::metadata_edit::{Envelope, TrackUpdate, encode};
    let encoded = encode(
        &Envelope {
            database_pid: before.current.catalog.database_pid(),
            primary_container_pid: before.current.catalog.primary_container_pid(),
            revision: before.anchor().checked_add(1).context("anchor exhausted")?,
            timestamp: SystemTime::now(),
        },
        &[TrackUpdate {
            sync_id: pid,
            item: BTreeMap::from([("comment".into(), plist::Value::String(String::new()))]),
        }],
    )?;
    let mut root = plist::Value::from_reader(std::io::Cursor::new(encoded))?;
    let update = root
        .as_dictionary_mut()
        .unwrap()
        .get_mut("operations")
        .unwrap()
        .as_array_mut()
        .unwrap()[1]
        .as_dictionary_mut()
        .unwrap();
    update.remove("item");
    let mut values = plist::Dictionary::new();
    values.insert("liked_state".into(), liked.into());
    update.insert(field.into(), plist::Value::Dictionary(values));
    let mut bytes = Vec::new();
    root.to_writer_binary(&mut bytes)?;
    Ok(bytes)
}
async fn send(
    root: &Path,
    before: &Before,
    pid: u64,
    field: &str,
    liked: u32,
    control: &Control,
) -> Result<()> {
    let operation = operation(before, pid, field, liked)?;
    let copied = operation.clone();
    match removal_batch::send_metadata_only(
        before,
        &operation,
        Requests::None,
        Sha256::digest(&operation).into(),
        "Flurb",
        root,
        control,
        |_, signature| {
            Ok(Permit {
                operation: copied,
                signature,
            })
        },
    )
    .await
    {
        Sent::Started(Ok(_)) => Ok(()),
        Sent::Started(Err(e)) | Sent::Unsent(e) => Err(e),
    }
}
fn state(before: &Before, pid: u64) -> Result<u32> {
    Ok(before
        .current
        .catalog
        .tracks()
        .iter()
        .find(|t| t.sync_id == pid)
        .context("probe track missing")?
        .playback
        .liked_state)
}
async fn run(root: &Path) -> Result<()> {
    layout::private_directory(root)?;
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(900),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let (_, initial) = removal_batch::capture_fresh(&root.join("initial"), &control).await?;
    let track = initial
        .current
        .catalog
        .tracks()
        .iter()
        .find(|t| {
            t.sync_id != 0
                && matches!(t.playback.liked_state, 0 | 2)
                && !t.availability.is_placeholder
                && t.availability.has_non_purgeable_asset
        })
        .context("no installed eligible track")?;
    let pid = track.sync_id;
    let original = track.playback.liked_state;
    durable_write(
        root,
        "original.json",
        &serde_json::to_vec_pretty(
            &json!({"pid":format!("{pid:016x}"),"title":track.metadata.title,"liked_state":original}),
        )?,
    )?;
    eprintln!("Favorite probe track={pid:016x} original={original}");
    let files = files_digest(&initial)?;
    let playlists = proof(&initial, 0, false)?.other_playlists;
    for field in ["item_stats", "item"] {
        let branch = root.join(field);
        layout::private_directory(&branch)?;
        let (_, before) = removal_batch::capture_fresh(&branch.join("before"), &control).await?;
        ensure!(
            state(&before, pid)? == original,
            "probe state drifted before write"
        );
        let target = if original == 2 { 0 } else { 2 };
        // Retain any transport error, then observe before deciding what to restore.
        let sent = send(&branch.join("send"), &before, pid, field, target, &control).await;
        let (_, after) = removal_batch::capture_fresh(&branch.join("after"), &control).await?;
        let observed = state(&after, pid)?;
        let verification = if observed == target {
            crate::engine::verification::metadata_edit_sql::verify_favorites(
                &before.sql.tables,
                &after.sql.tables,
                &[(pid, original, target)],
                before.anchor(),
                false,
            )
            .map(|_| ())
        } else {
            Ok(())
        };
        let changed: Vec<_> = before
            .sql
            .tables
            .iter()
            .filter_map(|(name, t)| {
                after
                    .sql
                    .tables
                    .get(name)
                    .filter(|a| a.rows != t.rows)
                    .map(|_| name.clone())
            })
            .collect();
        eprintln!(
            "field={field} requested={target} observed={observed} tables={changed:?} send={:?}",
            sent.as_ref().err()
        );
        durable_write(
            &branch,
            "observation.json",
            &serde_json::to_vec_pretty(
                &json!({"field":field,"requested":target,"observed":observed,"tables":changed,"send_error":sent.as_ref().err().map(|e|format!("{e:#}"))}),
            )?,
        )?;
        if observed != original {
            send(
                &branch.join("restore-send"),
                &after,
                pid,
                field,
                original,
                &control,
            )
            .await?;
        }
        let (_, restored) =
            removal_batch::capture_fresh(&branch.join("restored"), &control).await?;
        ensure!(
            state(&restored, pid)? == original,
            "favorite was not restored; retained original.json identifies track"
        );
        ensure!(files_digest(&restored)? == files, "audio files changed");
        ensure!(
            proof(&restored, 0, false)?.other_playlists == playlists,
            "playlists changed"
        );
        if observed == target {
            verification?;
            eprintln!(
                "Native favorite/unfavorite round trip verified via {field}; original restored; audio/playlists unchanged"
            );
            return Ok(());
        }
        sent?;
    }
    bail!("receiver did not apply either qualified favorite candidate; original state preserved")
}
#[test]
#[ignore = "requires connected iPhone and explicit FLURB_FAVORITES_PROBE directory"]
fn native_favorite_round_trip_preserves_audio_and_playlists() {
    let root = PathBuf::from(
        std::env::var_os("FLURB_FAVORITES_PROBE").expect("explicit probe root required"),
    );
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let engine = crate::engine::Engine::new(
                crate::engine::Config {
                    device: Default::default(),
                    deadline: None,
                    cancelled: Arc::new(AtomicBool::new(false)),
                },
                |_| Ok(()),
            )
            .with_fingerprint_cache(crate::engine::FingerprintCache::new(
                root.join("fingerprint-cache"),
                crate::engine::FingerprintCacheMode::Reuse,
            ));
            runtime.block_on(engine.scope(run(&root))).unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn sync_round(
    root: &Path,
    local: bool,
    baselines: &[(String, Vec<super::favorites::Baseline>)],
    track: &super::favorites::Track,
) -> Result<super::favorites::Report> {
    let tracks = [super::favorites::Track {
        favorite: local,
        ..track.clone()
    }];
    super::favorites::run(super::favorites::Request {
        state: &root.join("state"),
        library_id: "native-favorites-probe",
        tracks: &tracks,
        baselines,
        seconds: 900,
    })
    .await
}
async fn sync_cases(
    root: &Path,
    track: &super::favorites::Track,
    original: bool,
    pid: u64,
    control: &Control,
) -> Result<()> {
    let first = sync_round(root, original, &[], track).await?;
    ensure!(
        first.tracks.len() == 1 && first.tracks[0].favorite == original,
        "initial favorite association differs"
    );
    super::favorites::accept(&root.join("state"), &first.receipt)?;
    let old = vec![(
        first.scope.clone(),
        super::favorites::decode_baselines(&first.baselines_json)?,
    )];
    let pushed = sync_round(root, !original, &old, track).await?;
    ensure!(
        pushed.tracks.len() == 1 && pushed.tracks[0].favorite != original,
        "outbound favorite missing"
    );
    // Simulate a crash after phone verification, before the frontend commits:
    // do not acknowledge or install pushed.baselines_json. A newer local click
    // must win after rebasing on the durable native outbox.
    let reversed = sync_round(root, original, &old, track).await?;
    ensure!(
        reversed.tracks.len() == 1 && reversed.tracks[0].favorite == original,
        "verified outbox did not preserve a newer local click"
    );
    super::favorites::accept(&root.join("state"), &reversed.receipt)?;
    super::favorites::accept(&root.join("state"), &reversed.receipt)?;
    let mut baselines = vec![(
        reversed.scope.clone(),
        super::favorites::decode_baselines(&reversed.baselines_json)?,
    )];
    let mut local = original;
    for (index, phone) in [!original, original].into_iter().enumerate() {
        let (_, before) =
            removal_batch::capture_fresh(&root.join(format!("phone-edit-{index}-before")), control)
                .await?;
        send(
            &root.join(format!("phone-edit-{index}-send")),
            &before,
            pid,
            "item_stats",
            if phone { 2 } else { 1 },
            control,
        )
        .await?;
        let imported = sync_round(root, local, &baselines, track).await?;
        ensure!(
            imported.tracks.len() == 1 && imported.tracks[0].favorite == phone,
            "phone favorite change did not import"
        );
        super::favorites::accept(&root.join("state"), &imported.receipt)?;
        baselines = vec![(
            imported.scope,
            super::favorites::decode_baselines(&imported.baselines_json)?,
        )];
        local = phone;
    }
    let repeated = sync_round(root, local, &baselines, track).await?;
    ensure!(
        repeated.tracks.len() == 1 && repeated.tracks[0].favorite == original,
        "repeat favorite state differs"
    );
    super::favorites::accept(&root.join("state"), &repeated.receipt)?;
    Ok(())
}
#[test]
#[ignore = "requires connected iPhone and FLURB_FAVORITES_SYNC_PROBE private fixture"]
fn native_favorite_sync_imports_exports_and_recovers_without_audio_or_playlist_changes() {
    let root = PathBuf::from(
        std::env::var_os("FLURB_FAVORITES_SYNC_PROBE").expect("explicit favorite sync probe root"),
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
                    cancelled: Arc::new(AtomicBool::new(false)),
                },
                |_| Ok(()),
            )
            .with_fingerprint_cache(crate::engine::FingerprintCache::new(
                root.join("fingerprint-cache"),
                crate::engine::FingerprintCacheMode::Reuse,
            ));
            rt.block_on(engine.scope(async {
                let control = Control {
                    deadline: Instant::now() + Duration::from_secs(1800),
                    cancelled: Arc::new(AtomicBool::new(false)),
                };
                let (identity, initial) =
                    removal_batch::capture_fresh(&root.join("sync-initial"), &control).await?;
                let journal = journal_matches(&root.join("state"), &initial, &identity)?
                    .context("fixture journal absent")?;
                let binding = journal
                    .bindings
                    .iter()
                    .find(|b| {
                        journal
                            .sources
                            .get(&b.content.track_pid)
                            .is_some_and(|p| p.is_file())
                            && initial.snapshot.index.matches_transferred_source(
                                b.content.track_pid,
                                &b.content.path,
                                &b.content.source,
                            )
                            && initial.current.catalog.tracks().iter().any(|t| {
                                t.sync_id == b.content.track_pid
                                    && matches!(t.playback.liked_state, 0 | 2)
                            })
                    })
                    .context("no verified fixture track")?;
                let pid = binding.content.track_pid;
                let original = state(&initial, pid)?;
                let track = super::favorites::Track {
                    id: "probe:heart".into(),
                    path: journal.sources.get(&pid).cloned(),
                    favorite: original == 2,
                };
                durable_write(
                    &root,
                    "original.json",
                    &serde_json::to_vec(
                        &json!({"pid":format!("{pid:016x}"),"liked_state":original}),
                    )?,
                )?;
                eprintln!("Favorite sync probe track={pid:016x} original={original}");
                let files = files_digest(&initial)?;
                let playlists = proof(&initial, 0, false)?.other_playlists;
                let result = sync_cases(&root, &track, original == 2, pid, &control).await;
                let (_, current) =
                    removal_batch::capture_fresh(&root.join("sync-cleanup-before"), &control)
                        .await?;
                if state(&current, pid)? != original {
                    send(
                        &root.join("sync-cleanup-send"),
                        &current,
                        pid,
                        "item_stats",
                        original,
                        &control,
                    )
                    .await?;
                }
                let (_, restored) =
                    removal_batch::capture_fresh(&root.join("sync-cleanup-after"), &control)
                        .await?;
                ensure!(
                    state(&restored, pid)? == original
                        && files_digest(&restored)? == files
                        && proof(&restored, 0, false)?.other_playlists == playlists,
                    "favorite probe cleanup differed"
                );
                eprintln!("Original favorite restored; audio and playlists unchanged");
                result?;
                eprintln!(
                    "Two-way native favorite/unfavorite sync and verified-outbox recovery passed"
                );
                Ok::<_, anyhow::Error>(())
            }))
            .unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
}
