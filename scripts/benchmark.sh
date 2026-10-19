#!/usr/bin/env bash
# benchmark.sh — run the built-in loopback benchmark across a sweep of file
# sizes and chunk sizes, emitting one JSON line per run to stdout.
#
# The benchmark path is the *real* encrypted + per-chunk-verified transfer over
# a loopback TCP socket, so numbers include crypto + hashing overhead. Loopback
# removes the physical link, so results reflect the CPU/crypto ceiling, not your
# network. Use `laptoplink send`/`receive` between two machines for link tests.
#
# Usage: ./scripts/benchmark.sh > results.jsonl
set -euo pipefail

cargo build --release -q
BIN="target/release/laptoplink"
HOME_DIR="$(mktemp -d)"
trap 'rm -rf "$HOME_DIR"' EXIT

SIZES_MIB=(256 1024)
CHUNK_SIZES=(1048576 4194304 8388608)   # 1 MiB, 4 MiB, 8 MiB

for size in "${SIZES_MIB[@]}"; do
    for cs in "${CHUNK_SIZES[@]}"; do
        "$BIN" --home "$HOME_DIR" --json benchmark --size-mib "$size" --chunk-size "$cs"
    done
done
