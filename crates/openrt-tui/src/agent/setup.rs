//! First-run setup wizard: detect the REAL host (CPU/RAM/disk/kernel via
//! `/proc`), report the provider honestly, recommend models from real
//! authorities only (installed registry bytes, curated catalog rows, and
//! Hugging Face metadata when the network mode allows it), then apply the
//! operator's pick through the existing per-request model switch.
//!
//! Nothing here invents sizes, availability, or downloads: curated rows
//! carry no size (they never had one), HF sizes come from real file metadata,
//! and provider availability comes from real transport probes.

use openrt_registry::{curated_catalog, HfClient, SourceLabel};
use openrt_resources::SystemInfo;

use crate::agent::state::{AgentApp, Overlay};

/// Wizard page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStep {
    /// Detected host hardware.
    System,
    /// Provider identity + honest capability probe.
    Provider,
    /// Recommended models (local first, HF when allowed).
    Models,
    /// Confirm + apply.
    Review,
}

impl SetupStep {
    pub const ALL: [SetupStep; 4] = [
        SetupStep::System,
        SetupStep::Provider,
        SetupStep::Models,
        SetupStep::Review,
    ];

    pub fn title(self) -> &'static str {
        match self {
            SetupStep::System => "SYSTEM",
            SetupStep::Provider => "PROVIDER",
            SetupStep::Models => "MODELS",
            SetupStep::Review => "REVIEW",
        }
    }

    pub fn idx(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    pub fn next(self) -> SetupStep {
        Self::ALL[(self.idx() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> SetupStep {
        Self::ALL[(self.idx() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// One recommended model row. Every field is a real observation:
/// `tag` is a *suggestion* derived from the artifact id (verified live by
/// the provider probe), `size_mb` exists only when a real byte count is
/// known (installed manifest bytes or HF file metadata).
#[derive(Debug, Clone)]
pub struct SetupModel {
    /// Provider tag to apply (`Some`) when it can be suggested, otherwise
    /// the artifact id the operator must map by hand.
    pub tag: Option<String>,
    pub display: String,
    pub origin: &'static str,
    pub detail: String,
    /// Real byte total when known (MB, rounded down).
    pub size_mb: Option<u64>,
    /// `Some(true/false)` only when a real weight total was compared against
    /// real `MemAvailable`; `None` = unknown (never guessed).
    pub fit: Option<bool>,
}

impl SetupModel {
    /// What `apply` will actually set as the model id.
    pub fn apply_id(&self) -> String {
        self.tag.clone().unwrap_or_else(|| self.display.clone())
    }
}

/// Mutable wizard state (held while `Overlay::Setup` is active).
#[derive(Debug, Clone)]
pub struct SetupState {
    pub step: SetupStep,
    pub sel: usize,
    /// Real `(key, value)` rows from [`SystemInfo::collect`].
    pub specs: Vec<(String, String)>,
    /// Real provider rows (info + capabilities + probe).
    pub provider: Vec<(String, String)>,
    pub models: Vec<SetupModel>,
    pub hf_query: String,
    /// HF query line has focus (typing goes to `hf_query`).
    pub hf_typing: bool,
    pub note: String,
}

/// Suggest the provider tag for a registry artifact id.
///
/// Rule (deterministic, no network): strip any `namespace/` prefix, then parse
/// an optional trailing quant suffix (`-q8_0`, `-q4_k_m`, …) followed by a
/// `-<size>` (`8b` / `14b` / `8x7b`) and join with `:` —
/// `curated/llama3.1-8b` → `llama3.1:8b` and
/// `local/chat/Qwen_Qwen3.5-9B-Q8_0` → `qwen3.5:9b-q8_0`. A duplicated
/// family/org token (`Qwen_Qwen3.5`) is collapsed to the name. Ids that do
/// not match (HF-style multi-part paths, versioned stems, a size in the
/// middle) return `None`: the operator types the tag instead of being handed
/// a guess.
pub fn suggest_provider_tag(artifact_id: &str) -> Option<String> {
    let stem = artifact_id.rsplit('/').next().unwrap_or(artifact_id);

    // 1) optional quant suffix: `q8_0`, `q4_k_m`, `q5_k_s`, …
    let mut quant: Option<String> = None;
    let mut rest = stem;
    if let Some((head, tail)) = rest.rsplit_once('-') {
        let tail_l = tail.to_ascii_lowercase();
        let is_quant = tail_l.strip_prefix('q').is_some_and(|rest| {
            !rest.starts_with('_') && rest.chars().next().is_some_and(|c| c.is_ascii_digit())
        }) && tail_l.contains('_')
            && tail_l
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if is_quant {
            quant = Some(tail_l);
            rest = head;
        }
    }

    // 2) trailing size: `8b`, `14b`, `8x7b`.
    let (head, size) = rest.rsplit_once('-')?;
    let size_l = size.to_ascii_lowercase();
    let is_size = {
        let t = size_l.trim_end_matches('b');
        match t.split_once('x') {
            Some((a, b)) => {
                !a.is_empty()
                    && a.chars().all(|c| c.is_ascii_digit())
                    && !b.is_empty()
                    && b.chars().all(|c| c.is_ascii_digit())
            }
            None => !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()),
        }
    };
    if !is_size || head.is_empty() {
        return None;
    }

    // 3) normalize the name: collapse a duplicated family/org token
    //    (`Qwen_Qwen3.5` → `qwen3.5`) and lowercase.
    let normalized = match head.split_once('_') {
        Some((a, b)) if b.to_ascii_lowercase().starts_with(&a.to_ascii_lowercase()) => {
            b.to_ascii_lowercase()
        }
        _ => head.to_ascii_lowercase(),
    };

    match quant {
        Some(q) => Some(format!("{normalized}:{size_l}-{q}")),
        None => Some(format!("{normalized}:{size_l}")),
    }
}

fn mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

impl AgentApp {
    /// (Re)start the wizard: collect the host, probe the provider, build the
    /// local recommendation list. HF rows appear later via `setup_hf_search`.
    pub async fn setup_begin(&mut self) {
        let sys = SystemInfo::collect();
        let specs = vec![
            (
                "host".into(),
                format!("{} {} ({})", sys.os, sys.kernel, sys.arch),
            ),
            ("cpu".into(), {
                let model = sys.cpu.model.clone().unwrap_or_else(|| "unknown".into());
                format!("{} × {model}", sys.cpu.count)
            }),
            (
                "memory".into(),
                format!(
                    "{} MB available / {} MB total",
                    sys.mem.available_kb / 1024,
                    sys.mem.total_kb / 1024
                ),
            ),
            (
                "disk".into(),
                format!(
                    "{} MB free / {} MB total",
                    sys.disk.avail_bytes / 1024 / 1024,
                    sys.disk.total_bytes / 1024 / 1024
                ),
            ),
            ("uptime".into(), format!("{}s", sys.uptime_secs)),
        ];

        let info = self.runtime.provider.info();
        let caps = self.runtime.provider.capabilities();
        let mut provider = vec![
            ("provider".into(), self.runtime.provider.id().to_string()),
            ("display".into(), info.display_name.clone()),
            (
                "default model".into(),
                info.default_model
                    .clone()
                    .unwrap_or_else(|| "(none)".into()),
            ),
            (
                "ctx window".into(),
                caps.context_window
                    .map(|w| w.to_string())
                    .unwrap_or_else(|| "n/a".into()),
            ),
            (
                "tools".into(),
                if caps.supports_tools {
                    "advertised"
                } else {
                    "not advertised"
                }
                .to_string(),
            ),
            (
                "mode".into(),
                self.runtime
                    .modes
                    .current()
                    .await
                    .map(|m| m.mode.as_str().to_string())
                    .unwrap_or_else(|_| "unknown".into()),
            ),
        ];
        // Honest transport probe for the default tag (loopback-gated egress).
        if let Some(tag) = info.default_model.clone().filter(|t| !t.is_empty()) {
            match self.runtime.provider.probe_tool_support(&tag).await {
                Ok(Some(v)) => provider.push((
                    "native tools".into(),
                    format!("{tag} → {}", if v { "yes" } else { "no" }),
                )),
                Ok(None) => provider.push((
                    "native tools".into(),
                    format!("{tag} → unknown (provider cannot tell)"),
                )),
                Err(e) => {
                    provider.push(("native tools".into(), format!("{tag} → probe failed: {e}")))
                }
            }
        }

        let models = self.setup_local_models(&specs).await;
        self.setup = Some(SetupState {
            step: SetupStep::System,
            sel: 0,
            specs,
            provider,
            models,
            hf_query: String::new(),
            hf_typing: false,
            note: "Enter advances · ↑/↓ selects · Esc closes".into(),
        });
        self.overlay = Some(Overlay::Setup);
    }

    /// Local recommendations: provider default + installed cards (real
    /// stored bytes) + curated rows (catalog facts only, no size).
    async fn setup_local_models(&mut self, specs: &[(String, String)]) -> Vec<SetupModel> {
        let avail_mb = specs
            .iter()
            .find(|(k, _)| k == "memory")
            .and_then(|(_, v)| v.split_whitespace().next())
            .and_then(|t| t.parse::<u64>().ok())
            .unwrap_or(0);
        let mut out: Vec<SetupModel> = Vec::new();

        let info = self.runtime.provider.info();
        if let Some(def) = info.default_model.clone().filter(|t| !t.is_empty()) {
            out.push(SetupModel {
                tag: Some(def.clone()),
                display: def,
                origin: "provider",
                detail: "provider default — always available".into(),
                size_mb: None,
                fit: None,
            });
        }

        match self.runtime.model_registry.list().await {
            Ok(cards) => {
                let provider = self.runtime.provider.id().to_string();
                for c in cards.iter().filter(|c| {
                    c.installed_bytes > 0 || c.state == openrt_registry::LifecycleState::Active
                }) {
                    let size = (c.installed_bytes > 0).then(|| mb(c.installed_bytes));
                    let fit = size.map(|s| s + s / 10 <= avail_mb);
                    let detail = match size {
                        Some(s) => format!(
                            "installed · {s} MB stored{}",
                            match fit {
                                Some(true) => " · fits current RAM budget",
                                Some(false) => " · larger than available RAM",
                                None => "",
                            }
                        ),
                        None => "registry card · no stored bytes yet".into(),
                    };
                    // A persisted operator binding is authoritative; the
                    // derived suggestion is only the fallback hint.
                    let bound = self
                        .runtime
                        .model_registry
                        .bound_tag(&c.artifact_id, &provider)
                        .await
                        .ok()
                        .flatten();
                    let (tag, detail) = match bound {
                        Some(b) => (
                            Some(b.tag.clone()),
                            format!("{detail} · bound tag {}", b.tag),
                        ),
                        None => (suggest_provider_tag(&c.artifact_id), detail),
                    };
                    out.push(SetupModel {
                        tag,
                        display: c.display_name.clone(),
                        origin: "installed",
                        detail,
                        size_mb: size,
                        fit,
                    });
                }
            }
            Err(e) => self.status = format!("registry: {e}"),
        }

        for e in curated_catalog() {
            if out.iter().any(|m| m.display == e.display_name) {
                continue;
            }
            out.push(SetupModel {
                tag: suggest_provider_tag(e.artifact_id),
                display: e.display_name.to_string(),
                origin: "curated",
                detail: format!(
                    "{} · {} · catalog row (size not declared; import required)",
                    e.artifact_id, e.notes
                ),
                size_mb: None,
                fit: None,
            });
        }
        out
    }

    /// HF leg: registry search appends remote rows only when the network
    /// mode allows (Sovereign refuses before any socket). For the top hits
    /// we fetch real file metadata to rank against real `MemAvailable`.
    /// Search Hugging Face from the wizard. The network leg (search +
    /// up to six `model_info` size fetches) runs in the background — a slow
    /// hub response never freezes the setup flow.
    pub async fn setup_hf_search(&mut self) {
        let Some(state) = self.setup.as_mut() else {
            return;
        };
        let q = state.hf_query.trim().to_string();
        if q.is_empty() {
            state.note = "type a query first (e.g. llama, qwen)".into();
            return;
        }
        let network_allowed = self
            .runtime
            .modes
            .current()
            .await
            .map(|m| m.mode.network_allowed())
            .unwrap_or(false);
        let avail_mb = state
            .specs
            .iter()
            .find(|(k, _)| k == "memory")
            .and_then(|(_, v)| v.split_whitespace().next())
            .and_then(|t| t.parse::<u64>().ok())
            .unwrap_or(0);
        let rt = self.runtime.clone();
        use crate::agent::state::RefreshData;
        self.spawn_refresh("searching Hugging Face …", async move {
            let client = HfClient::new("", 10);
            let results = rt.model_registry.search(&q, Some(&client)).await;
            let hf_rows: Vec<_> = results
                .iter()
                .filter(|r| matches!(r.source, SourceLabel::HuggingFace))
                .cloned()
                .collect();
            let local_hits = results.len().saturating_sub(hf_rows.len());

            let mut fetched = 0usize;
            let mut new_rows: Vec<SetupModel> = Vec::new();
            for r in hf_rows.iter().take(6) {
                let (size_mb, detail) = match client.model_info(&r.artifact_id).await {
                    Ok(info) => {
                        let bytes: u64 = info
                            .siblings
                            .iter()
                            .filter(|s| s.rfilename.ends_with(".safetensors"))
                            .filter_map(|s| s.size)
                            .sum();
                        if bytes > 0 {
                            (
                                Some(mb(bytes)),
                                format!("HF metadata · {} MB of safetensors", mb(bytes)),
                            )
                        } else {
                            (None, "HF metadata · file sizes not declared".into())
                        }
                    }
                    Err(e) => (None, format!("HF metadata unavailable: {e}")),
                };
                let fit = size_mb.map(|s| s <= avail_mb);
                if size_mb.is_some() {
                    fetched += 1;
                }
                new_rows.push(SetupModel {
                    tag: suggest_provider_tag(&r.artifact_id),
                    display: r.display_name.clone(),
                    origin: "hugging face",
                    detail,
                    size_mb,
                    fit,
                });
            }
            Ok(RefreshData::SetupSearch {
                rows: new_rows,
                fetched,
                local_hits,
                network_allowed,
            })
        });
    }

    /// Apply the selected recommendation through the ONE model switcher.
    pub async fn setup_apply(&mut self) {
        let Some(id) = self
            .setup
            .as_ref()
            .and_then(|s| s.models.get(s.sel))
            .map(|m| m.apply_id())
        else {
            if let Some(s) = self.setup.as_mut() {
                s.note = "no model selected".into();
            }
            return;
        };
        let has_tag = self
            .setup
            .as_ref()
            .and_then(|s| s.models.get(s.sel))
            .is_some_and(|m| m.tag.is_some());
        if !has_tag {
            if let Some(s) = self.setup.as_mut() {
                s.note =
                    "no provider-tag suggestion for this row — pick another or set a tag in MODELS"
                        .into();
            }
            return;
        }
        if let Some(s) = self.setup.as_mut() {
            s.step = SetupStep::Review;
        }
        match &self.switcher {
            Some(sw) => {
                let info = self.runtime.provider.info();
                let is_default = info.default_model.as_deref() == Some(id.as_str());
                if is_default {
                    sw.set_model(None);
                } else {
                    sw.set_model(Some(id.clone()));
                }
                self.model_input = id.clone();
                // Background probe — the key handler never waits on the
                // transport; the note lands with the real label (PART 29).
                self.spawn_probe();
                if let Some(st) = self.setup.as_mut() {
                    st.note = format!("model → {id} · native tools: probing …");
                }
                self.status = format!("setup: model → {id}");
            }
            None => {
                if let Some(st) = self.setup.as_mut() {
                    st.note = "no switchable provider attached — model not applied (honest)".into();
                }
            }
        }
    }

    /// Enter / advance within the wizard.
    pub async fn setup_enter(&mut self) {
        let step = match &self.setup {
            Some(s) => s.step,
            None => return,
        };
        match step {
            SetupStep::System | SetupStep::Provider => {
                if let Some(s) = self.setup.as_mut() {
                    s.step = s.step.next();
                    s.sel = 0;
                    s.hf_typing = false;
                }
            }
            SetupStep::Models => self.setup_apply().await,
            SetupStep::Review => {
                self.overlay = None;
                // Persist completion through the ONE persistence store
                // (ADR-006 namespaced record) so the next startup does not
                // repeat first-run setup; `/setup` still reopens it.
                let model = self.switcher.as_ref().and_then(|sw| sw.selected());
                let rec = serde_json::json!({ "completed": true, "model": model });
                match self
                    .runtime
                    .store
                    .put_record("tui.setup", "state", &rec)
                    .await
                {
                    Ok(()) => self.status = "setup finished — /setup reopens it".into(),
                    Err(e) => {
                        self.status = format!(
                            "setup finished — setup state not persisted ({e}) — /setup reopens it"
                        )
                    }
                }
            }
        }
    }

    pub fn setup_back(&mut self) {
        if let Some(s) = self.setup.as_mut() {
            if s.step == SetupStep::System {
                self.overlay = None;
            } else {
                s.step = s.step.prev();
                s.sel = 0;
                s.hf_typing = false;
            }
        } else {
            self.overlay = None;
        }
    }

    pub fn setup_move(&mut self, delta: i32) {
        if let Some(s) = self.setup.as_mut() {
            let len = match s.step {
                SetupStep::Models => s.models.len(),
                _ => 0,
            };
            if len == 0 {
                return;
            }
            let cur = s.sel as i32;
            s.sel = (cur + delta).clamp(0, len as i32 - 1) as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::suggest_provider_tag;

    #[test]
    fn suggestion_handles_quant_suffixes() {
        assert_eq!(
            suggest_provider_tag("local/chat/Qwen_Qwen3.5-9B-Q8_0").as_deref(),
            Some("qwen3.5:9b-q8_0")
        );
        assert_eq!(
            suggest_provider_tag("local/chat/Qwen_Qwen3.5-9B-Q4_K_M").as_deref(),
            Some("qwen3.5:9b-q4_k_m")
        );
        assert_eq!(
            suggest_provider_tag("curated/llama3.1-8b").as_deref(),
            Some("llama3.1:8b")
        );
        assert_eq!(
            suggest_provider_tag("phi3-14b").as_deref(),
            Some("phi3:14b")
        );
    }

    #[test]
    fn suggestion_stays_honest_when_unparsable() {
        // Size in the middle, trailing role token → never guess.
        assert_eq!(suggest_provider_tag("Qwen2.5-Coder-7B-Instruct"), None);
        // HF-style id with no clean trailing size.
        assert_eq!(suggest_provider_tag("Qwen/Qwen2.5-72B-AWQ"), None);
        // No size at all.
        assert_eq!(suggest_provider_tag("local/chat/embedding"), None);
        // Bare quant without a size: unknown → honest None.
        assert_eq!(suggest_provider_tag("local/chat/mystery-q8_0"), None);
    }
}
