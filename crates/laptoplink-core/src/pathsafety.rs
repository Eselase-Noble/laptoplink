//! Destination-path validation.
//!
//! Every relative path received from a peer is untrusted. Before a received
//! file is written we must guarantee the final location stays *inside* the
//! transfer root. This module rejects absolute paths, parent-directory
//! traversal (`..`), Windows drive/UNC prefixes, and empty/degenerate paths,
//! then returns a normalized path joined under the root.
//!
//! Symlink defenses (not following untrusted links in the destination tree)
//! are handled at write time in the receiver, since they require inspecting
//! the on-disk state, not just the string.

use crate::error::{Error, Result};
use std::path::{Component, Path, PathBuf};

/// Validate an untrusted relative path and join it safely under `root`.
///
/// Returns the absolute destination path on success, or [`Error::UnsafePath`].
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() {
        return Err(Error::UnsafePath("empty path".into()));
    }
    // Reject NUL and other control characters outright.
    if rel.bytes().any(|b| b == 0) {
        return Err(Error::UnsafePath("path contains NUL byte".into()));
    }

    let rel_path = Path::new(rel);

    // Absolute paths and Windows prefixes (C:\, \\server\share) are rejected.
    let mut normalized = PathBuf::new();
    for comp in rel_path.components() {
        match comp {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {} // "." is harmless; skip it.
            Component::ParentDir => {
                return Err(Error::UnsafePath(format!(
                    "'..' component not allowed in '{rel}'"
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::UnsafePath(format!(
                    "absolute path not allowed: '{rel}'"
                )));
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(Error::UnsafePath(format!(
            "path resolves to nothing: '{rel}'"
        )));
    }

    let joined = root.join(&normalized);

    // Defense in depth: the lexical result must still be prefixed by root.
    if !joined.starts_with(root) {
        return Err(Error::UnsafePath(format!("path escapes root: '{rel}'")));
    }

    Ok(joined)
}

/// Normalize a relative path string into forward-slash form for the wire and
/// for manifest keys, so the same logical file matches across platforms.
pub fn to_wire_rel(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(p) => Some(p.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_absolute() {
        let root = Path::new("/tmp/dest");
        assert!(safe_join(root, "../etc/passwd").is_err());
        assert!(safe_join(root, "a/../../b").is_err());
        assert!(safe_join(root, "/etc/passwd").is_err());
        assert!(safe_join(root, "").is_err());
    }

    #[test]
    fn accepts_normal_nested() {
        let root = Path::new("/tmp/dest");
        let p = safe_join(root, "a/b/c.txt").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/dest/a/b/c.txt"));
        // "." components are stripped.
        let p = safe_join(root, "./a/./b.txt").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/dest/a/b.txt"));
    }
}
