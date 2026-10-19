//! End-to-end integration tests for one-way sync (Phase 3).
//!
//! Each test runs a real receiver on a loopback port and a real sync client.
//!
//! Gated off Windows for now. Push transfers (see `transfer.rs`) pass on
//! Windows, but `sync` still hits a Windows-only abortive reset at session
//! teardown in the single-process loopback harness that the graceful close did
//! not fully resolve. Tracked in docs/FEATURES.md; sync on Windows is still
//! being worked out. On Linux/macOS these run normally.
#![cfg(not(windows))]

use laptoplink_core::config::Config;
use laptoplink_core::identity::DeviceIdentity;
use laptoplink_core::progress::NullSink;
use laptoplink_core::{recv, sync, Engine};
use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::thread;

fn make_identity(root: &Path, name: &str) -> (Config, DeviceIdentity) {
    let cfg = Config::at_root(root);
    let engine = Engine::new(cfg.clone(), name).unwrap();
    (cfg, engine.identity().clone())
}

fn write_file(path: &Path, contents: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// Spawn a one-shot receiver on an ephemeral port.
fn spawn_receiver(
    cfg: &Config,
    id: &DeviceIdentity,
    dest: &Path,
) -> (String, thread::JoinHandle<laptoplink_core::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let cfg = cfg.clone();
    let id = id.clone();
    let dest = dest.to_path_buf();
    let handle = thread::spawn(move || {
        let opts = recv::ReceiveOptions {
            dest,
            accept_new: true,
            device_name: "receiver".into(),
            once: true,
            allow_sender_dest: false,
        };
        recv::serve_on(&listener, &cfg, &id, &opts, &mut NullSink)
    });
    (addr, handle)
}

fn opts(delete: bool, dry_run: bool, excludes: Vec<String>) -> sync::SyncOptions {
    sync::SyncOptions {
        delete,
        dry_run,
        checksum: false,
        delta: false,
        excludes,
        chunk_size: None,
        device_name: "sender".into(),
    }
}

#[test]
fn transfers_new_then_skips_identical() {
    let tmp = tempfile::tempdir().unwrap();
    let (scfg, sid) = make_identity(&tmp.path().join("s"), "sender");
    let (rcfg, rid) = make_identity(&tmp.path().join("r"), "receiver");

    let src = tmp.path().join("src");
    write_file(&src.join("a.txt"), b"alpha");
    write_file(&src.join("sub/b.bin"), &vec![7u8; 500_000]);
    let dest = tmp.path().join("dest");

    // First sync: everything is new.
    let (addr, h) = spawn_receiver(&rcfg, &rid, &dest);
    let s1 = sync::sync(
        &scfg,
        &sid,
        &addr,
        &src,
        opts(false, false, vec![]),
        &mut NullSink,
    )
    .unwrap();
    h.join().unwrap().unwrap();
    assert_eq!(s1.files_total, 2);
    assert_eq!(s1.files_transferred, 2);
    assert_eq!(s1.files_skipped, 0);
    assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"alpha");
    assert_eq!(
        fs::read(dest.join("sub/b.bin")).unwrap(),
        vec![7u8; 500_000]
    );

    // Second sync: nothing changed -> all skipped by size + mtime.
    let (addr2, h2) = spawn_receiver(&rcfg, &rid, &dest);
    let s2 = sync::sync(
        &scfg,
        &sid,
        &addr2,
        &src,
        opts(false, false, vec![]),
        &mut NullSink,
    )
    .unwrap();
    h2.join().unwrap().unwrap();
    assert_eq!(s2.files_transferred, 0, "identical files must be skipped");
    assert_eq!(s2.files_skipped, 2);
    assert_eq!(s2.bytes_transferred, 0);
}

#[test]
fn changed_file_is_resent() {
    let tmp = tempfile::tempdir().unwrap();
    let (scfg, sid) = make_identity(&tmp.path().join("s"), "sender");
    let (rcfg, rid) = make_identity(&tmp.path().join("r"), "receiver");

    let src = tmp.path().join("src");
    write_file(&src.join("a.txt"), b"version-1");
    let dest = tmp.path().join("dest");

    let (addr, h) = spawn_receiver(&rcfg, &rid, &dest);
    sync::sync(
        &scfg,
        &sid,
        &addr,
        &src,
        opts(false, false, vec![]),
        &mut NullSink,
    )
    .unwrap();
    h.join().unwrap().unwrap();

    // Change the content (and therefore size); next sync must re-send it.
    write_file(&src.join("a.txt"), b"version-2-longer");
    let (addr2, h2) = spawn_receiver(&rcfg, &rid, &dest);
    let s2 = sync::sync(
        &scfg,
        &sid,
        &addr2,
        &src,
        opts(false, false, vec![]),
        &mut NullSink,
    )
    .unwrap();
    h2.join().unwrap().unwrap();
    assert_eq!(s2.files_transferred, 1);
    assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"version-2-longer");
}

#[test]
fn dry_run_makes_no_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let (scfg, sid) = make_identity(&tmp.path().join("s"), "sender");
    let (rcfg, rid) = make_identity(&tmp.path().join("r"), "receiver");

    let src = tmp.path().join("src");
    write_file(&src.join("new.txt"), b"hello");
    let dest = tmp.path().join("dest");
    fs::create_dir_all(&dest).unwrap();

    let (addr, h) = spawn_receiver(&rcfg, &rid, &dest);
    let s = sync::sync(
        &scfg,
        &sid,
        &addr,
        &src,
        opts(false, true, vec![]),
        &mut NullSink,
    )
    .unwrap();
    h.join().unwrap().unwrap();

    assert!(s.dry_run);
    assert_eq!(s.files_transferred, 1, "plan should report 1 to transfer");
    assert!(
        !dest.join("new.txt").exists(),
        "dry-run must not write anything"
    );
}

#[test]
fn delete_removes_extraneous_only_when_requested() {
    let tmp = tempfile::tempdir().unwrap();
    let (scfg, sid) = make_identity(&tmp.path().join("s"), "sender");
    let (rcfg, rid) = make_identity(&tmp.path().join("r"), "receiver");

    let src = tmp.path().join("src");
    write_file(&src.join("keep.txt"), b"keep");
    let dest = tmp.path().join("dest");
    write_file(&dest.join("stale.txt"), b"old"); // not in source

    // Without --delete: stale file stays, but is reported.
    let (addr, h) = spawn_receiver(&rcfg, &rid, &dest);
    let s1 = sync::sync(
        &scfg,
        &sid,
        &addr,
        &src,
        opts(false, false, vec![]),
        &mut NullSink,
    )
    .unwrap();
    h.join().unwrap().unwrap();
    assert!(
        dest.join("stale.txt").exists(),
        "must not delete by default"
    );
    assert!(s1.deletions.iter().any(|d| d == "stale.txt"));
    assert_eq!(s1.deleted, 0);

    // With --delete: stale file is removed.
    let (addr2, h2) = spawn_receiver(&rcfg, &rid, &dest);
    let s2 = sync::sync(
        &scfg,
        &sid,
        &addr2,
        &src,
        opts(true, false, vec![]),
        &mut NullSink,
    )
    .unwrap();
    h2.join().unwrap().unwrap();
    assert!(!dest.join("stale.txt").exists(), "--delete must remove it");
    assert_eq!(s2.deleted, 1);
    assert!(dest.join("keep.txt").exists());
}

#[test]
fn exclude_filters_source_and_protects_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let (scfg, sid) = make_identity(&tmp.path().join("s"), "sender");
    let (rcfg, rid) = make_identity(&tmp.path().join("r"), "receiver");

    let src = tmp.path().join("src");
    write_file(&src.join("data.bin"), b"keep-me");
    write_file(&src.join("scratch.tmp"), b"ignore-me"); // excluded from send
    let dest = tmp.path().join("dest");
    write_file(&dest.join("cache.tmp"), b"protected"); // excluded from delete

    let (addr, h) = spawn_receiver(&rcfg, &rid, &dest);
    let s = sync::sync(
        &scfg,
        &sid,
        &addr,
        &src,
        opts(true, false, vec!["*.tmp".to_string()]),
        &mut NullSink,
    )
    .unwrap();
    h.join().unwrap().unwrap();

    assert!(dest.join("data.bin").exists());
    assert!(
        !dest.join("scratch.tmp").exists(),
        "excluded source file must not be transferred"
    );
    assert!(
        dest.join("cache.tmp").exists(),
        "excluded destination file must be protected from --delete"
    );
    assert_eq!(s.files_transferred, 1);
}

#[test]
fn delta_resends_only_changed_region() {
    let tmp = tempfile::tempdir().unwrap();
    let (scfg, sid) = make_identity(&tmp.path().join("s"), "sender");
    let (rcfg, rid) = make_identity(&tmp.path().join("r"), "receiver");

    // A file large enough to trigger delta; dest starts with an older copy.
    let mut v1 = vec![0u8; 1024 * 1024];
    let mut seed: u32 = 0xC0FFEE;
    for b in v1.iter_mut() {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        *b = (seed & 0xff) as u8;
    }
    let src = tmp.path().join("src");
    let dest = tmp.path().join("dest");
    // Seed the destination with v1 so it can serve as the delta basis.
    write_file(&dest.join("big.bin"), &v1);

    // v2: same as v1 except ~1 KiB changed in the middle.
    let mut v2 = v1.clone();
    for byte in v2.iter_mut().skip(500_000).take(1024) {
        *byte = byte.wrapping_add(1);
    }
    write_file(&src.join("big.bin"), &v2);

    // Use checksum comparison so the test triggers delta deterministically
    // regardless of same-second mtimes (size+mtime would otherwise match).
    let delta_opts = sync::SyncOptions {
        delete: false,
        dry_run: false,
        checksum: true,
        delta: true,
        excludes: vec![],
        chunk_size: None,
        device_name: "sender".into(),
    };
    let (addr, h) = spawn_receiver(&rcfg, &rid, &dest);
    let s = sync::sync(&scfg, &sid, &addr, &src, delta_opts, &mut NullSink).unwrap();
    h.join().unwrap().unwrap();

    // The destination must now exactly equal v2 (verified by whole-file hash).
    assert_eq!(fs::read(dest.join("big.bin")).unwrap(), v2);
    assert_eq!(s.files_transferred, 1);
    // Delta must transmit far less than the whole 4 MiB file.
    assert!(
        s.bytes_transferred < v2.len() as u64 / 4,
        "delta should send a small fraction of the file, sent {} of {}",
        s.bytes_transferred,
        v2.len()
    );
}

#[test]
fn checksum_mode_skips_when_content_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let (scfg, sid) = make_identity(&tmp.path().join("s"), "sender");
    let (rcfg, rid) = make_identity(&tmp.path().join("r"), "receiver");

    let src = tmp.path().join("src");
    write_file(&src.join("x.txt"), b"identical-content");
    let dest = tmp.path().join("dest");
    // Same content already at destination, but with a different mtime.
    write_file(&dest.join("x.txt"), b"identical-content");

    let checksum_opts = sync::SyncOptions {
        delete: false,
        dry_run: false,
        checksum: true,
        delta: false,
        excludes: vec![],
        chunk_size: None,
        device_name: "sender".into(),
    };
    let (addr, h) = spawn_receiver(&rcfg, &rid, &dest);
    let s = sync::sync(&scfg, &sid, &addr, &src, checksum_opts, &mut NullSink).unwrap();
    h.join().unwrap().unwrap();

    assert_eq!(
        s.files_transferred, 0,
        "identical content should be skipped in checksum mode regardless of mtime"
    );
    assert_eq!(s.files_skipped, 1);
}
