//! Tool registry: in-memory, ordered, name-indexed.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::{Tool, ToolSpec};

/// Registration errors.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RegistryError {
    #[error("tool `{0}` already registered")]
    Duplicate(String),
    #[error("tool name `{0}` is invalid: must be lowercase `a-z0-9._-`")]
    InvalidName(String),
}

/// Default Phase 1 tool-set factory.
impl ToolRegistry {
    /// A registry containing the builtin tools. The `filesystem.read` tool is
    /// registered *unscoped* here (it denies every request, SEC-01); real
    /// runtimes use [`ToolRegistry::with_filesystem_root`] to confine it.
    pub fn builtins() -> Self {
        let mut r = Self::new();
        r.register_custom(crate::builtin::echo::Echo::new());
        r.register_custom(crate::builtin::system_info::SystemInfoTool::new());
        r.register_custom(crate::builtin::filesystem_read::FilesystemRead::new());
        r
    }

    /// The builtin tool set with `filesystem.read` confined to `root`.
    pub fn with_filesystem_root(root: &std::path::Path) -> Self {
        let mut r = Self::builtins();
        let scoped = crate::builtin::filesystem_read::FilesystemRead::scoped(root);
        if r.by_name.contains_key("filesystem.read") {
            r.by_name.insert(
                "filesystem.read".to_string(),
                std::sync::Arc::new(scoped) as std::sync::Arc<dyn Tool>,
            );
        }
        r
    }
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    by_name: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool, replacing nothing (dup → error).
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), RegistryError> {
        let name = tool.spec().name.clone();
        validate_name(&name)?;
        if self.by_name.contains_key(&name) {
            return Err(RegistryError::Duplicate(name));
        }
        self.by_name.insert(name, tool);
        Ok(())
    }

    /// Ergonomic registration for concrete [`Tool`] types.
    pub fn register_custom<T: Tool + 'static>(&mut self, tool: T) {
        let _ = self.register(Arc::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.by_name.get(name).cloned()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// All specs, sorted by name. The shape `openrt model list`/`tool list` print.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.by_name.values().map(|t| t.spec().clone()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.by_name.keys().cloned().collect()
    }

    /// Provider-facing tool definitions (for function calling).
    pub fn provider_tools(&self) -> Vec<serde_json::Value> {
        self.specs()
            .iter()
            .map(ToolSpec::to_provider_tool)
            .collect()
    }
}

fn validate_name(name: &str) -> Result<(), RegistryError> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(RegistryError::InvalidName(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::echo::Echo;

    #[test]
    fn register_and_get() {
        let mut r = ToolRegistry::new();
        r.register_custom(Echo::new());
        assert_eq!(r.len(), 1);
        assert!(r.contains("echo"));
        assert!(r.get("echo").is_some());
        assert_eq!(r.names(), vec!["echo".to_string()]);
    }

    #[test]
    fn duplicate_rejected() {
        let mut r = ToolRegistry::new();
        let t: Arc<dyn Tool> = Arc::new(Echo::new());
        assert!(r.register(t.clone()).is_ok());
        assert_eq!(r.register(t), Err(RegistryError::Duplicate("echo".into())));
    }

    #[test]
    fn invalid_names_rejected() {
        let mut r = ToolRegistry::new();
        let e = Echo::new();
        assert!(r
            .register(Arc::new(crate::builtin::echo::Echo {
                spec: invalid_spec()
            }))
            .is_err());
        drop(e);
    }

    fn invalid_spec() -> crate::ToolSpec {
        let mut s = crate::ToolSpec::new("Echo Tool", "");
        s.name = "Bad Name!".to_string();
        s
    }

    #[test]
    fn builtins_present() {
        let r = ToolRegistry::builtins();
        assert_eq!(r.names(), vec!["echo", "filesystem.read", "system.info"]);
        assert_eq!(r.len(), 3);
    }
}
