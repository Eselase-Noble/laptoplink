//! LaptopLink desktop backend (Tauri v2).
//!
//! Thin command layer over `laptoplink-core`: the GUI calls these commands and
//! receives streamed progress through the `ll://progress` event. All transfer
//! logic lives in the shared engine, exactly as the CLI uses it.

use laptoplink_core::config::Config;
use laptoplink_core::identity::{DeviceIdentity, Fingerprint};
use laptoplink_core::progress::{ProgressEvent, ProgressSink};
use laptoplink_core::{recv, send, sync};
use serde::Serialize;
use std::path::PathBuf;
use std::str::FromStr;
use tauri::{AppHandle, Emitter, Manager, State};

/// Shared application state: resolved config + this device's identity.
struct AppState {
    config: Config,
    identity: DeviceIdentity,
}

/// This device's identity, as shown in the UI.
#[derive(Serialize, Clone)]
struct IdentityInfo {
    name: String,
    fingerprint: String,
    fingerprint_short: String,
}

/// A paired peer, as shown in the UI.
#[derive(Serialize, Clone)]
struct PeerInfo {
    name: String,
    fingerprint: String,
    revoked: bool,
}

/// Result of a completed send, returned to the UI.
#[derive(Serialize, Clone)]
struct SendResult {
    transfer_id: String,
    files: u64,
    bytes_sent: u64,
    bytes_skipped: u64,
}

/// A [`ProgressSink`] that forwards every engine event to the webview as a
/// JSON payload on the `ll://progress` channel.
struct EventSink {
    app: AppHandle,
}

impl ProgressSink for EventSink {
    fn on_event(&mut self, event: ProgressEvent) {
        let payload = match event {
            ProgressEvent::TransferStarted {
                transfer_id,
                total_files,
                total_bytes,
            } => serde_json::json!({
                "kind": "started", "transferId": transfer_id,
                "totalFiles": total_files, "totalBytes": total_bytes
            }),
            ProgressEvent::FileStarted {
                rel_path,
                size,
                resumed_bytes,
                ..
            } => serde_json::json!({
                "kind": "fileStarted", "path": rel_path,
                "size": size, "resumedBytes": resumed_bytes
            }),
            ProgressEvent::Progress { delta_bytes, .. } => {
                serde_json::json!({ "kind": "progress", "deltaBytes": delta_bytes })
            }
            ProgressEvent::FileCompleted { rel_path, .. } => {
                serde_json::json!({ "kind": "fileCompleted", "path": rel_path })
            }
            ProgressEvent::TransferCompleted {
                bytes_transferred,
                bytes_skipped,
                ..
            } => serde_json::json!({
                "kind": "completed",
                "bytesTransferred": bytes_transferred, "bytesSkipped": bytes_skipped
            }),
            ProgressEvent::Note(msg) => serde_json::json!({ "kind": "note", "message": msg }),
        };
        let _ = self.app.emit("ll://progress", payload);
    }
}

#[tauri::command]
fn my_identity(state: State<'_, AppState>) -> IdentityInfo {
    let id = &state.identity;
    IdentityInfo {
        name: id.name.clone(),
        fingerprint: id.fingerprint().full().to_string(),
        fingerprint_short: id.fingerprint().short(),
    }
}

#[tauri::command]
fn list_peers(state: State<'_, AppState>) -> Result<Vec<PeerInfo>, String> {
    let peers = state.config.load_peers().map_err(|e| e.to_string())?;
    Ok(peers
        .list()
        .map(|p| PeerInfo {
            name: p.name.clone(),
            fingerprint: p.fingerprint.clone(),
            revoked: p.revoked,
        })
        .collect())
}

#[tauri::command]
fn pair_peer(state: State<'_, AppState>, fingerprint: String, name: String) -> Result<(), String> {
    let fp = Fingerprint::from_str(&fingerprint).map_err(|e| e.to_string())?;
    let mut peers = state.config.load_peers().map_err(|e| e.to_string())?;
    peers.approve(&fp, &name).map_err(|e| e.to_string())
}

#[tauri::command]
fn revoke_peer(state: State<'_, AppState>, fingerprint: String) -> Result<bool, String> {
    let fp = Fingerprint::from_str(&fingerprint).map_err(|e| e.to_string())?;
    let mut peers = state.config.load_peers().map_err(|e| e.to_string())?;
    peers.revoke(&fp).map_err(|e| e.to_string())
}

/// Result of a sync, returned to the UI.
#[derive(Serialize, Clone)]
struct SyncResultInfo {
    dry_run: bool,
    files_total: u64,
    files_transferred: u64,
    files_skipped: u64,
    bytes_transferred: u64,
    deletions: Vec<String>,
    deleted: u64,
    conflicts: Vec<String>,
}

/// One-way sync a folder to a device. Runs off the UI thread; streams progress
/// on `ll://progress`; resolves with a summary.
#[tauri::command]
async fn run_sync(
    app: AppHandle,
    source: String,
    addr: String,
    delete: bool,
    dry_run: bool,
    delta: bool,
    checksum: bool,
) -> Result<SyncResultInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let target = if addr.contains(':') {
            addr.clone()
        } else {
            format!("{addr}:9031")
        };
        let mut sink = EventSink { app: app.clone() };
        let s = sync::sync(
            &state.config,
            &state.identity,
            &target,
            std::path::Path::new(&source),
            sync::SyncOptions {
                delete,
                dry_run,
                checksum,
                delta,
                excludes: vec![],
                chunk_size: None,
                device_name: state.identity.name.clone(),
            },
            &mut sink,
        )
        .map_err(|e| e.to_string())?;
        Ok(SyncResultInfo {
            dry_run: s.dry_run,
            files_total: s.files_total,
            files_transferred: s.files_transferred,
            files_skipped: s.files_skipped,
            bytes_transferred: s.bytes_transferred,
            deletions: s.deletions,
            deleted: s.deleted,
            conflicts: s.conflicts,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Start the receive server on a background thread. It runs until the app
/// exits. Progress and pairing notes arrive on `ll://progress`.
#[tauri::command]
fn start_receiver(
    app: AppHandle,
    state: State<'_, AppState>,
    dest: String,
    accept_new: bool,
    allow_sender_dest: bool,
) -> Result<String, String> {
    let dest = PathBuf::from(dest);
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    let config = state.config.clone();
    let identity = state.identity.clone();
    let bind = "0.0.0.0:9031".to_string();
    let bind_thread = bind.clone();
    let app2 = app.clone();
    std::thread::spawn(move || {
        let opts = recv::ReceiveOptions {
            dest,
            accept_new,
            device_name: identity.name.clone(),
            once: false,
            allow_sender_dest,
        };
        // Concurrent: the desktop receiver can accept several transfers at once.
        let app3 = app2.clone();
        let make_sink = move || -> Box<dyn laptoplink_core::progress::ProgressSink> {
            Box::new(EventSink { app: app3.clone() })
        };
        if let Err(e) =
            recv::listen_concurrent(&config, &identity, &bind_thread, &opts, make_sink)
        {
            let _ = app2.emit(
                "ll://progress",
                serde_json::json!({ "kind": "note", "message": format!("receiver stopped: {e}") }),
            );
        }
    });
    Ok(bind)
}

/// Send the given files/folders to `addr`. Runs off the UI thread; emits
/// progress on `ll://progress`; resolves with a summary.
#[tauri::command]
async fn send_files(
    app: AppHandle,
    paths: Vec<String>,
    addr: String,
) -> Result<SendResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let target = if addr.contains(':') {
            addr.clone()
        } else {
            format!("{addr}:9031")
        };
        let items = send::expand_sources(
            &paths.iter().map(PathBuf::from).collect::<Vec<_>>(),
        )
        .map_err(|e| e.to_string())?;
        if items.is_empty() {
            return Err("no regular files in the selection".to_string());
        }
        let mut sink = EventSink { app: app.clone() };
        let summary = send::send(
            &state.config,
            &state.identity,
            &target,
            items,
            send::SendOptions {
                transfer_id: None,
                chunk_size: None,
                device_name: state.identity.name.clone(),
                dest: None,
            },
            &mut sink,
        )
        .map_err(|e| e.to_string())?;
        Ok(SendResult {
            transfer_id: summary.transfer_id,
            files: summary.files,
            bytes_sent: summary.bytes_sent,
            bytes_skipped: summary.bytes_skipped,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Tauri entry point.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let config = Config::discover().expect("resolve config dir");
    let identity = config
        .load_or_create_identity("laptoplink-desktop")
        .expect("load or create identity");

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState { config, identity })
        .invoke_handler(tauri::generate_handler![
            my_identity,
            list_peers,
            pair_peer,
            revoke_peer,
            start_receiver,
            send_files,
            run_sync
        ])
        .run(tauri::generate_context!())
        .expect("error while running LaptopLink");
}
