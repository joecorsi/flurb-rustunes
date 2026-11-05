//! Best-effort read-only inventory of a captured Music database.
//! This deliberately does not produce a Catalog suitable for planning writes.
//! Broken joins become diagnostic counts rather than inferred identities.
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;
use rusqlite::Connection;
use serde_json::{Value, json};

/// Inspect only existing rows. No schema repair, identity synthesis, or writes.
pub(super) fn inspect(connection: &Connection, compare_playlist: Option<&str>, full_inventory: bool) -> Result<Value> {
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
    let (playlists, favorites, playlist_warnings) =
        super::playlist_inspection::inspect(connection);
    report["playlists"] = playlists;
    report["favorites"] = favorites;
    if let Some(name) = compare_playlist {
        report["favorites_comparison"] =
            super::playlist_inspection::compare_favorites(connection, name);
    }
    warnings.extend(playlist_warnings);
    report["warnings"] = json!(warnings);
    if full_inventory {
        let (tracks, albums, membership) = detailed_inventory(connection)?;
        report["tracks"] = json!(tracks);
        report["albums"] = json!(albums);
        report["playlist_membership"] = json!(membership);
    }
    Ok(report)
}

/// A JSON export must either include every matching row or fail explicitly.
fn detailed_inventory(connection: &Connection) -> Result<(Vec<Value>, Vec<Value>, BTreeMap<String, Vec<Value>>)> {
    const MAX_TRACKS: usize = 100_000;
    const MAX_ALBUMS: usize = 100_000;
    const MAX_MEMBERS: usize = 1_000_000;

    let mut tracks = Vec::new();
    let mut query = connection.prepare(
        "SELECT s.sync_id, i.item_pid, e.title, ia.item_artist, a.album,
                aa.album_artist, i.track_number, e.track_count, i.disc_number,
                e.disc_count, e.year, e.total_time_ms, e.file_size,
                g.genre, co.composer, st.liked_state, st.play_count_user,
                st.skip_count_user
         FROM item i JOIN item_store s USING(item_pid)
         LEFT JOIN item_extra e USING(item_pid)
         LEFT JOIN item_artist ia ON ia.item_artist_pid=i.item_artist_pid
         LEFT JOIN album a ON a.album_pid=i.album_pid
         LEFT JOIN album_artist aa ON aa.album_artist_pid=i.album_artist_pid
         LEFT JOIN genre g ON g.genre_id=i.genre_id
         LEFT JOIN composer co ON co.composer_pid=i.composer_pid
         LEFT JOIN item_stats st USING(item_pid)
         WHERE i.media_type=8 AND s.sync_id<>0
         ORDER BY s.sync_id"
    ).context("prepare detailed track inventory")?;
    for result in query.query_map([], |row| {
        let wire_id: i64 = row.get(0)?;
        Ok(json!({
            "id": format!("{:016x}", wire_id as u64),
            "item_pid": row.get::<_, i64>(1)?.to_string(),
            "title": row.get::<_, Option<String>>(2)?,
            "artist": row.get::<_, Option<String>>(3)?,
            "album": row.get::<_, Option<String>>(4)?,
            "album_artist": row.get::<_, Option<String>>(5)?,
            "track_number": row.get::<_, Option<i64>>(6)?,
            "track_count": row.get::<_, Option<i64>>(7)?,
            "disc_number": row.get::<_, Option<i64>>(8)?,
            "disc_count": row.get::<_, Option<i64>>(9)?,
            "year": row.get::<_, Option<i64>>(10)?,
            "duration_ms": row.get::<_, Option<f64>>(11)?,
            "file_bytes": row.get::<_, Option<i64>>(12)?,
            "genre": row.get::<_, Option<String>>(13)?,
            "composer": row.get::<_, Option<String>>(14)?,
            "liked_state": row.get::<_, Option<i64>>(15)?,
            "play_count": row.get::<_, Option<i64>>(16)?,
            "skip_count": row.get::<_, Option<i64>>(17)?
        }))
    })? {
        ensure!(tracks.len() < MAX_TRACKS, "track inventory exceeds {MAX_TRACKS} rows");
        tracks.push(result.context("read detailed track row")?);
    }
    drop(query);

    let mut albums = Vec::new();
    let mut query = connection.prepare(
        "SELECT a.album_pid, a.sync_id, a.album, aa.album_artist, COUNT(*)
         FROM album a JOIN item i ON i.album_pid=a.album_pid
         JOIN item_store s ON s.item_pid=i.item_pid
         LEFT JOIN album_artist aa ON aa.album_artist_pid=a.album_artist_pid
         WHERE i.media_type=8 AND s.sync_id<>0
         GROUP BY a.album_pid, a.sync_id, a.album, aa.album_artist
         ORDER BY aa.album_artist, a.album, a.album_pid"
    ).context("prepare album inventory")?;
    for result in query.query_map([], |row| {
        let wire_id: i64 = row.get(1)?;
        Ok(json!({
            "id": row.get::<_, i64>(0)?.to_string(),
            "sync_id": format!("{:016x}", wire_id as u64),
            "name": row.get::<_, String>(2)?,
            "artist": row.get::<_, Option<String>>(3)?,
            "tracks": row.get::<_, i64>(4)?
        }))
    })? {
        ensure!(albums.len() < MAX_ALBUMS, "album inventory exceeds {MAX_ALBUMS} rows");
        albums.push(result.context("read album row")?);
    }
    drop(query);

    let mut membership = BTreeMap::<String, Vec<Value>>::new();
    let mut total = 0usize;
    let mut query = connection.prepare(
        "SELECT ci.container_pid, ci.position, ci.item_pid, i.media_type, s.sync_id
         FROM container_item ci
         LEFT JOIN item i ON i.item_pid=ci.item_pid
         LEFT JOIN item_store s ON s.item_pid=ci.item_pid
         ORDER BY ci.container_pid, ci.position, ci.container_item_pid"
    ).context("prepare playlist membership inventory")?;
    for result in query.query_map([], |row| {
        let wire_id: Option<i64> = row.get(4)?;
        Ok((
            row.get::<_, i64>(0)?.to_string(),
            json!({
                "position": row.get::<_, i64>(1)?,
                "item_pid": row.get::<_, i64>(2)?.to_string(),
                "media_type": row.get::<_, Option<i64>>(3)?,
                "track_id": wire_id.filter(|id| *id != 0)
                    .map(|id| format!("{:016x}", id as u64))
            })
        ))
    })? {
        ensure!(total < MAX_MEMBERS, "playlist inventory exceeds {MAX_MEMBERS} member rows");
        let (playlist_id, member) = result.context("read membership row")?;
        membership.entry(playlist_id).or_default().push(member);
        total += 1;
    }
    Ok((tracks, albums, membership))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::catalogue::Fixture;

    #[test]
    fn missing_album_artist_is_reported_without_synthesizing_one() {
        let fixture = Fixture::new(false);
        fixture
            .connection
            .execute_batch("UPDATE album SET album_artist_pid=0 WHERE album_pid=31")
            .unwrap();
        let report = inspect(&fixture.connection, None, false).unwrap();
        assert_eq!(report["stats"]["tracks"], 1);
        let warnings = report["warnings"].as_array().unwrap();
        assert!(warnings.iter().any(|issue| {
            issue["code"] == "albums_without_artist" && issue["count"] == 1
        }));
        assert!(warnings.iter().any(|issue| {
            issue["code"] == "tracks_in_albums_without_artist" && issue["count"] == 1
        }));
        let pid: i64 = fixture.connection
            .query_row("SELECT album_artist_pid FROM album WHERE album_pid=31", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pid, 0);
    }

    #[test]
    fn full_inventory_has_track_metadata_album_and_ordered_membership() {
        let fixture = Fixture::new(false);
        fixture.connection.execute_batch(
            "ALTER TABLE container ADD COLUMN name TEXT;
             UPDATE container SET name='Primary Library';
             INSERT INTO container VALUES(82,-82,'Road Trip');
             CREATE TABLE container_item(
                 container_item_pid INTEGER PRIMARY KEY,
                 container_pid INTEGER, item_pid INTEGER, position INTEGER
             );
             INSERT INTO container_item VALUES(1,82,1,7);"
        ).unwrap();
        let report = inspect(&fixture.connection, None, true).unwrap();
        assert_eq!(report["tracks"][0]["id"], "ffffffffffffffff");
        assert_eq!(report["tracks"][0]["title"], "Synthetic Title");
        assert_eq!(report["tracks"][0]["album"], "Synthetic Album");
        assert_eq!(report["albums"][0]["name"], "Synthetic Album");
        assert_eq!(report["playlist_membership"]["82"][0]["position"], 7);
        assert_eq!(report["playlist_membership"]["82"][0]["track_id"], "ffffffffffffffff");
    }

    #[test]
    fn unexpected_schema_is_a_diagnostic_instead_of_a_parser_failure() {
        let fixture = Fixture::new(false);
        fixture.connection.execute_batch("DROP TABLE album_artist").unwrap();
        let report = inspect(&fixture.connection, None, false).unwrap();
        assert_eq!(report["stats"]["tracks"], 1);
        assert!(report["warnings"].as_array().unwrap().iter().any(|issue| {
            issue["code"] == "albums_without_artist" && issue.get("query_error").is_some()
        }));
    }
}
