#!/usr/bin/env bash
# demo.sh — end-to-end two-process LaptopLink transfer on one machine.
#
# Spins up a receiver (in trust-on-first-use pairing mode) and a sender in
# separate processes with isolated identities, transfers a directory tree plus
# a large file, then verifies every file with SHA-256.
#
# Usage: ./scripts/demo.sh [SIZE_MB]   (default large file: 200 MB)
set -euo pipefail

SIZE_MB="${1:-200}"
PORT=9099
ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT

# Build the release binary once.
cargo build --release -q
BIN="target/release/laptoplink"

SHOME="$ROOT/sender" RHOME="$ROOT/receiver"
SRC="$ROOT/src" DEST="$ROOT/dest"
mkdir -p "$SRC/nested" "$DEST"

echo ">> generating test data (${SIZE_MB} MB + small files)"
head -c "$((SIZE_MB * 1000000))" /dev/urandom > "$SRC/big.bin"
head -c 1048576 /dev/urandom > "$SRC/nested/a.bin"
echo "hello laptoplink" > "$SRC/note.txt"

echo ">> starting receiver on 127.0.0.1:${PORT}"
"$BIN" --home "$RHOME" receive --dest "$DEST" --bind "127.0.0.1:${PORT}" \
    --accept-new --once &
RECV_PID=$!
sleep 1

echo ">> sending"
"$BIN" --home "$SHOME" send "$SRC/big.bin" "$SRC/nested" "$SRC/note.txt" \
    --to "127.0.0.1:${PORT}"

wait "$RECV_PID" || true

echo ">> verifying integrity"
fail=0
for rel in big.bin note.txt nested/a.bin; do
    a="$(shasum -a 256 "$SRC/$rel" | awk '{print $1}')"
    b="$(shasum -a 256 "$DEST/$rel" | awk '{print $1}')"
    if [[ "$a" == "$b" ]]; then
        echo "   OK   $rel"
    else
        echo "   FAIL $rel ($a != $b)"
        fail=1
    fi
done

exit "$fail"
