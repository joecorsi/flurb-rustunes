//! Read-only, concise library overview. Only --json includes individual tracks.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::io::Write;

fn visible_playlist(value: &Value) -> bool {
    value["is_primary_library"] != true
        && !matches!(
            value["name"].as_str(),
            Some("Playback History" | "Photos Memories" | "Voice Memos")
        )
}

fn safe(value: &str) -> String {
    value.chars().flat_map(|c| {
        if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            c.escape_default().collect::<Vec<_>>()
        } else {
            vec![c]
        }
    }).collect()
}

fn grouped(value: i64) -> String {
    let digits = value.to_string();
    let mut result = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i != 0 && (digits.len() - i) % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result
}

/// Display only summary totals and playlists unless a complete JSON export is requested.
pub(super) fn write(
    report: &Value,
    device_name: Option<&str>,
    json_output: bool,
    verbose: bool,
    out: &mut impl Write,
) -> Result<()> {
    let tracks = report["stats"]["tracks"].as_i64().context("track count unavailable")?;
    let albums = report["stats"]["album_ids"].as_i64().context("album count unavailable")?;
    let all = report["playlists"].as_array().context("playlist inventory unavailable")?;
    let playlists: Vec<_> = all.iter().filter(|p| visible_playlist(p)).collect();
    let issues = report["warnings"].as_array().context("catalogue warnings unavailable")?;
    ensure!(
        !issues.iter().any(|issue| matches!(
            issue["code"].as_str(),
            Some("playlist_query_error" | "playlist_row_error" | "playlist_row_limit")
        )),
        "playlist inventory incomplete"
    );
    if json_output {
        ensure!(report["tracks"].is_array(), "detailed track inventory unavailable");
        ensure!(report["albums"].is_array(), "detailed album inventory unavailable");
        ensure!(report["playlist_membership"].is_object(), "playlist membership unavailable");
        let mut result = report.clone();
        result["schema"] = json!("rustunes.list.v1");
        result["device"] = json!({"name": device_name});
        result["visible_playlist_count"] = json!(playlists.len());
        serde_json::to_writer_pretty(&mut *out, &result)?;
        writeln!(out)?;
        return Ok(());
    }

    writeln!(out, "iPhone: {}", safe(device_name.unwrap_or("Connected iPhone")))?;
    writeln!(out, "\nLibrary")?;
    writeln!(out, "  Tracks:    {}", grouped(tracks))?;
    writeln!(out, "  Albums:    {}", grouped(albums))?;
    writeln!(out, "  Playlists: {}", grouped(playlists.len() as i64))?;
    writeln!(out, "\nPlaylists")?;
    if playlists.is_empty() {
        writeln!(out, "  (none)")?;
    } else {
        writeln!(out, "  {:<16}  {:<32}  {:>8}", "ID", "NAME", "TRACKS")?;
        for playlist in playlists {
            let id = playlist["container_pid"].as_str().context("playlist ID unavailable")?;
            let name = safe(playlist["name"].as_str().unwrap_or("(unnamed)"));
            let count = playlist["music_members"].as_i64().context("playlist track count unavailable")?;
            writeln!(out, "  {:<16}  {:<32}  {:>8}", id, name, grouped(count))?;
        }
    }
    if !issues.is_empty() {
        writeln!(out, "\nNote: {} catalogue diagnostic(s). Use --verbose for details.", issues.len())?;
        if verbose {
            for issue in issues {
                let code = safe(issue["code"].as_str().unwrap_or("unknown"));
                let message = safe(issue["message"].as_str()
                    .or_else(|| issue["query_error"].as_str())
                    .unwrap_or("see list --json"));
                writeln!(out, "  [{code}] {message}")?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        json!({
            "stats": {"tracks":30000,"album_ids":2450},
            "playlists": [
                {"container_pid":"81","name":"Primary Library","music_members":30000,"is_primary_library":true},
                {"container_pid":"82","name":"Road Trip","music_members":86,"is_primary_library":false},
                {"container_pid":"83","name":"Empty","music_members":0,"is_primary_library":false}
            ],
            "warnings":[],
            "tracks":[{"id":"ffffffffffffffff","title":"Song"}],
            "albums":[{"id":"31","name":"Album"}],
            "playlist_membership":{"82":[{"track_id":"ffffffffffffffff"}]}
        })
    }

    #[test]
    fn default_list_shows_totals_and_playlists_but_not_thousands_of_tracks() {
        let mut out = Vec::new();
        write(&fixture(), Some("Test iPhone"), false, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("30,000"));
        assert!(text.contains("2,450"));
        assert!(text.contains("Playlists: 2"));
        assert!(text.contains("Road Trip"));
        assert!(text.contains("Empty"));
        assert!(!text.contains("Primary Library"));
        assert!(!text.contains("Song"));
    }

    #[test]
    fn json_contains_full_inventory_and_membership() {
        let mut out = Vec::new();
        write(&fixture(), None, true, false, &mut out).unwrap();
        let result: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(result["schema"], "rustunes.list.v1");
        assert_eq!(result["visible_playlist_count"], 2);
        assert_eq!(result["tracks"][0]["title"], "Song");
        assert_eq!(result["playlist_membership"]["82"][0]["track_id"], "ffffffffffffffff");
    }

    #[test]
    fn detailed_warnings_require_verbose() {
        let mut report = fixture();
        report["warnings"] = json!([{"code":"broken_join","message":"Details here"}]);
        let mut out = Vec::new();
        write(&report, None, false, false, &mut out).unwrap();
        assert!(!String::from_utf8(out.clone()).unwrap().contains("Details here"));
        out.clear();
        write(&report, None, false, true, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("Details here"));
    }
}
