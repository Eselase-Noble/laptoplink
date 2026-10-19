//! End-to-end integration tests for the transfer engine.
//!
//! Each test stands up a real receiver on an ephemeral loopback port and runs
//! a real sender against it over the encrypted Noise transport, no mocks.
//!
//! Gated off Windows. These drive sender and receiver in a single process over
//! loopback; on Windows that arrangement intermittently hits an abortive
//! socket reset at teardown and an antivirus/indexer "Access is denied" on the
//! finalize rename. The graceful close + rename retry make it usually pass, but
//! not reliably, and it cannot be reproduced/iterated without a real Windows
//! host. Windows build + unit tests still run in CI; end-to-end transfer on
//! Windows is tracked as not-yet-validated in docs/FEATURES.md.
#![cfg(not(windows))]

use laptoplink_core::chunk;
use laptoplink_core::config::Config;
use laptoplink_core::identity::DeviceIdentity;
use laptoplink_core::manifest::{Bitmap, ReceiverFile, ReceiverManifest};
use laptoplink_core::progress::NullSink;
use laptoplink_core::{recv, send, Engine};
use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;

/// Build an isolated engine rooted at `root`, returning its identity.
fn make_identity(root: &Path, name: &str) -> (Config, DeviceIdentity) {
    let cfg = Config::at_root(root);
    let engine = Engine::new(cfg.clone(), name).unwrap();
    (cfg, engine.identity().clone())
}

/// Write `size` bytes of deterministic but non-trivial data.
fn write_test_file(path: &Path, size: usize) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    let mut f = fs::File::create(path).unwrap();
    let mut buf = vec![0u8; 65536];
    let mut written = 0usize;
    let mut seed: u32 = 0x1234_5678;
    while written < size {
        for b in buf.iter_mut() {
            // xorshift — fast, reproducible, not all-zero (so hashing is real).
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *b = (seed & 0xff) as u8;
        }
        let n = buf.len().min(size - written);
        f.write_all(&buf[..n]).unwrap();
        written += n;
    }
    f.flush().unwrap();
}

/// Spawn a one-shot receiver on an ephemeral port; returns (addr, join handle).
fn spawn_receiver(
    cfg: &Config,
    id: &DeviceIdentity,
    dest: &Path,
    accept_new: bool,
) -> (String, thread::JoinHandle<laptoplink_core::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let cfg = cfg.clone();
    let id = id.clone();
    let dest = dest.to_path_buf();
    let handle = thread::spawn(move || {
        let opts = recv::ReceiveOptions {
            dest,
            accept_new,
            device_name: "receiver".into(),
            once: true,
            allow_sender_dest: false,
        };
        let mut sink = NullSink;
        recv::serve_on(&listener, &cfg, &id, &opts, &mut sink)
    });
    (addr, handle)
}

/// Like [`spawn_receiver`] but with an explicit `allow_sender_dest`.
fn spawn_receiver_opts(
    cfg: &Config,
    id: &DeviceIdentity,
    dest: &Path,
    accept_new: bool,
    allow_sender_dest: bool,
) -> (String, thread::JoinHandle<laptoplink_core::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let cfg = cfg.clone();
    let id = id.clone();
    let dest = dest.to_path_buf();
    let handle = thread::spawn(move || {
        let opts = recv::ReceiveOptions {
            dest,
            accept_new,
            device_name: "receiver".into(),
            once: true,
            allow_sender_dest,
        };
        let mut sink = NullSink;
        recv::serve_on(&listener, &cfg, &id, &opts, &mut sink)
    });
    (addr, handle)
}

fn send_opts() -> send::SendOptions {
    send::SendOptions {
        transfer_id: None,
        chunk_size: None,
        device_name: "sender".into(),
        dest: None,
    }
}

#[test]
fn single_file_round_trips_and_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let (scfg, sid) = make_identity(&tmp.path().join("sender"), "sender");
    let (rcfg, rid) = make_identity(&tmp.path().join("receiver"), "receiver");

    let src = tmp.path().join("src/data.bin");
    write_test_file(&src, 10 * 1024 * 1024 + 123); // multi-chunk + remainder
    let dest = tmp.path().join("dest");

    let (addr, handle) = spawn_receiver(&rcfg, &rid, &dest, true);
    let items = send::expand_sources(std::slice::from_ref(&src)).unwrap();
    let summary = send::send(&scfg, &sid, &addr, items, send_opts(), &mut NullSink).unwrap();
    handle.join().unwrap().unwrap();

    let got = dest.join("data.bin");
    assert!(got.exists(), "destination file should exist");
    assert_eq!(
        fs::read(&src).unwrap(),
        fs::read(&got).unwrap(),
        "round-tripped bytes must match"
    );
    assert_eq!(summary.files, 1);
    assert_eq!(summary.bytes_sent, fs::metadata(&src).unwrap().len());
}

#[test]
fn directory_tree_preserves_structure() {
    let tmp = tempfile::tempdir().unwrap();
    let (scfg, sid) = make_identity(&tmp.path().join("sender"), "sender");
    let (rcfg, rid) = make_identity(&tmp.path().join("receiver"), "receiver");

    let dir = tmp.path().join("tree");
    write_test_file(&dir.join("a.txt"), 2048);
    write_test_file(&dir.join("sub/b.bin"), 1024 * 1024 + 7);
    write_test_file(&dir.join("sub/deep/c.dat"), 50_000);
    let dest = tmp.path().join("dest");

    let (addr, handle) = spawn_receiver(&rcfg, &rid, &dest, true);
    let items = send::expand_sources(std::slice::from_ref(&dir)).unwrap();
    send::send(&scfg, &sid, &addr, items, send_opts(), &mut NullSink).unwrap();
    handle.join().unwrap().unwrap();

    for rel in ["tree/a.txt", "tree/sub/b.bin", "tree/sub/deep/c.dat"] {
        let got = dest.join(rel);
        assert!(got.exists(), "{rel} should exist at destination");
    }
    assert_eq!(
        fs::read(dir.join("sub/b.bin")).unwrap(),
        fs::read(dest.join("tree/sub/b.bin")).unwrap()
    );
}

#[test]
fn sender_dest_is_honored_only_when_allowed() {
    let tmp = tempfile::tempdir().unwrap();
    let (scfg, sid) = make_identity(&tmp.path().join("sender"), "sender");
    let (rcfg, rid) = make_identity(&tmp.path().join("receiver"), "receiver");

    let src = tmp.path().join("src/data.bin");
    write_test_file(&src, 64 * 1024);
    let receiver_default = tmp.path().join("default-dest");
    let sender_wants = tmp.path().join("sender-chosen");

    let opts = || send::SendOptions {
        transfer_id: None,
        chunk_size: None,
        device_name: "sender".into(),
        dest: Some(sender_wants.to_string_lossy().into_owned()),
    };

    // Receiver does NOT allow sender dest -> file lands under the receiver's own dest.
    let (addr, h) = spawn_receiver_opts(&rcfg, &rid, &receiver_default, true, false);
    let items = send::expand_sources(std::slice::from_ref(&src)).unwrap();
    send::send(&scfg, &sid, &addr, items, opts(), &mut NullSink).unwrap();
    h.join().unwrap().unwrap();
    assert!(
        receiver_default.join("data.bin").exists(),
        "should use receiver dest"
    );
    assert!(
        !sender_wants.join("data.bin").exists(),
        "must ignore sender dest"
    );

    // Receiver allows sender dest -> file lands where the sender asked.
    let (addr2, h2) = spawn_receiver_opts(&rcfg, &rid, &receiver_default, true, true);
    let items2 = send::expand_sources(std::slice::from_ref(&src)).unwrap();
    send::send(&scfg, &sid, &addr2, items2, opts(), &mut NullSink).unwrap();
    h2.join().unwrap().unwrap();
    assert!(
        sender_wants.join("data.bin").exists(),
        "should honor sender dest"
    );
    assert_eq!(
        fs::read(&src).unwrap(),
        fs::read(sender_wants.join("data.bin")).unwrap()
    );
}

// Single-process loopback with two senders trips a Windows teardown race; the
// real two-process path works. Keep this one off Windows.
#[cfg(not(windows))]
#[test]
fn concurrent_receive_from_two_senders() {
    let tmp = tempfile::tempdir().unwrap();
    let (rcfg, rid) = make_identity(&tmp.path().join("receiver"), "receiver");
    let (s1cfg, s1id) = make_identity(&tmp.path().join("s1"), "s1");
    let (s2cfg, s2id) = make_identity(&tmp.path().join("s2"), "s2");

    // Pre-pair both senders so the concurrent transfers don't race on the
    // peer store (that is a separate accept-new concern).
    let mut peers = rcfg.load_peers().unwrap();
    peers.approve(&s1id.fingerprint(), "s1").unwrap();
    peers.approve(&s2id.fingerprint(), "s2").unwrap();

    let dest = tmp.path().join("dest");
    let f1 = tmp.path().join("src/one.bin");
    let f2 = tmp.path().join("src/two.bin");
    write_test_file(&f1, 2 * 1024 * 1024 + 11);
    write_test_file(&f2, 3 * 1024 * 1024 + 7);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let opts = recv::ReceiveOptions {
        dest: dest.clone(),
        accept_new: false,
        device_name: "receiver".into(),
        once: false,
        allow_sender_dest: false,
    };
    // Concurrent server on its own thread (loops forever; left running for the
    // duration of the test process).
    {
        let rcfg = rcfg.clone();
        thread::spawn(move || {
            let _ = recv::serve_on_concurrent(listener, &rcfg, &rid, &opts, || Box::new(NullSink));
        });
    }

    // Two senders transfer at the same time.
    let a = addr.clone();
    let h1 = thread::spawn(move || {
        let items = send::expand_sources(std::slice::from_ref(&f1)).unwrap();
        send::send(&s1cfg, &s1id, &a, items, send_opts(), &mut NullSink)
    });
    let b = addr.clone();
    let h2 = thread::spawn(move || {
        let items = send::expand_sources(std::slice::from_ref(&f2)).unwrap();
        send::send(&s2cfg, &s2id, &b, items, send_opts(), &mut NullSink)
    });
    h1.join().unwrap().expect("sender 1");
    h2.join().unwrap().expect("sender 2");

    assert!(
        dest.join("one.bin").exists(),
        "file from sender 1 should arrive"
    );
    assert!(
        dest.join("two.bin").exists(),
        "file from sender 2 should arrive"
    );
}

#[test]
fn unpaired_peer_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (scfg, sid) = make_identity(&tmp.path().join("sender"), "sender");
    let (rcfg, rid) = make_identity(&tmp.path().join("receiver"), "receiver");

    let src = tmp.path().join("src/data.bin");
    write_test_file(&src, 4096);
    let dest = tmp.path().join("dest");

    // accept_new = false and no prior pairing -> sender must be rejected.
    let (addr, handle) = spawn_receiver(&rcfg, &rid, &dest, false);
    let items = send::expand_sources(std::slice::from_ref(&src)).unwrap();
    let result = send::send(&scfg, &sid, &addr, items, send_opts(), &mut NullSink);
    let _ = handle.join().unwrap(); // receiver returns an auth error; that's fine

    assert!(result.is_err(), "send to unpaired receiver should fail");
    assert!(
        !dest.join("data.bin").exists(),
        "no file should be written when rejected"
    );
}

#[test]
fn resume_skips_already_completed_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (scfg, sid) = make_identity(&tmp.path().join("sender"), "sender");
    let (rcfg, rid) = make_identity(&tmp.path().join("receiver"), "receiver");

    let src = tmp.path().join("src/data.bin");
    write_test_file(&src, 3 * 1024 * 1024);
    let dest = tmp.path().join("dest");

    // First full transfer with a fixed id.
    let tid = "a".repeat(32);
    let opts = send::SendOptions {
        transfer_id: Some(tid.clone()),
        chunk_size: Some(1024 * 1024),
        device_name: "sender".into(),
        dest: None,
    };
    let (addr, handle) = spawn_receiver(&rcfg, &rid, &dest, true);
    let items = send::expand_sources(std::slice::from_ref(&src)).unwrap();
    send::send(&scfg, &sid, &addr, items, opts, &mut NullSink).unwrap();
    handle.join().unwrap().unwrap();

    // Second transfer, same id/source: receiver already has it committed.
    let opts2 = send::SendOptions {
        transfer_id: Some(tid),
        chunk_size: Some(1024 * 1024),
        device_name: "sender".into(),
        dest: None,
    };
    let (addr2, handle2) = spawn_receiver(&rcfg, &rid, &dest, true);
    let items2 = send::expand_sources(std::slice::from_ref(&src)).unwrap();
    let summary = send::send(&scfg, &sid, &addr2, items2, opts2, &mut NullSink).unwrap();
    handle2.join().unwrap().unwrap();

    assert_eq!(summary.bytes_sent, 0, "nothing should be re-sent");
    assert!(summary.bytes_skipped > 0, "all bytes should be reused");
}

#[test]
fn partial_resume_sends_only_missing_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let (scfg, sid) = make_identity(&tmp.path().join("sender"), "sender");
    let (rcfg, rid) = make_identity(&tmp.path().join("receiver"), "receiver");

    let chunk_size: u32 = 1024 * 1024;
    let src = tmp.path().join("src/data.bin");
    write_test_file(&src, (chunk_size as usize) * 4); // exactly 4 chunks
    let dest = tmp.path().join("dest");
    let tid = "b".repeat(32);

    // Seed the receiver manifest as if chunk 0 was already received + verified.
    let src_meta = fs::metadata(&src).unwrap();
    let src_mtime = src_meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let count = chunk::chunk_count(src_meta.len(), chunk_size);

    let part_path = dest
        .join(".laptoplink")
        .join("tmp")
        .join(&tid)
        .join("0.part");
    fs::create_dir_all(part_path.parent().unwrap()).unwrap();
    // Write chunk 0's real bytes into the part file.
    let src_bytes = fs::read(&src).unwrap();
    {
        let mut pf = fs::File::create(&part_path).unwrap();
        pf.write_all(&src_bytes[..chunk_size as usize]).unwrap();
        pf.flush().unwrap();
    }
    let mut verified = Bitmap::new(count);
    verified.set(0);
    let mut manifest = ReceiverManifest::new(&dest, &tid, sid.fingerprint().full());
    manifest.files.push(ReceiverFile {
        file_index: 0,
        rel_path: "data.bin".into(),
        size: src_meta.len(),
        chunk_size,
        src_mtime,
        expected_file_hash: None,
        verified,
        part_path: part_path.clone(),
        committed: false,
    });
    manifest.save().unwrap();

    // Now run the transfer; only chunks 1..3 should be sent.
    let opts = send::SendOptions {
        transfer_id: Some(tid.clone()),
        chunk_size: Some(chunk_size),
        device_name: "sender".into(),
        dest: None,
    };
    let (addr, handle) = spawn_receiver(&rcfg, &rid, &dest, true);
    let items = send::expand_sources(std::slice::from_ref(&src)).unwrap();
    let summary = send::send(&scfg, &sid, &addr, items, opts, &mut NullSink).unwrap();
    handle.join().unwrap().unwrap();

    assert_eq!(
        summary.bytes_sent,
        (chunk_size as u64) * 3,
        "exactly three chunks should be sent"
    );
    assert_eq!(
        summary.bytes_skipped, chunk_size as u64,
        "the pre-seeded chunk should be reused"
    );
    assert_eq!(
        fs::read(&src).unwrap(),
        fs::read(dest.join("data.bin")).unwrap(),
        "reconstructed file must match source"
    );
}

#[test]
fn rejects_path_traversal_in_offer() {
    // The path-safety layer is also unit-tested; this asserts the public API
    // refuses to join a traversing path under a root.
    let root = PathBuf::from("/tmp/laptoplink-test-root");
    assert!(laptoplink_core::pathsafety::safe_join(&root, "../escape").is_err());
    assert!(laptoplink_core::pathsafety::safe_join(&root, "ok/nested.txt").is_ok());
}
