//! Best-effort read-only inventory of a captured Music database.
//! This deliberately does not produce a Catalog suitable for planning writes.
//! Broken joins become diagnostic counts rather than inferred identities.
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};

/// Inspect only existing rows. No schema repair, identity synthesis, or writes.
pub(super) fn inspect(connection: &Connection) -> Result<Value> {
    let mut warnings = Vec::<Value>::new();
    let mut report = json!({
        "source": "read_only_sqlite_diagnostics",
        "stats": Value::Null,
        "warnings": [],
    });

    let stats = connection.query_row(
        "SELECT count(*), count(DISTINCT i.album_pid), count(DISTINCT i.album_artist_pid),          coalesce(sum(CASE WHEN e.file_size >= 0 THEN e.file_size ELSE 0 END), 0)          FROM item i JOIN item_store s USING(item_pid)          LEFT JOIN item_extra e USING(item_pid)          WHERE i.media_type=8 AND s.sync_id<>0",
        [],
        |row| {
            Ok(json!({
                "tracks": row.get::<_, i64>(0)?,
                "album_ids": row.get::<_, i64>(1)?,
                "album_artist_ids_including_zero": row.get::<_, i64>(2)?,
                "file_bytes_known": row.get::<_, i64>(3)?,
            }))
        },
    );
    match stats {
        Ok(stats) => report["stats"] = stats,
        Err(error) => warnings.push(json!({
            "code":"stats_query_error",
            "message":error.to_string()
        })),
    }

    // Each diagnostic is independent: an unexpected table or column shape
    // does not prevent us from reporting the other checks.
    const CHECKS: &[(&str, &str)] = &[
        (
            "albums_without_artist",
            "SELECT count(*) FROM album a LEFT JOIN album_artist aa              ON a.album_artist_pid=aa.album_artist_pid              WHERE aa.album_artist_pid IS NULL",
        ),
        (
            "tracks_in_albums_without_artist",
            "SELECT count(*) FROM item i JOIN item_store s USING(item_pid)              JOIN album a ON a.album_pid=i.album_pid              LEFT JOIN album_artist aa ON a.album_artist_pid=aa.album_artist_pid              WHERE i.media_type=8 AND s.sync_id<>0 AND aa.album_artist_pid IS NULL",
        ),
        (
            "tracks_without_album",
            "SELECT count(*) FROM item i JOIN item_store s USING(item_pid)              LEFT JOIN album a ON a.album_pid=i.album_pid              WHERE i.media_type=8 AND s.sync_id<>0 AND a.album_pid IS NULL",
        ),
        (
            "tracks_without_item_artist",
            "SELECT count(*) FROM item i JOIN item_store s USING(item_pid)              LEFT JOIN item_artist ia ON ia.item_artist_pid=i.item_artist_pid              WHERE i.media_type=8 AND s.sync_id<>0 AND ia.item_artist_pid IS NULL",
        ),
        (
            "tracks_with_album_artist_mismatch",
            "SELECT count(*) FROM item i JOIN item_store s USING(item_pid)              JOIN album a ON a.album_pid=i.album_pid              WHERE i.media_type=8 AND s.sync_id<>0              AND i.album_artist_pid<>a.album_artist_pid",
        ),
        (
            "tracks_missing_extra",
            "SELECT count(*) FROM item i JOIN item_store s USING(item_pid)              LEFT JOIN item_extra e USING(item_pid)              WHERE i.media_type=8 AND s.sync_id<>0 AND e.item_pid IS NULL",
        ),
        (
            "tracks_missing_stats",
            "SELECT count(*) FROM item i JOIN item_store s USING(item_pid)              LEFT JOIN item_stats st USING(item_pid)              WHERE i.media_type=8 AND s.sync_id<>0 AND st.item_pid IS NULL",
        ),
    ];
    for (code, query) in CHECKS {
        match connection.query_row(query, [], |row| row.get::<_, i64>(0)) {
            Ok(0) => {}
            Ok(count) => warnings.push(json!({"code":code,"count":count})),
            Err(error) => warnings.push(json!({
                "code":code,
                "query_error":error.to_string()
            })),
        }
    }
    report["warnings"] = json!(warnings);
    Ok(report)
}
