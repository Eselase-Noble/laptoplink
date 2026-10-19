# LaptopLink Wire Protocol (v1)

This document specifies the on-the-wire protocol as implemented in
`laptoplink-core`. It is versioned by `PROTOCOL_VERSION` (currently `1`); any
incompatible change bumps it, and peers refuse mismatched versions during
`Hello`.

## 1. Transport framing

All application bytes travel inside a Noise-encrypted `SecureStream`:

1. A TCP connection is established (initiator = sender, responder = receiver).
2. A **Noise `XX` handshake** runs. During the handshake, three messages are
   exchanged, each framed as a big-endian `u16` length prefix followed by that
   many bytes of Noise handshake data (these handshake frames are not yet
   encrypted at the application layer — that is the handshake's job).
3. After the handshake, the connection is in transport mode. Every subsequent
   logical write is chopped into ≤65519-byte plaintext segments; each segment is
   sealed into one Noise packet and written as:

   ```
   ┌────────────┬─────────────────────────────┐
   │ u16 BE len │ Noise ciphertext (len bytes) │   len ≤ 65535
   └────────────┴─────────────────────────────┘
   ```

Everything below is described in terms of the **plaintext** byte stream carried
over that encrypted channel.

## 2. Message framing (control plane)

Control messages are JSON, length-delimited with a big-endian `u32`:

```
┌────────────┬──────────────────────────┐
│ u32 BE len │ UTF-8 JSON (len bytes)    │
└────────────┴──────────────────────────┘
```

The receiver enforces a configurable maximum (`limits.max_message_bytes`,
default 16 MiB) and rejects any frame claiming to be larger.

Each message is a JSON object with a `"type"` tag naming the variant.

## 3. Bulk data (data plane)

A chunk payload is **not** JSON. The sender writes a `ChunkHeader` control
message, then immediately writes exactly `len` raw bytes of chunk data on the
same stream (still Noise-encrypted). The receiver reads the header, then reads
exactly `len` bytes.

## 4. Message catalogue

| `type`          | Direction | Fields |
|-----------------|-----------|--------|
| `Hello`         | S → R     | `protocol_version:u32`, `device_name:string`, `fingerprint:hex` |
| `HelloAck`      | R → S     | `accepted:bool`, `reason:string`, `device_name:string` |
| `Offer`         | S → R     | `transfer_id:string`, `files:[FileOffer]` |
| `OfferResponse` | R → S     | `files:[FilePlan]` |
| `ChunkHeader`   | S → R     | `file_index:u32`, `chunk_index:u64`, `offset:u64`, `len:u32`, `hash:hex` (+ raw payload follows) |
| `FileDone`      | S → R     | `file_index:u32`, `file_hash:hex` |
| `FileResult`    | R → S     | `file_index:u32`, `ok:bool`, `error:string` |
| `Complete`      | S → R     | — |
| `Error`         | either    | `message:string` |

### FileOffer
```json
{ "rel_path": "photos/a.jpg", "size": 12345678, "chunk_size": 4194304,
  "mode": 420, "mtime": 1760000000 }
```
* `rel_path` — forward-slash relative path; validated by the receiver.
* `size` — 64-bit byte length.
* `chunk_size` — sender's chosen chunk size for this file.
* `mode` — optional Unix mode bits (receiver may ignore).
* `mtime` — optional source mtime (Unix seconds). With `size` it forms the
  resume-validity signature.

### FilePlan
```json
{ "file_index": 0, "needed": {"kind": "Indices", "value": [1,2,3]},
  "resume": true }
```
`needed.kind` is one of:
* `"All"` — send every chunk (fresh transfer).
* `"None"` — file already present and verified; send nothing.
* `"Indices"` with `value:[u64]` — send exactly these chunk indices (resume).

## 5. Exchange sequence

```
S: Hello
R: HelloAck{accepted:true}            # or {accepted:false,...} then close
S: Offer{transfer_id, files}
R: OfferResponse{plans}               # per-file needed-chunk sets
loop over files:
   loop over needed chunks:
      S: ChunkHeader{...}
      S: <raw len bytes>
   S: FileDone{file_index, file_hash}
   R: FileResult{ok|error}
S: Complete
```

The sender streams a file in a single sequential pass: it folds every chunk into
the whole-file BLAKE3 hash (so resume still verifies correctly) and transmits
only the chunks named in the plan.

## 6. Integrity rules (normative)

1. The receiver MUST verify each chunk's BLAKE3 hash **before** writing it to the
   `.part` file. A mismatch aborts the connection with an integrity error.
2. A chunk MUST NOT be marked complete in the manifest until its bytes are
   written.
3. On `FileDone`, the receiver MUST recompute the whole-file BLAKE3 hash over the
   reconstructed `.part` file and compare it to `file_hash`. Only on a match may
   it fsync and atomically rename into place.
4. On whole-file mismatch, the receiver MUST discard the partial file and report
   `FileResult{ok:false}`; it MUST NOT commit.

## 7. Resume rules (normative)

1. The receiver keeps a durable manifest with a per-file bitmap of
   verified+written chunks.
2. On a new `Offer` with a known `transfer_id`, for each file the receiver
   compares `(size, chunk_size, mtime)` against the stored signature. If they
   match and the file is not yet committed, it reuses verified chunks and
   requests only the missing ones. If the signature differs, it discards partial
   data and requests all chunks.
3. If a file is already committed and present, it requests `None`.

## 8. Versioning and forward compatibility

* JSON was chosen for v1 for clarity and debuggability. A compact binary codec
  can replace it under a new `PROTOCOL_VERSION` without changing the semantics
  above.
* Unknown message types are a protocol error (connection torn down). New
  optional fields on existing messages should default-deserialize so minor
  additions need not bump the version.
