#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
# SQLite NOFOLLOW intentionally rejects aliases. macOS supplies /var (a
# symlink) in TMPDIR, so give every synthetic fixture the physical temp path.
export TMPDIR="$(cd "${TMPDIR:-/tmp}" && pwd -P)"
if [[ -x ../flurb-rust/target/tools/ffmpeg ]]; then
  export PATH="$(cd ../flurb-rust/target/tools && pwd -P):$PATH"
fi
cargo test --locked --workspace -- --test-threads="${RUST_TEST_THREADS:-2}" "$@"
