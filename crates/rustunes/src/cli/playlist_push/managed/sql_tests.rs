use super::*;
use crate::engine::verification::sql_tables::tests::Fixture;
use crate::engine::verification::sql_tables::{
    Capture,
    reader::{self, Budget, Cell},
};
fn read(f: &Fixture, label: &str) -> reader::Snapshot {
    let data = fs::read(&f.before).unwrap();
    reader::materialize(
        Capture {
            database: &f.before,
            database_sha256: Sha256::digest(data).into(),
            wal: None,
        },
        &f.dir.join(label),
        &mut Budget::new(),
    )
    .unwrap()
}
fn seed(f: &Fixture) {
    rusqlite::Connection::open(&f.before).unwrap().execute_batch("INSERT INTO container(container_pid,sync_id,name,is_src_remote)VALUES(900,900,'Managed',1),(901,901,'Unrelated',1); INSERT INTO container_item(container_pid,item_pid,position)VALUES(900,2,0),(900,1,1);").unwrap();
}
#[test]
fn ordered_members_use_sync_ids_and_explicit_positions_not_item_ids_or_sql_row_order() {
    let f = Fixture::new();
    seed(&f);
    let sql = read(&f, "read");
    assert_eq!(ordered_sql(&sql, 900).unwrap(), vec![1002, 1001]);
    assert_eq!(qualify_sql(&sql, -3i64 as u64, 900).unwrap(), "Managed");
    let mut bad = sql.clone();
    let table = bad.tables.get_mut("container_item").unwrap();
    let at = table.column("position").unwrap();
    let mut rows = reader::Rows::new();
    for (r, n) in &table.rows {
        let mut r = r.clone();
        r[at] = Cell::Integer(0);
        rows.insert(r, *n);
    }
    table.rows = rows;
    assert!(ordered_sql(&bad, 900).is_err());
    let mut bad = sql.clone();
    let table = bad.tables.get_mut("item_store").unwrap();
    let at = table.column("sync_id").unwrap();
    table.rows = table
        .rows
        .iter()
        .map(|(r, n)| {
            let mut r = r.clone();
            r[at] = Cell::Integer(0);
            (r, *n)
        })
        .collect();
    assert!(ordered_sql(&bad, 900).is_err());
}
#[test]
fn protected_playlists_are_never_eligible_for_managed_updates() {
    let f = Fixture::new();
    seed(&f);
    let sql = read(&f, "read");
    assert!(qualify_sql(&sql, 900, 900).is_err());
    for (key, value) in [
        ("smart_is_dynamic", Cell::Integer(1)),
        ("cloud_is_subscribed", Cell::Integer(1)),
        ("is_src_remote", Cell::Integer(0)),
        ("sync_id", Cell::Integer(0)),
        ("parent_pid", Cell::Integer(123)),
        ("cloud_global_id", Cell::Text("cloud".into())),
    ] {
        let mut bad = sql.clone();
        let t = bad.tables.get_mut("container").unwrap();
        let at = t.column(key).unwrap();
        let pid = t.column("container_pid").unwrap();
        t.rows = t
            .rows
            .iter()
            .map(|(r, n)| {
                let mut r = r.clone();
                if r[pid] == Cell::Integer(900) {
                    r[at] = value.clone();
                }
                (r, *n)
            })
            .collect();
        assert!(qualify_sql(&bad, 0, 900).is_err(), "field {key}");
    }
}
#[test]
fn noninterference_digest_excludes_only_the_target_and_known_keep_local_bookkeeping() {
    let f = Fixture::new();
    seed(&f);
    let sql = read(&f, "read");
    let original = sql_digest(&sql, &["container", "container_item"], Some(900)).unwrap();
    for (pid, key, value, unchanged) in [
        (900, "name", Cell::Text("New".into()), true),
        (901, "name", Cell::Text("Wrong".into()), false),
        (901, "keep_local_status", Cell::Integer(2), true),
        (901, "keep_local_status", Cell::Integer(1), false),
    ] {
        let mut after = sql.clone();
        let t = after.tables.get_mut("container").unwrap();
        let at = t.column(key).unwrap();
        let owner = t.column("container_pid").unwrap();
        t.rows = t
            .rows
            .iter()
            .map(|(r, n)| {
                let mut r = r.clone();
                if r[owner] == Cell::Integer(pid) {
                    r[at] = value.clone();
                }
                (r, *n)
            })
            .collect();
        assert_eq!(
            sql_digest(&after, &["container", "container_item"], Some(900)).unwrap() == original,
            unchanged,
            "{key} {pid}"
        );
    }
}

#[test]
fn verification_uses_native_sql_tables_and_ui_columns() {
    let f = Fixture::new();
    seed(&f);
    let sql = read(&f, "read");
    sql_digest(
        &sql,
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
    )
    .unwrap();
    sql_digest(
        &sql,
        &[
            "container",
            "container_item",
            "container_item_media_type",
            "container_seed",
        ],
        Some(900),
    )
    .unwrap();
    let original = ui_digest(&sql, 900).unwrap();
    let mut changed = sql.clone();
    let t = changed.tables.get_mut("container").unwrap();
    let at = t.column("shuffle_items").unwrap();
    let pid = t.column("container_pid").unwrap();
    t.rows = t
        .rows
        .iter()
        .map(|(r, n)| {
            let mut r = r.clone();
            if r[pid] == Cell::Integer(900) {
                r[at] = Cell::Integer(1);
            }
            (r, *n)
        })
        .collect();
    assert_ne!(ui_digest(&changed, 900).unwrap(), original);
}

#[test]
fn incoming_phone_owned_playlists_are_read_only_and_gapped_positions_keep_order() {
    let f = Fixture::new();
    seed(&f);
    let mut sql = read(&f, "phone-owned");
    let table = sql.tables.get_mut("container").unwrap();
    let owner = table.column("container_pid").unwrap();
    let remote = table.column("is_src_remote").unwrap();
    let sync = table.column("sync_id").unwrap();
    table.rows = table
        .rows
        .iter()
        .map(|(r, n)| {
            let mut r = r.clone();
            if r[owner] == Cell::Integer(900) {
                r[remote] = Cell::Integer(0);
                r[sync] = Cell::Integer(0);
            }
            (r, *n)
        })
        .collect();
    assert_eq!(
        reconcile::ordinary_sql(&sql, 0, 900).unwrap(),
        ("Managed".into(), false)
    );
    assert!(qualify_sql(&sql, 0, 900).is_err());
    let table = sql.tables.get_mut("container_item").unwrap();
    let pos = table.column("position").unwrap();
    table.rows = table
        .rows
        .iter()
        .map(|(r, n)| {
            let mut r = r.clone();
            let Cell::Integer(p) = r[pos] else { panic!() };
            r[pos] = Cell::Integer(p * 10 + 10);
            (r, *n)
        })
        .collect();
    assert_eq!(
        reconcile::phone_order_sql(&sql, 900).unwrap(),
        vec![1002, 1001]
    );
    assert!(ordered_sql(&sql, 900).is_err());
}

#[test]
fn batch_preservation_excludes_exactly_its_owned_targets() {
    let fixture = Fixture::new();
    seed(&fixture);
    let before = read(&fixture, "batch-before");
    let mut after = before.clone();
    let table = after.tables.get_mut("container").unwrap();
    let pid = table.column("container_pid").unwrap();
    let name = table.column("name").unwrap();
    table.rows = table
        .rows
        .iter()
        .map(|(row, n)| {
            let mut row = row.clone();
            if row[pid] == Cell::Integer(901) {
                row[name] = Cell::Text("Changed".into());
            }
            (row, *n)
        })
        .collect();
    let tables = ["container", "container_item"];
    let both = BTreeSet::from([900, 901]);
    assert_eq!(
        sql_digest_excluding(&before, &tables, &both).unwrap(),
        sql_digest_excluding(&after, &tables, &both).unwrap()
    );
    let one = BTreeSet::from([900]);
    assert_ne!(
        sql_digest_excluding(&before, &tables, &one).unwrap(),
        sql_digest_excluding(&after, &tables, &one).unwrap()
    );
}
