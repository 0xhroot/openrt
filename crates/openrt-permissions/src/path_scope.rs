//! Filesystem path jail (SEC-01).
//!
//! Tools that touch the filesystem must confine themselves to an operator-set
//! allowed root. This module is the *pure* enforcement point: it resolves a
//! requested path against an allowed root and returns a path that is provably
//! inside the root, or a structured rejection reason.
//!
//! Rules enforced here are additional to — never a replacement for — the
//! permission policy; the gate still runs first. This is the "enforced in the
//! tool layer" jail boundary required by the roadmap, so a correctly
//! politicied tool cannot accidentally escape its root (SEC-01).

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Why a filesystem path was rejected.
#[derive(Debug, thiserror::Error)]
pub enum PathScopeError {
    #[error("path is not absolute: `{0}`")]
    NotAbsolute(PathBuf),
    #[error("path contains a `..` component: `{0}`")]
    Traversal(PathBuf),
    #[error(
        "allowed root `{allowed}` does not exist (set runtime.workspace_dir to a real directory)"
    )]
    MissingRoot { allowed: PathBuf },
    #[error("`{requested}` is outside the allowed root `{allowed}`")]
    OutsideRoot {
        requested: PathBuf,
        allowed: PathBuf,
    },
    #[error("`{0}` resolves into a denied location (credential/secret store)")]
    DeniedLocation(PathBuf),
    #[error("failed to resolve `{path}`: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Base names that are always off-limits, regardless of the allowed root.
/// These are the known credential/secret-store locations on a typical Unix
/// workstation. The check runs on the *resolved* path components, so a root
/// that contains such a store is still unable to expose it.
pub const DENIED_BASE_NAMES: &[&str] = &[".ssh", ".gnupg"];

/// Canonical form of an allowed root, ready for repeated checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeRoot {
    canonical: PathBuf,
}

impl ScopeRoot {
    /// Canonicalize `root` (must exist). Fails closed: a missing root is an
    /// error, never a downgrade to "unscoped".
    pub fn new(root: &Path) -> Result<Self, PathScopeError> {
        if !root.is_absolute() {
            return Err(PathScopeError::NotAbsolute(root.to_path_buf()));
        }
        match std::fs::canonicalize(root) {
            Ok(canonical) => Ok(Self { canonical }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(PathScopeError::MissingRoot {
                    allowed: root.to_path_buf(),
                })
            }
            Err(e) => Err(PathScopeError::Io {
                path: root.to_path_buf(),
                source: e,
            }),
        }
    }

    pub fn as_path(&self) -> &Path {
        &self.canonical
    }
}

/// Resolve `requested` against the allowed root.
///
/// Guarantees on success: the returned path is absolute, contains no `..` or
/// `.` component, resolves every symlink present on the existing path prefix
/// (and the whole path when the file exists), and lies lexically inside the
/// canonicalized allowed root. Nonexistent file tails are appended after the
/// canonicalized nearest-existing ancestor, so a check for a file that does
/// not exist yet is still correctly confined (needed by filesystem.write).
pub fn check_path(requested: &Path, root: &ScopeRoot) -> Result<PathBuf, PathScopeError> {
    if !requested.is_absolute() {
        return Err(PathScopeError::NotAbsolute(requested.to_path_buf()));
    }
    if requested
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(PathScopeError::Traversal(requested.to_path_buf()));
    }

    let resolved = resolve_without_traversal(requested)?;
    if !resolved.starts_with(root.as_path()) {
        return Err(PathScopeError::OutsideRoot {
            requested: resolved.clone(),
            allowed: root.as_path().to_path_buf(),
        });
    }
    for comp in resolved.components() {
        if let Component::Normal(name) = comp {
            if DENIED_BASE_NAMES.iter().any(|d| name == *d) {
                return Err(PathScopeError::DeniedLocation(resolved.clone()));
            }
        }
    }
    Ok(resolved)
}

/// Canonicalize the nearest existing ancestor of `requested`, then rejoin the
/// remaining components lexically (dropping `.`). The result is absolute and
/// free of `..` by construction; existing files are fully symlink-resolved.
fn resolve_without_traversal(requested: &Path) -> Result<PathBuf, PathScopeError> {
    let mut existing = requested.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    loop {
        if existing.exists() || existing == Path::new("/") {
            break;
        }
        match existing.parent() {
            Some(parent) => {
                if let Some(name) = existing.file_name() {
                    tail.push(name.to_os_string());
                }
                existing = parent.to_path_buf();
            }
            None => break, // unreachable for a validated absolute path
        }
    }
    tail.reverse();

    let canonical = if existing == Path::new("/") {
        existing
    } else {
        std::fs::canonicalize(&existing).map_err(|e| PathScopeError::Io {
            path: requested.to_path_buf(),
            source: e,
        })?
    };

    let mut out = canonical;
    for piece in tail {
        out.push(piece);
    }
    Ok(out)
}

/// Convenience: resolve + check against a plain root path.
pub fn check_path_with_root(requested: &Path, root: &Path) -> Result<PathBuf, PathScopeError> {
    let root = ScopeRoot::new(root)?;
    check_path(requested, &root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("openrt-pathscope-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        dir
    }

    #[test]
    fn absolute_inside_root_is_allowed() {
        let dir = tmp_root("in");
        let root = ScopeRoot::new(&dir).unwrap();
        let f = dir.join("sub").join("file.txt");
        std::fs::write(&f, "x").unwrap();
        assert_eq!(check_path(&f, &root).unwrap(), f);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relative_path_denied() {
        let dir = tmp_root("rel");
        let root = ScopeRoot::new(&dir).unwrap();
        assert!(matches!(
            check_path(Path::new("file.txt"), &root),
            Err(PathScopeError::NotAbsolute(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parent_dir_component_denied() {
        let dir = tmp_root("trav");
        let root = ScopeRoot::new(&dir).unwrap();
        let evil = dir.join("..").join("..").join("etc");
        assert!(matches!(
            check_path(&evil, &root),
            Err(PathScopeError::Traversal(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sibling_escape_denied() {
        let dir = tmp_root("escape");
        let root = ScopeRoot::new(&dir).unwrap();
        let sibling = dir.parent().unwrap().join("outside-openrt").join("x.txt");
        std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        std::fs::write(&sibling, "x").unwrap();
        assert!(matches!(
            check_path(&sibling, &root),
            Err(PathScopeError::OutsideRoot { .. })
        ));
        let _ = std::fs::remove_dir_all(sibling.parent().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nonexistent_inside_root_is_allowed() {
        // The file doesn't exist yet (write path), but its ancestor is inside.
        let dir = tmp_root("nofile");
        let root = ScopeRoot::new(&dir).unwrap();
        let target = dir.join("sub").join("new.txt");
        let resolved = check_path(&target, &root).unwrap();
        assert!(resolved.starts_with(&dir));
        assert!(resolved.ends_with("new.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlink_escape_resolved_and_denied() {
        let dir = tmp_root("sym");
        let root = ScopeRoot::new(&dir).unwrap();
        let outside =
            std::env::temp_dir().join(format!("openrt-pathscope-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
            let via_link = dir.join("link").join("secret.txt");
            assert!(matches!(
                check_path(&via_link, &root),
                Err(PathScopeError::OutsideRoot { .. })
            ));
            let _ = std::fs::remove_file(dir.join("link"));
        }
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn denied_base_names_rejected_even_inside_root() {
        let dir = tmp_root("deny");
        let root = ScopeRoot::new(&dir).unwrap();
        std::fs::create_dir_all(dir.join(".ssh")).unwrap();
        let f = dir.join(".ssh").join("id_ed25519");
        std::fs::write(&f, "key").unwrap();
        assert!(matches!(
            check_path(&f, &root),
            Err(PathScopeError::DeniedLocation(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_root_fails_closed() {
        let root =
            std::env::temp_dir().join(format!("openrt-pathscope-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert!(matches!(
            ScopeRoot::new(&root),
            Err(PathScopeError::MissingRoot { .. })
        ));
    }

    #[test]
    fn non_absolute_root_fails_closed() {
        assert!(matches!(
            ScopeRoot::new(Path::new("relative-root")),
            Err(PathScopeError::NotAbsolute(_))
        ));
    }
}
