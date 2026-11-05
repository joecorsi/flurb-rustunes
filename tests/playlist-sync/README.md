# Flurb iPhone playlist sync fixture

Run `bash tests/playlist-sync/generate.sh` from the repository root. It
creates 10 **distinct** six-second MP3 tone files under `generated/`,
with **ID3v2.3** tags, one album, explicit 1/10 through 10/10 track
numbers, and unique song titles. The generator verifies every file begins
with the ID3v2.3 byte sequence (`49 44 33 03`). Requires `ffmpeg` and
`xxd` on macOS.

`playlist.m3u8` lists their desired order for the test. It is a **local
test manifest**, not a playlist that rustunes can currently transfer to
the iPhone.

Testing stages:

1. Audit the cleared phone with `cargo run -- library stats` and
   `cargo run -- library playlists`. Stop if unexpected tracks remain.
2. Run the generator. Push only the generated directory using the existing
   rustunes `push` command, first in dry-run mode. Verify exactly 10
   distinct tracks after the transfer.
3. Use the planned journaled playlist-create command to resolve all 10
   transferred track sync IDs, create one ordinary playlist via Apple's
   `insert_playlist`, and verify the exact 10 members in order.
4. Verify creating/deleting that playlist leaves all ten MP3 tracks intact.

A **serializer** for `insert_playlist` exists in
`music_protocol::catalogue::mutation::insert_playlist`; the journaled
phone-send and membership verification entry point is not yet implemented.
Do not use a raw unsigned operation payload as a sync command.

The generated files are excluded from git and can be reproduced as needed.
