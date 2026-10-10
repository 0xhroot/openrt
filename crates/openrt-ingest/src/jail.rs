//! Intake path jail (SEC-01). Every ingested path resolves inside an explicit
//! root, follows real canonical identity, and rejects traversal and symlinks
//! that escape the root.

use std::path::{Component, Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum JailError {
    #[error("path `{0}` resolves outside jail root `{1}`")]
    Outside(String, String),
    #[error("path `{0}` is relative to an undefined source")]
    NotAbsolute(String),
    #[error("path access failed: {0}")]
    Io(String),
}

/// A root-constrained directory view. `root` must be absolute and canonical.
#[derive(Debug, Clone)]
pub struct IngestJail {
    root: PathBuf,
}

impl IngestJail {
    pub fn new(root: PathBuf) -> Result<Self, JailError> {
        if !root.is_absolute() {
            return Err(JailError::NotAbsolute(root.display().to_string()));
        }
        let root = root
            .canonicalize()
            .map_err(|e| JailError::Io(format!("{}: {e}", root.display())))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve `path` inside the jail. The input may be absolute or relative;
    /// the returned path is canonical, verified to stay under `root`, and has
    /// no escaping symlink or `..` component.
    pub fn resolve(&self, path: &Path) -> Result<PathBuf, JailError> {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        if !joined.starts_with(&self.root) {
            return Err(JailError::Outside(
                joined.display().to_string(),
                self.root.display().to_string(),
            ));
        }
        // Reject lexical `..` before touching the filesystem.
        if joined
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(JailError::Outside(
                joined.display().to_string(),
                self.root.display().to_string(),
            ));
        }
        let canonical = std::fs::canonicalize(&joined)
            .map_err(|e| JailError::Io(format!("{}: {e}", joined.display())))?;
        if !canonical.starts_with(&self.root) {
            return Err(JailError::Outside(
                canonical.display().to_string(),
                self.root.display().to_string(),
            ));
        }
        Ok(canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_resolves_inside_root() {
        let root = std::env::temp_dir().join(format!(
            "openrt-jail-{}-{}",
            std::process::id(),
            openrt_core::now()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"x").unwrap();
        let jail = IngestJail::new(root.clone()).unwrap();
        let r = jail.resolve(Path::new("a.txt")).unwrap();
        assert!(r.starts_with(&root));
        assert_eq!(r.file_name().unwrap().to_str().unwrap(), "a.txt");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parent_dir_rejected() {
        let root = std::env::temp_dir().join(format!(
            "openrt-jail2-{}-{}",
            std::process::id(),
            openrt_core::now()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let jail = IngestJail::new(root.clone()).unwrap();
        assert!(matches!(
            jail.resolve(Path::new("../etc/passwd")),
            Err(JailError::Outside(_, _))
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn symlink_escape_rejected() {
        let base = std::env::temp_dir().join(format!(
            "openrt-jail3-{}-{}",
            std::process::id(),
            openrt_core::now()
        ));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"s").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("secret"), root.join("link")).unwrap();
        let jail = IngestJail::new(root.clone()).unwrap();
        let r = jail.resolve(Path::new("link"));
        assert!(matches!(r, Err(JailError::Outside(_, _))));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn relative_outside_rejected() {
        let root = std::env::temp_dir().join(format!(
            "openrt-jail4-{}-{}",
            std::process::id(),
            openrt_core::now()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let jail = IngestJail::new(root.clone()).unwrap();
        let abs_other = std::env::temp_dir().join("openrt-not-in-jail.txt");
        assert!(matches!(
            jail.resolve(&abs_other),
            Err(JailError::Outside(_, _))
        ));
        let _ = std::fs::remove_dir_all(root);
    }
}
