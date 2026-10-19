# LaptopLink Architecture

This document describes how LaptopLink is put together: the crates, the layers
inside the engine, the data model, and the decisions behind them. It reflects
what is **actually implemented today** (Phase 1 + Phase 2); planned work is
called out explicitly and tracked in [FEATURES.md](FEATURES.md).

## 1. Goals and non-goals

LaptopLink moves files and directory trees between computers — on a LAN today,
across the internet later — with these priorities, in order:

1. Security and data integrity
2. Correctness and recoverability
3. Throughput and efficient resource use
4. Cross-platform compatibility
5. Usability (CLI, desktop, web)
6. Maintainability

It is **not** a cloud storage product. Data goes directly between devices; there
is no requirement that bytes pass through a server, and the coordination server
(Phase 5) never needs the plaintext.

## 2. Repository layout

```
laptoplink/
├── Cargo.toml                 # workspace root
├── crates/
│   ├── laptoplink-core/       # the reusable engine (this is where the work is)
│   │   ├── src/
│   │   │   ├── lib.rs         # Engine handle + module wiring
│   │   │   ├── error.rs       # the single Error/Result type
│   │   │   ├── identity.rs    # device keypair + fingerprint
│   │   │   ├── config.rs      # on-disk layout, limits, paired-peer store
│   │   │   ├── fsutil.rs      # durable writes, atomic rename, perms
│   │   │   ├── transport.rs   # Noise handshake + SecureStream (AEAD byte stream)
│   │   │   ├── protocol.rs    # wire messages + framing
│   │   │   ├── chunk.rs       # chunk sizing + BLAKE3 hashing
│   │   │   ├── pathsafety.rs  # untrusted-path validation
│   │   │   ├── manifest.rs    # durable transfer state + chunk bitmap
│   │   │   ├── progress.rs    # ProgressSink trait + events
│   │   │   ├── send.rs        # sender state machine
│   │   │   └── recv.rs        # receiver state machine
│   │   └── tests/transfer.rs  # end-to-end integration tests
│   └── laptoplink-cli/        # the `laptoplink` binary (thin wrapper)
│       └── src/{main.rs, progress.rs}
├── docs/                      # this directory
├── scripts/                   # demo.sh, benchmark.sh
└── .github/workflows/ci.yml   # fmt + clippy + test on Linux/macOS/Windows
```

The guiding rule: **all transfer logic lives in `laptoplink-core`.** The CLI,
and later the Tauri desktop app and the Next.js web backend, are thin shells
that call the same engine so they share one security model and one wire format.

## 3. Layered design of the engine

From the socket up:

```
            ┌─────────────────────────────────────────────┐
  send.rs / │  Transfer state machines                     │
  recv.rs   │  offer → plan → stream chunks → verify → commit│
            ├─────────────────────────────────────────────┤
 protocol.rs│  Control messages (JSON, length-delimited)   │
            │  + raw chunk payloads                         │
            ├─────────────────────────────────────────────┤
transport.rs│  SecureStream: AEAD byte stream (Noise)       │
            │  Read/Write, one Noise packet buffered/dir    │
            ├─────────────────────────────────────────────┤
            │  std::net::TcpStream (blocking) + BufReader/  │
            │  BufWriter                                    │
            └─────────────────────────────────────────────┘
```

Cross-cutting modules — `identity`, `config`, `chunk`, `manifest`,
`pathsafety`, `fsutil`, `progress`, `error` — are used by both state machines.

### Transport (`transport.rs`)

* A **Noise `XX` handshake** (`Noise_XX_25519_ChaChaPoly_BLAKE2s`, via the
  `snow` crate) authenticates both peers by their static X25519 keys and
  establishes AEAD keys. We do **not** implement any cryptographic primitive
  ourselves.
* After the handshake, `SecureStream` implements `std::io::{Read, Write}`. Each
  write is split into ≤65519-byte segments, each encrypted into one Noise packet
  prefixed with a big-endian `u16` ciphertext length. Reads reverse this. At
  most one packet is buffered per direction, so memory is bounded regardless of
  payload size.
* The socket is wrapped in 512 KiB `BufReader`/`BufWriter` halves (via
  `try_clone`) so many small Noise packets coalesce into few syscalls — the
  dominant cost on fast links.

### Protocol (`protocol.rs`)

A small, versioned `Control` enum serialized as length-prefixed JSON. Bulk chunk
data is sent as raw bytes immediately after a `ChunkHeader` message on the same
encrypted stream. See [PROTOCOL.md](PROTOCOL.md) for the message catalogue and
the exact exchange.

### Chunking and hashing (`chunk.rs`)

Files are split into fixed-size chunks (last one short). Chunk size is adaptive
by file size (currently 256 KiB–8 MiB; see `choose_chunk_size`) and overridable.
Every chunk gets a BLAKE3 hash; the whole file gets a BLAKE3 hash too. BLAKE3 is
cryptographic and fast, and it is computed in a single streaming pass per file
on the sender.

### Durable state (`manifest.rs`)

* **ReceiverManifest** (`<dest>/.laptoplink/<id>.json`) records, per file, a
  packed bitmap of verified+written chunks. This is the resume source of truth.
* **SenderRecord** (`<config>/transfers/<id>.json`) backs `transfer
  list/status/resume/cancel`.
* Both are written with a temp-file-plus-atomic-rename dance and fsync, so a
  crash mid-write never corrupts them.

### Filesystem safety (`pathsafety.rs`, `fsutil.rs`)

Every relative path from a peer is validated (no `..`, no absolute paths, no
Windows prefixes) and re-checked to stay under the destination root. Received
data is written to a `.part` file, fsync'd, whole-file-verified, and only then
atomically renamed into place. Existing symlinks at the destination are refused.

## 4. The send/receive state machine

```
 SENDER (initiator)                         RECEIVER (responder)
 ──────────────────                         ────────────────────
 TCP connect ───────────────────────────▶   accept
 Noise XX handshake  ◀──────────────────▶   Noise XX handshake
 (learns receiver fingerprint)              (learns sender fingerprint)
 Hello ─────────────────────────────────▶   authorize by fingerprint
                                            (reject if unpaired & !accept_new)
        ◀──────────────────────────────── HelloAck{accepted}
 Offer{files, sizes, chunk sizes} ──────▶   load/build manifest, plan per file
        ◀──────────────────────────────── OfferResponse{needed chunks per file}
 for each file:
   (one streaming pass; whole-file hash)
   ChunkHeader + raw bytes ─────────────▶   verify chunk hash, write @offset,
   ...                                       mark bitmap, periodic fsync+save
   FileDone{file_hash} ─────────────────▶   verify whole-file hash,
                                            fsync, atomic rename, commit
        ◀──────────────────────────────── FileResult{ok}
 Complete ──────────────────────────────▶   finish
```

Backpressure is implicit: the receiver reads from the socket only as fast as it
can hash and write to disk, which throttles the sender through TCP flow control.
Exchanges are strictly request/response at file boundaries, so there is no
head-of-line deadlock.

## 5. Concurrency and I/O model

Phase 2 uses **blocking I/O with one thread per connection** (the receiver
handles connections sequentially today). The spec explicitly permits optimized
synchronous I/O, and this choice buys simplicity and correctness for the first
measurable milestone. Because all socket work is behind `SecureStream`, moving
to async (`tokio`/`quinn`) or adding parallel per-chunk pipelines is a localized
change. Parallel hashing, multi-connection transfers, and adaptive concurrency
are Phase 3/5 items in [FEATURES.md](FEATURES.md).

## 6. Where each interface plugs in

* **CLI** (`laptoplink-cli`) — implemented. Calls `send`/`recv`/`manifest`
  directly; provides human and `--json` output and stable exit codes.
* **Desktop** (Tauri + React/TS) — Phase 4. Tauri commands will wrap the same
  `Engine` API; no transfer logic in the UI.
* **Web** (Next.js + TS) — Phase 6. A backend service handles signaling,
  device registration, and authorization; browser transfers use streaming APIs
  with documented limits and fall back to the CLI/desktop for the largest jobs.

## 7. Key dependencies and why

| Crate         | Role                        | Why this one |
|---------------|-----------------------------|--------------|
| `snow`        | Noise handshake + AEAD      | Mature, audited-pattern implementation; no DIY crypto |
| `blake3`      | chunk + file hashing        | Cryptographic and very fast |
| `serde`/`serde_json` | wire + manifest encoding | Ubiquitous, debuggable; swappable for a binary codec |
| `walkdir`     | directory expansion         | Correct, non-symlink-following traversal |
| `directories` | per-OS config/data paths    | Correct Windows/macOS/Linux locations |
| `clap`        | CLI parsing                 | Standard, derive-based |

See [THREAT_MODEL.md](THREAT_MODEL.md) for the security analysis and
[BENCHMARKING.md](BENCHMARKING.md) for measured performance.
