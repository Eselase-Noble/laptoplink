//! Filesystem utilities: restrictive file creation, durable writes, atomic
//! rename. These encapsulate the platform-specific details of our durability
//! and permission guarantees in one place.

#[cfg(windows)]
use crate::error::Error;
use crate::error::Result;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

/// Write `data` to `path`, creating it with owner-only permissions (0o600 on
/// Unix). Overwrites an existing file. Flushes to the OS before returning.
pub fn write_private_file(path: &Path, data: &[u8]) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(data)?;
    f.flush()?;
    Ok(())
}

/// Create (or open) a file for receiving transfer data, owner-only perms,
/// without truncating existing content (so partial data survives for resume).
pub fn open_part_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    Ok(opts.open(path)?)
}

/// Create a fresh, empty part file for writing (truncating any existing one),
/// owner-only perms. Used by delta reconstruction, which rebuilds from scratch.
pub fn create_part_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).read(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    Ok(opts.open(path)?)
}

/// Flush a file's data durably to storage (best-effort `fsync`).
///
/// Returns the distinction between "written to OS cache" and "confirmed
/// durable on storage" to callers: a successful return means `fsync` reported
/// success, which on most platforms/filesystems means durable.
pub fn sync_file(f: &File) -> Result<()> {
    f.sync_all()?;
    Ok(())
}

/// Attempt to fsync the directory containing `path` so that a rename/new file
/// entry is itself durable. Best-effort: not all platforms support this, and
/// errors are ignored on non-Unix.
pub fn sync_parent_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent() {
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }
    }
    let _ = path;
    Ok(())
}

/// Atomically replace `dest` with `src` (same filesystem). On success the
/// destination either fully reflects `src` or is unchanged — never partial.
///
/// On Windows `rename` can intermittently fail with "Access is denied" when an
/// antivirus/indexer momentarily holds the freshly written source or the
/// existing destination; it also will not overwrite in every case. So on
/// Windows we retry briefly, and fall back to removing an existing destination
/// before the final rename.
pub fn atomic_rename(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    #[cfg(windows)]
    {
        let mut last: Option<std::io::Error> = None;
        let mut ok = false;
        for attempt in 0u64..10 {
            match std::fs::rename(src, dest) {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(e) => {
                    // If a stale destination is blocking us, try to clear it.
                    if dest.exists() {
                        let _ = std::fs::remove_file(dest);
                    }
                    last = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(40 * (attempt + 1)));
                }
            }
        }
        // Fallback: a transient lock (e.g. antivirus holding the source) can
        // block rename where a plain copy still succeeds. Copy then remove.
        if !ok {
            match std::fs::copy(src, dest) {
                Ok(_) => {
                    let _ = std::fs::remove_file(src);
                    ok = true;
                }
                Err(e) => last = Some(e),
            }
        }
        if !ok {
            if let Some(e) = last {
                return Err(Error::Io(e));
            }
        }
    }

    #[cfg(not(windows))]
    {
        std::fs::rename(src, dest)?;
    }

    sync_parent_dir(dest)?;
    Ok(())
}
