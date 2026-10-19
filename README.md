# LaptopLink

Move files between your computers the way it should have always worked: pick a
file or a whole folder, point it at another machine you trust, and watch it go —
encrypted end to end, verified byte for byte, and resumable if the network blinks.

LaptopLink is a from-scratch file-transfer engine written in Rust. It is **not**
a wrapper around `rsync` or `scp`. It speaks its own authenticated, chunked,
resumable protocol over a mutually-authenticated encrypted channel, and it is
built to carry anything from a one-line note to a multi-terabyte disk image
without ever loading the whole thing into memory.

> **Status:** Phases 1 and 2 are done and tested — a working, benchmarked
> command-line tool for secure transfers over a local network. The desktop app
> (Tauri), internet/NAT traversal, relay, and the web app (Next.js) are on the
> roadmap. See [docs/FEATURES.md](docs/FEATURES.md) for an honest line-by-line
> status. Nothing here is described as finished unless it has tests behind it.

---

## Why another transfer tool?

Because the existing options each make you give something up:

- **AirDrop / Nearby Share** are great until you cross an OS boundary.
- **`scp`/`rsync` over SSH** work everywhere but mean key management, shell
  accounts, and no friendly progress or resume for the non-expert.
- **Cloud drives** make you upload a terabyte to someone else's computer and
  download it again — twice the wait, none of the privacy.

LaptopLink aims for the useful middle: direct device-to-device transfer with
real cryptographic identities, integrity you can trust, resume that actually
resumes, and one engine shared by a CLI, a desktop app, and a browser — so the
security model is identical no matter how you drive it.

## What's working today

- 🔒 **Encrypted and authenticated** — every connection is a Noise `XX`
  handshake (`snow` crate) that authenticates both devices by their long-lived
  keys, then ChaCha20-Poly1305 on every byte. No home-grown crypto.
- 🧩 **Chunked with per-chunk + whole-file verification** — BLAKE3 on each chunk
  before it touches disk, and on the whole file before it's committed.
- ⏸️ **Resumable** — interrupt a transfer and run it again; the receiver already
  knows which chunks are verified and asks only for what's missing.
- 💾 **Bounded memory** — streaming throughout; a 1 TB file uses the same memory
  as a 1 MB file.
- 🛡️ **Safe by construction** — path-traversal, absolute-path, and symlink
  attacks are rejected; data lands in a temp file and is atomically renamed only
  after it fully verifies.
- 📁 **Files and folders** — whole directory trees transfer with structure
  intact (symlinks are not followed).
- 📟 **A real CLI** — live progress, `--json` output for scripts, stable exit
  codes, a built-in benchmark, and a `doctor`.
- ⚡ **Fast** — ~280 MiB/s (~2.2 Gbit/s) single-threaded on a 2019 i9 laptop,
  with encryption and double-hashing in the hot path. That saturates 1 GbE with
  room to spare. (Measured, not promised — see
  [docs/BENCHMARKING.md](docs/BENCHMARKING.md).)

What's **not** here yet: internet transfers / NAT traversal, relays, the desktop
and web apps, delta (rsync-style) transfer, two-way sync, and mDNS discovery.
Those are designed and scheduled, not done. Again: [docs/FEATURES.md](docs/FEATURES.md).

## Install / build

You need a recent Rust toolchain (1.80+). Then:

```bash
git clone https://github.com/Eselase-Noble/laptoplink.git
cd laptoplink
cargo build --release
# binary at target/release/laptoplink
```

Optionally put it on your PATH:

```bash
install -m 0755 target/release/laptoplink /usr/local/bin/   # macOS/Linux
```

It builds and is tested on Linux, macOS, and Windows (see the CI workflow).

## Quickstart: send a file from one machine to another

On the **receiving** computer, start a listener. The first time you connect a
new device, run it in pairing mode so it trusts the sender on first contact:

```bash
laptoplink receive --dest ~/incoming --accept-new
```

It prints the port it's listening on and its own fingerprint.

On the **sending** computer, point at the receiver's IP (default port 9031):

```bash
laptoplink send ./ubuntu.iso --to 192.168.1.50
```

You'll see a live progress line with percentage and MiB/s, and a summary when
it's done. The file appears under `~/incoming` on the other side, verified.

Want a whole folder? Just pass it:

```bash
laptoplink send ./Photos ./notes.txt --to 192.168.1.50
```

### Try it all on one machine

No second computer handy? The repo ships a demo that runs a real two-process
transfer over loopback and checks every file with SHA-256:

```bash
./scripts/demo.sh 200     # transfer a 200 MB file + small files, verify
```

## Pairing and trust (read this once)

Devices are identified by a **cryptographic key**, not an IP address — so nobody
can impersonate a machine you trust just by taking its address. Each device's
identity has a 64-character fingerprint; the first 16 characters are shown for
convenience.

```bash
laptoplink device show      # your identity + fingerprint
laptoplink device list      # devices you've paired with
```

The receiver refuses unknown devices by default. You have two ways to establish
trust:

1. **Trust on first use** — start the receiver with `--accept-new` for the
   initial connection. Convenient on a network you control.
2. **Verify out of band** — read the 16-character fingerprint aloud or over a
   trusted channel and pre-pair it:
   ```bash
   laptoplink device pair --fingerprint <THEIR_FINGERPRINT> --name their-laptop
   ```
   This is the strongest option and defeats even a first-contact MITM.

Changed your mind about a device?

```bash
laptoplink device revoke --fingerprint <FINGERPRINT>
```

## Resume an interrupted transfer

Transfers are tracked and recoverable. If one dies halfway:

```bash
laptoplink transfer list                 # find the id
laptoplink transfer status <TRANSFER_ID> # see what it was
laptoplink transfer resume <TRANSFER_ID> # continue — only missing chunks move
```

The receiver keeps a durable manifest of exactly which chunks it has verified
and written, so resume never re-sends data that already arrived, and it never
trusts partial data that fails the final whole-file check.

## Command reference

```
laptoplink device show                      Show this device's identity/fingerprint
laptoplink device list                      List paired devices
laptoplink device pair --fingerprint FP     Trust a device by fingerprint
            [--name NAME]
laptoplink device revoke --fingerprint FP   Stop trusting a device

laptoplink send PATH... --to ADDR           Send files/folders to a device
            [--chunk-size BYTES] [--id ID]
laptoplink receive [--dest DIR]             Listen for incoming transfers
            [--bind ADDR] [--accept-new] [--once]

laptoplink transfer list                    List known transfers
laptoplink transfer status ID               Show one transfer
laptoplink transfer resume ID               Resume an interrupted transfer
laptoplink transfer cancel ID               Mark a transfer cancelled

laptoplink benchmark [--size-mib N]         Measure throughput (loopback)
            [--chunk-size BYTES]
laptoplink doctor                           Check local setup / port / identity
```

Global flags: `--json` (machine-readable output), `--quiet`, `--verbose`,
`--home DIR` (use an isolated config/data root — handy for testing or running
multiple identities).

**Exit codes:** `0` success · `1` generic · `2` usage · `3` auth/rejected ·
`4` integrity failure · `5` transfer not found · `6` resource limit. Good for
scripting and CI.

## How it works (the short version)

```
  sender                                        receiver
  ──────                                        ────────
  TCP connect → Noise XX handshake  ←───────→   authenticate by key fingerprint
  "here are the files I have"       ─────────→  check manifest, decide what's needed
  send only the needed chunks       ─────────→  verify each chunk (BLAKE3), write
  "that's the whole file, here's its hash" ──→  re-verify whole file, then
                                                atomically move it into place
```

- One streaming pass per file hashes the whole file and emits chunks — no double
  read, bounded memory.
- Backpressure is natural: the receiver only pulls from the socket as fast as it
  can hash and write, which throttles the sender through TCP.
- Everything durable (the receiver's chunk bitmap, the sender's transfer record)
  is written with a temp-file-plus-atomic-rename so a crash can't corrupt it.

The full design is in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md); the exact
wire format is in [docs/PROTOCOL.md](docs/PROTOCOL.md); the security analysis is
in [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md).

## Performance

Measured on an Intel Core i9-9880H (2019), release build, 512 MiB over loopback
(so this is the CPU/crypto ceiling, not a network number):

| Chunk size | Throughput  |
|-----------:|------------:|
| 1 MiB      | 278 MiB/s   |
| 4 MiB      | 283 MiB/s   |
| 8 MiB      | 257 MiB/s   |

That's ~2.2 Gbit/s on a single core while encrypting and hashing every byte
twice. Reproduce it with `./scripts/benchmark.sh` and read the honest caveats in
[docs/BENCHMARKING.md](docs/BENCHMARKING.md). Your mileage will vary with CPU,
disk, and link — and we won't pretend otherwise.

## Repository layout

```
crates/laptoplink-core/   The engine: identity, transport, protocol, chunking,
                          manifests, path safety, send/receive. All the logic.
crates/laptoplink-cli/    The `laptoplink` binary — a thin wrapper over the core.
docs/                     Architecture, protocol, threat model, benchmarking,
                          troubleshooting, feature status.
scripts/                  demo.sh (verified two-process transfer), benchmark.sh
.github/workflows/        CI: fmt + clippy + tests on Linux, macOS, Windows.
```

The engine is a library on purpose: the CLI today, and the Tauri desktop app and
Next.js web backend later, all call the **same** code so they can't drift apart
on security or correctness.

## Testing

```bash
cargo test            # unit + end-to-end integration tests
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

The integration tests stand up a real receiver on a loopback port and run a real
sender against it — no mocks — covering round-trip integrity, directory trees,
rejection of unpaired peers, full-file resume, partial-chunk resume, and
path-traversal refusal. Large files are generated at test time; no big binaries
are committed to the repo.

## Roadmap

| Phase | What | State |
|------:|------|-------|
| 1 | Architecture & threat model | ✅ done |
| 2 | Secure local CLI engine (this) | ✅ done |
| 3 | Resume polish, one-way sync, rsync-style delta transfer | 🟡 partial |
| 4 | Desktop app (Tauri + React/TypeScript) | ⬜ planned |
| 5 | Internet: signaling, NAT traversal, encrypted relay, QUIC eval | ⬜ planned |
| 6 | Web app (Next.js + TypeScript) with documented browser limits | ⬜ planned |
| 7 | Packaging, signing, fuzzing, OS keystore, release automation | 🟡 started |

Details and the per-feature breakdown: [docs/FEATURES.md](docs/FEATURES.md).

## Contributing

Issues and pull requests are welcome. Please run `cargo fmt`, `cargo clippy
-- -D warnings`, and `cargo test` before opening a PR — CI runs all three on all
three platforms. New transfer behavior should come with a test that exercises
the real send/receive path.

## Security

LaptopLink puts security and data integrity ahead of speed, always. If you find
a vulnerability, please report it privately to the maintainers rather than
opening a public issue. The current defenses and their limits are documented
candidly in [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md) — including what is
**not** yet protected (traffic analysis, OS-keystore key storage, DoS from
unauthenticated peers).

## License

Dual-licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
