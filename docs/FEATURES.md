# Implemented vs. Remaining

An honest status of LaptopLink against the full specification. The project is
built phase by phase; this is the ground truth as of the current commit.

Legend: ✅ done & tested · 🟡 partial · ⬜ not started

## Phase 1 — Architecture & threat model ✅
- ✅ System architecture ([ARCHITECTURE.md](ARCHITECTURE.md))
- ✅ Network protocol spec ([PROTOCOL.md](PROTOCOL.md))
- ✅ Security/threat model ([THREAT_MODEL.md](THREAT_MODEL.md))
- ✅ Transfer manifest format ([manifest.rs](../crates/laptoplink-core/src/manifest.rs))
- ✅ Chunking strategy ([chunk.rs](../crates/laptoplink-core/src/chunk.rs))
- ✅ Resume algorithm (receiver bitmap + signature) 
- ✅ Filesystem safety rules ([pathsafety.rs](../crates/laptoplink-core/src/pathsafety.rs))
- ✅ Benchmarking methodology ([BENCHMARKING.md](BENCHMARKING.md))
- ✅ Repository structure
- 🟡 Delta-transfer design — documented as design; not yet implemented (Phase 3)

## Phase 2 — High-performance local CLI engine ✅ (core milestone)
- ✅ Cryptographic device identity (X25519 static keys, BLAKE3 fingerprint)
- ✅ Secure pairing with explicit receiver approval (+ TOFU `--accept-new`)
- ✅ Device revocation
- ✅ Mutually-authenticated, encrypted transport (Noise `XX` / ChaChaPoly)
- ✅ Streaming with bounded memory (one chunk in flight)
- ✅ Chunked transfer with per-chunk BLAKE3 verification
- ✅ Whole-file BLAKE3 verification before commit
- ✅ Atomic finalize (temp `.part` → fsync → rename), symlink-overwrite guard
- ✅ Path-traversal / absolute-path / prefix defenses
- ✅ Resource limits (files, chunks, message/chunk sizes, in-flight bytes)
- ✅ Durable manifests + crash-safe atomic manifest writes
- ✅ Resume: skip completed files; send only missing chunks (tested)
- ✅ Directory-tree transfer (structure preserved, no symlink following)
- ✅ Progress reporting (human live line + `--json` events)
- ✅ Stable exit codes, `--quiet`, `--verbose`, configurable chunk size
- ✅ CLI: `device {show,list,pair,revoke}`, `send`, `receive`,
      `transfer {list,status,resume,cancel}`, `benchmark`, `doctor`
- ✅ `send --dest` with receiver `--allow-sender-dest` (sender-chosen
      destination directory, opt-in for safety)
- ✅ Background agent: `agent {start,stop,status}` — an always-on receiver that
      runs detached and survives closing the terminal (stop it with `agent stop`)
- ✅ Real two-process transfers + SHA-256-verified integration tests
- 🟡 64-bit offsets & overflow-checked arithmetic — used throughout; more
      property tests wanted
- ⬜ mDNS/DNS-SD local discovery (manual addressing works today)
- ⬜ Concurrent inbound transfers (sequential today)
- ⬜ Parallel per-chunk / multi-connection send; adaptive concurrency
- ⬜ Bandwidth rate-limiting flag
- ⬜ Exclusion/inclusion patterns on send

## Phase 3 — Resume, sync & delta ✅ (core)
- ✅ Persistent manifests & verified-chunk reuse (from Phase 2)
- ✅ One-way directory sync (`laptoplink sync`)
- ✅ Skip-identical by size + mtime (mtime preserved on commit) or `--checksum`
- ✅ Dry-run mode (`sync --dry-run`) with no side effects
- ✅ Exclude patterns (`--exclude GLOB`, repeatable; also protect from delete)
- ✅ Safe deletion policy (`sync --delete`, confirmation-gated, post-transfer)
- ✅ Conflict detection (dest is a dir/symlink where a file is expected)
- ✅ Rolling-checksum + strong-hash (BLAKE3) delta transfer for changed files
      (`sync --delta`), with whole-file verification before commit and a size
      threshold below which full transfer is used
- ⬜ Two-way sync (deliberately deferred; conflict/deletion semantics)
- ⬜ Published, reproducible comparisons vs rsync under equal settings
- ⬜ Delta fallback heuristic when delta would send more than a full transfer
      (today delta is opt-in per run and gated by a minimum file size)

## Phase 4 — Desktop app (Tauri + React/TS) ⬜
- ⬜ Tauri shell wrapping the core `Engine`
- ⬜ Drag-and-drop, device management, incoming approval, live progress
- ⬜ Pause/resume/retry/cancel, history, settings, disk-space warnings
- ⬜ Direct-vs-relayed connection indicator

## Phase 5 — Internet networking ⬜
- ⬜ Secure signaling service (coordination separate from data)
- ⬜ NAT traversal (ICE/STUN/TURN or equivalent)
- ⬜ Encrypted relay fallback that cannot decrypt E2E content
- ⬜ QUIC vs TCP/TLS evaluation under representative workloads
- ⬜ Reconnection & resume across network failures
- ⬜ Network diagnostics in `doctor`

## Phase 6 — Web interface (Next.js + TS) ⬜
- ⬜ Next.js/TypeScript frontend
- ⬜ Backend for signaling, registration, authorization
- ⬜ Browser streaming transfers with documented per-browser limits & fallbacks

## Phase 7 — Production hardening 🟡
- ✅ Cross-platform CI (fmt + clippy + test on Linux/macOS/Windows)
- ✅ Benchmark scripts + demo script
- ✅ Architecture, protocol, threat-model, troubleshooting docs
- 🟡 Packaging/release — binaries build in release; installers/signing pending
- ⬜ OS keystore integration for private keys (file `0o600` today)
- ⬜ Fuzzing of protocol/manifest/path parsers; property-based tests
- ⬜ Rate-limiting of unauthenticated handshakes
- ⬜ Temp-file retention/cleanup policy
- ⬜ Signed release artifacts & SBOM

## Platform notes
- **Linux and macOS**: fully built and end-to-end tested in CI (unit +
  integration + loopback benchmark).
- **Windows**: builds, passes unit tests, and has a one-line installer. The
  end-to-end transfer tests are gated off Windows: in the single-process
  loopback test harness they intermittently hit an abortive socket reset at
  session teardown and an antivirus/indexer "Access is denied" on the finalize
  rename. A graceful half-close + drain (`SecureStream::graceful_close`) and a
  retrying atomic rename make it usually pass, but not reliably, and it cannot
  be reproduced or iterated without a real Windows host. **Windows transfer is
  therefore still experimental / not-yet-validated.** Proper Windows validation
  on a real machine (two processes, not one-process loopback) is the top
  remaining hardening item.

## Known limitations (today)
- Local network only; no NAT traversal or relay yet.
- Receiver handles one connection at a time.
- Private keys are file-based (`0o600`), not in the OS keystore.
- No bandwidth throttling or send-side include/exclude filters.
- Delta transfer and two-way sync are designed but not implemented.
