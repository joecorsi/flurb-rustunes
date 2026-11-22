//! Read-only reconciliation, followed by explicit acknowledgement after the
//! frontend commits its local playlist transaction. No phone writes here.
use super::*;
mod merge;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistState {
    pub name: String,
    pub tracks: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct LocalPlaylist {
    pub id: String,
    pub value: PlaylistState,
    pub derived: bool,
}
#[derive(Clone, Debug)]
pub struct LocalTrack {
    pub id: String,
    pub path: Option<PathBuf>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    Flurb,
    Iphone,
    Both,
}
#[derive(Clone, Debug)]
pub struct Choice {
    pub id: String,
    pub token: String,
    pub resolution: Resolution,
}
#[derive(Clone, Debug)]
pub struct Conflict {
    pub id: String,
    pub name: String,
    pub reason: String,
    pub token: String,
    pub local: Option<PlaylistState>,
    pub phone: Option<PlaylistState>,
    pub allow_flurb: bool,
    pub allow_iphone: bool,
    pub allow_both: bool,
}
#[derive(Clone, Debug)]
pub struct Change {
    pub id: String,
    pub before: Option<PlaylistState>,
    pub after: Option<PlaylistState>,
}
pub struct Request<'a> {
    pub state: &'a Path,
    pub prefix: &'a str,
    pub tracks: &'a [LocalTrack],
    pub playlists: &'a [LocalPlaylist],
    pub selected: &'a [String],
    pub choices: &'a [Choice],
    pub pending: &'a [(String, Vec<String>)],
    pub seconds: u64,
}
pub struct Report {
    pub changes: Vec<Change>,
    pub exports: Vec<String>,
    /// Managed phone playlists explicitly excluded from the current selection.
    /// Their local playlists and installed audio must remain intact.
    pub removals: Vec<String>,
    /// Exact source file hashes bound to audio verified in this capture.
    pub installed: Vec<String>,
    /// Selected playlists whose desired name and membership were already
    /// verified in this run's fresh phone capture. Audio checks still apply.
    pub verified_unchanged: Vec<String>,
    pub conflicts: Vec<Conflict>,
    pub notices: Vec<String>,
    pub acceptance_json: String,
    pub scope: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Acceptance {
    pub(super) prior: Option<Baseline>,
    pub(super) observed: Baseline,
}

fn state(b: &Baseline) -> Option<PlaylistState> {
    (!b.deleted).then(|| PlaylistState {
        name: b.name.clone(),
        tracks: b.members.iter().map(|m| m.local_id.clone()).collect(),
    })
}
pub(super) fn row_exists(before: &Before, pid: u64) -> Result<bool> {
    let table = before.sql.table("container")?;
    let at = table.column("container_pid")?;
    Ok(table
        .rows
        .keys()
        .any(|r| r[at] == music_protocol::catalogue::sqlite::tables::Cell::Integer(pid as i64)))
}
fn ordinary(before: &Before, pid: u64) -> Result<(String, bool)> {
    ordinary_sql(
        &before.sql,
        before.current.catalog.primary_container_pid(),
        pid,
    )
}
pub(super) fn ordinary_sql(
    sql: &crate::engine::verification::sql_tables::reader::Snapshot,
    primary: u64,
    pid: u64,
) -> Result<(String, bool)> {
    if let Ok(name) = qualify_sql(sql, primary, pid) {
        return Ok((name, true));
    }
    let table = sql.table("container")?;
    let row = table.one("container_pid", pid as i64)?;
    ensure!(pid != 0 && pid != primary, "protected library playlist");
    for field in [
        "distinguished_kind",
        "container_type",
        "is_hidden",
        "parent_pid",
        "smart_is_folder",
        "smart_is_dynamic",
        "smart_is_filtered",
        "smart_is_genius",
        "is_external_vendor_playlist",
        "is_collaborative",
        "cloud_is_subscribed",
        "store_cloud_id",
        "sync_id",
        "is_src_remote",
    ] {
        ensure!(
            table.at(row, field)?.integer()? == 0,
            "protected/cloud playlist: {field}"
        );
    }
    ensure!(
        table.at(row, "cloud_global_id")?.text()?.is_empty()
            && *table.at(row, "smart_criteria")?
                == music_protocol::catalogue::sqlite::tables::Cell::Null,
        "protected cloud/smart playlist"
    );
    Ok((table.at(row, "name")?.text()?.to_owned(), false))
}
pub(super) fn phone_order(before: &Before, pid: u64) -> Result<Vec<u64>> {
    let tracks = phone_order_sql(&before.sql, pid)?;
    ensure!(
        tracks
            .iter()
            .all(|id| before.snapshot.index.other_audio().contains_key(id)),
        "playlist track has no supported installed audio"
    );
    Ok(tracks)
}
pub(super) fn phone_order_sql(
    sql: &crate::engine::verification::sql_tables::reader::Snapshot,
    pid: u64,
) -> Result<Vec<u64>> {
    let members = sql.table("container_item")?;
    let store = sql.table("item_store")?;
    let mut result = Vec::new();
    let mut positions = BTreeSet::new();
    let mut tracks = BTreeSet::new();
    for (row, count) in &members.rows {
        if members.at(row, "container_pid")?.integer()? as u64 != pid {
            continue;
        }
        let position = members.at(row, "position")?.integer()?;
        ensure!(
            *count == 1 && position >= 0 && positions.insert(position),
            "ambiguous playlist positions"
        );
        let item = members.at(row, "item_pid")?.integer()?;
        let id = store
            .at(store.one("item_pid", item)?, "sync_id")?
            .integer()? as u64;
        ensure!(
            id != 0 && tracks.insert(id),
            "playlist contains cloud, unmapped, or repeated tracks"
        );
        result.push((position, id));
        ensure!(
            result.len() <= 499,
            "playlist exceeds the 499-track sync limit"
        );
    }
    result.sort_unstable();
    Ok(result.into_iter().map(|(_, id)| id).collect())
}
fn path_key(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
fn track_map(
    o: &Request<'_>,
    before: &Before,
    baselines: &[Baseline],
    journal: Option<&JournalMatches>,
) -> Result<BTreeMap<u64, String>> {
    let ids: BTreeSet<_> = o.tracks.iter().map(|t| t.id.as_str()).collect();
    ensure!(
        ids.len() == o.tracks.len() && ids.len() <= 50_000 && ids.iter().all(|id| valid_id(id)),
        "ambiguous or excessive local track inventory"
    );
    let mut paths: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    for track in o.tracks {
        if let Some(path) = &track.path {
            paths
                .entry(path_key(path))
                .or_default()
                .insert(track.id.clone());
        }
    }
    let mut candidates: BTreeMap<u64, BTreeSet<String>> = BTreeMap::new();
    for baseline in baselines {
        for member in &baseline.members {
            if before
                .snapshot
                .index
                .other_audio()
                .get(&member.phone_id)
                .is_some_and(|f| hex(&f.full_file_sha256) == member.file_sha256)
            {
                candidates
                    .entry(member.phone_id)
                    .or_default()
                    .insert(member.local_id.clone());
            }
        }
    }
    if let Some(journal) = journal {
        for binding in &journal.bindings {
            let id = binding.content.track_pid;
            if !before.snapshot.index.matches_transferred_source(
                id,
                &binding.content.path,
                &binding.content.source,
            ) {
                continue;
            }
            if let Some(path) = journal.sources.get(&id) {
                if let Some(local) = paths.get(&path_key(path)) {
                    candidates
                        .entry(id)
                        .or_default()
                        .extend(local.iter().cloned());
                }
            }
        }
    }
    Ok(candidates
        .into_iter()
        .filter_map(|(id, mut values)| {
            (values.len() == 1).then(|| (id, values.pop_first().unwrap()))
        })
        .collect())
}
fn installed_sources(before: &Before, journal: Option<&JournalMatches>) -> Vec<String> {
    let mut hashes = BTreeSet::new();
    if let Some(journal) = journal {
        for binding in &journal.bindings {
            if before.snapshot.index.matches_transferred_source(
                binding.content.track_pid,
                &binding.content.path,
                &binding.content.source,
            ) {
                hashes.insert(hex(&binding.content.source.full_file_sha256));
            }
        }
    }
    hashes.into_iter().collect()
}

fn observed(
    before: &Before,
    prior: Option<&Baseline>,
    id: &str,
    pid: u64,
    mapping: &BTreeMap<u64, String>,
    valid_ids: &BTreeSet<&str>,
) -> Result<Baseline> {
    let device = hex(&before.snapshot.identity.device_identifier_sha256);
    let library = hex(&before.snapshot.identity.library_identity_sha256);
    if !row_exists(before, pid)? {
        let old = prior.context("new playlist disappeared")?;
        let mut value = old.clone();
        value.deleted = true;
        value.members.clear();
        value.verified_anchor = before.anchor();
        value.verified_ns = now_ns()?;
        return Ok(value);
    }
    let (name, mutable) = ordinary(before, pid)?;
    ensure!(
        !name.trim().is_empty() && name.len() <= 255 && !name.chars().any(char::is_control),
        "invalid iPhone playlist name"
    );
    let created = created_hash(before, pid)?;
    if let Some(old) = prior {
        ensure!(
            old.created_sha256 == created,
            "playlist identity changed on the phone; refusing to reuse its ID"
        );
    }
    let members=phone_order(before,pid)?.into_iter().map(|wire|{
        let file=&before.snapshot.index.other_audio()[&wire];
        let known=prior.and_then(|b|b.members.iter().find(|m|m.phone_id==wire && m.file_sha256==hex(&file.full_file_sha256))).map(|m|&m.local_id);
        let local=known.or_else(||mapping.get(&wire)).with_context(||format!("Cannot uniquely match “{}” to this Flurb library. Add or relink it before syncing this playlist.",before.current.catalog.tracks().iter().find(|t|t.sync_id==wire).map(|t|t.metadata.title.as_str()).unwrap_or("Unknown track")))?;
        ensure!(valid_ids.contains(local.as_str()) || prior.is_some_and(|b|b.members.iter().any(|m|m.phone_id==wire && m.local_id==*local)),"This iPhone playlist references a track removed from Flurb. Add or relink it before importing.");
        Ok(Member{local_id:local.clone(),phone_id:wire,file_sha256:hex(&file.full_file_sha256),source:String::new()})
    }).collect::<Result<Vec<_>>>()?;
    Ok(Baseline {
        version: 1,
        device,
        library,
        local_id: id.into(),
        phone_id: pid,
        name,
        members,
        created_sha256: created,
        verified_anchor: before.anchor(),
        verified_ns: now_ns()?,
        deleted: false,
        mutable,
    })
}

fn effective_selected(o: &Request<'_>, scope: &str) -> Result<Vec<String>> {
    let mut ids: BTreeSet<String> = o.selected.iter().cloned().collect();
    for (saved_scope, saved_ids) in o.pending {
        if saved_scope == scope {
            ids.extend(
                saved_ids
                    .iter()
                    .filter(|id| id.starts_with(o.prefix))
                    .cloned(),
            );
        }
    }
    ensure!(
        ids.iter()
            .all(|id| id.starts_with(o.prefix) && valid_id(id)),
        "invalid selected playlist identity"
    );
    Ok(ids.into_iter().collect())
}
pub(crate) async fn run(o: Request<'_>, engine: &crate::engine::Engine) -> Result<Report> {
    run_once(o, engine, false).await
}
async fn run_once(
    o: Request<'_>,
    engine: &crate::engine::Engine,
    recovered: bool,
) -> Result<Report> {
    ensure!(
        valid_id(o.prefix) && o.prefix.ends_with(':') && (1..=1800).contains(&o.seconds),
        "invalid reconciliation request"
    );
    recover(o.state, o.seconds).await?;
    let store = Store::open(o.state)?;
    let root = o.state.join("playlist-sync-evidence").join(format!(
        "incoming-{}-{}",
        now_ns()?,
        std::process::id()
    ));
    layout::private_directory(root.parent().unwrap())?;
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(o.seconds),
        cancelled: crate::engine::cancellation(),
    };
    let (identity, before) = removal_batch::capture_for_reconciliation(&root, &control).await?;
    let device = hex(&identity.device_digest());
    let library = hex(&identity.library_digest());
    let scope = format!("{device}/{library}");
    // Reuse this fresh capture to discover an interrupted native audio batch.
    // Recovery observes the recorded operation; it never blindly resends it.
    let journal_path = layout::journal_path(
        o.state,
        &before.snapshot.identity.device_identifier_sha256,
        &before.snapshot.identity.library_identity_sha256,
    )?;
    if journal_path.join("cloud-import").try_exists()? {
        drop(store);drop(before);
        engine.pull(crate::engine::PullRequest{state:o.state,library:None,recursive:false,dry_run:false,seconds:o.seconds,tracks_into:None}).await.context("confirming a cloud-restored device snapshot against the connected iPhone")?;
        ensure!(!journal_path.join("cloud-import").try_exists()?,"cloud-restored journal was not refreshed; no transfer started");
        return Box::pin(run_once(o,engine,recovered)).await;
    }
    let pending = if journal_path.join("journal.sqlite").try_exists()? {
        let journal = Journal::open_read_only(&journal_path, &identity)?;
        journal.outstanding_insert_batch()?.is_some()
            || journal.outstanding_metadata_batch()?.is_some()
            || journal.outstanding_removal_batch()?.is_some()
            || journal.outstanding_attempt()?.is_some()
    } else {
        false
    };
    if pending {
        ensure!(
            !recovered,
            "the interrupted phone operation still needs reconciliation; no new transfer started"
        );
        crate::engine::observe(crate::engine::Event::Observation(
            crate::engine::Observation::CapturingCatalogue,
        ))?;
        let snapshot = before.directory.join("snapshot");
        let trace = root.join("recover-audio");
        let result = engine
            .recover(crate::engine::RecoveryRequest {
                snapshot: &snapshot,
                journal: &journal_path,
                run_dir: None,
                trace: &trace,
                cancel_prepared: None,
                selected_device: None,
            })
            .await?;
        ensure!(
            result.successful,
            "the interrupted operation was observed but could not be safely settled; no new transfer started"
        );
        drop(store);
        drop(before);
        return Box::pin(run_once(o, engine, true)).await;
    }
    ensure!(
        before.snapshot.index.unfilled().is_empty() && before.snapshot.index.gone().is_empty(),
        "the iPhone has unavailable audio without a recoverable operation; no new transfer started"
    );
    let explicit_ids = o.selected.to_vec();
    let effective_ids = effective_selected(&o, &scope)?;
    let o = Request {
        selected: &effective_ids,
        ..o
    };

    let all = store.list(&device, &library)?;
    let scoped: Vec<_> = all
        .iter()
        .filter(|b| b.local_id.starts_with(o.prefix))
        .cloned()
        .collect();
    let journal = journal_matches(o.state, &before, &identity)?;
    let mapping = track_map(&o, &before, &scoped, journal.as_ref())?;
    let valid_ids: BTreeSet<_> = o.tracks.iter().map(|t| t.id.as_str()).collect();
    let locals: BTreeMap<_, _> = o.playlists.iter().map(|p| (p.id.as_str(), p)).collect();
    ensure!(
        locals.len() == o.playlists.len()
            && locals
                .keys()
                .all(|id| id.starts_with(o.prefix) && valid_id(id)),
        "invalid local playlist inventory"
    );
    let mut report = Report {
        changes: vec![],
        exports: vec![],
        removals: vec![],
        installed: installed_sources(&before, journal.as_ref()),
        verified_unchanged: vec![],
        conflicts: vec![],
        notices: vec![],
        acceptance_json: String::new(),
        scope,
    };
    let mut accepted = Vec::new();
    let mut handled = BTreeSet::new();
    let mut names: BTreeSet<String> = o
        .playlists
        .iter()
        .map(|p| p.value.name.to_ascii_lowercase())
        .collect();
    let table = before.sql.table("container")?;
    for row in table.rows.keys() {
        if let Ok(name) = table.at(row, "name").and_then(|v| v.text()) {
            names.insert(name.to_ascii_lowercase());
        }
    }
    for base in &scoped {
        handled.insert(base.phone_id);
        let local = locals.get(base.local_id.as_str()).copied();
        if local.is_some() && !explicit_ids.contains(&base.local_id) {
            if removal_requested(base, local, &explicit_ids, o.prefix) {
                if row_exists(&before, base.phone_id)? {
                    report.removals.push(base.local_id.clone());
                    report.exports.push(base.local_id.clone());
                } else {
                    // A phone-side deletion already satisfied deselection.
                    // Record absence without deleting the Flurb playlist.
                    accepted.push(Acceptance {
                        prior: Some(base.clone()),
                        observed: observed(
                            &before,
                            Some(base),
                            &base.local_id,
                            base.phone_id,
                            &mapping,
                            &valid_ids,
                        )?,
                    });
                }
            }
            continue;
        }
        let remote = observed(
            &before,
            Some(base),
            &base.local_id,
            base.phone_id,
            &mapping,
            &valid_ids,
        )
        .with_context(|| format!("Reading iPhone playlist “{}”", base.name))?;
        reconcile_one(
            &o,
            Some(base),
            local,
            remote,
            &mut names,
            &mut report,
            &mut accepted,
        )?;
    }
    for row in table.rows.keys() {
        let pid = table.at(row, "container_pid")?.integer()? as u64;
        if handled.contains(&pid)
            || all.iter().any(|b| b.phone_id == pid)
            || ordinary(&before, pid).is_err()
        {
            continue;
        }
        let name = table.at(row, "name")?.text()?.to_owned();
        let id = format!(
            "{}iphone:{}:{}:{pid:016x}:{}",
            o.prefix,
            &device[..12],
            &library[..12],
            &created_hash(&before, pid)?[..12]
        );
        // An old verified Flurb creation can be adopted by its existing local
        // playlist. An unlinked same-name playlist requires explicit resolution.
        let matches: Vec<_> = o
            .playlists
            .iter()
            .filter(|p| {
                !p.derived
                    && p.value.name.eq_ignore_ascii_case(&name)
                    && !scoped.iter().any(|b| b.local_id == p.id)
            })
            .collect();
        let (local, target) = if matches.len() == 1 {
            (Some(matches[0]), matches[0].id.as_str())
        } else {
            (None, id.as_str())
        };
        match observed(&before, None, target, pid, &mapping, &valid_ids) {
            Ok(remote) => reconcile_one(
                &o,
                None,
                local,
                remote,
                &mut names,
                &mut report,
                &mut accepted,
            )?,
            Err(e) => report
                .notices
                .push(format!("“{name}” was not imported: {e:#}")),
        }
    }
    // Selected local playlists absent from the phone are still normal exports.
    for id in o.selected {
        if !scoped.iter().any(|b| b.local_id == *id) || explicit_ids.contains(id) {
            if locals.contains_key(id.as_str())
                && !report.exports.contains(id)
                && !accepted
                    .iter()
                    .any(|a: &Acceptance| a.observed.local_id == *id)
                && !report.conflicts.iter().any(|c| c.id == *id)
            {
                report.exports.push(id.clone());
            }
        }
    }
    report.exports.sort();
    report.exports.dedup();
    report.acceptance_json = serde_json::to_string(&accepted)?;
    ensure!(
        report.acceptance_json.len() <= 16 * 1024 * 1024,
        "incoming playlist receipt exceeds size limit"
    );
    Ok(report)
}

fn removal_requested(
    base: &Baseline,
    local: Option<&LocalPlaylist>,
    selected: &[String],
    prefix: &str,
) -> bool {
    local.is_some()
        && base.mutable
        && !base.deleted
        && !selected.contains(&base.local_id)
        && !base
            .local_id
            .strip_prefix(prefix)
            .is_some_and(|id| matches!(id, "collection:favorites" | "collection:recently-added"))
}

fn reconcile_one(
    o: &Request<'_>,
    base: Option<&Baseline>,
    local: Option<&LocalPlaylist>,
    remote: Baseline,
    names: &mut BTreeSet<String>,
    report: &mut Report,
    accepted: &mut Vec<Acceptance>,
) -> Result<()> {
    let local_state = local.map(|p| p.value.clone());
    let phone_state = state(&remote);
    let base_state = base.and_then(state);
    let mut proposed = merge::merge(&base_state, &local_state, &phone_state);
    if local.is_some_and(|p| p.derived) && proposed.as_ref().is_ok_and(|p| *p != local_state) {
        proposed=Err("A smart playlist cannot take phone-side membership changes. Keep both to save them as a regular playlist.".into());
    }
    if !remote.mutable && !remote.deleted && proposed.as_ref().is_ok_and(|p| *p != phone_state) {
        proposed=Err("This playlist is managed by the iPhone. Keep both to sync a separate copy of Flurb’s edits.".into());
    }
    let token = hex(&Sha256::digest(serde_json::to_vec(
        &json!({"id":remote.local_id,"device":remote.device,"library":remote.library,"pid":format!("{:016x}",remote.phone_id),"base":base_state,"local":local_state,"phone":phone_state,"created":remote.created_sha256,"members":remote.members,"mutable":remote.mutable}),
    )?));
    let result = match proposed {
        Ok(result) => result,
        Err(reason) => {
            let allow_flurb = remote.mutable || remote.deleted;
            let allow_iphone = !local.is_some_and(|p| p.derived);
            let allow_both = local_state.is_some() && phone_state.is_some();
            let choice = o
                .choices
                .iter()
                .find(|c| c.id == remote.local_id && c.token == token);
            match choice.map(|c| c.resolution) {
                Some(Resolution::Flurb) if allow_flurb => local_state.clone(),
                Some(Resolution::Iphone) if allow_iphone => phone_state.clone(),
                Some(Resolution::Both) if allow_both => {
                    let derived = local.is_some_and(|p| p.derived);
                    let original = if derived {
                        phone_state.as_ref().unwrap()
                    } else {
                        local_state.as_ref().unwrap()
                    };
                    let mut copy = original.clone();
                    copy.name = unique_name(
                        &original.name,
                        names,
                        if derived { "iPhone" } else { "Flurb" },
                    );
                    let id = format!("{}conflict:{}", o.prefix, &token[..32]);
                    report.changes.push(Change {
                        id: id.clone(),
                        before: None,
                        after: Some(copy),
                    });
                    report.exports.push(id);
                    if derived {
                        local_state.clone()
                    } else {
                        phone_state.clone()
                    }
                }
                _ => {
                    report.conflicts.push(Conflict {
                        id: remote.local_id.clone(),
                        name: local_state
                            .as_ref()
                            .or(phone_state.as_ref())
                            .map(|s| s.name.clone())
                            .unwrap_or_else(|| remote.name.clone()),
                        reason,
                        token,
                        local: local_state,
                        phone: phone_state,
                        allow_flurb,
                        allow_iphone,
                        allow_both,
                    });
                    return Ok(());
                }
            }
        }
    };
    if result != local_state {
        report.changes.push(Change {
            id: remote.local_id.clone(),
            before: local_state,
            after: result.clone(),
        });
    }
    if (remote.mutable || remote.deleted) && result != phone_state {
        report.exports.push(remote.local_id.clone());
    } else if o.selected.contains(&remote.local_id) && remote.mutable && result.is_some() {
        report.exports.push(remote.local_id.clone());
        report.verified_unchanged.push(remote.local_id.clone());
    }
    accepted.push(Acceptance {
        prior: base.cloned(),
        observed: remote,
    });
    Ok(())
}
fn unique_name(original: &str, names: &mut BTreeSet<String>, suffix: &str) -> String {
    let mut stem = original.to_owned();
    while stem.len() > 220 {
        stem.pop();
    }
    for n in 1.. {
        let name = if n == 1 {
            format!("{stem} ({suffix})")
        } else {
            format!("{stem} ({suffix} {n})")
        };
        if names.insert(name.to_ascii_lowercase()) {
            return name;
        }
    }
    unreachable!()
}

/// Acknowledgement is idempotent and CAS-protected. Call only after the
/// frontend durably commits the exact local changes and this receipt together.
pub(crate) fn accept(state: &Path, receipt: &str) -> Result<()> {
    ensure!(
        receipt.len() <= 16 * 1024 * 1024,
        "incoming receipt exceeds bound"
    );
    let accepted: Vec<Acceptance> = serde_json::from_str(receipt)?;
    ensure!(
        accepted.len() <= 5000,
        "incoming playlist count exceeds bound"
    );
    Store::open(state)?.accept(&accepted)
}

#[cfg(test)]
mod tests;
