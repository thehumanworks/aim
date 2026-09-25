#!/bin/sh
# Rebuild the checked-in WASIp2 components with the Rust target pinned in mise.toml.
set -eu

examples_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
target_dir=${CARGO_TARGET_DIR:-"$examples_dir/target"}
mode=${1:---verify}

case "$mode" in
  --verify|--update) ;;
  *) echo "usage: $0 [--verify|--update]" >&2; exit 2 ;;
esac

CARGO_TARGET_DIR="$target_dir" CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 \
  cargo build --manifest-path "$examples_dir/Cargo.toml" --target wasm32-wasip2 --release --locked

python3 -B - "$examples_dir" "$target_dir" "$mode" <<'PY'
from hashlib import sha256
from pathlib import Path
import sys

examples = Path(sys.argv[1])
target = Path(sys.argv[2])
mode = sys.argv[3]
names = (
    "aim_example_delegate_read.wasm",
    "aim_example_kv_counter.wasm",
    "aim_example_runaway.wasm",
)
build = examples / "build"
build.mkdir(exist_ok=True)

if mode == "--update":
    for name in names:
        (build / name).write_bytes((target / "wasm32-wasip2/release" / name).read_bytes())

expected = {}
for line in (build / "SHA256SUMS").read_text().splitlines() if (build / "SHA256SUMS").exists() else ():
    digest, name = line.split(maxsplit=1)
    expected[name.strip()] = digest

lines = []
for name in names:
    compiled = (target / "wasm32-wasip2/release" / name).read_bytes()
    checked = (build / name).read_bytes()
    if compiled != checked:
        raise SystemExit(f"{name}: checked-in component differs from rebuilt component")
    digest = sha256(checked).hexdigest()
    if mode == "--verify" and expected.get(name) != digest:
        raise SystemExit(f"{name}: SHA256SUMS mismatch")
    lines.append(f"{digest}  {name}\n")

if mode == "--update":
    (build / "SHA256SUMS").write_text("".join(lines))
print("All example components match their checked-in hashes.")
PY
