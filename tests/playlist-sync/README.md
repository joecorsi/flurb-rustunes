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
