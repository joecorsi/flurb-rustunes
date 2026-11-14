//! Bounded native playlist sessions with one durable outbox and one shared
//! preservation proof. A completed snapshot is the next batch's Before; Ready
//! still checks the phone against that exact state before a write can start.
use super::*;

pub struct Request<'a> {
    pub state: &'a Path,
    pub host_name: &'a str,
    pub seconds: u64,
    pub playlists: &'a [super::Request<'a>],
    pub deleted: &'a [String],
}
pub struct Progress<'a> {
    pub completed: usize,
    pub total: usize,
    pub batch_size: usize,
    /// Only independently verified/no-op identities, for durable frontend ACKs.
    pub verified: &'a [String],
}
pub struct Report {
    pub changed: usize,
    pub unchanged: usize,
}
const MAX_PLAYLISTS: usize = 64;
const MAX_DESIRED: usize = 512 * 1024;
const PLAYLIST_TABLES: &[&str] = &[
    "container",
    "container_item",
    "container_item_media_type",
    "container_seed",
];

pub(super) fn common_proof(before: &Before, excluded: &BTreeSet<u64>) -> Result<Proof> {
    Ok(Proof {
        base_anchor: before.anchor(),
        files: files_digest(before)?,
        tracks: table_digest(
            before,
            &[
                "item",
                "item_extra",
                "item_store",
                "item_state",
                "base_location",
                "album",
                "album_artist",
                "item_artist",
                "composer",
            ],
            None,
        )?,
        other_playlists: sql_digest_excluding(&before.sql, PLAYLIST_TABLES, excluded)?,
        created: None,
        ui: None,
    })
}
pub(super) fn save_verified(intent: &Intent, baselines: &[Baseline]) -> Result<()> {
    ensure!(
        baselines.len() == 1 + intent.additional.len(),
        "batch receipt count differs"
    );
    if intent.additional.is_empty() {
        return super::save_verified(intent, &baselines[0]);
    }
    durable_write(
        &intent.root,
        "verified.json",
        &serde_json::to_vec_pretty(
            &json!({"intent_sha256":intent.digest()?,"baselines":baselines}),
        )?,
    )
}
struct Planned {
    prior: Option<Baseline>,
    desired: Baseline,
    changed: bool,
}
fn parse(request: &super::Request<'_>) -> Result<Vec<PathBuf>> {
    ensure!(
        fs::metadata(request.playlist)?.len() <= 1024 * 1024,
        "M3U8 exceeds bound"
    );
    let text = fs::read_to_string(request.playlist)?;
    let selected = if text
        .lines()
        .all(|l| l.trim().is_empty() || l.trim().starts_with('#'))
    {
        vec![]
    } else {
        entries(&text)?
    };
    validate_request(request, &selected)?;
    Ok(selected)
}
fn plan_upsert(
    o: &super::Request<'_>,
    selected: &[PathBuf],
    before: &Before,
    identity: &Identity,
    store: &Store,
    journal: Option<&JournalMatches>,
    reserved: &mut BTreeSet<u64>,
) -> Result<Planned> {
    let device = hex(&identity.device_digest());
    let library = hex(&identity.library_digest());
    let prior = store.get(&device, &library, o.local_id)?;
    let baseline = match &prior {
        Some(b) if !b.deleted => {
            check_baseline(before, b)?;
            Some(b.clone())
        }
        _ => match collision(before, o.name, None)? {
            Some(pid) => {
                ensure!(
                    store.owner(&device, &library, pid)?.is_none(),
                    "this iPhone playlist is linked to another Flurb playlist"
                );
                Some(legacy_baseline(before,o.state,&device,&library,o.local_id,o.name,pid)?.context("an unmanaged iPhone playlist already has this name; refusing to overwrite it")?)
            }
            None => None,
        },
    };
    ensure!(
        collision(before, o.name, baseline.as_ref().map(|b| b.phone_id))?.is_none(),
        "another iPhone playlist already has the requested name"
    );
    let members = resolve_members(o.playlist, selected, before, journal)?;
    let pid = if let Some(b) = &baseline {
        b.phone_id
    } else {
        let mut candidate = fresh_pid(before)?;
        while reserved.contains(&candidate) {
            candidate = fresh_pid(before)?;
        }
        candidate
    };
    ensure!(
        reserved.insert(pid),
        "two local playlists refer to the same iPhone playlist"
    );
    let desired = Baseline {
        version: 1,
        device,
        library,
        local_id: o.local_id.into(),
        phone_id: pid,
        name: o.name.into(),
        members: members
            .iter()
            .zip(o.local_track_ids)
            .zip(selected)
            .map(|(((id, sha), local), source)| Member {
                local_id: local.clone(),
                phone_id: *id,
                file_sha256: sha.clone(),
                source: source.to_string_lossy().into_owned(),
            })
            .collect(),
        created_sha256: baseline
            .as_ref()
            .map(|b| b.created_sha256.clone())
            .unwrap_or_default(),
        verified_anchor: before.anchor(),
        verified_ns: now_ns()?,
        deleted: false,
        mutable: true,
    };
    let changed = baseline.as_ref().is_none_or(|b| {
        b.name != desired.name
            || b.members
                .iter()
                .map(|m| m.phone_id)
                .ne(desired.members.iter().map(|m| m.phone_id))
    });
    ensure!(
        !changed || baseline.as_ref().is_none_or(|b| b.mutable),
        "this playlist is managed by the iPhone"
    );
    let mut desired = desired;
    if !changed {
        desired.created_sha256 = created_hash(before, pid)?;
    }
    Ok(Planned {
        prior,
        desired,
        changed,
    })
}
fn plan_delete(
    id: &str,
    before: &Before,
    identity: &Identity,
    store: &Store,
    reserved: &mut BTreeSet<u64>,
) -> Result<Planned> {
    let prior = store
        .get(
            &hex(&identity.device_digest()),
            &hex(&identity.library_digest()),
            id,
        )?
        .context("playlist has no verified sync baseline")?;
    ensure!(reserved.insert(prior.phone_id), "duplicate playlist target");
    if prior.deleted {
        ensure!(
            !reconcile::row_exists(before, prior.phone_id)?,
            "the phone restored this playlist; reconcile again"
        );
        return Ok(Planned {
            desired: prior.clone(),
            prior: Some(prior),
            changed: false,
        });
    }
    ensure!(prior.mutable, "this playlist is managed by the iPhone");
    qualify_target(before, prior.phone_id)?;
    ensure!(
        created_hash(before, prior.phone_id)? == prior.created_sha256,
        "the linked playlist identity changed"
    );
    let mut desired = prior.clone();
    desired.deleted = true;
    desired.members.clear();
    Ok(Planned {
        prior: Some(prior),
        desired,
        changed: true,
    })
}

pub(crate) async fn run(
    o: Request<'_>,
    progress: &mut impl FnMut(Progress<'_>) -> Result<()>,
) -> Result<Report> {
    ensure!(
        (1..=1800).contains(&o.seconds),
        "invalid playlist batch deadline"
    );
    let total = o.playlists.len() + o.deleted.len();
    ensure!(total <= 5000, "too many playlist changes");
    let mut unique = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut parsed = Vec::new();
    for request in o.playlists {
        ensure!(
            request.state == o.state
                && request.host_name == o.host_name
                && request.seconds == o.seconds,
            "playlist batch request context differs"
        );
        ensure!(
            unique.insert(request.local_id.to_owned()) && names.insert(request.name.to_lowercase()),
            "repeated playlist identity or name in batch"
        );
        parsed.push(parse(request)?);
    }
    for id in o.deleted {
        ensure!(
            valid_id(id) && unique.insert(id.clone()),
            "invalid or duplicate playlist deletion"
        );
    }
    let mut report = Report {
        changed: 0,
        unchanged: 0,
    };
    if total == 0 {
        return Ok(report);
    }
    recover(o.state, o.seconds).await?;
    let parent = o.state.join("playlist-sync-evidence");
    layout::private_directory(&parent)?;
    let root = parent.join(format!("batch-{}-{}", now_ns()?, std::process::id()));
    create_trace(&root)?;
    let root = fs::canonicalize(root)?;
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(o.seconds),
        cancelled: crate::engine::cancellation(),
    };
    progress(Progress {
        completed: 0,
        total,
        batch_size: total.min(MAX_PLAYLISTS),
        verified: &[],
    })?;
    let (identity, mut before) =
        removal_batch::capture_fresh(&root.join("before"), &control).await?;
    if o.state.join("playlist-trials").exists() {
        refuse_unsettled_trial(
            &o.state.join("playlist-trials"),
            &hex(&identity.device_digest()),
            &hex(&identity.library_digest()),
        )?;
    }
    let mut store = Store::open(o.state)?;
    let mut completed = 0;
    while completed < total {
        control.check()?;
        let journal = journal_matches(o.state, &before, &identity)?;
        let mut planned = Vec::new();
        let mut reserved = BTreeSet::new();
        let mut size = 0;
        for index in completed..total.min(completed + MAX_PLAYLISTS) {
            control.check()?;
            let plan = if index < o.playlists.len() {
                plan_upsert(
                    &o.playlists[index],
                    &parsed[index],
                    &before,
                    &identity,
                    &store,
                    journal.as_ref(),
                    &mut reserved,
                )
            } else {
                plan_delete(
                    &o.deleted[index - o.playlists.len()],
                    &before,
                    &identity,
                    &store,
                    &mut reserved,
                )
            }
            .with_context(|| format!("planning playlist {} of {total}", index + 1))?;
            let bytes = serde_json::to_vec(&plan.desired)?.len();
            if !planned.is_empty() && size + bytes > MAX_DESIRED {
                break;
            }
            size += bytes;
            planned.push(plan);
        }
        let count = planned.len();
        progress(Progress {
            completed,
            total,
            batch_size: count,
            verified: &[],
        })?;
        let changed: Vec<_> = planned.iter().filter(|p| p.changed).collect();
        if !changed.is_empty() {
            let directory = root.join(format!("session-{completed:04}"));
            create_trace(&directory)?;
            let ids = changed.iter().map(|p| p.desired.phone_id).collect();
            let common = common_proof(&before, &ids)?;
            let mut targets = Vec::new();
            for p in &changed {
                let mut proof = common.clone();
                if p.prior.as_ref().is_some_and(|b| !b.deleted)
                    || !p.desired.created_sha256.is_empty()
                {
                    proof.created = Some(created_hash(&before, p.desired.phone_id)?);
                    proof.ui = Some(target_ui_digest(&before, p.desired.phone_id)?);
                }
                targets.push(BatchTarget {
                    desired: p.desired.clone(),
                    before: proof,
                    delete: p.desired.deleted,
                });
            }
            let member_ids: Vec<Vec<_>> = targets
                .iter()
                .map(|t| t.desired.members.iter().map(|m| m.phone_id).collect())
                .collect();
            let mutations: Vec<_> = targets
                .iter()
                .zip(&member_ids)
                .map(|(t, tracks)| {
                    if t.delete {
                        mutation::PlaylistMutation::Delete {
                            pid: t.desired.phone_id,
                        }
                    } else if t.before.created.is_some() {
                        mutation::PlaylistMutation::Update {
                            pid: t.desired.phone_id,
                            name: &t.desired.name,
                            tracks,
                        }
                    } else {
                        mutation::PlaylistMutation::Insert {
                            pid: t.desired.phone_id,
                            name: &t.desired.name,
                            tracks,
                        }
                    }
                })
                .collect();
            let operation =
                mutation::change_playlists(&before.current.catalog, &mutations, SystemTime::now())?;
            let first = targets.remove(0);
            let intent = Intent {
                version: 1,
                root: directory,
                desired: first.desired,
                before: first.before,
                delete: first.delete,
                additional: targets,
                operation_sha256: hex(&Sha256::digest(&operation)),
            };
            durable_write(
                &intent.root,
                "input.json",
                &serde_json::to_vec_pretty(&intent)?,
            )?;
            durable_write(&intent.root, "operation.plist", &operation)?;
            let priors: Vec<_> = changed.iter().map(|p| p.prior.clone()).collect();
            store.prepare_many(&priors, &intent)?;
            let digest = intent.digest()?;
            let copied = operation.clone();
            let sent = removal_batch::send_metadata_only(&before,&operation,Requests::None,Sha256::digest(serde_json::to_vec(&intent)?).into(),o.host_name,&intent.root.join("send"),&control,|ready,signature| {
                durable_write(&intent.root,"started.json",&serde_json::to_vec_pretty(&json!({"intent_sha256":digest,"ready_sha256":hex(&ready),"signature_sha256":hex(&Sha256::digest(signature))}))?)?;
                store.started(&intent)?;
                Ok(Permit {operation:copied,signature})
            }).await;
            match sent {
                Sent::Unsent(error) => {
                    store.abort(&intent)?;
                    return Err(error.context("playlist batch was not sent"));
                }
                Sent::Started(Err(error)) => bail!(
                    "playlist batch outcome is uncertain; retry will observe it without replaying it. {error:#}. Evidence: {}",
                    intent.root.display()
                ),
                Sent::Started(Ok(_)) => {}
            }
            crate::engine::observe(crate::engine::Event::VerificationStarting)?;
            let (_, after) =
                removal_batch::capture_fresh(&intent.root.join("after"), &control).await?;
            let verified = verify_all(&after, &intent).with_context(|| {
                format!(
                    "playlist batch requires verification; baselines unchanged. Evidence: {}",
                    intent.root.display()
                )
            })?;
            save_verified(&intent, &verified)?;
            store.commit_many(&intent, &verified)?;
            before = after;
        }
        let unchanged: Vec<_> = planned
            .iter()
            .filter(|p| !p.changed)
            .map(|p| reconcile::Acceptance {
                prior: p.prior.clone(),
                observed: p.desired.clone(),
            })
            .collect();
        if !unchanged.is_empty() {
            store.accept(&unchanged)?;
        }
        report.changed += changed.len();
        report.unchanged += unchanged.len();
        let verified: Vec<_> = planned.iter().map(|p| p.desired.local_id.clone()).collect();
        completed += count;
        progress(Progress {
            completed,
            total,
            batch_size: count,
            verified: &verified,
        })?;
    }
    Ok(report)
}
