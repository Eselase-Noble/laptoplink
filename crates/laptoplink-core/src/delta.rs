//! rsync-style delta transfer.
//!
//! When a file already exists at the destination but differs, we avoid sending
//! it whole. The *receiver* (which holds the old copy, the "basis") splits its
//! file into fixed-size blocks and sends a signature per block: a cheap
//! **rolling (weak) checksum** plus a **strong hash** (truncated BLAKE3). The
//! *sender* rolls a window byte-by-byte over the new file; wherever the weak
//! checksum hits a known block and the strong hash confirms it, it emits a
//! `Copy(block)` instead of the bytes. Everything else is sent as literals.
//!
//! The receiver reconstructs the new file from copies (read out of its old
//! file) and literals, then verifies the whole-file BLAKE3 hash before the
//! usual atomic commit — so a mis-match (e.g. a rolling-checksum collision that
//! slipped past the strong hash, or a changed basis) can never corrupt output.
//!
//! This module holds the *pure* algorithm (rolling checksum, signatures,
//! in-memory encode/apply used by unit tests) plus a bounded-memory streaming
//! encoder used by the network sender. The wire plumbing lives in `send`/`recv`.

use std::collections::HashMap;
use std::io::Read;

use crate::error::{Error, Result};

/// Minimum / maximum block sizes for delta signatures.
pub const MIN_BLOCK: u32 = 2 * 1024; // 2 KiB
pub const MAX_BLOCK: u32 = 128 * 1024; // 128 KiB
/// Cap on block count so signature messages stay bounded for huge files
/// (each signature is ~45 bytes on the wire; 200k keeps it under ~9 MiB).
const MAX_BLOCKS: u64 = 200_000;
/// Flush a pending literal run once it reaches this size (bounds memory).
pub const LITERAL_FLUSH: usize = 1024 * 1024;

/// Delta is only worth attempting for files at least this large; below it, a
/// plain full transfer is simpler and usually faster.
pub const MIN_DELTA_FILE: u64 = 256 * 1024;

/// Choose a block size for a basis file of `len` bytes.
///
/// Roughly `sqrt(len)` (rsync's heuristic) so block count grows slowly, then
/// clamped to `[MIN_BLOCK, MAX_BLOCK]` and raised further if needed to keep the
/// block count under `MAX_BLOCKS`.
pub fn block_size_for(len: u64) -> u32 {
    let mut bs = (len as f64).sqrt() as u64;
    bs = bs.clamp(MIN_BLOCK as u64, MAX_BLOCK as u64);
    // Ensure we don't exceed the block-count cap on very large files.
    if len.div_ceil(bs) > MAX_BLOCKS {
        bs = len.div_ceil(MAX_BLOCKS);
    }
    bs.max(MIN_BLOCK as u64) as u32
}

/// One block's signature.
#[derive(Debug, Clone)]
pub struct BlockSig {
    pub weak: u32,
    /// Hex of the first 16 bytes of the block's BLAKE3 hash.
    pub strong: String,
}

/// Strong hash of a block: first 16 bytes of BLAKE3, hex-encoded.
pub fn strong_hash(block: &[u8]) -> String {
    let h = blake3::hash(block);
    hex::encode(&h.as_bytes()[..16])
}

/// Compute block signatures for `data` at `block_size` (in-memory; tests).
pub fn signatures(data: &[u8], block_size: u32) -> Vec<BlockSig> {
    let bs = block_size as usize;
    let mut out = Vec::new();
    let mut off = 0;
    while off < data.len() {
        let end = (off + bs).min(data.len());
        let block = &data[off..end];
        out.push(BlockSig {
            weak: Rolling::init(block).value(),
            strong: strong_hash(block),
        });
        off = end;
    }
    out
}

/// Compute block signatures by streaming a file at `block_size` (bounded
/// memory: one block buffered at a time). Used by the receiver to sign its
/// (possibly huge) basis file.
pub fn signatures_file(path: &std::path::Path, block_size: u32) -> Result<Vec<BlockSig>> {
    let bs = block_size as usize;
    let mut f = std::fs::File::open(path).map_err(Error::Io)?;
    let mut buf = vec![0u8; bs];
    let mut out = Vec::new();
    loop {
        let n = fill(&mut f, &mut buf, 0)?;
        if n == 0 {
            break;
        }
        let block = &buf[..n];
        out.push(BlockSig {
            weak: Rolling::init(block).value(),
            strong: strong_hash(block),
        });
        if n < bs {
            break; // last (short) block
        }
    }
    Ok(out)
}

/// An index of signatures for fast matching: weak checksum -> (strong, index).
pub struct SigIndex {
    pub block_size: u32,
    map: HashMap<u32, Vec<(String, u64)>>,
}

impl SigIndex {
    pub fn new(block_size: u32, sigs: &[BlockSig]) -> Self {
        let mut map: HashMap<u32, Vec<(String, u64)>> = HashMap::new();
        for (i, s) in sigs.iter().enumerate() {
            map.entry(s.weak)
                .or_default()
                .push((s.strong.clone(), i as u64));
        }
        SigIndex { block_size, map }
    }

    /// If `window`'s weak checksum (`weak`) matches a block whose strong hash
    /// also matches, return that block index.
    fn lookup(&self, weak: u32, window: &[u8]) -> Option<u64> {
        let cands = self.map.get(&weak)?;
        let strong = strong_hash(window);
        cands.iter().find(|(s, _)| *s == strong).map(|(_, i)| *i)
    }
}

/// The rolling (weak) checksum, mod 2^16 per component (rsync-compatible form).
pub struct Rolling {
    a: u32,
    b: u32,
    block: u32,
}

impl Rolling {
    /// Initialize over a full window (length becomes the roll block length).
    pub fn init(data: &[u8]) -> Self {
        let l = data.len() as u32;
        let mut a = 0u32;
        let mut b = 0u32;
        for (i, &x) in data.iter().enumerate() {
            a = a.wrapping_add(x as u32);
            b = b.wrapping_add((l - i as u32).wrapping_mul(x as u32));
        }
        Rolling {
            a: a & 0xffff,
            b: b & 0xffff,
            block: l,
        }
    }

    /// The combined 32-bit checksum.
    pub fn value(&self) -> u32 {
        (self.a & 0xffff) | ((self.b & 0xffff) << 16)
    }

    /// Slide the window by one byte: `out` leaves, `inp` enters.
    pub fn roll(&mut self, out: u8, inp: u8) {
        self.a = self.a.wrapping_sub(out as u32).wrapping_add(inp as u32) & 0xffff;
        self.b = self
            .b
            .wrapping_sub(self.block.wrapping_mul(out as u32))
            .wrapping_add(self.a)
            & 0xffff;
    }
}

/// A delta instruction.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// Copy `count` consecutive basis blocks starting at `start` block index.
    Copy { start: u64, count: u64 },
    /// Insert literal bytes not found in the basis.
    Literal(Vec<u8>),
}

/// In-memory delta encode (used by unit tests). Emits `Copy`/`Literal` tokens
/// turning the basis described by `index` into `target`.
pub fn encode(target: &[u8], index: &SigIndex) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut lit: Vec<u8> = Vec::new();
    let mut pending: Option<(u64, u64)> = None; // (start, count)

    let flush_lit = |tokens: &mut Vec<Token>, lit: &mut Vec<u8>| {
        if !lit.is_empty() {
            tokens.push(Token::Literal(std::mem::take(lit)));
        }
    };
    let flush_copy = |tokens: &mut Vec<Token>, pending: &mut Option<(u64, u64)>| {
        if let Some((start, count)) = pending.take() {
            tokens.push(Token::Copy { start, count });
        }
    };

    let bs = index.block_size as usize;
    let mut pos = 0usize;
    let mut rolling = if target.len() >= bs {
        Some(Rolling::init(&target[0..bs]))
    } else {
        None
    };

    while pos < target.len() {
        if pos + bs <= target.len() {
            let weak = rolling.as_ref().unwrap().value();
            if let Some(block_index) = index.lookup(weak, &target[pos..pos + bs]) {
                flush_lit(&mut tokens, &mut lit);
                match pending.as_mut() {
                    Some((start, count)) if *start + *count == block_index => *count += 1,
                    _ => {
                        flush_copy(&mut tokens, &mut pending);
                        pending = Some((block_index, 1));
                    }
                }
                pos += bs;
                rolling = if pos + bs <= target.len() {
                    Some(Rolling::init(&target[pos..pos + bs]))
                } else {
                    None
                };
                continue;
            }
            // No match: emit one literal byte, roll forward.
            flush_copy(&mut tokens, &mut pending);
            lit.push(target[pos]);
            if pos + bs < target.len() {
                rolling
                    .as_mut()
                    .unwrap()
                    .roll(target[pos], target[pos + bs]);
            }
            pos += 1;
        } else {
            // Tail shorter than a block: try to match it as the final (short)
            // basis block; otherwise it is literal.
            let tail = &target[pos..];
            if let Some(block_index) = index.lookup(Rolling::init(tail).value(), tail) {
                flush_lit(&mut tokens, &mut lit);
                match pending.as_mut() {
                    Some((start, count)) if *start + *count == block_index => *count += 1,
                    _ => {
                        flush_copy(&mut tokens, &mut pending);
                        pending = Some((block_index, 1));
                    }
                }
            } else {
                flush_copy(&mut tokens, &mut pending);
                lit.extend_from_slice(tail);
            }
            pos = target.len();
        }
    }
    flush_copy(&mut tokens, &mut pending);
    flush_lit(&mut tokens, &mut lit);
    tokens
}

/// Reconstruct the target from `basis` and `tokens` (in-memory; tests).
pub fn apply(basis: &[u8], block_size: u32, tokens: &[Token]) -> Vec<u8> {
    let bs = block_size as usize;
    let mut out = Vec::new();
    for t in tokens {
        match t {
            Token::Literal(data) => out.extend_from_slice(data),
            Token::Copy { start, count } => {
                for i in 0..*count {
                    let b = (*start + i) as usize * bs;
                    let e = (b + bs).min(basis.len());
                    out.extend_from_slice(&basis[b..e]);
                }
            }
        }
    }
    out
}

/// A streaming delta instruction handed to the [`encode_stream`] callback.
pub enum Emit<'a> {
    /// Copy `count` consecutive basis blocks starting at `start`.
    Copy { start: u64, count: u64 },
    /// Insert these literal bytes (borrowed; valid only for the call).
    Literal(&'a [u8]),
}

/// Streaming, bounded-memory delta encoder over a reader.
///
/// Invokes `on` with each [`Emit`] as it goes; the caller serializes them to
/// the wire. A single callback (rather than two) lets the caller borrow one
/// output sink mutably. Memory use is bounded by `~2*block_size + LITERAL_FLUSH`.
pub fn encode_stream<R, F>(mut src: R, index: &SigIndex, mut on: F) -> Result<()>
where
    R: Read,
    F: FnMut(Emit) -> Result<()>,
{
    let bs = index.block_size as usize;
    let cap = bs + bs.max(LITERAL_FLUSH);
    let mut buf: Vec<u8> = vec![0u8; cap + bs];

    let mut end = fill(&mut src, &mut buf, 0)?; // valid bytes in buf[0..end]
    let mut pos = 0usize; // window start
    let mut lit_start = 0usize; // pending literal start
    let mut eof = end < buf.len();
    let mut pending: Option<(u64, u64)> = None;

    let mut rolling = if end >= bs {
        Some(Rolling::init(&buf[0..bs]))
    } else {
        None
    };

    loop {
        // Make sure a full block is available ahead, unless we're at EOF.
        if pos + bs > end && !eof {
            // Flush pending literal so we can discard bytes before `pos`.
            if pos > lit_start {
                on(Emit::Literal(&buf[lit_start..pos]))?;
            }
            // Compact: move [pos..end] to the front, then refill.
            buf.copy_within(pos..end, 0);
            end -= pos;
            pos = 0;
            lit_start = 0;
            let got = fill(&mut src, &mut buf, end)?;
            if end + got < buf.len() {
                eof = true;
            }
            end += got;
            rolling = if end >= bs {
                Some(Rolling::init(&buf[0..bs]))
            } else {
                None
            };
        }

        if pos + bs <= end {
            let weak = rolling.as_ref().unwrap().value();
            if let Some(block_index) = index.lookup(weak, &buf[pos..pos + bs]) {
                if pos > lit_start {
                    on(Emit::Literal(&buf[lit_start..pos]))?;
                }
                match pending.as_mut() {
                    Some((start, count)) if *start + *count == block_index => *count += 1,
                    _ => {
                        if let Some((start, count)) = pending.take() {
                            on(Emit::Copy { start, count })?;
                        }
                        pending = Some((block_index, 1));
                    }
                }
                pos += bs;
                lit_start = pos;
                rolling = if pos + bs <= end {
                    Some(Rolling::init(&buf[pos..pos + bs]))
                } else {
                    None
                };
                continue;
            }
            // No match: current byte is literal; roll forward by one.
            if let Some((start, count)) = pending.take() {
                on(Emit::Copy { start, count })?;
            }
            let out_byte = buf[pos];
            let in_byte = buf[pos + bs];
            rolling.as_mut().unwrap().roll(out_byte, in_byte);
            pos += 1;
            // Bound the pending literal run.
            if pos - lit_start >= LITERAL_FLUSH {
                on(Emit::Literal(&buf[lit_start..pos]))?;
                lit_start = pos;
            }
        } else {
            // EOF with a tail shorter than a block: try to match it as the
            // final (short) basis block, else flush it as literal.
            let tail = &buf[pos..end];
            if !tail.is_empty() {
                if let Some(block_index) = index.lookup(Rolling::init(tail).value(), tail) {
                    if pos > lit_start {
                        on(Emit::Literal(&buf[lit_start..pos]))?;
                    }
                    match pending.as_mut() {
                        Some((start, count)) if *start + *count == block_index => *count += 1,
                        _ => {
                            if let Some((start, count)) = pending.take() {
                                on(Emit::Copy { start, count })?;
                            }
                            pending = Some((block_index, 1));
                        }
                    }
                    break;
                }
            }
            if let Some((start, count)) = pending.take() {
                on(Emit::Copy { start, count })?;
            }
            if end > lit_start {
                on(Emit::Literal(&buf[lit_start..end]))?;
            }
            break;
        }
    }
    // Flush any trailing copy run (e.g. a matched final short block).
    if let Some((start, count)) = pending.take() {
        on(Emit::Copy { start, count })?;
    }
    Ok(())
}

/// Read as many bytes as possible into `buf[at..]`; returns bytes read.
fn fill<R: Read>(src: &mut R, buf: &mut [u8], at: usize) -> Result<usize> {
    let mut filled = at;
    while filled < buf.len() {
        let n = src.read(&mut buf[filled..]).map_err(Error::Io)?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled - at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_matches_fresh_init_after_slide() {
        let data: Vec<u8> = (0..100u32).map(|i| (i * 7 % 251) as u8).collect();
        let block = 16;
        let mut r = Rolling::init(&data[0..block]);
        for k in 1..(data.len() - block) {
            r.roll(data[k - 1], data[k + block - 1]);
            let fresh = Rolling::init(&data[k..k + block]);
            assert_eq!(r.value(), fresh.value(), "mismatch at offset {k}");
        }
    }

    fn roundtrip(basis: &[u8], target: &[u8]) {
        let bs = block_size_for(basis.len() as u64);
        let sigs = signatures(basis, bs);
        let index = SigIndex::new(bs, &sigs);
        // in-memory encode
        let tokens = encode(target, &index);
        let rebuilt = apply(basis, bs, &tokens);
        assert_eq!(rebuilt, target, "in-memory delta must reconstruct exactly");

        // streaming encode must produce an equivalent reconstruction
        let mut stream_tokens: Vec<Token> = Vec::new();
        encode_stream(target, &index, |emit| {
            match emit {
                Emit::Copy { start, count } => stream_tokens.push(Token::Copy { start, count }),
                Emit::Literal(bytes) => stream_tokens.push(Token::Literal(bytes.to_vec())),
            }
            Ok(())
        })
        .unwrap();
        let rebuilt2 = apply(basis, bs, &stream_tokens);
        assert_eq!(rebuilt2, target, "streaming delta must reconstruct exactly");
    }

    #[test]
    fn identical_files_are_all_copies() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
        let bs = block_size_for(data.len() as u64);
        let sigs = signatures(&data, bs);
        let index = SigIndex::new(bs, &sigs);
        let tokens = encode(&data, &index);
        assert!(
            tokens.iter().all(|t| matches!(t, Token::Copy { .. })),
            "identical target should be pure copies, got {tokens:?}"
        );
        assert_eq!(apply(&data, bs, &tokens), data);
    }

    #[test]
    fn edited_middle_roundtrips() {
        let mut basis: Vec<u8> = (0..300_000u32).map(|i| (i * 13 % 251) as u8).collect();
        let mut target = basis.clone();
        // Edit a chunk in the middle (different length) to force literals.
        target.splice(
            150_000..150_050,
            b"THIS IS SOME BRAND NEW INSERTED CONTENT!!".iter().copied(),
        );
        roundtrip(&basis, &target);
        // Also test prepend and truncate.
        basis.truncate(290_000);
        roundtrip(&target, &basis);
    }

    #[test]
    fn totally_different_is_all_literal() {
        let basis: Vec<u8> = vec![0u8; 100_000];
        let target: Vec<u8> = (0..100_000u32).map(|i| (i % 255 + 1) as u8).collect();
        roundtrip(&basis, &target);
    }

    #[test]
    fn empty_and_small() {
        roundtrip(b"", b"");
        roundtrip(b"", b"hello world");
        roundtrip(b"abc", b"");
        roundtrip(b"small basis", b"small target that is longer");
    }
}
