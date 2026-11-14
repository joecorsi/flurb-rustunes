# Flurb / rustunes MPEG reader compatibility

Sync source inspection and phone readback share audio boundaries. Original files
are never repaired, retagged, transcoded or rewritten by these read adapters.

| Case | Read behavior |
| --- | --- |
| Zero gap after ID3, including gaps over 16 MiB | Scan in fixed 8 KiB chunks; do not allocate the gap as a tag |
| ASCII-space / mixed zero-space gap after ID3 | Same chunked scan after the declared tag; validate every audio frame and preserve all padding bytes |
| Zero prefix without ID3 | Same streaming rule; require consecutive complete frames afterward |
| Trailing APE / ID3v1 / enhanced ID3v1 | Exclude verified tag boundaries from audio timing; retain all bytes in copy/hash |
| Lyrics3v1 / Lyrics3v2 / appended ID3v2.4 | Recognize explicit boundary markers and sizes, including stacked tags |
| MPEG-2 / MPEG-2.5 | Use the same padding and trailing-container rules as MPEG-1 |
| ID3v2.2 / v2.3 / v2.4 | Read version-specific frame sizes and known IDs |
| ID3 extended headers / experimental marker | Respect extended-header size/flag structure; experimental is advisory |
| UTF-8 declared in v2.3 | Decode the declared encoding without rewriting |
| Empty text with a declared encoding, including UTF-16 without a BOM | Treat cleared text/numeric/sort fields as empty across ID3 versions; nonempty ambiguous UTF-16 still needs byte order |
| Odd UTF-16 terminal zero | Ignore the extra zero; retain all source bytes |
| Invalid UTF-8 / UTF-16 code units | Display replacement characters, preserving original frame evidence and source |
| Stale Xing byte estimate | Retain declaration; use the complete frame scan for observed identity |
| Complete frames plus incomplete final frame | Existing bounded partial-tail rule; no claim that missing audio was repaired |

Except for explicit ASCII-space tag padding, unidentified nonzero bytes are not skipped to find a convenient frame. A size
that exceeds its file, transformed ID3 frames without a decoder, changing MPEG
stream shape, and invalid frame chains still fail. Malformed tag errors after a
validated audio scan identify ID3 metadata rather than claiming unreadable music.
ID3 metadata and trailing containers stay bounded to 16 MiB; the streamed prefix
is bounded by the existing 512 MiB file limit. Full-file identity includes gaps,
metadata, and any partial tail. Source and receipt size accounting must agree.

Regression coverage includes boundary markers, malformed lengths, metadata/text
variants, artwork readback with prefix padding, unchanged encoded audio identity,
source byte preservation, and receipt accounting for padded files. Real-file
checks are performed explicitly, not by launching a library-wide background scan.

Optional FFmpeg validation keeps successful decoding separate from recoverable
metadata diagnostics: exit-zero messages become visible warnings. Nonzero exits,
timeouts, and excessive/truncated diagnostics remain failures; `-xerror` stays on.
