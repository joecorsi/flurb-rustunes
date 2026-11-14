use super::*;
fn value(name: &str, ids: &[&str]) -> PlaylistState {
    PlaylistState {
        name: name.into(),
        tracks: ids.iter().map(|v| (*v).into()).collect(),
    }
}
fn baseline(value: &PlaylistState) -> Baseline {
    Baseline {
        version: 1,
        device: "a".repeat(64),
        library: "b".repeat(64),
        local_id: "lib:mix".into(),
        phone_id: 100,
        name: value.name.clone(),
        members: value
            .tracks
            .iter()
            .enumerate()
            .map(|(i, id)| Member {
                local_id: id.clone(),
                phone_id: i as u64 + 1,
                file_sha256: "c".repeat(64),
                source: String::new(),
            })
            .collect(),
        created_sha256: "d".repeat(64),
        verified_anchor: 10,
        verified_ns: "1000".into(),
        deleted: false,
        mutable: true,
    }
}
fn report() -> Report {
    Report {
        changes: vec![],
        exports: vec![],
        removals: vec![],
        installed: vec![],
        verified_unchanged: vec![],
        conflicts: vec![],
        notices: vec![],
        acceptance_json: String::new(),
        scope: String::new(),
    }
}
fn run(base: &Baseline, local: &LocalPlaylist, remote: Baseline, choices: &[Choice]) -> Report {
    let mut result = report();
    let mut names = BTreeSet::new();
    let mut accepted = vec![];
    let request = Request {
        state: Path::new("state"),
        prefix: "lib:",
        tracks: &[],
        playlists: std::slice::from_ref(local),
        selected: &["lib:mix".into()],
        choices,
        pending: &[],
        seconds: 60,
    };
    reconcile_one(
        &request,
        Some(base),
        Some(local),
        remote,
        &mut names,
        &mut result,
        &mut accepted,
    )
    .unwrap();
    result
}
#[test]
fn only_freshly_matching_playlists_skip_the_individual_outbound_check() {
    let b = baseline(&value("Mix", &["a", "b"]));
    let local = LocalPlaylist {
        id: b.local_id.clone(),
        value: value("Mix", &["a", "b"]),
        derived: false,
    };
    let unchanged = run(&b, &local, b.clone(), &[]);
    assert_eq!(unchanged.verified_unchanged, vec![b.local_id.clone()]);
    // Keep the audio selection and durable retry queue even on the fast path.
    assert_eq!(unchanged.exports, vec![b.local_id.clone()]);
    assert!(unchanged.changes.is_empty());

    // Phone-only changes still import before being recognized as matching.
    let remote = baseline(&value("Phone rename", &["b", "a"]));
    let imported = run(&b, &local, remote, &[]);
    assert_eq!(imported.verified_unchanged, vec![b.local_id.clone()]);
    assert_eq!(
        imported.changes[0].after,
        Some(value("Phone rename", &["b", "a"]))
    );

    for desired in [
        value("Local rename", &["a", "b"]),
        value("Mix", &["b", "a"]),
        value("Mix", &["a", "b", "c"]),
    ] {
        let changed = run(
            &b,
            &LocalPlaylist {
                value: desired,
                ..local.clone()
            },
            b.clone(),
            &[],
        );
        assert!(changed.verified_unchanged.is_empty());
        assert_eq!(changed.exports, vec![b.local_id.clone()]);
    }
    let conflict = run(
        &b,
        &LocalPlaylist {
            value: value("Local rename", &["a", "b"]),
            ..local
        },
        baseline(&value("Phone rename", &["a", "b"])),
        &[],
    );
    assert_eq!(conflict.conflicts.len(), 1);
    assert!(conflict.verified_unchanged.is_empty());
}
#[test]
fn conflicts_preserve_both_versions_and_stale_choices_cannot_overwrite_new_edits() {
    let b = baseline(&value("Mix", &["a", "b"]));
    let local = LocalPlaylist {
        id: b.local_id.clone(),
        value: value("Flurb name", &["a", "b", "c"]),
        derived: false,
    };
    let mut remote = baseline(&value("Phone name", &["b", "a"]));
    remote.verified_anchor = 11;
    let conflict = run(&b, &local, remote.clone(), &[]).conflicts.remove(0);
    let choice = Choice {
        id: conflict.id,
        token: conflict.token,
        resolution: Resolution::Both,
    };
    let result = run(&b, &local, remote.clone(), std::slice::from_ref(&choice));
    assert!(result.conflicts.is_empty());
    assert_eq!(result.changes.len(), 2);
    let copy = result
        .changes
        .iter()
        .find(|c| c.id != b.local_id)
        .unwrap()
        .after
        .as_ref()
        .unwrap();
    assert_eq!(copy.tracks, local.value.tracks);
    assert!(copy.name.ends_with("(Flurb)"));
    assert_eq!(
        result
            .changes
            .iter()
            .find(|c| c.id == b.local_id)
            .unwrap()
            .after,
        Some(value("Phone name", &["b", "a"]))
    );
    remote.name = "A newer phone edit".into();
    assert_eq!(run(&b, &local, remote, &[choice]).conflicts.len(), 1);
}
#[test]
fn iphone_owned_playlists_import_but_are_not_silently_overwritten() {
    let mut b = baseline(&value("Mix", &["a"]));
    b.mutable = false;
    let local = LocalPlaylist {
        id: b.local_id.clone(),
        value: value("Changed", &["a"]),
        derived: false,
    };
    let result = run(&b, &local, b.clone(), &[]);
    assert_eq!(result.conflicts.len(), 1);
    assert!(!result.conflicts[0].allow_flurb);
    assert!(result.conflicts[0].allow_both);
    let mut restored = b.clone();
    restored.deleted = true;
    restored.members.clear();
    let result = run(&restored, &local, restored.clone(), &[]);
    assert!(result.conflicts.is_empty());
    assert_eq!(result.exports, vec!["lib:mix"]);
}
#[test]
fn derived_playlist_conflicts_keep_phone_edits_in_a_regular_copy_without_changing_hearts() {
    let b = baseline(&value("Favorites", &["a"]));
    let local = LocalPlaylist {
        id: b.local_id.clone(),
        value: value("Favorites", &["a"]),
        derived: true,
    };
    let remote = baseline(&value("Favorites", &["a", "b"]));
    let conflict = run(&b, &local, remote.clone(), &[]).conflicts.remove(0);
    assert!(!conflict.allow_iphone);
    let result = run(
        &b,
        &local,
        remote,
        &[Choice {
            id: conflict.id,
            token: conflict.token,
            resolution: Resolution::Both,
        }],
    );
    assert!(result.changes.iter().all(|c| c.id != b.local_id));
    let copy = result.changes[0].after.as_ref().unwrap();
    assert_eq!(copy.tracks, vec!["a", "b"]);
    assert!(copy.name.ends_with("(iPhone)"));
    assert!(result.exports.contains(&b.local_id));
}

#[test]
fn unlinked_phone_playlists_create_local_playlists_and_their_baseline_without_native_writes() {
    let mut remote = baseline(&value("Phone-created", &["a", "b"]));
    remote.mutable = false;
    let request = Request {
        state: Path::new("state"),
        prefix: "lib:",
        tracks: &[],
        playlists: &[],
        selected: &[],
        choices: &[],
        pending: &[],
        seconds: 60,
    };
    let mut result = report();
    let mut accepted = vec![];
    reconcile_one(
        &request,
        None,
        None,
        remote.clone(),
        &mut BTreeSet::new(),
        &mut result,
        &mut accepted,
    )
    .unwrap();
    assert_eq!(result.changes.len(), 1);
    assert!(result.changes[0].before.is_none());
    assert_eq!(result.changes[0].after, state(&remote));
    assert!(result.exports.is_empty());
    assert_eq!(accepted.len(), 1);
    assert!(!accepted[0].observed.mutable);
}

#[test]
fn unfinished_exports_resume_only_on_the_matching_device_and_library() {
    let pending = vec![
        ("phone-a/library-a".into(), vec!["lib:copy-a".into()]),
        ("phone-b/library-b".into(), vec!["lib:copy-b".into()]),
    ];
    let request = Request {
        state: Path::new("state"),
        prefix: "lib:",
        tracks: &[],
        playlists: &[],
        selected: &[],
        choices: &[],
        pending: &pending,
        seconds: 60,
    };
    assert_eq!(
        effective_selected(&request, "phone-a/library-a").unwrap(),
        vec!["lib:copy-a"]
    );
    assert_eq!(
        effective_selected(&request, "phone-b/library-b").unwrap(),
        vec!["lib:copy-b"]
    );
    assert!(
        effective_selected(&request, "another-phone/library-a")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn deselection_removes_only_active_managed_phone_playlists() {
    let b = baseline(&value("Mix", &["a"]));
    let local = LocalPlaylist {
        id: b.local_id.clone(),
        value: value("Mix", &["a"]),
        derived: false,
    };
    assert!(removal_requested(&b, Some(&local), &[], "lib:"));
    assert!(!removal_requested(
        &b,
        Some(&local),
        &[b.local_id.clone()],
        "lib:"
    ));
    assert!(!removal_requested(
        &Baseline {
            deleted: true,
            ..b.clone()
        },
        Some(&local),
        &[],
        "lib:"
    ));
    assert!(!removal_requested(
        &Baseline {
            mutable: false,
            ..b.clone()
        },
        Some(&local),
        &[],
        "lib:"
    ));
    assert!(!removal_requested(
        &Baseline {
            local_id: "lib:collection:favorites".into(),
            ..b.clone()
        },
        Some(&local),
        &[],
        "lib:"
    ));
    // A deselected playlist's saved order remains available for reselection.
    let mut tombstone = b.clone();
    tombstone.deleted = true;
    tombstone.members.clear();
    let restored = run(&tombstone, &local, tombstone.clone(), &[]);
    assert!(restored.changes.is_empty());
    assert_eq!(restored.exports, vec![b.local_id]);
}
