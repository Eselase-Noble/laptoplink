# Troubleshooting & Network Requirements

## Network & firewall requirements (Phase 2, local network)

* **Transport:** a single TCP connection from sender → receiver.
* **Default port:** `9031` (override with `receive --bind` and `send --to host:port`).
* **Direction:** the *receiver* listens; the *sender* connects. The receiver's
  host/firewall must allow inbound TCP on the chosen port from the sender's IP.
* **No multicast required** for a transfer itself. (mDNS auto-discovery, when
  added, will use UDP 5353 and may be blocked on some networks — manual
  addressing always works.)
* **No internet or server** is used for a direct LAN transfer; bytes go straight
  between the two devices.

### Opening the port

* **macOS:** System Settings → Network → Firewall. Either allow the
  `laptoplink` binary or temporarily disable the firewall on a trusted LAN.
* **Linux (ufw):** `sudo ufw allow 9031/tcp`
* **Linux (firewalld):** `sudo firewall-cmd --add-port=9031/tcp`
* **Windows:** `New-NetFirewallRule -DisplayName "LaptopLink" -Direction Inbound
  -Protocol TCP -LocalPort 9031 -Action Allow` (elevated PowerShell).

## Quick diagnosis

Run the built-in checks first:
```bash
laptoplink doctor
```
It reports your identity/fingerprint, paired-peer count, and whether the default
port is bindable.

## Common problems

### "device not paired" / send exits with code 3
The receiver rejected your device because its fingerprint isn't trusted. Either:
* start the receiver in pairing mode once: `laptoplink receive --accept-new`, or
* pre-pair on the receiver: `laptoplink device pair --fingerprint <SENDER_FP>
  --name my-laptop` (get `<SENDER_FP>` from `laptoplink device show` on the
  sender).

### "Connection refused"
The receiver isn't listening, or a firewall is blocking the port.
* Confirm the receiver is running `laptoplink receive` and note its bind address.
* Check the port with `laptoplink doctor` on the receiver.
* Verify both machines are on the same network and can ping each other.

### "connection timed out"
Usually a firewall silently dropping packets. Open the port (above), or test
with the receiver bound to `127.0.0.1` and a local sender first to isolate.

### Transfer stops partway, then I want to continue
Nothing is lost. Re-run the same `send` with `--id <TRANSFER_ID>` (see
`laptoplink transfer list`), or `laptoplink transfer resume <TRANSFER_ID>`. The
receiver already recorded which chunks are verified and will ask only for the
missing ones. Verified data is never re-sent.

### "integrity check failed" / exit code 4
A chunk or the whole-file hash didn't match. This is the system doing its job —
it refuses to commit corrupt data. Causes:
* the **source file changed** during the transfer (edit it after it finishes), or
* genuine corruption on the wire or disk.
Re-run the transfer; resume will re-fetch the bad chunks.

### "source file changed during transfer"
The file's size or mtime changed between the offer and sending. Let the file
settle (don't modify it mid-transfer) and retry.

### "destination '…' is a symlink; refusing to overwrite"
The receiver found a symlink where it was about to write a file and refused, to
avoid being redirected outside the destination. Remove/relocate the symlink or
choose a different `--dest`.

### "resource limit exceeded" / exit code 6
A peer offered more files/chunks or a larger message/chunk than the configured
limits allow. This protects you from a hostile or buggy peer. If it's a
legitimate huge transfer, the limits in `config.rs` are the knobs (raising them
is a code change today; a config file is planned).

### Slow throughput
* Build in **release** (`cargo build --release`); debug builds run crypto and
  hashing ~50× slower.
* On Wi-Fi, expect far less than wired; try Ethernet.
* Try `--chunk-size 4194304` (4 MiB), the current sweet spot (see
  [BENCHMARKING.md](BENCHMARKING.md)).
* The engine is single-threaded today; a single stream tops out around the
  CPU's AEAD+hash rate (~280 MiB/s on an i9-9880H). Multi-stream is roadmap.

## Where things live

* Identity & peers: your OS config dir (shown by `laptoplink device show`), or
  `--home <dir>/config` when you pass `--home`.
* Received files: `--dest` (default: your OS data dir `.../received`).
* In-progress data & receiver manifest: `<dest>/.laptoplink/`.
* Sender transfer records: `<config>/transfers/`.

Deleting `<dest>/.laptoplink/` discards resume state for that destination (a
future transfer will start fresh). Deleting `identity.json` makes the device
generate a **new** identity — peers will see it as a different, unpaired device.
