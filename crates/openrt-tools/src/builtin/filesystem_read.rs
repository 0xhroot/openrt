//! `filesystem.read` — jail-scoped, size-capped file reads (SEC-01).
//!
//! The tool refuses to run without a configured allowed root: an unscoped
//! instance (as built by the default runtime when `runtime.workspace_dir` is
//! unset) denies every request. When a root is configured, every path is
//! resolved against it by [`openrt_permissions::path_scope`] before the file
//! is touched, so traversal, symlink escape and absolute-outside-root reads
//! are rejected in the *tool layer*, not just by policy.

use std::path::{Path, PathBuf};

use openrt_permissions::{check_path_with_root, ScopeRoot};
use serde_json::{json, Value};

use crate::{Tool, ToolError, ToolOutput, ToolSpec};

/// Default cap on bytes read per invocation (guards against huge files).
pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024;

pub struct FilesystemRead {
    pub spec: ToolSpec,
    /// Canonicalized allowed root. `None` = deny-all (fail closed).
    pub root: Option<ScopeRoot>,
}

impl FilesystemRead {
    /// Deny-all instance: keeps the tool registered/introspectable but every
    /// invocation fails with a [`ToolError::ScopeViolation`].
    pub fn new() -> Self {
        let spec = ToolSpec::new(
            "filesystem.read",
            "Reads the content of a file at an absolute path inside the configured workspace root. The read FAILS if the file is larger than `max_bytes` (default 65536): it never returns a partial file, so raise `max_bytes` above the file size for large files, or use `terminal.run` with `head`/`sed` to inspect very large files.",
        )
        .with_caps(&["filesystem.read.all"])
        .with_schema(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "absolute path to read" },
                "max_bytes": { "type": "integer", "minimum": 1, "description": "size limit in bytes (default 65536); the call errors if the file is bigger than this, so raise it to read large files" }
            },
            "required": ["path"]
        }));
        Self { spec, root: None }
    }

    /// Scoped instance confined to `root` (canonicalized at construction).
    /// If `root` cannot be canonicalized the instance fails closed (deny-all)
    /// rather than silently degrading to unscoped reads.
    pub fn scoped(root: &Path) -> Self {
        let mut t = Self::new();
        t.root = ScopeRoot::new(root).ok();
        t
    }

    fn scope_error(&self, reason: impl Into<String>) -> ToolError {
        ToolError::ScopeViolation {
            tool: "filesystem.read".to_string(),
            reason: reason.into(),
        }
    }
}

impl Default for FilesystemRead {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for FilesystemRead {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let path = args.get("path").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidArguments("filesystem.read".into(), "missing `path` string".into())
        })?;
        // Models routinely send numbers as JSON strings ("65536"); coerce so an
        // explicit max_bytes is never silently dropped back to the default.
        let max_bytes = args
            .get("max_bytes")
            .and_then(|v| match v {
                Value::Number(n) => n.as_u64(),
                Value::String(s) => s.trim().parse::<u64>().ok(),
                _ => None,
            })
            .unwrap_or(DEFAULT_MAX_BYTES)
            .max(1);

        // SEC-01: the jail. No root ⇒ deny by default.
        let root = self.root.as_ref().ok_or_else(|| {
            self.scope_error(
                "no workspace root configured (set runtime.workspace_dir); filesystem.read is denied by default",
            )
        })?;
        let requested = PathBuf::from(path);
        let resolved = check_path_with_root(&requested, root.as_path())
            .map_err(|e| self.scope_error(e.to_string()))?;

        let data = std::fs::read(&resolved).map_err(|e| {
            ToolError::Internal("filesystem.read".into(), format!("{resolved:?}: {e}"))
        })?;
        if data.len() as u64 > max_bytes {
            return Err(ToolError::Internal(
                "filesystem.read".into(),
                format!("file exceeds {max_bytes} byte cap"),
            ));
        }
        let text = String::from_utf8_lossy(&data).into_owned();
        Ok(ToolOutput::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("openrt-fsread-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tmp_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let f = dir.join(name);
        let mut h = std::fs::File::create(&f).unwrap();
        h.write_all(content).unwrap();
        f
    }

    #[tokio::test]
    async fn unscoped_instance_denies_everything() {
        let t = FilesystemRead::new();
        let err = t.run(json!({ "path": "/etc/hostname" })).await.unwrap_err();
        assert!(matches!(err, ToolError::ScopeViolation { .. }));
    }

    /// The tool errors (never truncates) on oversized files, so the
    /// description must say so and tell the model to raise `max_bytes`.
    #[test]
    fn spec_description_explains_oversized_file_error() {
        let d = &FilesystemRead::new().spec.description;
        assert!(d.contains("FAILS if the file is larger"), "{d}");
        assert!(d.contains("raise `max_bytes`"), "{d}");
    }

    #[tokio::test]
    async fn reads_text_inside_root() {
        let dir = tmp_root("in");
        let f = tmp_file(&dir, "reads.txt", b"hello tool");
        let t = FilesystemRead::scoped(&dir);
        let out = t
            .run(json!({ "path": f.display().to_string() }))
            .await
            .unwrap();
        assert_eq!(out.render(), "hello tool");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn enforces_cap() {
        let dir = tmp_root("cap");
        let f = tmp_file(&dir, "cap.txt", b"0123456789abcdef");
        let t = FilesystemRead::scoped(&dir);
        let err = t
            .run(json!({ "path": f.display().to_string(), "max_bytes": 8 }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Internal(_, _)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Models send integers as JSON strings; the explicit cap must still be
    /// honoured instead of silently falling back to the default.
    #[tokio::test]
    async fn string_max_bytes_is_coerced() {
        let dir = tmp_root("strcap");
        let f = tmp_file(&dir, "big.txt", &[b'x'; 4096]);
        let t = FilesystemRead::scoped(&dir);

        let err = t
            .run(json!({ "path": f.display().to_string(), "max_bytes": "1024" }))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Internal(_, _)),
            "string cap must be enforced, got {err:?}"
        );

        let out = t
            .run(json!({ "path": f.display().to_string(), "max_bytes": "8192" }))
            .await
            .unwrap();
        assert_eq!(out.render().len(), 4096);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn outside_root_denied() {
        let dir = tmp_root("out");
        let t = FilesystemRead::scoped(&dir);
        let err = t.run(json!({ "path": "/etc/hostname" })).await.unwrap_err();
        assert!(matches!(err, ToolError::ScopeViolation { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn traversal_denied() {
        let dir = tmp_root("trav");
        let t = FilesystemRead::scoped(&dir);
        let err = t
            .run(json!({ "path": format!("{}/../../etc/shadow", dir.display()) }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ScopeViolation { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_denied() {
        let dir = tmp_root("sym");
        let outside =
            std::env::temp_dir().join(format!("openrt-fsread-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        tmp_file(&outside, "secret.txt", b"secret");
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        let t = FilesystemRead::scoped(&dir);
        let err = t
            .run(json!({ "path": format!("{}/link/secret.txt", dir.display()) }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ScopeViolation { .. }));
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn denied_location_denied() {
        let dir = tmp_root("ssh");
        std::fs::create_dir_all(dir.join(".ssh")).unwrap();
        let f = tmp_file(&dir, ".ssh/id_ed25519", b"key");
        let t = FilesystemRead::scoped(&dir);
        let err = t
            .run(json!({ "path": f.display().to_string() }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ScopeViolation { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
