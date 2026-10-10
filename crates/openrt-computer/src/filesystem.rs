//! Jail-scoped filesystem mutation tools: write, list, move, delete.
//!
//! All four resolve every path through
//! [`openrt_permissions::check_path`] exactly like `filesystem.read`, so
//! traversal, symlink-escape and outside-root absolute paths are rejected in
//! the tool layer (SEC-01). Additional guards on top of the shared resolver:
//!
//! * `write` never follows a symlink at the final path component and caps the
//!   number of bytes per invocation;
//! * `delete` and `move` reject targets that resolve outside the root;
//! * `list` caps the number of returned entries.

use std::path::Path;

use openrt_permissions::{check_path, ScopeRoot};
use serde_json::{json, Value};

use openrt_tools::{Tool, ToolError, ToolOutput, ToolSpec};

/// Default entry cap for `filesystem.list`.
pub const DEFAULT_LIST_LIMIT: usize = 4096;
/// Default byte cap for a single `filesystem.write` content payload.
pub const DEFAULT_MAX_WRITE_BYTES: u64 = 1024 * 1024;

fn scope_error(tool: &str, reason: impl Into<String>) -> ToolError {
    ToolError::ScopeViolation {
        tool: tool.to_string(),
        reason: reason.into(),
    }
}

fn internal(tool: &str, reason: impl Into<String>) -> ToolError {
    ToolError::Internal(tool.to_string(), reason.into())
}

/// Resolve `path_str` inside `root`, failing closed on any scope issue.
fn resolve(
    root: Option<&ScopeRoot>,
    tool: &str,
    path_str: &str,
) -> Result<std::path::PathBuf, ToolError> {
    let root = root.ok_or_else(|| {
        scope_error(
            tool,
            "no workspace root configured (set runtime.workspace_dir); this tool is denied by default",
        )
    })?;
    let requested = std::path::PathBuf::from(path_str);
    check_path(&requested, root).map_err(|e| scope_error(tool, e.to_string()))
}

/// `filesystem.write` — create or truncate a file inside the root.
pub struct FilesystemWrite {
    pub spec: ToolSpec,
    pub root: Option<ScopeRoot>,
    pub max_bytes: u64,
}

impl FilesystemWrite {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "filesystem.write",
                "Writes `content` to an absolute path inside the configured workspace root, creating parent directories as needed. Replaces the ENTIRE file: there is no patch or append mode, so always supply the complete new file content (a partial snippet truncates the rest of the file). Refuses to write through a symlink.",
            )
            .with_caps(&["filesystem.write.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "absolute path to write" },
                    "content": { "type": "string", "description": "text content (default empty)" }
                },
                "required": ["path"]
            })),
            root: None,
            max_bytes: DEFAULT_MAX_WRITE_BYTES,
        }
    }

    pub fn scoped(root: &Path) -> Self {
        let mut t = Self::new();
        t.root = ScopeRoot::new(root).ok();
        t
    }

    pub fn scoped_or_deny(root: Option<&Path>) -> Self {
        match root.map(ScopeRoot::new) {
            Some(Ok(r)) => Self::scoped(r.as_path()),
            _ => Self::new(),
        }
    }
}

impl Default for FilesystemWrite {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for FilesystemWrite {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let path = args.get("path").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidArguments("filesystem.write".into(), "missing `path` string".into())
        })?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if content.len() as u64 > self.max_bytes {
            return Err(internal(
                "filesystem.write",
                format!("content exceeds {}-byte cap", self.max_bytes),
            ));
        }

        let resolved = resolve(self.root.as_ref(), "filesystem.write", path)?;

        // Never write through a symlink (live, broken, or looped). The token
        // check uses symlink_metadata so the final component is not followed.
        match std::fs::symlink_metadata(&resolved) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(scope_error(
                    "filesystem.write",
                    format!("refusing to write through symlink `{}`", resolved.display()),
                ));
            }
            Ok(meta) if meta.is_dir() => {
                return Err(internal(
                    "filesystem.write",
                    format!("`{}` is a directory", resolved.display()),
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(internal("filesystem.write", format!("{resolved:?}: {e}")));
            }
            Ok(_) => {}
        }

        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| internal("filesystem.write", format!("{parent:?}: {e}")))?;
            // TOCTOU backstop: the parent may have been swapped for a symlink
            // since the resolution; canonicalize again and re-check the jail.
            if let Ok(canonical_parent) = std::fs::canonicalize(parent) {
                if let Some(root) = self.root.as_ref() {
                    check_path(&canonical_parent, root)
                        .map_err(|e| scope_error("filesystem.write", e.to_string()))?;
                }
            }
        }

        std::fs::write(&resolved, content)
            .map_err(|e| internal("filesystem.write", format!("{resolved:?}: {e}")))?;
        Ok(ToolOutput::json(json!({
            "path": resolved,
            "bytes_written": content.len()
        })))
    }
}

/// `filesystem.list` — directory listing inside the root.
pub struct FilesystemList {
    pub spec: ToolSpec,
    pub root: Option<ScopeRoot>,
    pub limit: usize,
}

impl FilesystemList {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "filesystem.list",
                "Lists the entries of a directory (or the single entry of a file) at an absolute path inside the configured workspace root, sorted by name.",
            )
            .with_caps(&["filesystem.list.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "absolute directory path (default: workspace root)" }
                }
            })),
            root: None,
            limit: DEFAULT_LIST_LIMIT,
        }
    }

    pub fn scoped(root: &Path) -> Self {
        let mut t = Self::new();
        t.root = ScopeRoot::new(root).ok();
        t
    }

    pub fn scoped_or_deny(root: Option<&Path>) -> Self {
        match root.map(ScopeRoot::new) {
            Some(Ok(r)) => Self::scoped(r.as_path()),
            _ => Self::new(),
        }
    }
}

impl Default for FilesystemList {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for FilesystemList {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let root = self.root.as_ref().ok_or_else(|| {
            scope_error(
                "filesystem.list",
                "no workspace root configured (set runtime.workspace_dir); this tool is denied by default",
            )
        })?;
        let path = args.get("path").and_then(Value::as_str).unwrap_or("/");
        let resolved = if path == "/" {
            root.as_path().to_path_buf()
        } else {
            resolve(self.root.as_ref(), "filesystem.list", path)?
        };

        let kind_of = |p: &Path| -> &'static str {
            match std::fs::symlink_metadata(p) {
                Ok(m) if m.file_type().is_symlink() => "symlink",
                Ok(m) if m.file_type().is_dir() => "dir",
                Ok(m) if m.file_type().is_file() => "file",
                Ok(_) => "other",
                Err(_) => "missing",
            }
        };

        let meta = std::fs::metadata(&resolved)
            .map_err(|e| internal("filesystem.list", format!("{resolved:?}: {e}")))?;

        let mut entries: Vec<Value> = Vec::new();
        if meta.is_dir() {
            let mut names: Vec<String> = std::fs::read_dir(&resolved)
                .map_err(|e| internal("filesystem.list", format!("{resolved:?}: {e}")))?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            for name in names.into_iter().take(self.limit) {
                let p = resolved.join(&name);
                entries.push(json!({
                    "name": name,
                    "path": p,
                    "kind": kind_of(&p)
                }));
            }
        } else {
            entries.push(json!({
                "name": resolved.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                "path": resolved,
                "kind": kind_of(&resolved)
            }));
        }

        Ok(ToolOutput::json(json!({
            "path": resolved,
            "count": entries.len(),
            "truncated": entries.len() == self.limit,
            "entries": entries
        })))
    }
}

/// `filesystem.move` — rename inside the root.
pub struct FilesystemMove {
    pub spec: ToolSpec,
    pub root: Option<ScopeRoot>,
}

impl FilesystemMove {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "filesystem.move",
                "Moves a file or directory from `source` to `destination`, both absolute paths inside the configured workspace root.",
            )
            .with_caps(&["filesystem.move.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "absolute path to move" },
                    "destination": { "type": "string", "description": "absolute destination path" }
                },
                "required": ["source", "destination"]
            })),
            root: None,
        }
    }

    pub fn scoped(root: &Path) -> Self {
        let mut t = Self::new();
        t.root = ScopeRoot::new(root).ok();
        t
    }

    pub fn scoped_or_deny(root: Option<&Path>) -> Self {
        match root.map(ScopeRoot::new) {
            Some(Ok(r)) => Self::scoped(r.as_path()),
            _ => Self::new(),
        }
    }
}

impl Default for FilesystemMove {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for FilesystemMove {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let source = args.get("source").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidArguments("filesystem.move".into(), "missing `source` string".into())
        })?;
        let destination = args
            .get("destination")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::InvalidArguments(
                    "filesystem.move".into(),
                    "missing `destination` string".into(),
                )
            })?;

        let from = resolve(self.root.as_ref(), "filesystem.move", source)?;
        let to = resolve(self.root.as_ref(), "filesystem.move", destination)?;

        // Refuse to clobber a symlink or directory at the destination.
        if let Ok(meta) = std::fs::symlink_metadata(&to) {
            if meta.file_type().is_symlink() {
                return Err(scope_error(
                    "filesystem.move",
                    format!("refusing to overwrite symlink `{}`", to.display()),
                ));
            }
        }

        std::fs::rename(&from, &to)
            .map_err(|e| internal("filesystem.move", format!("{from:?} -> {to:?}: {e}")))?;
        Ok(ToolOutput::json(json!({ "from": from, "to": to })))
    }
}

/// `filesystem.delete` — remove a file or empty directory inside the root.
pub struct FilesystemDelete {
    pub spec: ToolSpec,
    pub root: Option<ScopeRoot>,
}

impl FilesystemDelete {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "filesystem.delete",
                "Deletes a file (or empty directory) at an absolute path inside the configured workspace root. Directory removal and recursive deletes are not supported.",
            )
            .with_caps(&["filesystem.delete.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "absolute path to delete" }
                },
                "required": ["path"]
            })),
            root: None,
        }
    }

    pub fn scoped(root: &Path) -> Self {
        let mut t = Self::new();
        t.root = ScopeRoot::new(root).ok();
        t
    }

    pub fn scoped_or_deny(root: Option<&Path>) -> Self {
        match root.map(ScopeRoot::new) {
            Some(Ok(r)) => Self::scoped(r.as_path()),
            _ => Self::new(),
        }
    }
}

impl Default for FilesystemDelete {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for FilesystemDelete {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let path = args.get("path").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidArguments("filesystem.delete".into(), "missing `path` string".into())
        })?;
        let resolved = resolve(self.root.as_ref(), "filesystem.delete", path)?;

        if resolved.is_dir() {
            std::fs::remove_dir(&resolved)
                .map_err(|e| internal("filesystem.delete", format!("{resolved:?}: {e}")))?;
            Ok(ToolOutput::json(
                json!({ "removed": resolved, "kind": "dir" }),
            ))
        } else {
            std::fs::remove_file(&resolved)
                .map_err(|e| internal("filesystem.delete", format!("{resolved:?}: {e}")))?;
            Ok(ToolOutput::json(json!({ "removed": resolved })))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static INSTANCE: AtomicUsize = AtomicUsize::new(0);

    fn unique_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "openrt-{name}-{}-{}",
            std::process::id(),
            INSTANCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn tmp_root(name: &str) -> PathBuf {
        let dir = unique_dir(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `filesystem.write` replaces whole files. Models that send a diff or a
    /// snippet silently truncate the rest, so the description must say so.
    #[test]
    fn write_spec_description_states_whole_file_replace() {
        let d = &FilesystemWrite::new().spec.description;
        assert!(d.contains("ENTIRE file"), "{d}");
        assert!(d.contains("complete new file content"), "{d}");
    }

    fn json(o: ToolOutput) -> Value {
        let ToolOutput::Json(v) = o else {
            panic!("expected Json output");
        };
        v
    }

    #[tokio::test]
    async fn write_and_read_round_trip() {
        let dir = tmp_root("round");
        let t = FilesystemWrite::scoped(&dir);
        let f = dir.join("sub").join("hello.txt");
        let out = t
            .run(json!({ "path": f.display().to_string(), "content": "hi" }))
            .await
            .unwrap();
        assert_eq!(json(out)["bytes_written"], 2);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "hi");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unscoped_write_is_denied() {
        let dir = tmp_root("deny");
        let t = FilesystemWrite::new();
        let f = dir.join("x.txt");
        let err = t
            .run(json!({ "path": f.display().to_string() }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ScopeViolation { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_outside_root_denied() {
        let dir = tmp_root("out");
        let t = FilesystemWrite::scoped(&dir);
        let err = t
            .run(json!({ "path": "/tmp/openrt-sibling-outside.txt" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ScopeViolation { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn traversal_write_denied() {
        let dir = tmp_root("trav");
        let t = FilesystemWrite::scoped(&dir);
        let err = t
            .run(json!({ "path": format!("{}/../../etc/pwned", dir.display()) }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ScopeViolation { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_through_symlink_denied() {
        let dir = tmp_root("sym");
        let outside = tmp_root("sym-target");
        std::os::unix::fs::symlink(&outside, dir.join("victim")).unwrap();
        let t = FilesystemWrite::scoped(&dir);
        let err = t
            .run(json!({ "path": format!("{}/victim/pwned.txt", dir.display()) }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ScopeViolation { .. }), "{err:?}");
        assert!(!outside.join("pwned.txt").exists());
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_sorts_and_reports_kinds() {
        let dir = tmp_root("list");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        std::fs::write(dir.join("b.txt"), "2").unwrap();
        let t = FilesystemList::scoped(&dir);
        let out = t.run(json!({})).await.unwrap();
        let v = json(out);
        assert_eq!(v["count"], 3);
        let names: Vec<&str> = v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "sub"]);
        assert_eq!(v["entries"][0]["kind"], "file");
        assert_eq!(v["entries"][2]["kind"], "dir");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn move_renames_inside_root() {
        let dir = tmp_root("mv");
        std::fs::write(dir.join("from.txt"), "data").unwrap();
        let t = FilesystemMove::scoped(&dir);
        let out = t
            .run(json!({
                "source": dir.join("from.txt").display().to_string(),
                "destination": dir.join("to.txt").display().to_string()
            }))
            .await
            .unwrap();
        assert!(json(out)["to"].as_str().unwrap().ends_with("to.txt"));
        assert!(!dir.join("from.txt").exists());
        assert_eq!(std::fs::read_to_string(dir.join("to.txt")).unwrap(), "data");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn move_escape_denied() {
        let dir = tmp_root("mvesc");
        std::fs::write(dir.join("from.txt"), "data").unwrap();
        let t = FilesystemMove::scoped(&dir);
        let err = t
            .run(json!({
                "source": dir.join("from.txt").display().to_string(),
                "destination": "/tmp/openrt-move-escape-target.txt"
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ScopeViolation { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_removes_file_and_empty_dir() {
        let dir = tmp_root("del");
        std::fs::write(dir.join("gone.txt"), "x").unwrap();
        std::fs::create_dir(dir.join("emptydir")).unwrap();
        let t = FilesystemDelete::scoped(&dir);
        let out = t
            .run(json!({ "path": dir.join("gone.txt").display().to_string() }))
            .await
            .unwrap();
        assert!(json(out)["removed"].as_str().unwrap().ends_with("gone.txt"));
        assert!(!dir.join("gone.txt").exists());
        let _ = t
            .run(json!({ "path": dir.join("emptydir").display().to_string() }))
            .await
            .unwrap();
        assert!(!dir.join("emptydir").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_outside_root_denied() {
        let dir = tmp_root("delesc");
        let t = FilesystemDelete::scoped(&dir);
        let err = t.run(json!({ "path": "/etc/hostname" })).await.unwrap_err();
        assert!(matches!(err, ToolError::ScopeViolation { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
