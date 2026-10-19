# LaptopLink

Moving a big file between two of your own computers is still annoying in 2026.
AirDrop stops at the Apple fence. `scp` means fiddling with SSH keys. Cloud
drives make you upload a terabyte to someone else's server and pull it back
down again. LaptopLink is an attempt to make the direct path the easy path: pick
a file or a folder, point it at another machine you trust, and it goes straight
there, encrypted the whole way and checked byte for byte when it lands.

It is written from scratch in Rust. It is not a wrapper around `rsync` or `scp`.
It speaks its own authenticated protocol over an encrypted connection, splits
files into verifiable chunks, and can pick up where it left off if the network
drops. It is built to carry a one line note or a multi terabyte disk image
without ever holding the whole thing in memory.

> **Where things stand.** Phases 1 through 3 are built and tested: a real,
> benchmarked command line tool that does secure transfers and rsync style
> folder sync over a local network. The desktop app, internet/NAT traversal,
> relay, and the web app are still ahead. `docs/FEATURES.md` tracks every
> feature line by line and is honest about what is not done. Nothing here is
> called finished unless there are tests behind it.

## What it does today

* Encrypts and authenticates every connection. The handshake is Noise `XX` (via
  the `snow` crate), which proves both machines' identities by their long lived
  keys before any data moves, then protects every byte with ChaCha20-Poly1305.
  No home rolled cryptography anywhere.
* Splits files into chunks and verifies each one with BLAKE3 before it touches
  the disk, then verifies the whole file again before it is committed.
* Resumes. If a transfer dies halfway, run it again and the receiver asks only
  for the chunks it is still missing. Nothing that already arrived is sent twice.
* Keeps memory flat. A one terabyte file uses the same memory as a one megabyte
  file because everything streams.
* Refuses dangerous paths. Directory traversal, absolute paths, and symlink
  tricks are all rejected, and data lands in a temporary file that is renamed
  into place only after it fully verifies.
* Carries whole folders, keeping their structure, and never follows symlinks out
  of the tree you picked.
* Syncs a folder the way rsync does: skip files that are already there, and for
  a large file that changed only a little, send just the pieces that differ.
* Ships a real command line tool with a live progress line, `--json` output for
  scripts, stable exit codes, a built in benchmark, and a `doctor` check.
* Is fast. On a 2019 i9 laptop it sustains around 280 MiB/s (about 2.2 Gbit/s)
  on a single core with encryption and double hashing in the hot path, which
  saturates gigabit ethernet with headroom to spare. These are measured numbers,
  not promises. See `docs/BENCHMARKING.md`.

What is not here yet: transfers across the internet, NAT traversal, relays, the
desktop and web apps, and two way sync. Those are designed and scheduled, not
written. Again, the full status lives in `docs/FEATURES.md`.

## Install

The short command is `laplink`. The longer `laptoplink` is kept as an alias, so
either name works everywhere.

The quickest way, with no Rust toolchain, is the install script. It grabs the
latest prebuilt release for your OS and architecture:

```bash
curl -fsSL https://raw.githubusercontent.com/Eselase-Noble/laptoplink/production/scripts/install.sh | bash
```

On Windows, download the `.zip` from the
[Releases page](https://github.com/Eselase-Noble/laptoplink/releases) and put
`laplink.exe` somewhere on your PATH.

### Or build from source

You need a Rust toolchain, 1.80 or newer.

```bash
git clone https://github.com/Eselase-Noble/laptoplink.git
cd laptoplink
cargo build --release
# binaries land at target/release/laplink and target/release/laptoplink
install -m 0755 target/release/laplink target/release/laptoplink /usr/local/bin/
```

It is built and end-to-end tested on Linux and macOS. It also builds and
passes unit tests on Windows, but Windows end-to-end transfer is still
experimental and unvalidated (see docs/FEATURES.md).

## Send a file to another machine

On the computer that will receive, start a listener. The first time you connect
a new machine, run it in pairing mode so it trusts the sender on first contact:

```bash
laptoplink receive --dest ~/incoming --accept-new
```

It prints the port it is listening on and its own fingerprint.

On the computer that will send, point at the receiver's address (the default
port is 9031):

```bash
laptoplink send ./ubuntu.iso --to 192.168.1.50
```

You get a live progress line with a percentage and a rate, and a short summary
when it finishes. The file shows up under `~/incoming`, verified. A whole folder
works the same way, and you can list several things at once:

```bash
laptoplink send ./Photos ./notes.txt --to 192.168.1.50
```

If you want the sender to choose the destination folder in one command, start
the receiver with `--allow-sender-dest` and pass `--dest` on the send:

```bash
# receiver (opts in to sender-chosen folders)
laptoplink receive --accept-new --allow-sender-dest
# sender (one command: file, address, and where it should land)
laptoplink send ./movie.mkv --to 192.168.1.50 --dest ~/Movies/incoming
```

The receiver still has to be listening (like `scp` needs `sshd`), but after that
the sender drives everything. Letting a sender write to an arbitrary folder is
powerful, so it is off until the receiver opts in.

### Always-on receiving (no terminal to keep open)

If you do not want to leave `laplink receive` running in a terminal, start a
background agent instead. It keeps listening after you close the terminal and
survives until you stop it:

```bash
laplink agent start --accept-new --allow-sender-dest   # start, detached
laplink agent status                                   # running? where?
laplink agent stop                                     # stop it
```

With the agent running, the other machine just sends, one command, any time:

```bash
laplink send ./movie.mkv --to 192.168.1.50 --dest ~/Movies/incoming
```

The agent writes a small log you can tail (its path is shown by `agent status`).

**No internet needed on the same network.** When both machines are on the same
Wi-Fi or LAN, the transfer is direct over the local network and never touches
the internet. No data plan, no cloud. It even works on a router or phone hotspot
that has no internet at all, as long as both machines are joined to it. (Talking
between two *different* networks is the future Phase 5.)

### Try it on one machine

No second computer handy? There is a demo that runs a real two process transfer
over loopback and checks every file with SHA-256:

```bash
./scripts/demo.sh 200     # move a 200 MB file plus small ones, then verify
```

## Pairing and trust

Machines are identified by a cryptographic key, not by an IP address, so nobody
can pretend to be a machine you trust just by taking its address. Each identity
has a 64 character fingerprint, and the first 16 characters are shown for
convenience.

```bash
laptoplink device show      # your identity and fingerprint
laptoplink device list      # machines you have paired with
```

The receiver turns away unknown machines by default. There are two ways to set
up trust. The easy one is trust on first use: start the receiver with
`--accept-new` for the first connection, which is fine on a network you control.
The careful one is to read the 16 character fingerprint to each other over a
channel you already trust, in person or over the phone, and pair it by hand:

```bash
laptoplink device pair --fingerprint <THEIR_FINGERPRINT> --name their-laptop
```

That second method defeats even a machine in the middle on the very first
connection. Changed your mind about a device later?

```bash
laptoplink device revoke --fingerprint <FINGERPRINT>
```

## Pick up an interrupted transfer

Transfers are tracked and recoverable. If one dies partway through:

```bash
laptoplink transfer list                 # find the id
laptoplink transfer status <TRANSFER_ID> # see what it was
laptoplink transfer resume <TRANSFER_ID> # continue; only missing chunks move
```

The receiver keeps a durable record of exactly which chunks it has verified and
written, so a resume never re sends data that already arrived and never trusts
partial data that fails the final whole file check.

## Keep a folder in sync

`sync` makes a device's destination match a source folder, moving only what
changed, much like `rsync`, but over LaptopLink's encrypted and verified
connection.

```bash
# On the receiver, choose where synced files land.
laptoplink receive --dest ~/backup --accept-new

# On the sender, preview first. This shows what would transfer, skip, or delete.
laptoplink sync ./project --to 192.168.1.50 --dry-run --delete

# Then run it for real. With --delete it asks before removing anything unless
# you pass --yes.
laptoplink sync ./project --to 192.168.1.50 --delete --exclude '*.tmp'
```

Files that are already there get skipped, compared by size and modification time
by default or by content hash with `--checksum`. Re running a sync after a small
edit is cheap. When a large file changed only a little, add `--delta` and
LaptopLink sends just the blocks that differ. In testing, a 2 KB edit to a 20 MB
file re synced by moving 4.4 KB, and the result was checked against its whole
file hash before it was committed.

Deletion is never the default, it only happens after every transfer has
succeeded, and anything you exclude is protected from it.

## Command reference

```
laptoplink device show                      Show this device's identity/fingerprint
laptoplink device list                      List paired devices
laptoplink device pair --fingerprint FP     Trust a device by fingerprint
            [--name NAME]
laptoplink device revoke --fingerprint FP   Stop trusting a device

laptoplink send PATH... --to ADDR           Send files or folders to a device
            [--chunk-size BYTES] [--id ID]
            [--dest DIR]                    Ask the receiver to save here
                                            (receiver must allow it; see below)
laptoplink sync SRC_DIR --to ADDR           One way sync a folder's contents
            [--dry-run] [--delete] [--exclude GLOB]...
            [--checksum] [--delta] [--yes]
laptoplink receive [--dest DIR]             Listen for incoming transfers
            [--bind ADDR] [--accept-new] [--once]
            [--allow-sender-dest]           Let the sender choose the folder

laplink agent start [--dest DIR]            Always-on background receiver
            [--bind ADDR] [--accept-new] [--allow-sender-dest]
laplink agent status                        Is the background receiver running?
laplink agent stop                          Stop the background receiver

laptoplink transfer list                    List known transfers
laptoplink transfer status ID               Show one transfer
laptoplink transfer resume ID               Resume an interrupted transfer
laptoplink transfer cancel ID               Mark a transfer cancelled

laptoplink benchmark [--size-mib N]         Measure throughput over loopback
            [--chunk-size BYTES]
laptoplink doctor                           Check local setup, port, identity
```

Global flags that work anywhere: `--json` for machine readable output, `--quiet`,
`--verbose`, and `--home DIR` to use an isolated config and data root, which is
handy for testing or running more than one identity on a machine.

Exit codes are stable for scripting: 0 success, 1 generic, 2 usage, 3
authorization or rejection, 4 integrity failure, 5 transfer not found, 6
resource limit.

## How it works, briefly

```
  sender                                        receiver
  ------                                        --------
  connect, then Noise XX handshake   <------->  authenticate by key fingerprint
  "here are the files I have"        -------->  check what is needed
  send only the needed chunks        -------->  verify each chunk, write to disk
  "that is the whole file, here is   -------->  re verify the whole file, then
   its hash"                                    move it into place atomically
```

One streaming pass over each file both hashes it and sends its chunks, so a large
file is read once, not twice. Backpressure is natural: the receiver only pulls
from the socket as fast as it can hash and write, which throttles the sender
through ordinary TCP flow control. Everything durable, the receiver's chunk
record and the sender's transfer log, is written with a temporary file and an
atomic rename so a crash cannot leave it half written.

The full design is in `docs/ARCHITECTURE.md`, the exact wire format is in
`docs/PROTOCOL.md`, and the security analysis, including what is not yet
protected, is in `docs/THREAT_MODEL.md`.

## Performance

Measured on an Intel Core i9-9880H (2019), release build, 512 MiB over loopback.
Loopback removes the physical link, so this is the ceiling set by the CPU doing
encryption and hashing, not a network figure.

| Chunk size | Throughput  |
|-----------:|------------:|
| 1 MiB      | 278 MiB/s   |
| 4 MiB      | 283 MiB/s   |
| 8 MiB      | 257 MiB/s   |

That is roughly 2.2 Gbit/s on a single core while encrypting and hashing every
byte twice. Reproduce it with `./scripts/benchmark.sh` and read the honest
caveats in `docs/BENCHMARKING.md`. Your numbers will depend on your CPU, disk,
and link, and the docs do not pretend otherwise.

## Layout of the repository

```
crates/laptoplink-core/   The engine: identity, transport, protocol, chunking,
                          manifests, path safety, send, receive, sync, delta.
crates/laptoplink-cli/    The `laptoplink` binary, a thin shell over the core.
docs/                     Architecture, protocol, threat model, benchmarking,
                          troubleshooting, and feature status.
scripts/                  demo.sh (a verified two process transfer), benchmark.sh
.github/workflows/        CI: fmt, clippy, and tests on Linux, macOS, Windows.
```

The engine is a library on purpose. The command line tool today, and the desktop
and web front ends later, all call the same code, so they cannot drift apart on
security or correctness.

## Testing

```bash
cargo test            # unit and end to end integration tests
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

The integration tests stand up a real receiver on a loopback port and run a real
sender against it, with no mocks. They cover round trip integrity, folder trees,
turning away unpaired machines, full and partial resume, path traversal refusal,
sync skip and delete and exclude behavior, and delta reconstruction. Large files
are generated at test time, so no big binaries live in the repository.

## Roadmap

| Phase | What | State |
|------:|------|-------|
| 1 | Architecture and threat model | done |
| 2 | Secure local command line engine | done |
| 3 | Resume, one way sync, rsync style delta transfer | done (core) |
| 4 | Desktop app (Tauri with React and TypeScript) | planned |
| 5 | Internet: signaling, NAT traversal, encrypted relay, QUIC evaluation | planned |
| 6 | Web app (Next.js with TypeScript) with documented browser limits | planned |
| 7 | Packaging, signing, fuzzing, OS keystore, release automation | started |

The per feature breakdown is in `docs/FEATURES.md`.

## Contributing

Issues and pull requests are welcome. Please run `cargo fmt`, `cargo clippy
-- -D warnings`, and `cargo test` before opening a pull request, since CI runs
all three on all three platforms. New transfer behavior should come with a test
that exercises the real send and receive path.

## Security

LaptopLink puts security and data integrity ahead of speed, every time. If you
find a vulnerability, please report it privately to the maintainers rather than
opening a public issue. The current defenses and their limits are written down
plainly in `docs/THREAT_MODEL.md`, including the things that are not yet
protected, such as traffic analysis, storing keys in the OS keystore, and denial
of service from machines that have not been paired.

## License

Dual licensed under either the Apache License, Version 2.0 (`LICENSE-APACHE`) or
the MIT license (`LICENSE-MIT`), at your option.
