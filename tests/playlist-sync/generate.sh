#!/usr/bin/env bash
# Reproducible, local ID3v2.3 MP3 fixture for the first iPhone playlist sync.
set -euo pipefail

command -v ffmpeg >/dev/null || {
  echo "ffmpeg is required (on macOS: brew install ffmpeg)" >&2
  exit 1
}
command -v xxd >/dev/null || {
  echo "xxd is required to validate the ID3v2.3 header" >&2
  exit 1
}

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="$root/generated"
mkdir -p "$out"

# Different audio for each file: a content-based sync must not collapse them.
frequencies=(220 246 262 294 330 349 392 440 494 523)
for i in "${!frequencies[@]}"; do
  number="$(printf '%02d' "$((i + 1))")"
  file="$out/Flurb-Test-${number}.mp3"
  ffmpeg -hide_banner -loglevel error -nostdin -y \
    -f lavfi -i "sine=frequency=${frequencies[i]}:sample_rate=44100:duration=6" \
    -map 0:a:0 -map_metadata -1 \
    -c:a libmp3lame -q:a 5 -ar 44100 -ac 2 \
    -id3v2_version 3 -write_id3v1 0 \
    -metadata "title=Flurb Test ${number}" \
    -metadata "artist=Flurb Test Artist" \
    -metadata "album_artist=Flurb Test Artist" \
    -metadata "album=Flurb Playlist Test" \
    -metadata "track=$((i + 1))/10" \
    -metadata "date=2026" \
    -metadata "genre=Electronic" \
    "$file"
  header="$(xxd -p -l 4 "$file" | tr -d '\n')"
  if [[ "$header" != "49443303" ]]; then
    echo "Incorrect ID3 tag version for $file: $header" >&2
    exit 1
  fi
  echo "OK  $number  ${frequencies[i]}Hz  ID3v2.3"
done
echo
echo "Generated 10 distinct MP3 files in $out"
echo "Playlist manifest: $root/playlist.m3u8"
