# Flurb iPhone playlist test

This directory includes **10 committed, playable MP3 files**, each approximately
five seconds long (5.04 seconds due to MPEG frame boundaries). They contain
different kinds of synthesized noise: white, pink, brown, blue, violet, velvet,
low rumble, bright hiss, radio static and wind gust.

All ten are **ID3v2.3** files. They have distinct titles, sequential
1/10–10/10 track numbers, artist `Flurb Test Artist` and album
`Flurb Playlist Test`. Each was decoded, checked for a ~5-second duration
and verified as ID3v2.3 before committing.

**No Homebrew, FFmpeg, or generator command is required on your Mac.**
The audio files are already in `generated/` and tracked by git.

From the repository root, with the iPhone connected:

```sh
git pull
cargo run -- library stats
cargo run -- push tests/playlist-sync/generated --dry-run
```

Only after confirming that the phone has the expected empty baseline and the
dry run proposes exactly 10 additions:

```sh
cargo run -- push tests/playlist-sync/generated
cargo run -- library stats
```

`playlist.m3u8` is the intended ten-song ordering **for later playlist
creation**; it is not automatically synchronized by `push`.

Rustunes now has a pure `insert_playlist` operation encoder, but the journaled
playlist-create CLI, signed transport and after-state membership verification
are still to be implemented. Do not treat a successful track push as evidence
that the playlist was created.


## Experimental playlist creation (feature branch only)

Once the `push generated --dry-run` result confirms exactly ten matched
Rustunes-owned MP3 tracks (zero additions, zero conflicts), use its retained
`plan.json` to bind the M3U8 entries to existing device track IDs.

First check the code builds and the new tests pass:

```sh
cargo test -p rustunes playlist_push
cargo test -p rustunes playlist_push_requires_an_m3u8
```

Point `PLAN` to the exact `plan.json` reported by the most recent successful
track push dry-run:

```sh
PLAN="/absolute/path/to/plan.json"
cargo run -- push tests/playlist-sync/playlist.m3u8 --from-plan "$PLAN" --playlist-name "Flurb Playlist Test" --dry-run
```

**Only after verifying that the dry run reports ten matched tracks**, run:

```sh
cargo run -- push tests/playlist-sync/playlist.m3u8 --from-plan "$PLAN" --playlist-name "Flurb Playlist Test" --yes
cargo run -- list
```

This is an isolated trial of one confirmed `insert_playlist` shape, not a
production playlist sync. It reuses the existing signed metadata-only transport,
writes durable input/Started evidence under the local state `playlist-trials`
directory, refuses duplicate names and ambiguous IDs, and checks the full
ordered membership and unchanged audio after sending. **Do not blindly rerun
a started trial after any transport or verification error**; inspect the phone
with `list` and retain its evidence. The normal `resume` CLI does not yet
handle these experimental playlist trial records.
