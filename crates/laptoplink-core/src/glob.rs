//! A minimal, dependency-free glob matcher for sync include/exclude patterns.
//!
//! Supported wildcards, matched against the full forward-slash relative path:
//! * `*` matches any run of characters, **including** `/` (so a single `*`
//!   crosses directory boundaries — simpler than rsync, and documented);
//! * `?` matches exactly one character.
//!
//! A pattern with no wildcards matches if it equals the path, is a path
//! prefix directory (e.g. `node_modules` matches `node_modules/x/y`), or
//! equals the final path component (e.g. `.DS_Store`). These rules cover the
//! common "exclude this dir / this filename / this glob" cases.

/// Does `path` match the glob `pattern`?
pub fn matches(pattern: &str, path: &str) -> bool {
    if !pattern.contains('*') && !pattern.contains('?') {
        // Literal pattern: exact, directory-prefix, or basename match.
        if pattern == path {
            return true;
        }
        if path.starts_with(pattern) && path.as_bytes().get(pattern.len()) == Some(&b'/') {
            return true;
        }
        if let Some(base) = path.rsplit('/').next() {
            if base == pattern {
                return true;
            }
        }
        return false;
    }
    wildcard(pattern.as_bytes(), path.as_bytes())
}

/// Returns true if `path` matches any pattern in `patterns`.
pub fn any_match(patterns: &[String], path: &str) -> bool {
    patterns.iter().any(|p| matches(p, path))
}

/// Classic iterative wildcard match with `*` (any run) and `?` (one char),
/// O(n) in practice with backtracking on `*`.
fn wildcard(pat: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while t < text.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(sp) = star {
            // Backtrack: let the last `*` consume one more character.
            p = sp + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_and_basename() {
        assert!(matches("node_modules", "node_modules/x/y.js"));
        assert!(matches(".DS_Store", "a/b/.DS_Store"));
        assert!(matches("a/b.txt", "a/b.txt"));
        assert!(!matches("b.txt", "a/bb.txt"));
    }

    #[test]
    fn wildcards() {
        assert!(matches("*.log", "server.log"));
        assert!(matches("*.log", "deep/dir/server.log")); // `*` crosses `/`
        assert!(matches("build/*", "build/out.bin"));
        assert!(matches("tmp?", "tmpX"));
        assert!(!matches("*.log", "notes.txt"));
    }
}
