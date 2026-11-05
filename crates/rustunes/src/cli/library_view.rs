//! Human and machine views of the same freshly captured device catalogue.
use std::io::Write;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use music_protocol::catalogue::sqlite::{Catalog, Track};

#[derive(Default)]
pub struct Filter {
    pub query: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

pub enum View {
    List(Filter),
    Show(u64),
    Stats,
}

/// Fixed-width hexadecimal IDs remain strings in JSON, avoiding JavaScript's
/// integer precision limit and confusion with internal SQLite row identifiers.
pub fn parse_id(text: &str) -> Result<u64> {
    ensure!(
        text.len() == 16 && text.bytes().all(|b| b.is_ascii_hexdigit()),
        "track ID must contain exactly 16 hexadecimal digits from library list"
    );
    let id = u64::from_str_radix(text, 16)?;
    ensure!(id != 0, "track ID must be nonzero");
    Ok(id)
}

pub fn write(
    catalog: &Catalog,
    view: &View,
    json_output: bool,
    out: &mut impl Write,
) -> Result<()> {
    let body = match view {
        View::Stats => stats(catalog)?,
        View::Show(id) => {
            let Some(track) = catalog.tracks().iter().find(|t| t.sync_id == *id) else {
                bail!("track {id:016x} is absent from the current catalogue");
            };
            json!({"track": track_json(catalog, track)})
        }
        View::List(filter) => {
            let mut tracks: Vec<_> = catalog
                .tracks()
                .iter()
                .filter(|t| matches(t, filter))
                .collect();
            tracks.sort_by(|a, b| {
                (
                    &a.metadata.artist,
                    &a.metadata.album,
                    a.metadata.disc_number,
                    a.metadata.track_number,
                    &a.metadata.title,
                    a.sync_id,
                )
                    .cmp(&(
                        &b.metadata.artist,
                        &b.metadata.album,
                        b.metadata.disc_number,
                        b.metadata.track_number,
                        &b.metadata.title,
                        b.sync_id,
                    ))
            });
            json!({"matched_tracks":tracks.len(), "total_tracks":catalog.tracks().len(),
                "tracks":tracks.into_iter().map(|t|track_json(catalog,t)).collect::<Vec<_>>()})
        }
    };
    if json_output {
        let report = json!({"schema":"rustunes.library",
            "media_anchor":catalog.persisted_media_anchor(),"catalog_database_sha256":hex(&catalog.source_hashes().database),
            "catalog_wal_sha256":catalog.source_hashes().wal.map(|v|hex(&v)),"data":body});
        serde_json::to_writer_pretty(&mut *out, &report)?;
        writeln!(out)?;
    } else {
        match view {
            View::List(_) => {
                writeln!(out, "ID                TITLE · ARTIST · ALBUM")?;
                for track in body["tracks"].as_array().expect("constructed array") {
                    writeln!(
                        out,
                        "{}  {} · {} · {}",
                        track["id"].as_str().unwrap(),
                        display(track["title"].as_str().unwrap()),
                        display(track["artist"].as_str().unwrap()),
                        display(track["album"].as_str().unwrap())
                    )?;
                }
                writeln!(
                    out,
                    "{} of {} tracks",
                    body["matched_tracks"], body["total_tracks"]
                )?;
            }
            View::Show(_) => {
                let t = &body["track"];
                for (label, key) in [
                    ("ID", "id"),
                    ("Title", "title"),
                    ("Sort title", "sort_title"),
                    ("Artist", "artist"),
                    ("Album", "album"),
                    ("Album artist", "album_artist"),
                    ("Genre", "genre"),
                    ("Composer", "composer"),
                    ("Copyright", "copyright"),
                    ("Comment", "comment"),
                    ("Year", "year"),
                    ("Track", "track_number"),
                    ("Track total", "track_count"),
                    ("Disc", "disc_number"),
                    ("Disc total", "disc_count"),
                    ("Duration (ms)", "duration_ms"),
                    ("Bytes", "file_bytes"),
                    ("Device path", "afc_path"),
                    ("Artwork registration", "artwork_cache_id"),
                    ("Content rating code", "content_rating"),
                    ("Compilation", "compilation"),
                    ("Play count", "play_count_user"),
                    ("Recent play count", "play_count_recent"),
                    ("Skip count", "skip_count_user"),
                    ("Recent skip count", "skip_count_recent"),
                ] {
                    let value = &t[key];
                    let text = value.as_str().map(display).unwrap_or_else(|| {
                        if value.is_null() {
                            "—".into()
                        } else {
                            value.to_string()
                        }
                    });
                    writeln!(out, "{label}: {text}")?;
                }
            }
            View::Stats => {
                writeln!(
                    out,
                    "{} tracks · {} albums · {} artists",
                    body["tracks"], body["albums_with_tracks"], body["artists_with_tracks"]
                )?;
                writeln!(
                    out,
                    "{} bytes · {} ms total duration",
                    body["file_bytes"], body["duration_ms"]
                )?;
                writeln!(
                    out,
                    "{} tracks have local artwork registrations",
                    body["tracks_with_artwork_registration"]
                )?;
                writeln!(out, "Media revision: {}", catalog.persisted_media_anchor())?;
            }
        }
    }
    Ok(())
}


fn count(value: &Value) -> String {
    let digits = value.as_i64().unwrap_or(0).to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, c) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    grouped
}

const PLAYLIST_NAME_WIDTH: usize = 28;

fn short_label(value: &str, width: usize) -> String {
    let safe = display(value);
    if safe.chars().count() <= width {
        safe
    } else {
        safe.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn container_name(playlist: &Value) -> String {
    let name = playlist["name"].as_str().unwrap_or("(unnamed)");
    let suffix = if playlist["is_primary_library"] == true {
        " [primary]"
    } else {
        ""
    };
    short_label(&format!("{name}{suffix}"), PLAYLIST_NAME_WIDTH)
}

fn system_container(playlist: &Value) -> bool {
    playlist["is_primary_library"] == true
        || matches!(
            playlist["name"].as_str(),
            Some("Playback History" | "Photos Memories" | "Voice Memos")
        )
}

/// Human-readable terminal output; --json retains the complete raw evidence.
pub fn write_playlists(
    report: &Value,
    comparison_requested: bool,
    out: &mut impl Write,
) -> Result<()> {
    if comparison_requested {
        write_favorites_comparison(report, out)?;
    } else {
        write_playlist_inventory(report, out)?;
    }
    write_catalog_notes(report, out)
}

fn write_playlist_inventory(report: &Value, out: &mut impl Write) -> Result<()> {
    let all = report["playlists"].as_array().cloned().unwrap_or_default();
    let active: Vec<_> = all.iter()
        .filter(|p| !system_container(p) && p["music_members"].as_i64().unwrap_or(0) > 0)
        .collect();
    let unmatched: Vec<_> = all.iter()
        .filter(|p| !system_container(p) && p["music_members"].as_i64().unwrap_or(0) == 0)
        .collect();
    let system: Vec<_> = all.iter().filter(|p| system_container(p)).collect();
    let favorites = &report["favorites"]["candidate_favorite_tracks_liked_state_2"];

    writeln!(out, "\nIPHONE MUSIC  /  PLAYLISTS")?;
    writeln!(out, "{}", "─".repeat(62))?;
    writeln!(
        out,
        "  {} containers    {} playlists with music    {} favorites",
        count(&json!(all.len())),
        count(&json!(active.len())),
        count(favorites)
    )?;

    writeln!(out, "\nPLAYLISTS  ({})", active.len())?;
    writeln!(out, "  {:<28} {:>9}", "Name", "Tracks")?;
    writeln!(out, "  {}", "─".repeat(38))?;
    for p in &active {
        writeln!(
            out,
            "  {:<28} {:>9}",
            container_name(p),
            count(&p["music_members"])
        )?;
    }

    if !unmatched.is_empty() {
        writeln!(out, "\nPLAYLISTS WITH NO MATCHED MUSIC  ({})", unmatched.len())?;
        writeln!(out, "  {:<28} {:>9} {:>11}", "Name", "Entries", "Unresolved")?;
        writeln!(out, "  {}", "─".repeat(50))?;
        for p in &unmatched {
            writeln!(
                out,
                "  {:<28} {:>9} {:>11}",
                container_name(p),
                count(&p["members"]),
                count(&p["dangling_members"])
            )?;
        }
    }

    if !system.is_empty() {
        writeln!(out, "\nBUILT-IN / LIBRARY CONTAINERS  ({})", system.len())?;
        writeln!(out, "  {:<28} {:>9} {:>11}", "Name", "Entries", "Unresolved")?;
        writeln!(out, "  {}", "─".repeat(50))?;
        for p in &system {
            writeln!(
                out,
                "  {:<28} {:>9} {:>11}",
                container_name(p),
                count(&p["members"]),
                count(&p["dangling_members"])
            )?;
        }
    }

    writeln!(
        out,
        "\nUnresolved = membership refers to an item not present in this database snapshot."
    )?;
    Ok(())
}

fn write_favorites_comparison(report: &Value, out: &mut impl Write) -> Result<()> {
    let comparison = &report["favorites_comparison"];
    let name = short_label(
        comparison["playlist_name"].as_str().unwrap_or("playlist"),
        PLAYLIST_NAME_WIDTH,
    );
    writeln!(out, "\nIPHONE MUSIC  /  FAVORITES vs {name}")?;
    writeln!(out, "{}", "─".repeat(62))?;
    if let Some(error) = comparison["error"].as_str() {
        writeln!(out, "Comparison unavailable: {}", display(error))?;
        return Ok(());
    }

    for (label, key) in [
        ("Favorited on iPhone", "liked_state_2_tracks"),
        ("In playlist", "playlist_synced_music_tracks"),
        ("In both", "overlap"),
    ] {
        writeln!(out, "  {:<32} {:>8}", label, count(&comparison[key]))?;
    }

    let missing = comparison["favorite_not_in_playlist"].as_array();
    let extra = comparison["playlist_not_favorited"].as_array();
    writeln!(out, "  {}", "─".repeat(43))?;
    writeln!(
        out,
        "  {:<32} {:>8}",
        "Favorites missing from playlist",
        count(&json!(missing.map_or(0, Vec::len)))
    )?;
    writeln!(
        out,
        "  {:<32} {:>8}",
        "Playlist tracks not favorited",
        count(&json!(extra.map_or(0, Vec::len)))
    )?;

    for (tracks, title) in [
        (missing, "FAVORITES NOT IN PLAYLIST"),
        (extra, "PLAYLIST TRACKS NOT FAVORITED"),
    ] {
        if let Some(tracks) = tracks.filter(|rows| !rows.is_empty()) {
            writeln!(out, "\n{title}  ({})", tracks.len())?;
            for track in tracks {
                let artist = display(track["artist"].as_str().unwrap_or("Unknown artist"));
                let song = display(track["title"].as_str().unwrap_or("Unknown title"));
                writeln!(out, "  {artist} — {song}")?;
            }
        }
    }
    let unresolved = comparison["playlist_unmatched_members"].as_u64().unwrap_or(0);
    let duplicated = comparison["duplicate_membership_rows"].as_u64().unwrap_or(0);
    if unresolved != 0 || duplicated != 0 {
        writeln!(
            out,
            "\nMembership notes: {unresolved} unresolved, {duplicated} duplicate rows"
        )?;
    }
    Ok(())
}

fn write_catalog_notes(report: &Value, out: &mut impl Write) -> Result<()> {
    let Some(warnings) = report["warnings"].as_array() else {
        return Ok(());
    };
    let amount = |code: &str| warnings.iter()
        .find(|warning| warning["code"] == code)
        .and_then(|warning| warning["count"].as_i64())
        .unwrap_or(0);
    let albums = amount("albums_without_artist");
    let tracks = amount("tracks_in_albums_without_artist");
    let other: Vec<_> = warnings.iter()
        .filter(|warning| !matches!(
            warning["code"].as_str(),
            Some("albums_without_artist" | "tracks_in_albums_without_artist")
        ))
        .collect();
    if albums == 0 && other.is_empty() {
        return Ok(());
    }
    writeln!(out, "\nCATALOG NOTES")?;
    if albums > 0 {
        writeln!(
            out,
            "  {albums} albums have no artist link ({tracks} tracks affected)"
        )?;
    }
    for warning in other {
        let code = warning["code"].as_str().unwrap_or("unknown");
        if let Some(n) = warning["count"].as_i64() {
            writeln!(out, "  {code}: {n}")?;
        } else if let Some(error) = warning["query_error"]
            .as_str()
            .or_else(|| warning["message"].as_str())
        {
            writeln!(out, "  {code}: {}", display(error))?;
        } else {
            writeln!(out, "  {code}")?;
        }
    }
    Ok(())
}


fn matches(track: &Track, filter: &Filter) -> bool {
    let m = &track.metadata;
    filter
        .artist
        .as_ref()
        .is_none_or(|v| m.artist == *v || m.album_artist == *v)
        && filter.album.as_ref().is_none_or(|v| m.album == *v)
        && filter.query.as_ref().is_none_or(|v| {
            let query = v.to_lowercase();
            [&m.title, &m.artist, &m.album, &m.album_artist]
                .iter()
                .any(|s| s.to_lowercase().contains(&query))
        })
}

fn track_json(catalog: &Catalog, track: &Track) -> Value {
    let m = &track.metadata;
    json!({"id":format!("{:016x}",track.sync_id),"album_id":format!("{:016x}",track.album_sync_id),
        "artist_id":format!("{:016x}",track.artist_sync_id),"title":m.title,
        "sort_title":catalog.title_sort(track.sync_id),"artist":m.artist,"album":m.album,
        "album_artist":m.album_artist,"genre":m.genre,"composer":m.composer,"copyright":m.copyright,
        "comment":catalog.comment(track.sync_id),
        "track_number":m.track_number,"track_count":m.track_count,"disc_number":m.disc_number,"disc_count":m.disc_count,
        "year":m.year,"duration_ms":m.total_time_ms,"file_bytes":m.file_size,"afc_path":track.afc_path,
        "content_rating":m.content_rating,"content_rating_level":m.content_rating_level,"compilation":m.is_compilation,
        "play_count_user":track.playback.play_count_user,"play_count_recent":track.playback.play_count_recent,
        "skip_count_user":track.playback.skip_count_user,"skip_count_recent":track.playback.skip_count_recent,
        "has_been_played":track.playback.has_been_played,"user_rating":track.playback.user_rating,
        "user_rating_is_derived":track.playback.user_rating_is_derived,"liked_state":track.playback.liked_state,
        "artwork_cache_id":catalog.artwork().track_cache_ids().get(&track.sync_id),
        "availability":{"placeholder":track.availability.is_placeholder,"library_added":track.availability.is_library_added,
        "non_purgeable_asset":track.availability.has_non_purgeable_asset,"supported_type":track.availability.is_supported_type}})
}
fn stats(catalog: &Catalog) -> Result<Value> {
    let bytes = catalog.tracks().iter().try_fold(0u64, |sum, t| {
        sum.checked_add(t.metadata.file_size)
            .context("catalogue total file size overflows")
    })?;
    Ok(json!({"tracks":catalog.tracks().len(),
        "albums_with_tracks":catalog.albums().values().filter(|v|v.live_track_count>0).count(),
        "artists_with_tracks":catalog.artists().values().filter(|v|v.live_track_count>0).count(),
        "file_bytes":bytes,
        "duration_ms":catalog.tracks().iter().map(|t|u64::from(t.metadata.total_time_ms)).sum::<u64>(),
        "tracks_with_artwork_registration":catalog.artwork().track_cache_ids().len()}))
}
fn display(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_width_ids_preserve_high_bits_and_reject_ambiguous_decimal_inputs() {
        assert_eq!(parse_id("ffffffffffffffff").unwrap(), u64::MAX);
        for text in [
            "42",
            "-1",
            "0x1000000000000000",
            "0000000000000000",
            "gggggggggggggggg",
        ] {
            assert!(parse_id(text).is_err());
        }
        assert_eq!(display("name\x1b[31m\n"), "name\\u{1b}[31m\\n");
    }
    #[test]
    fn playlist_output_is_compact_and_grouped() {
        let report = json!({
            "playlists": [
                {"name":"80s","music_members":18,"members":18,"dangling_members":0,"is_primary_library":false},
                {"name":"pancakes","music_members":0,"members":2643,"dangling_members":2643,"is_primary_library":true}
            ],
            "favorites":{"candidate_favorite_tracks_liked_state_2":1594},
            "warnings":[
                {"code":"albums_without_artist","count":18},
                {"code":"tracks_in_albums_without_artist","count":38}
            ]
        });
        let mut out = Vec::new();
        write_playlists(&report, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("PLAYLISTS  (1)"));
        assert!(text.contains("BUILT-IN / LIBRARY CONTAINERS  (1)"));
        assert!(text.contains("pancakes [primary]"));
        assert!(text.contains("2,643"));
        assert!(text.contains("1,594 favorites"));
        assert!(text.contains("18 albums have no artist link (38 tracks affected)"));
        assert!(!text.contains("Raw liked_state breakdown"));
    }

    #[test]
    fn full_playlist_output_separates_built_in_music_and_unmatched_lists() {
        let report = json!({
            "playlists": [
                {"name":"flurbz","music_members":1591,"members":1591,"dangling_members":0,"is_primary_library":false},
                {"name":"clandestine","music_members":0,"members":2424,"dangling_members":2424,"is_primary_library":false},
                {"name":"Playback History","music_members":250,"members":251,"dangling_members":1,"is_primary_library":false},
                {"name":"Voice Memos","music_members":0,"members":0,"dangling_members":0,"is_primary_library":false}
            ],
            "favorites":{"candidate_favorite_tracks_liked_state_2":1594},
            "warnings":[]
        });
        let mut out = Vec::new();
        write_playlists(&report, false, &mut out).unwrap();
        let display = String::from_utf8(out).unwrap();
        assert!(display.contains("4 containers"));
        assert!(display.contains("1 playlists with music"));
        assert!(display.contains("PLAYLISTS  (1)"));
        assert!(display.contains("flurbz"));
        assert!(display.contains("1,591"));
        assert!(display.contains("PLAYLISTS WITH NO MATCHED MUSIC  (1)"));
        assert!(display.contains("clandestine"));
        assert!(display.contains("2,424"));
        assert!(display.contains("BUILT-IN / LIBRARY CONTAINERS  (2)"));
        assert!(display.contains("Playback History"));
        assert!(display.contains("Voice Memos"));
        assert!(display.contains("Unresolved = membership"));
    }

    #[test]
    fn comparison_view_only_shows_mismatched_tracks() {
        let report = json!({
            "playlists":[{"name":"80s","music_members":18}],
            "favorites_comparison":{
                "playlist_name":"flurbz",
                "liked_state_2_tracks":1594,
                "playlist_synced_music_tracks":1591,
                "overlap":1591,
                "favorite_not_in_playlist":[
                    {"artist":"Spacemen 3","title":"Call The Doctor"},
                    {"artist":"Basa Basa","title":"Homowo"}
                ],
                "playlist_not_favorited":[],
                "playlist_unmatched_members":0,
                "duplicate_membership_rows":0
            },
            "warnings":[]
        });
        let mut out = Vec::new();
        write_playlists(&report, true, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("FAVORITES vs flurbz"));
        assert!(text.contains("1,594"));
        assert!(text.contains("Spacemen 3 — Call The Doctor"));
        assert!(text.contains("Basa Basa — Homowo"));
        assert!(!text.contains("PLAYLISTS  ("));
        assert!(!text.contains("BUILT-IN / LIBRARY CONTAINERS"));
    }

    #[test]
    fn json_filters_and_missing_id_are_explicit() {
        let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
        let mut out = Vec::new();
        write(
            &catalog,
            &View::List(Filter {
                artist: Some("absent".into()),
                ..Filter::default()
            }),
            true,
            &mut out,
        )
        .unwrap();
        let json: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(json["data"]["matched_tracks"], 0);
        assert_eq!(json["data"]["total_tracks"], 1);
        assert_eq!(json["media_anchor"], 13);
        assert!(write(&catalog, &View::Show(42), false, &mut Vec::new()).is_err());
        write(&catalog, &View::Stats, true, &mut Vec::new()).unwrap();
    }
}
