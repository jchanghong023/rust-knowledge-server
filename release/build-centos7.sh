#!/usr/bin/env bash
set -euo pipefail

if [[ $(uname -m) != x86_64 || $(getconf GNU_LIBC_VERSION) != 'glibc 2.17' ]]; then
    printf '%s\n' 'Build on CentOS 7 x64 or a glibc 2.17 x64 build host.' >&2
    exit 1
fi

cd -- "$(dirname -- "$0")/.."
repo=$PWD
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

# Cargo first verifies each locked registry archive. Apply only the recorded
# change to the scratch copy; never update the Cargo.lock or registry cache.
cargo vendor --locked --versioned-dirs "$work/vendor" > "$work/source.toml"
edge=$work/vendor/qdrant-edge-0.9.0
patch --batch --forward --fuzz=0 --directory "$edge" -p1 < "$repo/release/qdrant-edge-0.9.0-centos7.patch"
python3 - "$edge" <<'PY'
import hashlib
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
metadata = root / '.cargo-checksum.json'
checksums = json.loads(metadata.read_text(encoding='utf-8'))
changed = 'src/common/universal_io/io_uring/mod.rs'
if changed not in checksums['files']:
    raise SystemExit(f'Patched source not listed in Cargo checksums: {changed}')
checksums['files'][changed] = hashlib.sha256((root / changed).read_bytes()).hexdigest()
metadata.write_text(json.dumps(checksums), encoding='utf-8')
PY
cargo --config "$work/source.toml" build --locked --offline --release --target x86_64-unknown-linux-gnu
