//! Explicit opt-in receiver test. Only its uniquely named scratch playlist is
//! written/deleted; existing audio and every other playlist must be unchanged.
use super::*;

#[test]
#[ignore = "requires a connected trusted iPhone and FLURB_PLAYLIST_PROBE fixture directory"]
fn receiver_updates_one_owned_playlist_without_touching_audio() {
    let root = PathBuf::from(
        std::env::var_os("FLURB_PLAYLIST_PROBE").expect("explicit probe directory required"),
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
            runtime.block_on(engine.scope(async {
                let state = root.join("state");
                let m3u = root.join("playlist.m3u8");
                let name = format!("Flurb playlist update test {}", now_ns().unwrap());
                let local = format!("probe:{}", name);
                let ids: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
                let mut expected_pid = None;
                let mut failure = None;
                for (order, rename, expected_change) in [
                    (vec![0, 1], false, true),
                    (vec![0, 1], false, false),
                    (vec![1, 0, 2], false, true),
                    (vec![2, 0], false, true),
                    (vec![], false, true),
                    (vec![], true, true),
                    (vec![], true, false),
                ] {
                    fs::write(
                        &m3u,
                        order
                            .iter()
                            .map(|i| format!("{}.mp3\n", ids[*i]))
                            .collect::<String>(),
                    )
                    .unwrap();
                    let selected: Vec<_> = order.iter().map(|i| ids[*i].clone()).collect();
                    let current_name = if rename {
                        format!("{name} renamed")
                    } else {
                        name.clone()
                    };
                    let result = run(Request {
                        playlist: &m3u,
                        local_id: &local,
                        local_track_ids: &selected,
                        name: &current_name,
                        state: &state,
                        host_name: "Flurb",
                        seconds: 300,
                    })
                    .await;
                    match result {
                        Ok(report) => {
                            eprintln!(
                                "verified pid={:016x} members={} changed={}",
                                report.playlist_id, report.tracks, report.changed
                            );
                            if expected_pid.is_some_and(|p| p != report.playlist_id)
                                || report.tracks != order.len()
                                || report.changed != expected_change
                            {
                                failure = Some(anyhow::anyhow!("receiver report differed"));
                                break;
                            }
                            expected_pid = Some(report.playlist_id);
                        }
                        Err(e) => {
                            eprintln!("receiver step failed: {e:#}");
                            failure = Some(e);
                            break;
                        }
                    }
                }
                if failure.is_none() {
                    if let Some(pid) = expected_pid {
                        if let Err(error) =
                            reverse_cases(&root, &state, &m3u, &local, pid, &name).await
                        {
                            failure = Some(error);
                        }
                    }
                }
                // Cleanup even when verification caught an unexpected receiver change.
                let mut store = Store::open(&state).unwrap();
                if expected_pid.is_none() && store.pending().unwrap().is_empty() {
                    panic!("probe stopped before any write: {:#}", failure.unwrap());
                }
                let control = Control {
                    deadline: Instant::now() + Duration::from_secs(300),
                    cancelled: Arc::new(AtomicBool::new(false)),
                };
                let (_, before) = removal_batch::capture_fresh(
                    &root.join(format!("cleanup-before-{}", now_ns().unwrap())),
                    &control,
                )
                .await
                .unwrap();
                let device = hex(&before.snapshot.identity.device_identifier_sha256);
                let library = hex(&before.snapshot.identity.library_identity_sha256);
                let mapped = store
                    .get(&device, &library, &local)
                    .unwrap()
                    .map(|b| b.phone_id);
                let pending = store.pending().unwrap();
                let pid = mapped.or_else(|| {
                    pending
                        .iter()
                        .find(|i| i.desired.local_id == local)
                        .map(|i| i.desired.phone_id)
                });
                if let Some(pid) =
                    pid.filter(|pid| reconcile::row_exists(&before, *pid).unwrap_or(true))
                {
                    let current = qualify_target(&before, pid).unwrap();
                    assert!(current.starts_with(&name));
                    let original = proof(&before, pid, false).unwrap();
                    let operation = mutation::delete_playlists(
                        &before.current.catalog,
                        &[pid],
                        SystemTime::now(),
                    )
                    .unwrap();
                    let operation_copy = operation.clone();
                    let sent = removal_batch::send_metadata_only(
                        &before,
                        &operation,
                        Requests::None,
                        Sha256::digest(&operation).into(),
                        "Flurb",
                        &root.join(format!("cleanup-send-{}", now_ns().unwrap())),
                        &control,
                        |_, signature| {
                            Ok(Permit {
                                operation: operation_copy,
                                signature,
                            })
                        },
                    )
                    .await;
                    match sent {
                        Sent::Started(Ok(_)) => {}
                        Sent::Started(Err(e)) | Sent::Unsent(e) => {
                            panic!("scratch cleanup failed: {e:#}")
                        }
                    }
                    let (_, after) = removal_batch::capture_fresh(
                        &root.join(format!("cleanup-after-{}", now_ns().unwrap())),
                        &control,
                    )
                    .await
                    .unwrap();
                    assert!(qualify_target(&after, pid).is_err());
                    let observed = proof(&after, pid, false).unwrap();
                    assert_eq!(observed.files, original.files);
                    assert_eq!(observed.tracks, original.tracks);
                    assert_eq!(observed.other_playlists, original.other_playlists);
                    assert_eq!(after.anchor(), before.anchor() + 1);
                    for i in pending {
                        store.abort(&i).unwrap();
                    }
                    eprintln!("scratch playlist removed; all audio and other playlists unchanged");
                }
                if let Some(e) = failure {
                    panic!("playlist receiver test failed: {e:#}");
                }
            }));
        })
        .unwrap()
        .join()
        .unwrap();
}

// Simulate edits made outside Flurb's playlist journal while retaining all
// native noninterference checks. Only the explicitly owned scratch PID is used.
async fn external_edit(
    root: &Path,
    state: &Path,
    m3u: &Path,
    local: &str,
    pid: u64,
    name: &str,
    order: &[&str],
) -> Result<()> {
    let trace = root.join(format!("external-{}", now_ns()?));
    layout::private_directory(&trace)?;
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(300),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let (identity, before) = removal_batch::capture_fresh(&trace.join("before"), &control).await?;
    ensure!(
        qualify_target(&before, pid)?.starts_with("Flurb playlist update test "),
        "not the scratch playlist"
    );
    let mut expected = Store::open(state)?
        .get(
            &hex(&identity.device_digest()),
            &hex(&identity.library_digest()),
            local,
        )?
        .context("scratch baseline absent")?;
    ensure!(expected.phone_id == pid, "scratch ownership differs");
    let paths: Vec<_> = order
        .iter()
        .map(|id| PathBuf::from(format!("{id}.mp3")))
        .collect();
    let journal = journal_matches(state, &before, &identity)?;
    let matched = resolve_members(m3u, &paths, &before, journal.as_ref())?;
    expected.name = name.into();
    expected.members = matched
        .iter()
        .zip(order)
        .map(|((wire, sha), local)| Member {
            local_id: (*local).into(),
            phone_id: *wire,
            file_sha256: sha.clone(),
            source: format!("{local}.mp3"),
        })
        .collect();
    let ids: Vec<_> = matched.iter().map(|(id, _)| *id).collect();
    let operation =
        mutation::update_playlist(&before.current.catalog, pid, name, &ids, SystemTime::now())?;
    let intended = Intent {
        version: 1,
        root: trace.clone(),
        desired: expected,
        before: proof(&before, pid, true)?,
        operation_sha256: hex(&Sha256::digest(&operation)),
        delete: false,
        additional: vec![],
    };
    let copied = operation.clone();
    match removal_batch::send_metadata_only(
        &before,
        &operation,
        Requests::None,
        Sha256::digest(&operation).into(),
        "Flurb",
        &trace.join("send"),
        &control,
        |_, signature| {
            Ok(Permit {
                operation: copied,
                signature,
            })
        },
    )
    .await
    {
        Sent::Started(Ok(_)) => {}
        Sent::Started(Err(e)) | Sent::Unsent(e) => return Err(e),
    }
    let (_, after) = removal_batch::capture_fresh(&trace.join("after"), &control).await?;
    verify_intent(&after, &intended)?;
    Ok(())
}
async fn reverse_cases(
    root: &Path,
    state: &Path,
    m3u: &Path,
    local: &str,
    pid: u64,
    name: &str,
) -> Result<()> {
    use reconcile::*;
    let engine = crate::engine::test_engine();
    let tracks: Vec<_> = ["a", "b", "c"]
        .into_iter()
        .map(|id| LocalTrack {
            id: id.into(),
            path: Some(root.join(format!("{id}.mp3"))),
        })
        .collect();
    let selected = vec![local.to_owned()];
    let phone_name = format!("{name} phone");
    external_edit(root, state, m3u, local, pid, &phone_name, &["b", "a"]).await?;
    let initial = LocalPlaylist {
        id: local.into(),
        derived: false,
        value: PlaylistState {
            name: format!("{name} renamed"),
            tracks: vec![],
        },
    };
    macro_rules! request {
        ($playlists:expr,$choices:expr $(,)?) => {
            reconcile::Request {
                state,
                prefix: "probe:",
                tracks: &tracks,
                playlists: $playlists,
                selected: &selected,
                choices: $choices,
                pending: &[],
                seconds: 300,
            }
        };
    }
    let first = reconcile::run(request!(std::slice::from_ref(&initial), &[]), &engine).await?;
    ensure!(
        first.conflicts.is_empty() && first.changes.iter().any(|c| c.id == local),
        "one-sided phone edit was not imported"
    );
    let observed = first
        .changes
        .iter()
        .find(|c| c.id == local)
        .unwrap()
        .after
        .clone()
        .context("phone edit disappeared")?;
    ensure!(
        observed.name == phone_name && observed.tracks == ["b", "a"],
        "phone name/order differs"
    );
    durable_write(
        root,
        "local-incoming-committed.json",
        &serde_json::to_vec(&observed)?,
    )?;
    reconcile::accept(state, &first.acceptance_json)?;
    reconcile::accept(state, &first.acceptance_json)?;
    fs::write(m3u, "b.mp3\na.mp3\n")?;
    let ids = vec!["b".into(), "a".into()];
    let report = super::run(super::Request {
        playlist: m3u,
        local_id: local,
        local_track_ids: &ids,
        name: &phone_name,
        state,
        host_name: "Flurb",
        seconds: 300,
    })
    .await?;
    ensure!(
        !report.changed && report.playlist_id == pid,
        "imported state was unnecessarily rewritten"
    );
    eprintln!("reverse import verified; matching outbound sync is a no-op");
    let both_phone_name = format!("{name} other phone");
    external_edit(root, state, m3u, local, pid, &both_phone_name, &["a", "b"]).await?;
    let local_change = LocalPlaylist {
        id: local.into(),
        derived: false,
        value: PlaylistState {
            name: format!("{name} local"),
            tracks: vec!["b".into(), "c".into(), "a".into()],
        },
    };
    let conflict =
        reconcile::run(request!(std::slice::from_ref(&local_change), &[]), &engine).await?;
    ensure!(
        conflict.conflicts.len() == 1,
        "simultaneous edits did not produce a conflict"
    );
    let c = &conflict.conflicts[0];
    let resolved = reconcile::run(
        request!(
            std::slice::from_ref(&local_change),
            &[Choice {
                id: c.id.clone(),
                token: c.token.clone(),
                resolution: Resolution::Iphone,
            }],
        ),
        &engine,
    )
    .await?;
    ensure!(
        resolved.conflicts.is_empty(),
        "explicit choice was not accepted"
    );
    let merged = resolved
        .changes
        .iter()
        .find(|c| c.id == local)
        .unwrap()
        .after
        .as_ref()
        .context("phone resolution absent")?;
    ensure!(
        merged.name == both_phone_name && merged.tracks == ["a", "b"],
        "resolved phone state differs"
    );
    durable_write(
        root,
        "local-conflict-committed.json",
        &serde_json::to_vec(merged)?,
    )?;
    reconcile::accept(state, &resolved.acceptance_json)?;
    let deleted = delete(DeleteRequest {
        local_id: local,
        state,
        host_name: "Flurb",
        seconds: 300,
    })
    .await?;
    ensure!(
        deleted.changed && deleted.tracks == 0,
        "native playlist deletion did not verify"
    );
    ensure!(
        !delete(DeleteRequest {
            local_id: local,
            state,
            host_name: "Flurb",
            seconds: 300
        })
        .await?
        .changed,
        "repeated deletion was not idempotent"
    );
    eprintln!("conflict resolution and playlist deletion verified; audio unchanged");
    Ok(())
}
