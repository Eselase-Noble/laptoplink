//! Secure transport: a mutually-authenticated Noise (`XX`) handshake over TCP,
//! followed by an AEAD-protected byte stream (`SecureStream`).
//!
//! The handshake authenticates both peers by their static X25519 keys. After
//! the handshake we expose the remote's static public key so the caller can
//! check it against the paired-peer store. All subsequent bytes are encrypted
//! and authenticated with ChaChaPoly; framing is per Noise packet (max 65535
//! bytes), each length-prefixed with a big-endian `u16`.

use crate::error::{Error, Result};
use crate::identity::{DeviceIdentity, Fingerprint};
use crate::NOISE_PARAMS;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;

/// Buffer size for the framed socket halves. Large enough to batch many Noise
/// packets into one syscall, which is the dominant cost on fast links.
const SOCK_BUF: usize = 512 * 1024;

/// Noise packets carry at most 65535 bytes of ciphertext (plaintext + 16-byte
/// AEAD tag), so the usable plaintext per packet is 65519 bytes.
const NOISE_MAX_MSG: usize = 65535;
const MAX_PLAINTEXT: usize = NOISE_MAX_MSG - 16;

/// Write a raw, *unencrypted* length-prefixed frame. Used only during the
/// handshake, before transport keys exist.
fn write_raw_frame(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    let len =
        u16::try_from(data.len()).map_err(|_| Error::protocol("handshake frame too large"))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(data)?;
    stream.flush()?;
    Ok(())
}

/// Read a raw, unencrypted length-prefixed frame (handshake only).
fn read_raw_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf)?;
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

/// Perform the Noise `XX` handshake as the initiator (connecting side).
pub fn handshake_initiator(
    mut stream: TcpStream,
    identity: &DeviceIdentity,
) -> Result<SecureStream> {
    let mut noise = snow::Builder::new(NOISE_PARAMS.parse().expect("valid params"))
        .local_private_key(identity.private_key())
        .build_initiator()?;

    let mut buf = vec![0u8; NOISE_MAX_MSG];

    // -> e
    let n = noise.write_message(&[], &mut buf)?;
    write_raw_frame(&mut stream, &buf[..n])?;
    // <- e, ee, s, es
    let msg = read_raw_frame(&mut stream)?;
    noise.read_message(&msg, &mut buf)?;
    // -> s, se
    let n = noise.write_message(&[], &mut buf)?;
    write_raw_frame(&mut stream, &buf[..n])?;

    finish(stream, noise)
}

/// Perform the Noise `XX` handshake as the responder (listening side).
pub fn handshake_responder(
    mut stream: TcpStream,
    identity: &DeviceIdentity,
) -> Result<SecureStream> {
    let mut noise = snow::Builder::new(NOISE_PARAMS.parse().expect("valid params"))
        .local_private_key(identity.private_key())
        .build_responder()?;

    let mut buf = vec![0u8; NOISE_MAX_MSG];

    // -> e
    let msg = read_raw_frame(&mut stream)?;
    noise.read_message(&msg, &mut buf)?;
    // <- e, ee, s, es
    let n = noise.write_message(&[], &mut buf)?;
    write_raw_frame(&mut stream, &buf[..n])?;
    // -> s, se
    let msg = read_raw_frame(&mut stream)?;
    noise.read_message(&msg, &mut buf)?;

    finish(stream, noise)
}

fn finish(stream: TcpStream, noise: snow::HandshakeState) -> Result<SecureStream> {
    let remote_static = noise
        .get_remote_static()
        .map(|s| s.to_vec())
        .ok_or_else(|| Error::protocol("handshake did not yield remote static key"))?;
    let fingerprint = Fingerprint::of_public_key(&remote_static);
    let transport = noise.into_transport_mode()?;
    // Independent buffered handles over the same socket: one for reading, one
    // for writing. `socket` is kept for timeout/option tweaks.
    let socket = stream.try_clone()?;
    let reader = BufReader::with_capacity(SOCK_BUF, stream.try_clone()?);
    let writer = BufWriter::with_capacity(SOCK_BUF, stream);
    Ok(SecureStream {
        socket,
        reader,
        writer,
        transport,
        remote_static,
        remote_fingerprint: fingerprint,
        read_plain: Vec::new(),
        read_pos: 0,
        cipher_scratch: vec![0u8; NOISE_MAX_MSG],
        plain_scratch: vec![0u8; MAX_PLAINTEXT],
    })
}

/// An encrypted, authenticated bidirectional byte stream.
///
/// Implements [`Read`] and [`Write`]; callers layer the length-delimited
/// control protocol and raw chunk bytes on top. Memory use is bounded: at most
/// one Noise packet is buffered in each direction.
pub struct SecureStream {
    /// Spare handle to the socket for setting options/timeouts.
    socket: TcpStream,
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
    transport: snow::TransportState,
    remote_static: Vec<u8>,
    remote_fingerprint: Fingerprint,
    // Decrypted-but-not-yet-consumed plaintext from the last inbound packet.
    read_plain: Vec<u8>,
    read_pos: usize,
    cipher_scratch: Vec<u8>,
    plain_scratch: Vec<u8>,
}

impl SecureStream {
    /// The authenticated static public key of the remote peer.
    pub fn remote_static_key(&self) -> &[u8] {
        &self.remote_static
    }

    /// The fingerprint of the remote peer's static key.
    pub fn remote_fingerprint(&self) -> &Fingerprint {
        &self.remote_fingerprint
    }

    /// Set a read timeout on the underlying socket.
    pub fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> Result<()> {
        self.socket.set_read_timeout(dur)?;
        Ok(())
    }

    fn fill_read_buf(&mut self) -> io::Result<()> {
        // Read one length-prefixed ciphertext packet and decrypt it.
        let mut len_buf = [0u8; 2];
        self.reader.read_exact(&mut len_buf)?;
        let clen = u16::from_be_bytes(len_buf) as usize;
        if clen > NOISE_MAX_MSG {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "noise packet exceeds maximum size",
            ));
        }
        let cipher = &mut self.cipher_scratch[..clen];
        self.reader.read_exact(cipher)?;
        let n = self
            .transport
            .read_message(cipher, &mut self.plain_scratch)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        self.read_plain.clear();
        self.read_plain.extend_from_slice(&self.plain_scratch[..n]);
        self.read_pos = 0;
        Ok(())
    }

    /// End the session cleanly.
    ///
    /// Flushes, half-closes our write direction (a FIN, so the last message is
    /// delivered gracefully), then drains inbound bytes until the peer's FIN.
    /// When both sides do this, neither drops a socket with data still in
    /// flight, which is what makes Windows raise an abortive reset
    /// (WSAECONNRESET / "failed to fill whole buffer") and lose the final
    /// message. Best-effort: every step ignores errors.
    pub fn graceful_close(&mut self) {
        use std::net::Shutdown;
        let _ = self.writer.flush();
        let _ = self.socket.shutdown(Shutdown::Write);
        let _ = self
            .socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let mut scratch = [0u8; 256];
        // Read raw bytes off the socket directly (not decrypted): we only care
        // about reaching EOF so we close after the peer does.
        loop {
            match self.reader.read(&mut scratch) {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
    }
}

impl Read for SecureStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.read_pos >= self.read_plain.len() {
            // Need a fresh packet. Propagate EOF cleanly.
            match self.fill_read_buf() {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
                Err(e) => return Err(e),
            }
            if self.read_plain.is_empty() {
                return Ok(0);
            }
        }
        let available = &self.read_plain[self.read_pos..];
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.read_pos += n;
        Ok(n)
    }
}

impl Write for SecureStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        // Encrypt in <=MAX_PLAINTEXT segments; each becomes one Noise packet.
        let mut written = 0;
        for seg in data.chunks(MAX_PLAINTEXT) {
            let n = self
                .transport
                .write_message(seg, &mut self.cipher_scratch)
                .map_err(|e| io::Error::other(e.to_string()))?;
            let len = n as u16;
            self.writer.write_all(&len.to_be_bytes())?;
            self.writer.write_all(&self.cipher_scratch[..n])?;
            written += seg.len();
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}
