# LaptopLink Threat Model

This is a working threat model for the Phase 2 (local-network CLI) engine, with
notes on how it extends to the internet and relay phases. It states what we
defend against, what we explicitly do **not** yet defend against, and why.

Security and data integrity come before performance everywhere in LaptopLink.
We never weaken authentication or integrity to go faster.

## 1. Assets

* **File contents** in transit and the `.part` data at rest on the receiver.
* **Device private keys** (`identity.json`) — the long-lived X25519 static keys.
* **The paired-peer list** (`peers.json`) — who a device trusts.
* **Transfer metadata** — file names, sizes, offsets, hashes.
* **Destination filesystem integrity** — nothing should be written outside the
  chosen destination directory.

## 2. Trust model and identity

* Each device has a long-lived **X25519 static keypair**. The public key *is*
  the identity; its **fingerprint** is `hex(BLAKE3(public_key))`.
* Trust is **pinned to the fingerprint, never to an IP address or hostname.**
  An attacker who controls DNS, ARP, or routing cannot impersonate a paired
  device without its private key.
* **Pairing is explicit.** A receiver rejects any peer whose fingerprint is not
  in its peer store, unless it is deliberately started in trust-on-first-use
  mode (`receive --accept-new`) for initial pairing. Operators can also pre-pair
  a fingerprint out-of-band with `device pair`.
* **Revocation** marks a fingerprint denied; future connections from it fail
  authorization.

## 3. Adversaries considered

### A. Passive network eavesdropper (LAN sniffer, Wi-Fi monitor)
*Goal:* read file contents or metadata.
*Defense:* the Noise `XX` handshake establishes forward-secret session keys; all
bytes after the handshake — control messages, file names, and payloads — are
encrypted and authenticated with ChaCha20-Poly1305. The eavesdropper sees only
ciphertext sizes and timing. **Covered.**

### B. Active man-in-the-middle
*Goal:* impersonate one party, tamper with or inject data.
*Defense:* Noise `XX` mutually authenticates both static keys. A MITM without
the target's private key cannot complete the handshake; the receiver checks the
authenticated remote key against its peer store, and the sender is told the
receiver's fingerprint. Any ciphertext tampering fails the AEAD tag and tears
down the connection. **Covered**, with the usual Noise caveat that the *first*
pairing (TOFU) trusts the key it sees — verify the short fingerprint out-of-band
for the strongest guarantee (see §7).

### C. Malicious or buggy *sender* (authenticated peer sending hostile data)
*Goal:* escape the destination directory, exhaust resources, or corrupt files.
Defenses:
* **Path traversal / absolute paths / Windows prefixes** — every relative path
  is validated and re-checked to remain under the destination root
  (`pathsafety::safe_join`). `..`, root, and drive/UNC prefixes are rejected.
* **Symlink redirection** — the receiver refuses to overwrite an existing
  symlink at the destination, and never follows untrusted links when writing.
* **Resource exhaustion** — configurable limits cap file count, total chunk
  count, per-message size, and per-chunk size; the receiver buffers at most one
  chunk at a time. A peer cannot force unbounded memory with a huge declared
  message or chunk.
* **Integrity** — every chunk is BLAKE3-verified before it touches disk, and the
  whole file is re-verified before commit. A corrupted or substituted chunk is
  rejected; a wrong reconstruction is discarded, not committed.
* **Atomicity** — data lands in a `.part` file and is atomically renamed only
  after full verification, so an aborted/hostile transfer never yields a
  partially-written file in place of a real one.

### D. Malicious *receiver*
*Goal:* trick a sender into revealing files it should not, or alter data.
*Defense:* the sender only sends what the operator selected; the receiver cannot
request arbitrary paths from the sender (the protocol has no "pull by path"
primitive). The receiver *can* decline chunks (resume), but cannot cause the
sender to read outside the chosen sources. Content the sender sends is its own
choice. **Covered** for the stated goal.

### E. Local attacker on the same machine
*Goal:* steal the private key or read `.part` data.
*Defense:* `identity.json` and `peers.json` are created with `0o600` on Unix.
**Partial:** we do not yet use the OS keystore (Keychain / DPAPI / Secret
Service); a local user with equivalent privileges can read the key file. Tracked
as remaining work. `.part` files inherit `0o600` too.

### F. Chunk replay / cross-transfer mixing
*Goal:* replay a captured chunk or splice chunks between transfers.
*Defense:* the Noise transport uses per-session keys and monotonically
increasing nonces, so ciphertext from one session cannot be replayed into
another, and in-session replay/reorder breaks the AEAD. Within a transfer, a
chunk is bound to a `(file_index, offset, len)` header and checked against the
expected content hash, and the whole-file hash catches any mis-assembly.
**Covered** for the transport-level attacks. (A stronger binding of the header
into the AEAD associated-data is a hardening item.)

## 4. Explicitly NOT yet in scope (and honestly so)

* **OS keystore integration** — keys are file-based with restrictive perms today.
* **Traffic analysis / size & timing hiding** — ciphertext lengths and timing
  leak approximate file sizes; we do not pad.
* **Denial of service from unauthenticated peers** — anyone who can reach the
  port can start a handshake; we reject unpaired peers immediately, but a flood
  of handshakes is not rate-limited yet.
* **Internet transport, NAT traversal, and relays** — Phase 5. The relay will be
  designed so it **cannot decrypt** end-to-end content (it only forwards
  ciphertext between already-authenticated peers).
* **Supply-chain / dependency compromise** — mitigated operationally by
  committing `Cargo.lock` and running CI, not by anything in the protocol.
* **Two-way sync conflict/deletion semantics** — one-way transfer only today.

## 5. Cryptographic choices

* **No custom primitives.** All crypto is from `snow` (Noise) and `blake3`.
* **Handshake/transport:** `Noise_XX_25519_ChaChaPoly_BLAKE2s`.
* **Content hashing:** BLAKE3 (per-chunk and whole-file).
* **Randomness:** OS CSPRNG (`OsRng`) for key generation and transfer ids.

## 6. Data-at-rest

* Private key and peer store: `0o600`, owner-only (Unix). OS keystore planned.
* In-progress data: `.part` files under `<dest>/.laptoplink/tmp/<id>/`, `0o600`,
  removed or renamed on completion. Abandoned temp data is overwritten/validated
  on resume; a documented retention/cleanup policy is a Phase 3 item.

## 7. Operational guidance

* For the strongest guarantee, **compare the 16-character short fingerprint**
  shown by `device show` on both machines (over a trusted channel — in person,
  phone) before the first transfer, instead of relying on TOFU.
* Run `receive` **without** `--accept-new` in normal operation; only enable it
  briefly to pair a new device.
* Keep `identity.json` out of backups that are themselves not encrypted. It is
  git-ignored by default.

## 8. Reporting

Security issues should be reported privately to the maintainers (see the
repository's security policy once published) rather than via public issues.
