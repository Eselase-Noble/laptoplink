# LaptopLink Benchmarking

This document explains how to measure LaptopLink and records real results from
the development machine. **We do not fabricate numbers or promise fixed
throughput.** Reproduce them yourself with the scripts below; your hardware,
disk, and network will differ.

## 1. What the built-in benchmark measures

`laptoplink benchmark` runs the **real** transfer path end-to-end over a
loopback TCP socket in a single process:

* generates a synthetic file of the requested size,
* stands up a one-shot receiver on `127.0.0.1`,
* sends the file over the actual Noise-encrypted, per-chunk-verified,
  whole-file-verified pipeline,
* reports bytes, elapsed seconds, and MiB/s.

Because it is loopback, it removes the physical link and therefore measures the
**CPU/crypto/hashing ceiling** of one sender+receiver on one machine — not your
network. For link throughput, run a real two-machine transfer (§4).

```bash
laptoplink benchmark --size-mib 512
laptoplink --json benchmark --size-mib 1024 --chunk-size 4194304
```

## 2. Recorded results (development machine)

Hardware: **Intel Core i9-9880H @ 2.30 GHz**, macOS (Darwin 25.5.0, x86_64),
release build (`lto=thin`, `codegen-units=1`). Loopback, warm cache. 512 MiB
synthetic file. Single-threaded engine.

| Chunk size | Throughput  | Time (512 MiB) |
|-----------:|------------:|---------------:|
| 1 MiB      | 278.0 MiB/s | 1.84 s         |
| 4 MiB      | 283.3 MiB/s | 1.81 s         |
| 8 MiB      | 257.0 MiB/s | 1.99 s         |

Observations:
* ~280 MiB/s ≈ **2.2 Gbit/s** single-threaded, with ChaCha20-Poly1305 + BLAKE3
  (chunk **and** whole-file) in the hot path. This comfortably saturates a
  1 GbE link (~118 MiB/s) and a good chunk of 2.5 GbE.
* 4 MiB is the current sweet spot on this CPU; 8 MiB regresses slightly. This is
  why `choose_chunk_size` defaults mid-range and is overridable.
* The ceiling here is a single core doing AEAD + double hashing. Parallel
  hashing and multi-stream transfers (Phase 3/5) are the path to going faster on
  10 GbE-class links.

Earlier measurement showed socket buffering alone lifting loopback from
~234 MiB/s to ~270 MiB/s by coalescing Noise packets into fewer syscalls.

## 3. Reproduce the sweep

```bash
./scripts/benchmark.sh > results.jsonl
cat results.jsonl
```

Each line is a JSON object: `{"bytes":...,"seconds":...,"mib_per_s":...,"chunk_size":...}`.

## 4. Real two-machine measurement

On the receiver:
```bash
laptoplink receive --dest ~/incoming --accept-new
```
On the sender (replace with the receiver's LAN IP):
```bash
time laptoplink send ./big.iso --to 192.168.1.50
```
`laptoplink send` prints a live rate and a final summary; `time` gives
wall-clock. Use `--json` to capture machine-readable progress for scripting.

## 5. Methodology notes and honesty rules

Record, with every result: hardware (CPU, RAM), storage type, OS, build profile,
network topology and link speed, file sizes, warm/cold cache state, chunk size,
and concurrency. Keep security/verification settings identical across any tool
comparison — comparing a verified transfer against an unverified one is
meaningless.

When comparing against `rsync`, `scp`, `bbcp`, or raw `iperf3`, state exactly
which flags were used and whether encryption/verification were on for both
sides. We will publish comparisons only with those conditions documented.

## 6. Benchmark scenarios to cover (roadmap)

The spec calls for these; the ones not yet automated are tracked in
[FEATURES.md](FEATURES.md):

- [x] One multi-gigabyte file (loopback; real-link via §4)
- [x] A large directory tree (see `scripts/demo.sh`)
- [x] Chunk-size sweep
- [ ] Many small files (metadata-bound) — harness pending
- [ ] Partially-modified large file (delta) — Phase 3
- [ ] Already-synchronized directory (no-op cost) — partial (resume skip works)
- [ ] Interrupted-then-resumed transfer timing — manual today
- [ ] Wi-Fi vs Ethernet vs internet vs relay — Phase 5
- [ ] CPU %, peak RSS, disk throughput capture — harness pending
