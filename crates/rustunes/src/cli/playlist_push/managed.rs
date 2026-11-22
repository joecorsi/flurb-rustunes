//! Stable-ID playlist synchronization. Only verified phone state advances the
//! baseline; a Started operation is observed on retry, never replayed blindly.
use super::*;
use serde::{Deserialize, Serialize};

pub(crate) mod batch;
pub(crate) mod favorites;
pub(crate) mod reconcile;
mod store;
use store::Store;

pub struct Request<'a> {
    pub playlist: &'a Path,
    pub local_id: &'a str,
    pub local_track_ids: &'a [String],
    pub name: &'a str,
    pub state: &'a Path,
    pub host_name: &'a str,
    pub seconds: u64,
}
#[derive(Debug)]
pub struct Report {
    pub playlist_id: u64,
    pub tracks: usize,
    pub changed: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    local_id: String,
    #[serde(with = "wire_id")]
    phone_id: u64,
    file_sha256: String,
    source: String,
}
fn yes() -> bool {
    true
}
fn is_yes(v: &bool) -> bool {
    *v
}
fn is_no(v: &bool) -> bool {
    !*v
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    version: u8,
    device: String,
    library: String,
    local_id: String,
    #[serde(with = "wire_id")]
    phone_id: u64,
    name: String,
    members: Vec<Member>,
    created_sha256: String,
    verified_anchor: u32,
    verified_ns: String,
    #[serde(default, skip_serializing_if = "is_no")]
    deleted: bool,
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    mutable: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    base_anchor: u32,
    files: String,
    tracks: String,
    other_playlists: String,
    created: Option<String>,
    ui: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchTarget {
    desired: Baseline,
    before: Proof,
    delete: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    version: u8,
    root: PathBuf,
    desired: Baseline,
    before: Proof,
    operation_sha256: String,
    #[serde(default, skip_serializing_if = "is_no")]
    delete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    additional: Vec<BatchTarget>,
}
impl Intent {
    fn targets(&self) -> impl Iterator<Item = (&Baseline, &Proof, bool)> {
        std::iter::once((&self.desired, &self.before, self.delete)).chain(
            self.additional
                .iter()
                .map(|t| (&t.desired, &t.before, t.delete)),
        )
    }
    fn digest(&self) -> Result<String> {
        Ok(hex(&Sha256::digest(serde_json::to_vec(self)?)))
    }
}
fn now_ns() -> Result<String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string())
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control)
}
fn validate_request(o: &Request<'_>, selected: &[PathBuf]) -> Result<()> {
    ensure!(valid_id(o.local_id), "invalid local playlist identity");
    ensure!(
        !o.name.trim().is_empty() && o.name.len() <= 255 && !o.name.chars().any(char::is_control),
        "invalid playlist name"
    );
    ensure!((1..=1800).contains(&o.seconds), "invalid playlist deadline");
    ensure!(
        selected.len() <= 499 && o.local_track_ids.len() == selected.len(),
        "playlist track identities and paths differ"
    );
    let mut unique = BTreeSet::new();
    for id in o.local_track_ids {
        ensure!(
            valid_id(id) && unique.insert(id),
            "playlist local track identity is invalid or repeated"
        );
    }
    Ok(())
}
fn ordered(before: &Before, pid: u64) -> Result<Vec<u64>> {
    ordered_sql(&before.sql, pid)
}
fn ordered_sql(
    sql: &crate::engine::verification::sql_tables::reader::Snapshot,
    pid: u64,
) -> Result<Vec<u64>> {
    let members = sql.table("container_item")?;
    let mut found = Vec::new();
    for (row, n) in &members.rows {
        if members.at(row, "container_pid")?.integer()? == pid as i64 {
            ensure!(*n == 1, "duplicate playlist membership");
            let item = members.at(row, "item_pid")?.integer()?;
            let store = sql.table("item_store")?;
            let wire = store
                .at(store.one("item_pid", item)?, "sync_id")?
                .integer()? as u64;
            ensure!(wire != 0, "playlist contains a cloud/unmapped track");
            found.push((members.at(row, "position")?.integer()?, wire));
        }
    }
    found.sort_unstable();
    ensure!(
        found.len() <= 499
            && found
                .iter()
                .enumerate()
                .all(|(i, (pos, _))| *pos == i as i64),
        "playlist order is noncontiguous or exceeds the supported size"
    );
    let mut unique = BTreeSet::new();
    ensure!(
        found.iter().all(|(_, id)| unique.insert(*id)),
        "playlist contains repeated tracks"
    );
    Ok(found.into_iter().map(|(_, id)| id).collect())
}
fn qualify_target(before: &Before, pid: u64) -> Result<String> {
    qualify_sql(
        &before.sql,
        before.current.catalog.primary_container_pid(),
        pid,
    )
}
fn qualify_sql(
    sql: &crate::engine::verification::sql_tables::reader::Snapshot,
    primary: u64,
    pid: u64,
) -> Result<String> {
    let table = sql.table("container")?;
    let row = table.one("container_pid", pid as i64)?;
    ensure!(
        pid != 0
            && pid != primary
            && table.at(row, "sync_id")?.integer()? as u64 == pid
            && table.at(row, "is_src_remote")?.integer()? == 1,
        "target is not an ordinary synced playlist"
    );
    for key in [
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
    ] {
        ensure!(
            table.at(row, key)?.integer()? == 0,
            "protected playlist field {key} prevents updating it"
        );
    }
    ensure!(
        *table.at(row, "smart_criteria")? == music_protocol::catalogue::sqlite::tables::Cell::Null
            && table.at(row, "cloud_global_id")?.text()?.is_empty(),
        "smart/cloud playlists cannot be managed"
    );
    Ok(table.at(row, "name")?.text()?.to_owned())
}
fn created_hash(before: &Before, pid: u64) -> Result<String> {
    let table = before.sql.table("container")?;
    Ok(hex(&Sha256::digest(serde_json::to_vec(table.at(
        table.one("container_pid", pid as i64)?,
        "date_created",
    )?)?)))
}
fn table_digest(before: &Before, tables: &[&str], exclude: Option<u64>) -> Result<String> {
    sql_digest(&before.sql, tables, exclude)
}
fn sql_digest(
    sql: &crate::engine::verification::sql_tables::reader::Snapshot,
    tables: &[&str],
    exclude: Option<u64>,
) -> Result<String> {
    sql_digest_excluding(sql, tables, &exclude.into_iter().collect())
}
fn sql_digest_excluding(
    sql: &crate::engine::verification::sql_tables::reader::Snapshot,
    tables: &[&str],
    exclude: &BTreeSet<u64>,
) -> Result<String> {
    let mut digest = Sha256::new();
    for name in tables {
        let table = sql.table(name)?;
        let owner = (!exclude.is_empty())
            .then(|| table.column("container_pid"))
            .transpose()?;
        let keep = if *name == "container" {
            table.columns.iter().position(|n| n == "keep_local_status")
        } else {
            None
        };
        let mut rows = Vec::new();
        for (row, count) in &table.rows {
            if owner.is_some_and(|at| matches!(row[at], music_protocol::catalogue::sqlite::tables::Cell::Integer(pid) if exclude.contains(&(pid as u64)))) {
                continue;
            }
            let mut row = row.clone();
            // iOS may evaluate keep-local bookkeeping for untouched playlists.
            if let Some(at) = keep
                && matches!(
                    row[at],
                    music_protocol::catalogue::sqlite::tables::Cell::Integer(0 | 2)
                )
            {
                row[at] = music_protocol::catalogue::sqlite::tables::Cell::Integer(0);
            }
            rows.push((row, *count));
        }
        digest.update(serde_json::to_vec(&(name, &table.columns, rows))?);
    }
    Ok(hex(&digest.finalize()))
}
fn files_digest(before: &Before) -> Result<String> {
    let mut files = Vec::new();
    for (id, file) in before.snapshot.index.tracks() {
        files.push((
            *id,
            file.afc_path.clone(),
            file.fingerprint.file_bytes,
            hex(&file.fingerprint.full_file_sha256),
        ));
    }
    for (id, file) in before.snapshot.index.other_audio() {
        files.push((
            *id,
            file.afc_path.clone(),
            file.file_bytes,
            hex(&file.full_file_sha256),
        ));
    }
    files.sort();
    Ok(hex(&Sha256::digest(serde_json::to_vec(&files)?)))
}
fn proof(before: &Before, pid: u64, updating: bool) -> Result<Proof> {
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
        other_playlists: table_digest(
            before,
            &[
                "container",
                "container_item",
                "container_item_media_type",
                "container_seed",
            ],
            Some(pid),
        )?,
        created: updating.then(|| created_hash(before, pid)).transpose()?,
        ui: updating
            .then(|| target_ui_digest(before, pid))
            .transpose()?,
    })
}
fn target_ui_digest(before: &Before, pid: u64) -> Result<String> {
    ui_digest(&before.sql, pid)
}
fn ui_digest(
    sql: &crate::engine::verification::sql_tables::reader::Snapshot,
    pid: u64,
) -> Result<String> {
    // container_ui is a wire dictionary; iOS stores its fields on container.
    let table = sql.table("container")?;
    let row = table.one("container_pid", pid as i64)?;
    let values = [
        "play_order",
        "album_field_order",
        "repeat_mode",
        "is_reversed",
        "shuffle_items",
        "has_been_shuffled",
    ]
    .into_iter()
    .map(|key| Ok((key, table.at(row, key)?.clone())))
    .collect::<Result<Vec<_>>>()?;
    Ok(hex(&Sha256::digest(serde_json::to_vec(&values)?)))
}
fn verify_intent(after: &Before, intent: &Intent) -> Result<Baseline> {
    ensure!(
        intent.additional.is_empty(),
        "use batch verification for a playlist batch"
    );
    Ok(verify_all(after, intent)?.remove(0))
}
fn verify_all(after: &Before, intent: &Intent) -> Result<Vec<Baseline>> {
    let targets: Vec<_> = intent.targets().collect();
    let excluded = targets.iter().map(|(d, _, _)| d.phone_id).collect();
    let common = batch::common_proof(after, &excluded)?;
    ensure!(
        after.anchor()
            == intent
                .before
                .base_anchor
                .checked_add(1)
                .context("anchor overflow")?,
        "playlist sync anchor did not advance exactly once"
    );
    ensure!(
        common.files == intent.before.files
            && common.tracks == intent.before.tracks
            && common.other_playlists == intent.before.other_playlists,
        "playlist batch changed audio, track metadata or an unrelated playlist"
    );
    let mut verified = Vec::with_capacity(targets.len());
    for (desired, prior, delete) in targets {
        ensure!(
            hex(&after.snapshot.identity.device_identifier_sha256) == desired.device
                && hex(&after.snapshot.identity.library_identity_sha256) == desired.library,
            "verification saw another phone/library"
        );
        ensure!(
            prior.base_anchor == intent.before.base_anchor
                && prior.files == intent.before.files
                && prior.tracks == intent.before.tracks
                && prior.other_playlists == intent.before.other_playlists,
            "playlist batch proof differs between targets"
        );
        if delete {
            ensure!(
                !reconcile::row_exists(after, desired.phone_id)?,
                "deleted playlist is still present"
            );
        } else {
            ensure!(
                qualify_target(after, desired.phone_id)? == desired.name,
                "playlist name differs after sync: {}",
                desired.name
            );
            ensure!(
                ordered(after, desired.phone_id)?
                    == desired
                        .members
                        .iter()
                        .map(|m| m.phone_id)
                        .collect::<Vec<_>>(),
                "playlist ordered membership differs after sync: {}",
                desired.name
            );
            for member in &desired.members {
                let file = after
                    .snapshot
                    .index
                    .other_audio()
                    .get(&member.phone_id)
                    .context("playlist track audio disappeared")?;
                ensure!(
                    hex(&file.full_file_sha256) == member.file_sha256,
                    "playlist track file changed"
                );
            }
            if let Some(created) = &prior.created {
                ensure!(
                    &created_hash(after, desired.phone_id)? == created,
                    "playlist creation identity changed"
                );
            }
            if let Some(ui) = &prior.ui {
                ensure!(
                    &target_ui_digest(after, desired.phone_id)? == ui,
                    "playlist update changed UI settings"
                );
            }
        }
        let mut baseline = desired.clone();
        if !delete {
            baseline.created_sha256 = created_hash(after, desired.phone_id)?;
        }
        baseline.verified_anchor = after.anchor();
        baseline.verified_ns = now_ns()?;
        verified.push(baseline);
    }
    Ok(verified)
}
fn check_baseline(before: &Before, baseline: &Baseline) -> Result<()> {
    ensure!(
        qualify_target(before, baseline.phone_id)? == baseline.name,
        "the linked playlist was renamed on the iPhone; two-way merging is not enabled yet"
    );
    ensure!(
        created_hash(before, baseline.phone_id)? == baseline.created_sha256,
        "the linked playlist identity changed"
    );
    ensure!(
        reconcile::phone_order(before, baseline.phone_id)?
            == baseline
                .members
                .iter()
                .map(|m| m.phone_id)
                .collect::<Vec<_>>(),
        "the playlist was edited on the iPhone since the last sync; preserving those edits until two-way sync is available"
    );
    // Audio sync may have just refreshed tags/artwork on a verified binding.
    // The playlist baseline protects membership/name; the fresh before/after
    // proof independently requires all audio bytes to survive this operation.
    Ok(())
}
fn collision(before: &Before, name: &str, own: Option<u64>) -> Result<Option<u64>> {
    let table = before.sql.table("container")?;
    let mut found = None;
    for (row, n) in &table.rows {
        ensure!(*n == 1, "duplicate container row");
        if table
            .at(row, "name")?
            .text()
            .is_ok_and(|v| v.eq_ignore_ascii_case(name))
        {
            let id = table.at(row, "container_pid")?.integer()? as u64;
            if Some(id) != own {
                ensure!(
                    found.replace(id).is_none(),
                    "multiple iPhone playlists have this name"
                );
            }
        }
    }
    Ok(found)
}
fn legacy_baseline(
    before: &Before,
    state: &Path,
    device: &str,
    library: &str,
    local: &str,
    name: &str,
    pid: u64,
) -> Result<Option<Baseline>> {
    let trials = state.join("playlist-trials");
    if !trials.exists() {
        return Ok(None);
    }
    for entry in fs::read_dir(trials)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let root = entry.path();
        if !root.join("verified.json").is_file() {
            continue;
        }
        let input: Value = serde_json::from_slice(&bounded(&root.join("input.json"))?)?;
        let verified: Value = serde_json::from_slice(&bounded(&root.join("verified.json"))?)?;
        if input["device_sha256"] != device
            || input["library_sha256"] != library
            || input["pid"] != format!("{pid:016x}")
            || verified["playlist_id"] != format!("{pid:016x}")
            || verified["status"] != "verified"
            || verified["name"] != name
        {
            continue;
        }
        let members = input["members"]
            .as_array()
            .context("legacy playlist members absent")?
            .iter()
            .map(|v| {
                u64::from_str_radix(v.as_str().context("invalid legacy member")?, 16)
                    .map_err(Into::into)
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            ordered(before, pid)? == members,
            "the earlier Flurb playlist was edited on the iPhone; refusing to overwrite it"
        );
        ensure!(
            qualify_target(before, pid)? == name,
            "legacy playlist name changed"
        );
        return Ok(Some(Baseline {
            version: 1,
            device: device.into(),
            library: library.into(),
            local_id: local.into(),
            phone_id: pid,
            name: name.into(),
            members: members
                .into_iter()
                .map(|id| {
                    let file = before
                        .snapshot
                        .index
                        .other_audio()
                        .get(&id)
                        .context("legacy playlist audio absent")?;
                    Ok(Member {
                        local_id: String::new(),
                        phone_id: id,
                        file_sha256: hex(&file.full_file_sha256),
                        source: String::new(),
                    })
                })
                .collect::<Result<_>>()?,
            created_sha256: created_hash(before, pid)?,
            verified_anchor: before.anchor(),
            verified_ns: now_ns()?,
            deleted: false,
            mutable: true,
        }));
    }
    Ok(None)
}
fn bounded(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= 2 * 1024 * 1024,
        "playlist state is not a bounded regular file"
    );
    Ok(fs::read(path)?)
}

/// Reconcile only our own interrupted operation before any new audio sync.
/// A durable verified receipt or an exact freshly observed outcome can commit
/// the baseline; an uncertain write is never repeated automatically.
pub(crate) async fn recover(state: &Path, seconds: u64) -> Result<()> {
    let mut store = Store::open(state)?;
    let mut pending = Vec::new();
    for intent in store.pending()? {
        if store.was_started(&intent)? {
            pending.push(intent);
        } else {
            // FULL-synchronized Started is required before a native permit
            // can be returned. Prepared alone therefore proves no send.
            store.abort(&intent)?;
        }
    }
    if pending.is_empty() {
        return Ok(());
    }
    let root = state.join("playlist-sync-evidence").join(format!(
        "reconcile-{}-{}",
        now_ns()?,
        std::process::id()
    ));
    layout::private_directory(root.parent().unwrap())?;
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(seconds),
        cancelled: crate::engine::cancellation(),
    };
    let (identity, before) = removal_batch::capture_fresh(&root, &control).await?;
    for intent in pending {
        if intent.desired.device != hex(&identity.device_digest())
            || intent.desired.library != hex(&identity.library_digest())
        {
            continue;
        }
        validate_retained_intent(state, &intent)?;
        let digest = intent.digest()?;
        let receipt = intent.root.join("verified.json");
        if receipt.exists() {
            let verified: Value = serde_json::from_slice(&bounded(&receipt)?)?;
            ensure!(
                verified["intent_sha256"] == digest,
                "verified playlist receipt does not bind its intent"
            );
            let baselines = if intent.additional.is_empty() {
                vec![serde_json::from_value(verified["baseline"].clone())?]
            } else {
                serde_json::from_value(verified["baselines"].clone())?
            };
            store.commit_many(&intent, &baselines)?;
        } else {
            let baselines=verify_all(&before,&intent).with_context(||format!("cannot confirm the interrupted playlist batch; nothing was replayed. Evidence: {}",intent.root.display()))?;
            batch::save_verified(&intent, &baselines)?;
            store.commit_many(&intent, &baselines)?;
        }
    }
    Ok(())
}
fn validate_retained_intent(state: &Path, intent: &Intent) -> Result<()> {
    let root = fs::canonicalize(&intent.root)?;
    let allowed = fs::canonicalize(state)?.join("playlist-sync-evidence");
    ensure!(
        root == intent.root && root.starts_with(allowed),
        "playlist recovery evidence is outside its state directory"
    );
    let recorded: Intent = serde_json::from_slice(&bounded(&root.join("input.json"))?)?;
    ensure!(
        recorded.digest()? == intent.digest()?,
        "retained playlist intent differs from pending state"
    );
    let operation = bounded(&root.join("operation.plist"))?;
    ensure!(
        hex(&Sha256::digest(operation)) == intent.operation_sha256,
        "retained playlist operation differs"
    );
    let started: Value = serde_json::from_slice(&bounded(&root.join("started.json"))?)?;
    ensure!(
        started["intent_sha256"] == intent.digest()?,
        "playlist Started receipt does not bind its intent"
    );
    Ok(())
}

mod wire_id {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{value:016x}"))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.len() != 16 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(serde::de::Error::custom("invalid playlist wire identity"));
        }
        u64::from_str_radix(&value, 16).map_err(serde::de::Error::custom)
    }
}

fn save_verified(intent: &Intent, baseline: &Baseline) -> Result<()> {
    durable_write(
        &intent.root,
        "verified.json",
        &serde_json::to_vec_pretty(&json!({"intent_sha256":intent.digest()?,"baseline":baseline}))?,
    )?;
    Ok(())
}

pub(crate) async fn run(o: Request<'_>) -> Result<Report> {
    ensure!(
        fs::metadata(o.playlist)?.len() <= 1024 * 1024,
        "M3U8 exceeds bound"
    );
    let text = fs::read_to_string(o.playlist)?;
    let selected = if text
        .lines()
        .all(|v| v.trim().is_empty() || v.trim().starts_with('#'))
    {
        vec![]
    } else {
        entries(&text)?
    };
    validate_request(&o, &selected)?;
    recover(o.state, o.seconds).await?;
    let mut store = Store::open(o.state)?;
    let parent = o.state.join("playlist-sync-evidence");
    layout::private_directory(&parent)?;
    let root = parent.join(format!("{}-{}", now_ns()?, std::process::id()));
    create_trace(&root)?;
    let root = fs::canonicalize(root)?;
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(o.seconds),
        cancelled: crate::engine::cancellation(),
    };
    let (identity, before) = removal_batch::capture_fresh(&root.join("before"), &control).await?;
    let device = hex(&identity.device_digest());
    let library = hex(&identity.library_digest());
    refuse_unsettled_trial(&o.state.join("playlist-trials"), &device, &library).or_else(|e| {
        if !o.state.join("playlist-trials").exists() {
            Ok(())
        } else {
            Err(e)
        }
    })?;
    let prior = store.get(&device, &library, o.local_id)?;
    let baseline = match &prior {
        Some(b) if !b.deleted => {
            check_baseline(&before, b)?;
            Some(b.clone())
        }
        _ => match collision(&before, o.name, None)? {
            Some(pid) => {
                ensure!(
                    store.owner(&device, &library, pid)?.is_none(),
                    "this iPhone playlist is linked to another Flurb playlist"
                );
                Some(legacy_baseline(&before,o.state,&device,&library,o.local_id,o.name,pid)?.context("an unmanaged iPhone playlist already has this name; refusing to overwrite it")?)
            }
            None => None,
        },
    };
    ensure!(
        collision(&before, o.name, baseline.as_ref().map(|b| b.phone_id))?.is_none(),
        "another iPhone playlist already has the requested name"
    );
    let journal = journal_matches(o.state, &before, &identity)?;
    let members = resolve_members(o.playlist, &selected, &before, journal.as_ref())?;
    let updating = baseline.is_some();
    let pid = match &baseline {
        Some(b) => b.phone_id,
        None => fresh_pid(&before)?,
    };
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
            .zip(&selected)
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
    let ids: Vec<_> = members.iter().map(|(id, _)| *id).collect();
    if baseline.as_ref().is_some_and(|b| {
        b.name == o.name && b.members.iter().map(|m| m.phone_id).collect::<Vec<_>>() == ids
    }) {
        let mut desired = desired;
        desired.created_sha256 = created_hash(&before, pid)?;
        store.noop(prior.as_ref(), &desired)?;
        return Ok(Report {
            playlist_id: pid,
            tracks: ids.len(),
            changed: false,
        });
    }
    ensure!(
        baseline.as_ref().is_none_or(|b| b.mutable),
        "this playlist is managed by the iPhone"
    );
    let operation = if updating {
        mutation::update_playlist(
            &before.current.catalog,
            pid,
            o.name,
            &ids,
            SystemTime::now(),
        )?
    } else {
        mutation::insert_playlist(
            &before.current.catalog,
            pid,
            o.name,
            &ids,
            SystemTime::now(),
        )?
    };
    let intent = Intent {
        version: 1,
        root: root.clone(),
        desired,
        before: proof(&before, pid, updating)?,
        operation_sha256: hex(&Sha256::digest(&operation)),
        delete: false,
        additional: vec![],
    };
    apply(
        &mut store,
        &before,
        intent,
        operation,
        o.host_name,
        prior.as_ref(),
        &control,
    )
    .await
}

async fn apply(
    store: &mut Store,
    before: &Before,
    intent: Intent,
    operation: Vec<u8>,
    host_name: &str,
    prior: Option<&Baseline>,
    control: &Control,
) -> Result<Report> {
    let root = intent.root.clone();
    let pid = intent.desired.phone_id;
    durable_write(&root, "input.json", &serde_json::to_vec_pretty(&intent)?)?;
    durable_write(&root, "operation.plist", &operation)?;
    store.prepare(prior, &intent)?;
    let digest = intent.digest()?;
    let operation_copy = operation.clone();
    let sent=removal_batch::send_metadata_only(before,&operation,Requests::None,Sha256::digest(serde_json::to_vec(&intent)?).into(),host_name,&root.join("send"),control,|ready,signature|{
        durable_write(&root,"started.json",&serde_json::to_vec_pretty(&json!({"intent_sha256":digest,"ready_sha256":hex(&ready),"signature_sha256":hex(&Sha256::digest(signature))}))?)?;
        store.started(&intent)?;
        Ok(Permit{operation:operation_copy,signature})
    }).await;
    match sent {
        Sent::Unsent(error) => {
            store.abort(&intent)?;
            return Err(error.context("playlist was not sent"));
        }
        Sent::Started(Err(error)) => bail!(
            "playlist update outcome is uncertain; retry will observe it, not repeat it. {error:#}. Evidence: {}",
            root.display()
        ),
        Sent::Started(Ok(_)) => {}
    }
    crate::engine::observe(crate::engine::Event::VerificationStarting)?;
    let (_, after) = removal_batch::capture_fresh(&root.join("after"), control).await?;
    let verified = verify_intent(&after, &intent).with_context(|| {
        format!(
            "playlist outcome requires verification; baseline unchanged. Evidence: {}",
            root.display()
        )
    })?;
    save_verified(&intent, &verified)?;
    store.commit(&intent, &verified)?;
    Ok(Report {
        playlist_id: pid,
        tracks: intent.desired.members.len(),
        changed: true,
    })
}

pub struct DeleteRequest<'a> {
    pub local_id: &'a str,
    pub state: &'a Path,
    pub host_name: &'a str,
    pub seconds: u64,
}
pub(crate) async fn delete(o: DeleteRequest<'_>) -> Result<Report> {
    ensure!(
        valid_id(o.local_id) && (1..=1800).contains(&o.seconds),
        "invalid playlist deletion request"
    );
    recover(o.state, o.seconds).await?;
    let root = o.state.join("playlist-sync-evidence").join(format!(
        "delete-{}-{}",
        now_ns()?,
        std::process::id()
    ));
    layout::private_directory(root.parent().unwrap())?;
    create_trace(&root)?;
    let root = fs::canonicalize(root)?;
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(o.seconds),
        cancelled: crate::engine::cancellation(),
    };
    let (identity, before) = removal_batch::capture_fresh(&root.join("before"), &control).await?;
    let mut store = Store::open(o.state)?;
    let prior = store
        .get(
            &hex(&identity.device_digest()),
            &hex(&identity.library_digest()),
            o.local_id,
        )?
        .context("playlist has no verified sync baseline")?;
    if prior.deleted {
        ensure!(
            !reconcile::row_exists(&before, prior.phone_id)?,
            "the phone restored this playlist; reconcile again"
        );
        return Ok(Report {
            playlist_id: prior.phone_id,
            tracks: 0,
            changed: false,
        });
    }
    ensure!(prior.mutable, "this playlist is managed by the iPhone");
    qualify_target(&before, prior.phone_id)?;
    ensure!(
        created_hash(&before, prior.phone_id)? == prior.created_sha256,
        "the linked playlist identity changed"
    );
    let operation = mutation::delete_playlists(
        &before.current.catalog,
        &[prior.phone_id],
        SystemTime::now(),
    )?;
    let mut desired = prior.clone();
    desired.deleted = true;
    desired.members.clear();
    let intent = Intent {
        version: 1,
        root,
        desired,
        before: proof(&before, prior.phone_id, true)?,
        operation_sha256: hex(&Sha256::digest(&operation)),
        delete: true,
        additional: vec![],
    };
    apply(
        &mut store,
        &before,
        intent,
        operation,
        o.host_name,
        Some(&prior),
        &control,
    )
    .await
}

#[cfg(test)]
mod favorites_probe;
#[cfg(test)]
mod live_tests;

#[cfg(test)]
mod request_tests {
    use super::*;
    #[test]
    fn request_requires_stable_unique_local_identities_and_exact_path_alignment() {
        let ids = vec!["first".into(), "second".into()];
        let paths = vec![PathBuf::from("A.mp3"), PathBuf::from("B.mp3")];
        let mut request = Request {
            playlist: Path::new("list.m3u8"),
            local_id: "flurb:library:playlist",
            local_track_ids: &ids,
            name: "Playlist",
            state: Path::new("state"),
            host_name: "Flurb",
            seconds: 300,
        };
        validate_request(&request, &paths).unwrap();
        assert!(validate_request(&request, &paths[..1]).is_err());
        let duplicate = vec!["first".into(), "first".into()];
        request.local_track_ids = &duplicate;
        assert!(validate_request(&request, &paths).is_err());
        request.local_track_ids = &[];
        validate_request(&request, &[]).unwrap();
        request.local_id = "";
        assert!(validate_request(&request, &[]).is_err());
        request.local_id = "stable";
        request.name = "Bad\nName";
        assert!(validate_request(&request, &[]).is_err());
    }
}

#[cfg(test)]
mod sql_tests;

pub(crate) fn export_cloud_baselines(state: &Path, root: &Path) -> Result<String> {
    let body = Store::open(state)?.cloud_export()?;
    let mut values: Vec<Baseline> = serde_json::from_str(&body)?;
    let canonical = fs::canonicalize(root).unwrap_or(root.to_owned());
    for value in &mut values {
        for member in &mut value.members {
            let path = PathBuf::from(&member.source);
            let path = fs::canonicalize(&path).unwrap_or(path);
            member.source = path
                .strip_prefix(&canonical)
                .ok()
                .and_then(|p| p.to_str())
                .map(|p| format!("flurb-music:{p}"))
                .unwrap_or_default();
        }
    }
    Ok(serde_json::to_string(&values)?)
}
pub(crate) fn import_cloud_baselines(state: &Path, root: &Path, body: &str) -> Result<()> {
    Store::open(state)?.cloud_import(body, root)
}
