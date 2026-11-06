# Flurb iPhone playlist test

This directory holds **10 committed, playable MP3 files**, each around five
seconds long, with artist `Flurb Test Artist` and album
`Flurb Playlist Test`. Their desired order is in `playlist.m3u8`.

## Create a playlist from audio already on the phone

The experimental `push playlist.m3u8` creates an iPhone playlist by matching
the source files directly against a **fresh, read-only, complete device audio
index**. It does **not** need a retained plan or journal path.

From the repository root, with the iPhone connected, first inspect:

```sh
cargo run -- push tests/playlist-sync/playlist.m3u8 --playlist-name "Flurb Playlist Test" --dry-run
```

A successful dry run reports 10 exact, unique installed MP3 matches and
**no phone writes**. Then create the playlist:

```sh
cargo run -- push tests/playlist-sync/playlist.m3u8 --playlist-name "Flurb Playlist Test"
cargo run -- list
```

The name defaults to the M3U8 filename stem; `--playlist-name` overrides it.

If an MP3 is missing on the phone, **first push its audio**:

```sh
cargo run -- push tests/playlist-sync/generated
```

That step sends tracks but does not create the playlist. The second step
references the tracks already on the phone; it does not transfer audio or
artwork again.

## Limitations of this first implementation

- Currently accepts 1–499 distinct MP3 paths, relative to the M3U8 file.
  Other codecs and playlists of 500+ tracks will be supported separately.
- Only a **unique byte-for-byte match** to an already installed MP3 qualifies.
  If tags differ, or multiple device copies match the same file, it refuses.
- Creates an ordinary playlist in M3U8 order. It refuses an existing playlist
  with the same name rather than updating or duplicating it.
- A signed session saves persistent Started evidence before writing. After a
  lost connection, **do not blindly rerun**; an unresolved Started trial blocks
  another trial on the same phone/library until investigated. The general
  `resume` command does not yet support these trial records.
- A successful run verifies the new playlist's identity, name and ordered
  membership and confirms the phone audio inventory was preserved.

No external audio generator or FFmpeg step is needed for these MP3 fixtures.
