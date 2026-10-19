//! End-to-end integration tests for the transfer engine.
//!
//! Each test stands up a real receiver on an ephemeral loopback port and runs
//! a real sender against it over the encrypted Noise transport — no mocks.

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
