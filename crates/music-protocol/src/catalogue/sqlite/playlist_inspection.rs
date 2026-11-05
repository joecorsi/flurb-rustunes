//! Best-effort read-only playlist membership and liked-state inspection.
//! Raw container IDs and membership rows are reported as stored; no
//! inferences are used by synchronization or transfer logic.
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};
use serde_json::{Value, json};

const MAX_PLAYLIST_ROWS: usize = 10_000;
const MAX_LIKE_STATES: usize = 64;

pub(super) fn inspect(connection: &Connection) -> (Value, Value, Vec<Value>) {
    let mut warnings = Vec::new();
    let mut playlists = Vec::new();

    // Membership counts come from actual container_item rows, not the
    // possibly stale container_item_media_type summary table.
    // The primary library and built-in containers are included and labeled.
    const QUERY: &str = r#"
        SELECT c.container_pid, c.sync_id, c.name,
               COALESCE(m.members,0), COALESCE(m.music_members,0),
               COALESCE(m.synced_music_members,0), COALESCE(m.dangling_members,0),
               CASE WHEN c.container_pid = (
                    SELECT primary_container_pid FROM db_info LIMIT 1
               ) OR c.sync_id = (
                    SELECT primary_container_pid FROM db_info LIMIT 1
               ) THEN 1 ELSE 0 END
        FROM container c
        LEFT JOIN (
            SELECT ci.container_pid,
                   COUNT(*) AS members,
                   SUM(CASE WHEN i.media_type=8 THEN 1 ELSE 0 END) AS music_members,
                   SUM(CASE WHEN i.media_type=8 AND s.sync_id<>0 THEN 1 ELSE 0 END)
                       AS synced_music_members,
                   SUM(CASE WHEN i.item_pid IS NULL THEN 1 ELSE 0 END)
                       AS dangling_members
            FROM container_item ci
            LEFT JOIN item i ON i.item_pid=ci.item_pid
            LEFT JOIN item_store s ON s.item_pid=ci.item_pid
            GROUP BY ci.container_pid
        ) m ON m.container_pid=c.container_pid
        ORDER BY c.name COLLATE NOCASE, c.container_pid
        LIMIT 10001
    "#;

    match connection.prepare(QUERY) {
        Ok(mut statement) => {
            let query = statement.query_map([], |row| {
                let container_pid: i64 = row.get(0)?;
                let sync_id: Option<i64> = row.get(1)?;
                let name: Option<String> = row.get(2)?;
                let members: i64 = row.get(3)?;
                let music_members: i64 = row.get(4)?;
                let synced_music_members: i64 = row.get(5)?;
                let dangling_members: i64 = row.get(6)?;
                let primary: i64 = row.get(7)?;
                Ok(json!({
                    "container_pid": container_pid.to_string(),
                    "sync_id": sync_id.map(|value| value.to_string()),
                    "name": name,
                    "members": members,
                    "music_members": music_members,
                    "synced_music_members": synced_music_members,
                    "dangling_members": dangling_members,
                    "is_primary_library": primary != 0
                }))
            });
            match query {
                Ok(rows) => {
                    for result in rows {
                        match result {
                            Ok(playlist) if playlists.len() < MAX_PLAYLIST_ROWS => playlists.push(playlist),
                            Ok(_) => {
                                warnings.push(json!({
                                    "code": "playlist_row_limit",
                                    "limit": MAX_PLAYLIST_ROWS
                                }));
                                break;
                            }
                            Err(error) => {
                                warnings.push(json!({
                                    "code": "playlist_row_error",
                                    "message": error.to_string()
                                }));
                                break;
                            }
                        }
                    }
                }
                Err(error) => warnings.push(json!({
                    "code":"playlist_query_error",
                    "message": error.to_string()
                })),
            }
        }
        Err(error) => warnings.push(json!({
            "code": "playlist_query_error",
            "message": error.to_string()
        })),
    }

    // Value 2 is the usual 'loved/favorited' code in older Apple music
    // libraries. Keep the raw distribution because iOS versions may differ.
    let mut liked_states = Vec::new();
    const LIKES: &str = r#"
        SELECT st.liked_state, COUNT(*)
        FROM item_stats st
        JOIN item i ON i.item_pid=st.item_pid
        JOIN item_store s ON s.item_pid=st.item_pid
        WHERE i.media_type=8 AND s.sync_id<>0
        GROUP BY st.liked_state
        ORDER BY st.liked_state
        LIMIT 65
    "#;
    match connection.prepare(LIKES) {
        Ok(mut statement) => match statement.query_map([], |row| {
            Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, i64>(1)?))
        }) {
            Ok(rows) => {
                for result in rows {
                    match result {
                        Ok((state, count)) if liked_states.len() < MAX_LIKE_STATES => {
                            liked_states.push(json!({"state": state, "tracks": count}));
                        }
                        Ok(_) => {
                            warnings.push(json!({"code":"liked_state_row_limit","limit":MAX_LIKE_STATES}));
                            break;
                        }
                        Err(error) => {
                            warnings.push(json!({"code":"liked_state_row_error","message":error.to_string()}));
                            break;
                        }
                    }
                }
            }
            Err(error) => warnings.push(json!({"code":"liked_state_query_error","message":error.to_string()})),
        },
        Err(error) => warnings.push(json!({"code":"liked_state_query_error","message":error.to_string()})),
    }

    let favored = liked_states.iter()
        .find(|entry| entry["state"] == 2)
        .and_then(|entry| entry["tracks"].as_i64())
        .unwrap_or(0);
    let favorites = json!({
        "candidate_favorite_tracks_liked_state_2": favored,
        "liked_state_counts": liked_states,
        "note": "Favorites are provisionally mapped to liked_state=2; raw values are included for validation."
    });
    (Value::Array(playlists), favorites, warnings)
}


const COMPARE_TRACK_LIMIT: usize = 50_000;
const COMPARE_MEMBER_LIMIT: usize = 50_000;

/// Compare a named playlist's actual members to the raw liked_state=2 track set.
/// All values are read from a captured SQLite copy, never fed to sync planning.
pub(super) fn compare_favorites(connection: &Connection, name: &str) -> Value {
    match compare_favorites_inner(connection, name) {
        Ok(value) => value,
        Err(error) => json!({
            "playlist_name": name,
            "error": format!("{error:#}"),
        }),
    }
}

fn compare_favorites_inner(connection: &Connection, name: &str) -> Result<Value> {
    let mut statement = connection.prepare(
        "SELECT container_pid FROM container WHERE name=?1 ORDER BY container_pid LIMIT 2",
    )?;
    let mut rows = statement.query([name])?;
    let Some(row) = rows.next()? else {
        return Ok(json!({"playlist_name": name, "error": "playlist not found"}));
    };
    let container_pid: i64 = row.get(0)?;
    ensure!(
        rows.next()?.is_none(),
        "multiple playlists are named {name:?}; select an unambiguous name"
    );
    drop(rows);
    drop(statement);

    // SQL's internal item_pid joins playlist membership and the stats table.
    // sync_id is only for identifying tracks in the resulting report.
    let mut tracks = BTreeMap::<i64, Value>::new();
    let mut favorite_ids = BTreeSet::<i64>::new();
    let mut statement = connection.prepare(
        "SELECT i.item_pid, s.sync_id, e.title, ia.item_artist, st.liked_state
         FROM item i
         JOIN item_store s ON s.item_pid=i.item_pid
         LEFT JOIN item_extra e ON e.item_pid=i.item_pid
         LEFT JOIN item_artist ia ON ia.item_artist_pid=i.item_artist_pid
         LEFT JOIN item_stats st ON st.item_pid=i.item_pid
         WHERE i.media_type=8 AND s.sync_id<>0"
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        ensure!(tracks.len() < COMPARE_TRACK_LIMIT, "track comparison row limit exceeded");
        let item_pid: i64 = row.get(0)?;
        let sync_id: i64 = row.get(1)?;
        let title: Option<String> = row.get(2)?;
        let artist: Option<String> = row.get(3)?;
        let liked_state: Option<i64> = row.get(4)?;
        let info = json!({
            "item_pid": item_pid.to_string(),
            "sync_id": (sync_id as u64).to_string(),
            "title": title,
            "artist": artist,
            "liked_state": liked_state,
        });
        ensure!(tracks.insert(item_pid, info).is_none(), "duplicate track identity");
        if liked_state == Some(2) {
            favorite_ids.insert(item_pid);
        }
    }
    drop(rows);
    drop(statement);

    let mut members = BTreeSet::<i64>::new();
    let mut membership_rows = 0usize;
    let mut statement = connection.prepare(
        "SELECT item_pid FROM container_item WHERE container_pid=?1"
    )?;
    let mut rows = statement.query([container_pid])?;
    while let Some(row) = rows.next()? {
        ensure!(
            membership_rows < COMPARE_MEMBER_LIMIT,
            "playlist membership comparison row limit exceeded"
        );
        let item_pid: i64 = row.get(0)?;
        members.insert(item_pid);
        membership_rows += 1;
    }

    let track_ids: BTreeSet<_> = tracks.keys().copied().collect();
    let playlist_music_ids: BTreeSet<_> = members.intersection(&track_ids).copied().collect();
    let overlap = favorite_ids.intersection(&playlist_music_ids).count();
    let favorite_not_in_playlist: Vec<_> = favorite_ids
        .difference(&playlist_music_ids)
        .filter_map(|id| tracks.get(id).cloned())
        .collect();
    let playlist_not_favorited: Vec<_> = playlist_music_ids
        .difference(&favorite_ids)
        .filter_map(|id| tracks.get(id).cloned())
        .collect();
    Ok(json!({
        "playlist_name": name,
        "container_pid": container_pid.to_string(),
        "playlist_membership_rows": membership_rows,
        "playlist_unique_members": members.len(),
        "playlist_synced_music_tracks": playlist_music_ids.len(),
        "playlist_unmatched_members": members.difference(&track_ids).count(),
        "duplicate_membership_rows": membership_rows - members.len(),
        "liked_state_2_tracks": favorite_ids.len(),
        "overlap": overlap,
        "favorite_not_in_playlist": favorite_not_in_playlist,
        "playlist_not_favorited": playlist_not_favorited,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::catalogue::Fixture;

    #[test]
    fn playlist_membership_and_liked_states_are_read_only() {
        let fixture = Fixture::new(false);
        fixture.connection.execute_batch(
            "ALTER TABLE container ADD COLUMN name TEXT; \
             UPDATE container SET name='Primary Library'; \
             INSERT INTO container VALUES(82,-82,'Favorites'); \
             CREATE TABLE container_item(container_item_pid INTEGER PRIMARY KEY,container_pid INTEGER,item_pid INTEGER,position INTEGER); \
             INSERT INTO container_item VALUES(1,81,1,0); \
             INSERT INTO container_item VALUES(2,82,1,0); \
             UPDATE item_stats SET liked_state=2 WHERE item_pid=1;"
        ).unwrap();
        let (playlists, favorites, warnings) = inspect(&fixture.connection);
        assert!(warnings.is_empty(), "{warnings:?}");
        let rows = playlists.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["members"], 1);
        assert_eq!(rows[0]["name"], "Favorites");
        assert_eq!(rows[1]["name"], "Primary Library");
        assert_eq!(rows[1]["is_primary_library"], true); // matches the fixture's wire PID -11
        assert_eq!(favorites["candidate_favorite_tracks_liked_state_2"], 1);
        assert_eq!(favorites["liked_state_counts"][0]["state"], 2);
    }

    #[test]
    fn missing_membership_table_is_reported_not_fatal() {
        let fixture = Fixture::new(false);
        let (playlists, favorites, warnings) = inspect(&fixture.connection);
        assert!(playlists.as_array().unwrap().is_empty());
        assert_eq!(favorites["candidate_favorite_tracks_liked_state_2"], 0);
        assert!(warnings.iter().any(|entry| entry["code"] == "playlist_query_error"));
    }
}
