//! M10.7 ECC: the ONE hook-firing implementation shared by the runtime
//! lifecycle paths and the gate (`ToolCall`). Permission-gated hooks consult
//! the SAME `Policy` the gate enforces; a denied permission fails closed
//! (the hook errors — never silently skipped-and-ignored by callers).

use std::sync::Arc;

use openrt_permissions::Policy;
use openrt_persistence::PersistenceStore;
use openrt_workspace_assets::{AssetStore, HookOutcome, HookTarget};

use crate::Error;

/// Shared hook layer: durable store + the gate's policy. Constructed once by
/// [`crate::RuntimeBuilder`] and cloned into the gate and every lifecycle
/// fire point (one implementation — no second hook path).
#[derive(Clone)]
pub struct HookLayer {
    store: Arc<dyn PersistenceStore>,
    policy: Arc<Policy>,
}

impl HookLayer {
    pub fn new(store: Arc<dyn PersistenceStore>, policy: Arc<Policy>) -> Self {
        Self { store, policy }
    }

    /// Permission check for a hook's `requires_permission`, evaluated against
    /// the SAME `Policy` the gate enforces (no decorative flag).
    pub fn permission_granted(&self, capability: &str) -> bool {
        use openrt_permissions::{Capability, PermissionRequest, PolicyEvaluator};
        let req = PermissionRequest::capability(
            "hook.fire",
            Capability::from_str_unchecked(capability),
            None,
        );
        PolicyEvaluator::new(&self.policy)
            .evaluate(&req)
            .allows_execution()
    }

    /// Fire every *active* hook registered for `target`. Returns the outcomes
    /// of hooks that fired. A store with no `assets.meta` yet has no hooks
    /// (fresh runtime — equivalent to the runtime's default-asset view).
    pub async fn fire(&self, target: HookTarget) -> Result<Vec<HookOutcome>, Error> {
        let assets = match AssetStore::load(&*self.store).await {
            Ok(s) => s,
            Err(openrt_workspace_assets::AssetError::Store(msg))
                if msg.contains("missing assets.meta") =>
            {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };
        let ids = assets.active_hooks_for(target);
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for id in ids {
            let requires = assets.get_hook(&id)?.requires_permission.clone();
            let granted = match &requires {
                None => true,
                Some(cap) => self.permission_granted(cap),
            };
            out.push(assets.run_hook(&id, granted)?);
        }
        Ok(out)
    }
}
