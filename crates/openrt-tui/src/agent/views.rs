//! Per-view loaders for Agent Mode. Every loader is a read of the ONE
//! Runtime, its store, the real filesystem, or a read-only `git` subprocess —
//! never UI-invented rows.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openrt_tools::ToolExecutionContext;
use serde_json::json;

use crate::agent::state::{
    AgentApp, FileRow, GitInfo, GpuHw, ModelProbe, RefreshData, SearchHit, TermLine,
};
use crate::app::AppError;

/// Directories that never appear in Files/Search (build/VCS noise).
const SKIP_DIRS: [&str; 9] = [
    ".git",
    "target",
    "node_modules",
    ".cache",
    "dist",
    "build",
    ".venv",
    "__pycache__",
    ".next",
];

/// Weight-file extensions the drop-in scan and the `i` import accept.
/// Anything else is refused with a structured MODEL IMPORT FAILED reason.
pub const SUPPORTED_MODEL_EXTS: [&str; 7] =
    ["safetensors", "gguf", "bin", "pt", "pth", "onnx", "npz"];

/// Category directories every `models/` drop-in root should contain —
/// auto-created on Models view entry and on drop-in scan (create only,
/// never delete).
pub const MODEL_CATEGORY_DIRS: [&str; 12] = [
    "chat",
    "coding",
    "vision",
    "image",
    "video",
    "audio",
    "enhance",
    "embedding",
    "reranker",
    "ocr",
    "multimodal",
    "utility",
];

/// Create `root` plus every missing category directory. Returns the names
/// that were actually created (empty = everything already existed).
/// Existing directories and any file weight content are never touched.
pub fn ensure_model_dirs(root: &Path) -> Result<Vec<String>, String> {
    if let Err(e) = std::fs::create_dir_all(root) {
        return Err(format!("create {}: {e}", root.display()));
    }
    let mut created = Vec::new();
    for d in MODEL_CATEGORY_DIRS {
        let p = root.join(d);
        if !p.is_dir() {
            std::fs::create_dir_all(&p).map_err(|e| format!("create {}: {e}", p.display()))?;
            created.push(d.to_string());
        }
    }
    Ok(created)
}

impl AgentApp {
    // ------------------------------------------------------------------
    // FILES
    // ------------------------------------------------------------------

    pub async fn load_files(&mut self) {
        let root = self.files_root.clone();
        self.spawn_refresh("loading files …", async move {
            let rows = tokio::task::spawn_blocking(move || read_dir_rows(&root))
                .await
                .map_err(|e| e.to_string())?;
            Ok(RefreshData::Files(rows))
        });
    }

    /// Enter on the selected row: descend into a directory, preview a file.
    pub async fn files_enter(&mut self) {
        let Some(row) = self.files.get(self.sel).cloned() else {
            return;
        };
        if row.is_dir {
            self.files_root.push(&row.name);
            self.sel = 0;
            self.load_files().await;
            self.status = format!("{}", self.files_root.display());
        } else {
            self.preview_file(&row.name).await;
        }
    }

    /// Backspace / `-`: leave the current directory.
    pub async fn files_up(&mut self) {
        if self.files_root.pop() {
            self.sel = 0;
            self.load_files().await;
            self.status = format!("{}", self.files_root.display());
        }
    }

    pub async fn preview_file(&mut self, name: &str) {
        let path = self.files_root.join(name);
        self.spawn_refresh("loading preview …", async move {
            let (title, lines) = tokio::task::spawn_blocking(move || read_preview(&path, None))
                .await
                .map_err(|e| e.to_string())??;
            Ok(RefreshData::Preview { title, lines })
        });
    }

    /// `d` on the selected file: the REAL `git diff` for that path.
    pub async fn preview_diff(&mut self) {
        let Some(row) = self.files.get(self.sel).cloned() else {
            return;
        };
        if row.is_dir {
            self.status = "select a file for diff".into();
            return;
        }
        let root = self.files_root.clone();
        let name = row.name.clone();
        self.spawn_refresh("running git diff …", async move {
            let (title, lines) = tokio::task::spawn_blocking(move || git_diff_for(&root, &name))
                .await
                .map_err(|e| e.to_string())??;
            Ok(RefreshData::Preview { title, lines })
        });
    }

    // ------------------------------------------------------------------
    // SEARCH
    // ------------------------------------------------------------------

    /// Commit the input line as the query and walk the project (bounded,
    /// background — the draw loop never waits for the walk).
    pub async fn run_search(&mut self) -> Result<(), AppError> {
        let q = if self.input.trim().is_empty() {
            self.search.clone()
        } else {
            let q = self.input.trim().to_string();
            self.input.clear();
            self.search = q.clone();
            q
        };
        if q.is_empty() {
            self.status = "type a query, then Enter".into();
            return Ok(());
        }
        let root = self.files_root.clone();
        self.spawn_refresh(&format!("searching “{q}” …"), async move {
            let hits = tokio::task::spawn_blocking({
                let root = root.clone();
                let q = q.clone();
                move || search_walk(&root, &q)
            })
            .await
            .map_err(|e| e.to_string())?;
            let status = format!("{} hits for “{q}”", hits.len());
            Ok(RefreshData::Search { hits, status })
        });
        Ok(())
    }

    // ------------------------------------------------------------------
    // GIT
    // ------------------------------------------------------------------

    pub async fn load_git(&mut self) {
        let root = self.files_root.clone();
        self.spawn_refresh("reading git state …", async move {
            let res = tokio::task::spawn_blocking(move || git_snapshot(&root))
                .await
                .map_err(|e| e.to_string())?;
            Ok(RefreshData::Git(res))
        });
    }

    // ------------------------------------------------------------------
    // TERMINAL
    // ------------------------------------------------------------------

    /// Run the input line through the GATED `terminal.run` tool (same policy,
    /// shield, resources, and audit trail as the agent itself). The call is
    /// spawned onto the runtime so a long command never blocks the draw loop
    /// (approvals, refresh, and drawing keep working while it runs).
    pub async fn run_terminal(&mut self) {
        let cmd = self.input.trim().to_string();
        if cmd.is_empty() {
            self.status = "type a command, then Enter".into();
            return;
        }
        if self.term_join.is_some() {
            self.status = "a command is still running — its output arrives when it finishes".into();
            return;
        }
        self.input.clear();
        self.term_log.push(TermLine {
            kind: "in",
            text: cmd.clone(),
        });
        self.term_log.push(TermLine {
            kind: "note",
            text: "… running in the background (the UI stays responsive)".into(),
        });
        let ctx = ToolExecutionContext {
            session_id: self.current_session.map(|s| s.to_string()),
            task_id: None,
            channel: Some("tui.terminal".into()),
        };
        let exec = self.runtime.executor.clone();
        self.term_join = Some(tokio::spawn(async move {
            match exec
                .execute("terminal.run", json!({ "command": cmd }), &ctx)
                .await
            {
                Ok(out) => out
                    .render()
                    .lines()
                    .map(|l| TermLine {
                        kind: "out",
                        text: l.to_string(),
                    })
                    .collect(),
                Err(e) => vec![TermLine {
                    kind: "err",
                    text: e.to_string(),
                }],
            }
        }));
        self.status = "terminal · gated terminal.run running in background".into();
    }

    // ------------------------------------------------------------------
    // ARTIFACTS / MODELS / MCP / EXTENSIONS / SECURITY
    // ------------------------------------------------------------------

    /// Load artifacts through the GATED `artifact.list` tool — the same
    /// authority the agent uses (policy denials surface honestly).
    pub async fn load_artifacts(&mut self) {
        let ctx = ToolExecutionContext {
            session_id: self.current_session.map(|s| s.to_string()),
            task_id: None,
            channel: Some("tui.views".into()),
        };
        let exec = self.runtime.executor.clone();
        self.spawn_refresh("listing artifacts via artifact.list …", async move {
            match exec.execute("artifact.list", json!({}), &ctx).await {
                Ok(out) => {
                    let rendered = out.render();
                    let value = serde_json::from_str(&rendered)
                        .unwrap_or_else(|_| serde_json::Value::String(rendered));
                    Ok(RefreshData::Artifacts(Ok((
                        value,
                        "loaded via artifact.list".to_string(),
                    ))))
                }
                Err(e) => Ok(RefreshData::Artifacts(Err(format!("artifact.list: {e}")))),
            }
        });
    }

    pub async fn refresh_models(&mut self) {
        let rt = self.runtime.clone();
        // Resolve the display/probe tag BEFORE the background hop (pure
        // in-memory reads of the provider info + switcher).
        let fallback = if self.model_input.is_empty() {
            Some(
                self.switcher
                    .as_ref()
                    .and_then(|s| s.selected())
                    .unwrap_or_else(|| {
                        self.runtime
                            .provider
                            .info()
                            .default_model
                            .clone()
                            .unwrap_or_default()
                    }),
            )
        } else {
            None
        };
        let mut tag = self.model_input.trim().to_string();
        if tag == "default" {
            tag = self
                .runtime
                .provider
                .info()
                .default_model
                .unwrap_or_default();
        }
        let known: std::collections::BTreeSet<String> =
            self.model_profiles.keys().cloned().collect();
        self.spawn_refresh("loading model registry …", async move {
            let mut cards = rt.model_registry.list().await.map_err(|e| e.to_string())?;
            let n = cards.len();
            // Grouping order for the `models/<category>` sections.
            cards.sort_by_key(|c| {
                (
                    category_rank(model_category(&c.tags)),
                    c.display_name.to_lowercase(),
                )
            });
            // Honest per-model capability probe (Ollama /api/show via the
            // provider trait) — unknown stays unknown; a transport failure
            // is kept as a note instead of a silent unknown. The probe
            // round-trip itself doubles as the provider-status check.
            let probe = if tag.is_empty() {
                ModelProbe::default()
            } else {
                let mut p = ModelProbe::default();
                match rt.provider.probe_tool_support(&tag).await {
                    Ok(v) => {
                        p.native_tools = v;
                        p.status = Some(Ok(()));
                    }
                    Err(e) => {
                        p.native_tools_err = Some(e.to_string());
                        p.status = Some(Err(e.to_string()));
                    }
                }
                // Vision is a second /api/show call, but the capability is
                // cached per tag — only run it when the transport answered.
                if p.status == Some(Ok(())) {
                    match rt.provider.probe_vision(&tag).await {
                        Ok(v) => p.vision = v,
                        Err(e) => p.vision_err = Some(e.to_string()),
                    }
                }
                p
            };
            // Resource profiles, gathered here because this is already a
            // background task: a header read per artifact, capped so a large
            // registry cannot turn a refresh into a disk sweep. Anything past
            // the cap stays unprofiled and the UI says NOT PROBED.
            let mut profiles = std::collections::BTreeMap::new();
            const PROFILE_BUDGET: usize = 8;
            let mut profiled = 0usize;
            for c in cards
                .iter()
                .filter(|c| c.blob_location.as_deref() == Some("external"))
            {
                if profiled >= PROFILE_BUDGET || known.contains(&c.artifact_id) {
                    continue;
                }
                let Ok(Some(m)) = rt.model_registry.manifest(&c.artifact_id).await else {
                    continue;
                };
                let Some(path) = m.blobs.first().and_then(|b| b.location.path()) else {
                    continue;
                };
                profiled += 1;
                let p = path.to_string();
                let size = c
                    .artifact_bytes
                    .unwrap_or_else(|| m.blobs.iter().map(|b| b.size_bytes).sum());
                let profile =
                    tokio::task::spawn_blocking(
                        move || match openrt_models::artifact::read_metadata(std::path::Path::new(
                            &p,
                        )) {
                            Ok(md) => openrt_models::artifact::ModelResourceProfile::from_metadata(
                                &md, size,
                            ),
                            Err(_) => openrt_models::artifact::ModelResourceProfile {
                                artifact_bytes: size,
                                notes: vec![
                                    "artifact header could not be read; metadata NOT PROBED".into(),
                                ],
                                ..Default::default()
                            },
                        },
                    )
                    .await;
                if let Ok(profile) = profile {
                    profiles.insert(c.artifact_id.clone(), profile);
                }
            }
            Ok(RefreshData::Models {
                cards,
                probe,
                status: format!("{n} registry cards"),
                profiles,
            })
        });
        if let Some(fb) = fallback {
            if self.model_input.is_empty() {
                self.model_input = fb;
            }
        }
    }

    /// Spawn the transport capability probe for the current tag in the
    /// BACKGROUND (single-flight: a newer probe aborts the in-flight one).
    /// `Ok(None)` (provider cannot tell) lands as `None` = "unknown"; a
    /// transport failure lands as a note instead of a silent unknown.
    /// PART 29: the probe is two HTTP round-trips — it must never run
    /// inline in the key handler (a dead provider would freeze the TUI for
    /// the full client timeout).
    pub fn spawn_probe(&mut self) {
        let mut tag = self.model_input.trim().to_string();
        if tag == "default" {
            // "default" is the operator's way of saying "provider default":
            // probe the tag the provider actually resolves to.
            tag = self
                .runtime
                .provider
                .info()
                .default_model
                .unwrap_or_default();
        }
        if tag.is_empty() {
            self.native_tools = None;
            self.native_tools_err = None;
            self.vision = None;
            self.vision_err = None;
            self.provider_status = None;
            self.pending_switch = None;
            return;
        }
        if let Some(old) = self.probe_join.take() {
            old.abort();
        }
        let rt = self.runtime.clone();
        let probed = tag.clone();
        self.probe_join = Some(tokio::spawn(async move {
            let mut p = ModelProbe {
                tag: probed,
                ..Default::default()
            };
            match rt.provider.probe_tool_support(&tag).await {
                Ok(v) => {
                    p.native_tools = v;
                    p.status = Some(Ok(()));
                }
                Err(e) => {
                    p.native_tools_err = Some(e.to_string());
                    p.status = Some(Err(e.to_string()));
                }
            }
            // Vision is a second `/api/show`, but the capability is cached
            // per tag — only run it when the transport answered.
            if p.status == Some(Ok(())) {
                match rt.provider.probe_vision(&tag).await {
                    Ok(v) => p.vision = v,
                    Err(e) => p.vision_err = Some(e.to_string()),
                }
            }
            p
        }));
    }

    /// Ctrl+F model search over the ONE registry (`registry.search`):
    /// installed cards + curated catalog locally, Hugging Face rows when the
    /// network mode allows (honest empty result when offline/disabled).
    /// The search itself runs in the background — an HF round-trip never
    /// blocks the draw/key loop.
    pub async fn run_model_search(&mut self) {
        let q = self.model_query.trim().to_string();
        if q.is_empty() {
            self.status = "search: type a query, then Enter".into();
            return;
        }
        let rt = self.runtime.clone();
        self.spawn_refresh(&format!("searching registry for “{q}” …"), async move {
            let client = openrt_registry::HfClient::new("", 15);
            let results = rt.model_registry.search(&q, Some(&client)).await;
            let hf = results
                .iter()
                .filter(|r| matches!(r.source, openrt_registry::SourceLabel::HuggingFace))
                .count();
            let n = results.len();
            Ok(RefreshData::ModelSearch {
                query: q,
                results,
                status: format!("{n} hits ({hf} from Hugging Face)"),
            })
        });
    }

    /// Fetch real file sizes for the hub rows in the current results
    /// (background, top 8, ONLY after the network gate allows it — the same
    /// `ensure_network_allowed` authority `registry.search` uses).
    pub async fn probe_hf_sizes(&mut self) {
        if self.hf_size_join.is_some() {
            return;
        }
        let mut ids: Vec<String> = self
            .model_results
            .iter()
            .filter(|r| matches!(r.source, openrt_registry::SourceLabel::HuggingFace))
            .take(8)
            .map(|r| r.artifact_id.clone())
            .collect();
        // Marketplace rows (Hub section) get the same real-size probe.
        for row in self.hub_rows.iter().take(8) {
            if !ids.contains(&row.id) {
                ids.push(row.id.clone());
            }
        }
        if ids.is_empty() {
            return;
        }
        if let Err(e) = self.runtime.modes.ensure_network_allowed().await {
            self.status = format!("HF sizes unavailable: {e}");
            return;
        }
        let client = openrt_registry::HfClient::new("", 12);
        self.hf_size_join = Some(tokio::spawn(async move {
            let mut out: Vec<(String, crate::agent::state::HfSize)> = Vec::new();
            for id in ids {
                let info = match client.model_info(&id).await {
                    Ok(i) => i,
                    Err(e) => {
                        out.push((
                            id,
                            crate::agent::state::HfSize {
                                total_bytes: None,
                                gated: false,
                                error: Some(e.to_string()),
                            },
                        ));
                        continue;
                    }
                };
                let total = if info.siblings.iter().any(|s| s.size.is_some()) {
                    Some(info.siblings.iter().filter_map(|s| s.size).sum())
                } else {
                    None
                };
                out.push((
                    id,
                    crate::agent::state::HfSize {
                        total_bytes: total,
                        gated: info.gated,
                        error: None,
                    },
                ));
            }
            out
        }));
    }

    /// Marketplace search over the Hugging Face Hub (the `[Hugging Face]`
    /// section): metadata only, top 10 per page, background. The mode authority
    /// runs BEFORE anything is staged: in a blocked mode no task is spawned,
    /// no socket is opened, and the section says so plainly (`hub not
    /// contacted — <reason>`) — never a "refused" error for a request that
    /// was never attempted. Empty query = the hub's recent listing.
    pub async fn run_hub_search(&mut self) {
        let q = self.model_query.trim().to_string();
        let skip = self.hub_skip;
        self.model_search_active = false;
        if let Err(e) = self.runtime.modes.ensure_network_allowed().await {
            self.hub_query = q;
            self.hub_loaded = true;
            self.hub_status = format!("hub not contacted — {e}");
            self.status = format!("Hugging Face — {}", self.hub_status);
            return;
        }
        let label = if q.is_empty() {
            format!("querying Hugging Face (recent, skip {skip}) …")
        } else {
            format!("querying Hugging Face for “{q}” (skip {skip}) …")
        };
        self.spawn_refresh(&label, async move {
            let client = openrt_registry::HfClient::new("", 15);
            match client.search_page(&q, 10, skip as u32).await {
                Ok(rows) => {
                    let n = rows.len();
                    Ok(RefreshData::HubSearch {
                        query: q,
                        rows,
                        status: format!(
                            "{n} hub row(s) · skip {skip} · metadata only — weights are never fetched"
                        ),
                    })
                }
                Err(e) => Ok(RefreshData::HubSearch {
                    query: q,
                    rows: Vec::new(),
                    status: format!("hub fetch failed: {e}"),
                }),
            }
        });
    }

    /// `[` / `]` in the Hub section: page the hub listing by 10 rows
    /// (skip/limit on the HF API). Never spawns a task under a blocked mode —
    /// `run_hub_search` re-checks the gate first.
    pub async fn hub_page(&mut self, delta: i32) {
        let next = (self.hub_skip as i32 + delta * 10).max(0);
        let next = next.min(4990) as usize;
        if next == self.hub_skip {
            self.status = if delta < 0 {
                "already on the first page (skip 0)".into()
            } else {
                format!("last page reached (skip {})", self.hub_skip)
            };
            return;
        }
        self.hub_skip = next;
        self.sel = 0;
        self.run_hub_search().await;
    }

    /// `f` in the Hub section: open the client-side filter overlay.
    pub fn hub_filter_open(&mut self) {
        self.hub_filter_active = true;
        self.hub_filter_sel = 0;
        self.status = "filter — ↑/↓ dimension · ←/→ value · c clear · Enter/Esc close".into();
    }

    /// Close the filter overlay (Enter / Esc).
    pub fn hub_filter_close(&mut self) {
        self.hub_filter_active = false;
        self.status = format!("filter — {} row(s) match", self.hub_indices().len());
    }

    /// ↑/↓ inside the filter overlay: move the dimension cursor.
    pub fn hub_filter_move(&mut self, delta: i32) {
        let n = crate::agent::state::HubFilterDim::ALL.len() as i32;
        self.hub_filter_sel = ((self.hub_filter_sel as i32 + delta).rem_euclid(n)) as usize;
    }

    /// ←/→ inside the filter overlay: cycle the value of the cursor's
    /// dimension (`(all)` first, then the distinct values from the loaded
    /// rows). Dimensions with no data in the loaded rows cannot be set.
    pub fn hub_filter_cycle(&mut self, delta: i32) {
        let dim = crate::agent::state::HubFilterDim::ALL[self.hub_filter_sel];
        let mut vals = vec!["(all)".to_string()];
        vals.extend(self.hub_filter_values(dim));
        if vals.len() == 1 {
            self.status = format!("{} — no data in loaded rows", dim.title());
            return;
        }
        let cur = self
            .hub_filters
            .iter()
            .find(|(d, _)| *d == dim)
            .and_then(|(_, v)| v.clone());
        let idx = cur
            .and_then(|v| vals.iter().position(|x| *x == v))
            .unwrap_or(0);
        let next = (idx as i32 + delta).rem_euclid(vals.len() as i32) as usize;
        let newv = if next == 0 {
            None
        } else {
            Some(vals[next].clone())
        };
        for (d, v) in self.hub_filters.iter_mut() {
            if *d == dim {
                *v = newv.clone();
            }
        }
        self.sel = 0;
        self.status = format!(
            "filter {} = {} — {} row(s) match",
            dim.title(),
            newv.as_deref().unwrap_or("(all)"),
            self.hub_indices().len()
        );
    }

    /// `c` inside the filter overlay: clear every dimension.
    pub fn hub_filter_clear(&mut self) {
        for (_, v) in self.hub_filters.iter_mut() {
            *v = None;
        }
        self.sel = 0;
        self.status = format!("filters cleared — {} row(s)", self.hub_indices().len());
    }

    /// Fetch full `model_info` for the selected hub row (single-flight,
    /// background). The mode gate runs BEFORE the spawn — under Sovereign
    /// the detail pane shows the refusal instead of a silent blank.
    /// Moving to another row aborts the stale fetch and starts the new one.
    pub async fn spawn_hub_detail(&mut self) {
        let Some(id) = self.hub_selected().map(|r| r.id.clone()) else {
            return;
        };
        if let Some(h) = self.hf_detail_join.take() {
            if self.hf_detail_id == id {
                // Fetch for THIS row is already in flight (or it landed/failed).
                self.hf_detail_join = Some(h);
                return;
            }
            h.abort(); // superseded by the new cursor position
        }
        // Already showed (or already failed) for this exact row — no loop.
        if self.hf_detail_id == id && (self.hf_detail.is_some() || self.hf_detail_err.is_some()) {
            return;
        }
        if let Err(e) = self.runtime.modes.ensure_network_allowed().await {
            self.hf_detail = None;
            self.hf_detail_id = id;
            // No request was staged: show the mode's own reason, not a
            // "refusal" of something that never happened.
            self.hf_detail_err = Some(e.to_string());
            return;
        }
        self.hf_detail = None;
        self.hf_detail_err = None;
        self.hf_detail_id = id.clone();
        self.hf_detail_join = Some(tokio::spawn(async move {
            let client = openrt_registry::HfClient::new("", 15);
            let res = client.model_info(&id).await.map_err(|e| e.to_string());
            (id, res)
        }));
    }

    /// `p` in the Hub section: cycle the pipeline facet
    /// (All → each distinct `pipeline_tag` in listing order → All).
    pub fn cycle_facet(&mut self) {
        let mut tags: Vec<String> = Vec::new();
        for r in &self.hub_rows {
            if let Some(t) = &r.pipeline_tag {
                if !tags.iter().any(|x| x == t) {
                    tags.push(t.clone());
                }
            }
        }
        if tags.is_empty() {
            self.status = "no pipeline tags in the loaded rows — nothing to facet".into();
            return;
        }
        let next = match &self.hub_facet {
            None => Some(tags[0].clone()),
            Some(cur) => match tags.iter().position(|t| t == cur) {
                Some(i) if i + 1 < tags.len() => Some(tags[i + 1].clone()),
                _ => None,
            },
        };
        self.hub_facet = next;
        self.sel = 0;
        self.status = match &self.hub_facet {
            Some(f) => format!("facet: {f} — {} row(s)", self.hub_indices().len()),
            None => format!("facet: all — {} row(s)", self.hub_indices().len()),
        };
    }

    /// `d` in the Hub section: the detail facts as a Preview overlay (the
    /// keyboard-reachable fallback for narrow terminals). Data comes from
    /// the same `hf_detail_lines` the right pane renders — no extra fetch.
    pub fn open_hub_detail(&mut self) {
        if self.hub_selected().is_none() {
            self.status = "no hub row selected — ↑/↓ first".into();
            return;
        }
        let lines = self.hf_detail_lines(100).unwrap_or_default();
        let title = self
            .hub_selected()
            .map(|r| format!("hub {}", r.id))
            .unwrap_or_else(|| "hub detail".into());
        self.preview = Some(crate::agent::state::PreviewData { title, lines });
        self.preview_scroll = 0;
        self.overlay = Some(crate::agent::state::Overlay::Preview);
    }

    /// Cursor moved inside Models: keep the Hub detail pane in sync with
    /// the selection (no-op outside the Hub section).
    pub async fn models_nav_hook(&mut self) {
        if self.view == crate::agent::state::View::Models
            && self.model_section == crate::agent::state::ModelSection::Hub
            && !self.model_search_active
            && !self.import_mode
            && self.overlay.is_none()
        {
            self.spawn_hub_detail().await;
        }
    }

    /// Probe the local GPU with `nvidia-smi` (background, once). No tool /
    /// no output = `None` → the browser renders "n/a", never a fake device.
    pub fn spawn_gpu_hw(&mut self) {
        if self.hw.is_some() || self.hw_join.is_some() {
            return;
        }
        self.hw_join = Some(tokio::spawn(async move {
            tokio::task::spawn_blocking(|| -> Result<Option<GpuHw>, String> {
                let out = std::process::Command::new("nvidia-smi")
                    .args([
                        "--query-gpu=name,memory.total,memory.used",
                        "--format=csv,noheader,nounits",
                    ])
                    .output()
                    .map_err(|e| format!("nvidia-smi: {e}"))?;
                if !out.status.success() {
                    return Err(format!("nvidia-smi exited {}", out.status));
                }
                let text = String::from_utf8_lossy(&out.stdout);
                let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                if line.is_empty() {
                    return Ok(None);
                }
                let parts: Vec<&str> = line.split(',').map(str::trim).collect();
                if parts.len() < 3 {
                    return Err("nvidia-smi output unparseable".into());
                }
                let total = parts[1].parse::<u64>().map_err(|_| "bad total")?;
                let used = parts[2].parse::<u64>().map_err(|_| "bad used")?;
                if total == 0 {
                    return Err("nvidia-smi reported 0 MiB total".into());
                }
                Ok(Some(GpuHw {
                    name: parts[0].to_string(),
                    total_mib: total,
                    used_mib: used,
                }))
            })
            .await
            .map_err(|e| format!("gpu probe task crashed: {e}"))?
        }));
    }

    /// Tab / BackTab: cycle `[Local] [Hugging Face] [Installed] [Recommended]`
    /// with per-section honest status. First entry into Hub fetches the recent
    /// listing (gate inside the task); later entries just refresh details.
    pub async fn cycle_section(&mut self, delta: i32) {
        self.model_section = self.model_section.cycle(delta);
        self.sel = 0;
        match self.model_section {
            crate::agent::state::ModelSection::Hub => {
                self.spawn_gpu_hw();
                if !self.hub_loaded {
                    self.run_hub_search().await;
                } else {
                    if !self.hub_rows.is_empty() {
                        self.spawn_hub_detail().await;
                    }
                    self.status = format!("Hugging Face — {}", self.hub_status);
                }
            }
            crate::agent::state::ModelSection::Local => {
                self.status = format!("{} local registry card(s)", self.model_cards.len());
            }
            crate::agent::state::ModelSection::Installed => {
                let n = self.installed_cards().len();
                self.status = format!("{n} installed model(s) on disk");
            }
            crate::agent::state::ModelSection::Recommended => {
                let n = self.recommended_cards().len();
                self.status = if n == 0 {
                    "no curated entries in this catalog".into()
                } else {
                    format!("{n} curated recommendation(s)")
                };
            }
        }
    }

    /// Enter in the `[Hugging Face]` section: applied only what is honest.
    /// Empty hub (e.g. gate refusal) → re-run the search; an installed row →
    /// apply its provider tag; otherwise metadata-only with the import hint.
    pub async fn enter_hub_row(&mut self) {
        if self.hub_indices().is_empty() {
            if self.hf_detail_join.is_some() || self.hf_size_join.is_some() {
                self.status = "hub fetch in flight — one moment".into();
                return;
            }
            self.run_hub_search().await;
            return;
        }
        let Some(row) = self.hub_selected().cloned() else {
            self.status = "no hub row selected — ↑/↓ first".into();
            return;
        };
        if let Some(card) = self
            .model_cards
            .iter()
            .find(|c| c.artifact_id == row.id && c.installed_bytes > 0)
        {
            let provider = self.runtime.provider.id().to_string();
            let bound = self
                .runtime
                .model_registry
                .bound_tag(&card.artifact_id, &provider)
                .await
                .ok()
                .flatten();
            if let Some(tag) = bound
                .map(|b| b.tag)
                .or_else(|| crate::agent::setup::suggest_provider_tag(&card.artifact_id))
            {
                self.apply_model_id(tag).await;
            } else {
                self.status = format!(
                    "{} is installed — no provider tag bound or suggested for {}; type a provider tag, then Enter",
                    card.display_name, card.artifact_id
                );
            }
            return;
        }
        self.status = format!(
            "{} — hub row (metadata only; weights are never fetched). Press i to import a local copy as `{}`",
            row.id, row.id
        );
    }

    /// `s` in the Models view: drop-in scan of `models/<category>/` under the
    /// workdir — every weight file found is imported through the ONE registry
    /// (real bytes, real sha256). Per-file failures (mode gate, scan refusal)
    /// are collected honestly; nothing is skipped silently. A missing
    /// `models/` root is auto-initialized (category dirs created) instead of
    /// failing with "No such file or directory"; an empty scan says
    /// NO MODELS FOUND rather than an error.
    pub fn dropin_scan(&mut self) {
        if self.import_join.is_some() {
            self.status = "an import is already running — it reports here when done".into();
            return;
        }
        let root = self.files_root.join("models");
        let init = ensure_model_dirs(&root);
        if let Err(e) = &init {
            self.status = format!("drop-in scan: {e} — nothing imported");
            return;
        }
        let created = init.unwrap_or_default();
        let mut found: Vec<(String, String)> = Vec::new();
        let dirs = match std::fs::read_dir(&root) {
            Ok(d) => d,
            Err(e) => {
                self.status = format!(
                    "drop-in scan: {} not readable ({e}) — nothing imported",
                    root.display()
                );
                return;
            }
        };
        for entry in dirs.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let category = entry.file_name().to_string_lossy().to_string();
            let Ok(files) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            for f in files.flatten() {
                let Ok(ft) = f.file_type() else { continue };
                if !ft.is_file() {
                    continue;
                }
                let name = f.file_name().to_string_lossy().to_string();
                let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
                if ext
                    .map(|e| SUPPORTED_MODEL_EXTS.contains(&e.as_str()))
                    .unwrap_or(false)
                {
                    let stem = name
                        .rsplit_once('.')
                        .map(|(s, _)| s.to_string())
                        .unwrap_or_else(|| name.clone());
                    found.push((
                        format!("local/{category}/{stem}"),
                        f.path().to_string_lossy().into(),
                    ));
                }
            }
        }
        if found.is_empty() {
            self.status = if created.is_empty() {
                format!(
                    "drop-in scan: NO MODELS FOUND under {} ({} category dirs ready — drop weight files in and press s)",
                    root.display(),
                    MODEL_CATEGORY_DIRS.len()
                )
            } else {
                format!(
                    "drop-in scan: NO MODELS FOUND under {} (created {} category dirs — drop weight files in and press s)",
                    root.display(),
                    created.len()
                )
            };
            return;
        }
        let total = found.len();
        let rt = self.runtime.clone();
        self.import_error = None;
        self.status = format!("drop-in scan: registering {total} file(s) in the background…");
        self.import_join = Some(tokio::spawn(async move {
            let mut ok = 0usize;
            let mut errs: Vec<String> = Vec::new();
            let mut limited: Vec<String> = Vec::new();
            for (id, path) in found {
                match import_model_file(rt.clone(), id.clone(), path).await {
                    Ok(outcome) => {
                        ok += 1;
                        // Surface the resource-limited ones by name: a drop-in
                        // that "succeeded" 40 files and could run none of them
                        // is exactly the case an operator needs spelled out.
                        if let openrt_models::artifact::ExecutionFit::ResourceLimited { .. } =
                            outcome.execution
                        {
                            limited.push(outcome.summary());
                        }
                    }
                    Err(e) => errs.push(format!("`{id}`: {e}")),
                }
            }
            if errs.is_empty() {
                Ok(format!("drop-in: registered {ok}/{total} file(s) on disk"))
            } else if ok == 0 {
                Err(format!(
                    "drop-in: 0/{total} registered — first error: {}",
                    errs.first().unwrap_or(&String::new())
                ))
            } else {
                Ok(format!(
                    "drop-in: registered {ok}/{total} — {} refused · {} RESOURCE-LIMITED for execution (registered, not loadable here): {}",
                    errs.len(),
                    limited.len(),
                    limited
                        .first()
                        .map(|s| s.as_str())
                        .unwrap_or("none")
                ))
            }
        }));
    }

    /// Enter in search mode: apply the selected result (installed → via its
    /// suggested provider tag; not installed → honest import hint). A changed
    /// query re-runs the search instead (handled in `input.rs`).
    pub async fn apply_selected_result(&mut self) {
        let Some(r) = self.model_results.get(self.sel).cloned() else {
            self.status = "no result selected — ↑/↓ first".into();
            return;
        };
        if r.installed {
            match crate::agent::setup::suggest_provider_tag(&r.artifact_id) {
                Some(tag) => {
                    self.model_search_active = false;
                    self.apply_model_id(tag).await;
                }
                None => {
                    self.status = format!(
                        "{} is installed — no tag suggestion for {}; type a provider tag, then Enter",
                        r.display_name, r.artifact_id
                    );
                }
            }
        } else {
            self.status = format!(
                "{} — hub row (metadata only; weights are never fetched). Press i to import a local file as `{}`",
                r.display_name, r.artifact_id
            );
        }
    }

    /// `i` in the Models view: start the local-file import workflow for the
    /// selected artifact (card, hub row, or search result).
    pub fn import_begin(&mut self) {
        if self.import_mode {
            return;
        }
        let id = if self.model_search_active {
            self.model_results
                .get(self.sel)
                .map(|r| r.artifact_id.clone())
        } else if self.model_section == crate::agent::state::ModelSection::Hub {
            self.hub_selected().map(|r| r.id.clone())
        } else {
            self.section_card().map(|c| c.artifact_id.clone())
        };
        let Some(id) = id else {
            self.status = "select a model first (↑/↓), then press i".into();
            return;
        };
        self.import_mode = true;
        self.import_id = id.clone();
        self.input.clear();
        self.status =
            format!("import `{id}` — type the path to a local model file, then Enter (Esc cancel)");
    }

    /// Enter in import mode: delegate to the shared import helper (the
    /// draw loop never blocks; the size guard lives in the helper).
    pub fn import_run(&mut self) {
        if self.import_join.is_some() {
            self.status = "an import is already running — it reports here when done".into();
            return;
        }
        let path = self.input.trim().to_string();
        if path.is_empty() {
            self.status = "type the path to a local model file, then Enter".into();
            return;
        }
        let id = self.import_id.clone();
        self.input.clear();
        let rt = self.runtime.clone();
        self.import_error = None;
        self.status = format!("register `{id}` · streaming header + digest in the background…");
        self.import_join = Some(tokio::spawn(async move {
            import_model_file(rt, id.clone(), path)
                .await
                .map(|outcome| outcome.summary())
        }));
    }

    /// Apply the model override for subsequent requests (real per-request
    /// `model_id` — see `agent::provider`). The overlay edits `model_input`
    /// directly; the Models view types into the shared input line.
    pub async fn apply_model(&mut self) {
        let id = if self.overlay == Some(crate::agent::state::Overlay::Model) {
            // Overlay edits its own buffer; the chat input is untouched.
            self.model_input.trim().to_string()
        } else {
            // Models view: the shared input line is the text field.
            let typed = self.input.trim().to_string();
            if typed.is_empty() {
                self.status = "type a provider tag (e.g. llama3.1:8b), then Enter".into();
                return;
            }
            self.input.clear();
            self.model_input = typed.clone();
            typed
        };
        self.apply_model_id(id).await;
    }

    /// Enter in the Models view with an empty input line: apply the
    /// *selected card* through its suggested provider tag — no typing
    /// required. A typed line still wins (it is the explicit override).
    /// The `[Hugging Face]` section dispatches to the hub-row contract.
    pub async fn apply_selected_model(&mut self) {
        if !self.input.trim().is_empty() {
            self.apply_model().await;
            return;
        }
        if self.model_section == crate::agent::state::ModelSection::Hub {
            self.enter_hub_row().await;
            return;
        }
        let Some(card) = self.section_card().cloned() else {
            self.status = "no model selected — ↑/↓ first".into();
            return;
        };
        // A persisted operator binding is authoritative (Part D); the derived
        // suggestion is only the fallback hint.
        let provider = self.runtime.provider.id().to_string();
        let bound = self
            .runtime
            .model_registry
            .bound_tag(&card.artifact_id, &provider)
            .await
            .ok()
            .flatten();
        let Some(tag) = bound
            .map(|b| b.tag)
            .or_else(|| crate::agent::setup::suggest_provider_tag(&card.artifact_id))
        else {
            self.status = format!(
                "{} — no provider tag bound or suggested for {}; type a provider tag, then Enter",
                card.display_name, card.artifact_id
            );
            return;
        };
        self.apply_model_id(tag).await;
    }

    /// The single apply path: switch the ONE per-request model override and
    /// re-probe through the honest transport path.
    pub async fn apply_model_id(&mut self, id: String) {
        if id.is_empty() {
            self.status = "type a provider tag (e.g. llama3.1:8b)".into();
            return;
        }
        match &self.switcher {
            Some(sw) => {
                if id == "default" {
                    // Going back to the provider default cannot fail: there is
                    // nothing to reach.
                    sw.set_model(None);
                    self.pending_switch = None;
                    self.status = "model → provider default".into();
                    self.overlay = None;
                } else {
                    // Stage the override. It becomes the active model only once
                    // the provider has answered for it — see `commit_switch`.
                    self.pending_switch = Some(id.clone());
                    self.status = format!("switching to {id} — probing before it takes effect");
                    self.overlay = None;
                }
            }
            None => {
                self.status = "no switchable provider attached".into();
            }
        }
        self.model_input = id.clone();
        // Background probe: the key handler must not wait on the network.
        self.spawn_probe();
    }

    /// Land a staged switch once its probe has answered.
    ///
    /// Commit on success, keep the previous model on failure. A provider that
    /// cannot be reached for the candidate tag must not leave the session
    /// pointed at a model that was never loaded — the failure mode this replaces
    /// wrote the override first and reported the error afterwards, which left
    /// every later request carrying a model nobody had confirmed.
    pub fn commit_switch(&mut self, candidate: &str, probe: &ModelProbe) {
        let Some(sw) = &self.switcher else {
            self.status = "no switchable provider attached".into();
            return;
        };
        match &probe.status {
            Some(Ok(())) => {
                sw.set_model(Some(candidate.to_string()));
                self.status = format!("model → {candidate} (probed {}, next requests)", probe.tag);
            }
            Some(Err(e)) => {
                let previous = sw.selected().unwrap_or_else(|| {
                    self.runtime
                        .provider
                        .info()
                        .default_model
                        .unwrap_or_default()
                });
                self.status = if previous.is_empty() {
                    format!("switch to {candidate} refused: {e} — provider default still active")
                } else {
                    format!("switch to {candidate} refused: {e} — still on {previous}")
                };
            }
            None => {
                self.status = format!("switch to {candidate} not attempted: no probe result");
            }
        }
    }

    pub async fn load_mcp(&mut self) {
        match self.runtime.mcp_states().await {
            Ok(v) => {
                let rows: Vec<crate::agent::state::McpRow> = v["servers"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| {
                                Some(crate::agent::state::McpRow {
                                    name: s["name"].as_str()?.to_string(),
                                    trust: s["trust"].as_str().unwrap_or("?").to_string(),
                                    enabled: s["enabled"] == serde_json::json!(true),
                                    security: s["security_status"]
                                        .as_str()
                                        .unwrap_or("?")
                                        .to_string(),
                                    tools: s["discovered_tool_count"].as_i64().unwrap_or(0),
                                    source: s["declared_by"].as_str().unwrap_or("?").to_string(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.mcp_rows = rows;
                self.mcp = v;
            }
            Err(e) => {
                self.mcp = json!({ "error": e.to_string() });
                self.mcp_rows.clear();
            }
        }
    }

    /// Selected MCP row (through the active filter).
    pub fn mcp_selected(&self) -> Option<&crate::agent::state::McpRow> {
        let idx = self.filtered_mcp().get(self.sel).copied()?;
        self.mcp_rows.get(idx)
    }

    /// Run one operator action on the selected server through the ONE
    /// runtime registry — spawned so scans/health probes never block the
    /// draw loop.
    pub fn mcp_spawn(&mut self, op: crate::agent::state::McpOp) {
        use crate::agent::state::McpOp;
        if self.op_edit {
            self.status = "finish editing the operator id first (Enter saves, Esc cancels)".into();
            return;
        }
        if self.op_join.is_some() {
            self.status = "an MCP operation is running — it reports here when done".into();
            return;
        }
        let Some(row) = self.mcp_selected().cloned() else {
            self.status = "no server selected — ↑/↓ first".into();
            return;
        };
        let operator = self.operator_id.trim().to_string();
        if operator.is_empty() {
            self.status = "set an operator id first (o), then retry".into();
            return;
        }
        let name = row.name.clone();
        let rt = self.runtime.clone();
        let label = match &op {
            McpOp::Enable => "enable".to_string(),
            McpOp::Disable => "disable".to_string(),
            McpOp::Trust(l) => format!("trust → {l}"),
            McpOp::Scan => "scan".to_string(),
            McpOp::Health => "health".to_string(),
            McpOp::Inspect => "inspect".to_string(),
        };
        self.status = format!("mcp {label} `{name}` …");
        self.op_join = Some(tokio::spawn(async move {
            let res: Result<(String, Option<Vec<String>>), String> = match op {
                McpOp::Enable => rt
                    .mcp_enable(&name, &operator)
                    .await
                    .map(|_| {
                        (
                            format!("`{name}` enabled (scan clean; connects on next build)"),
                            None,
                        )
                    })
                    .map_err(|e| e.to_string()),
                McpOp::Disable => rt
                    .mcp_disable(&name, &operator)
                    .await
                    .map(|_| {
                        (
                            format!("`{name}` disabled (stops connecting on next build)"),
                            None,
                        )
                    })
                    .map_err(|e| e.to_string()),
                McpOp::Trust(level) => rt
                    .mcp_set_trust(&name, &level, &operator)
                    .await
                    .map(|_| {
                        (
                            format!("`{name}` trust = {level} (gates still apply at invoke)"),
                            None,
                        )
                    })
                    .map_err(|e| e.to_string()),
                McpOp::Scan => rt
                    .mcp_scan(&name)
                    .await
                    .map(|r| {
                        let mut lines = vec![
                            format!("action   {}", r.action.as_str()),
                            format!("findings {}", r.findings.len()),
                            String::new(),
                        ];
                        for f in &r.findings {
                            lines.push(format!(
                                "{}: {} — {}",
                                f.category.as_str(),
                                f.severity.as_str(),
                                f.reason
                            ));
                        }
                        (
                            format!(
                                "`{name}` scan → {} · {} finding(s)",
                                r.action.as_str(),
                                r.findings.len()
                            ),
                            Some(lines),
                        )
                    })
                    .map_err(|e| e.to_string()),
                McpOp::Health => rt
                    .mcp_health(&name)
                    .await
                    .map(|v| {
                        let pretty =
                            serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
                        (
                            format!("`{name}` health probe finished"),
                            Some(pretty.lines().map(str::to_string).collect()),
                        )
                    })
                    .map_err(|e| e.to_string()),
                McpOp::Inspect => rt
                    .mcp_inspect(&name)
                    .await
                    .map(|v| {
                        let pretty =
                            serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
                        (
                            format!("mcp `{name}` record · live · sources"),
                            Some(pretty.lines().map(str::to_string).collect()),
                        )
                    })
                    .map_err(|e| e.to_string()),
            };
            res
        }));
    }

    pub async fn load_extensions(&mut self) {
        match self.runtime.extension_states().await {
            Ok(v) => {
                let rows: Vec<crate::agent::state::ExtRow> = v
                    .iter()
                    .filter_map(|e| {
                        Some(crate::agent::state::ExtRow {
                            id: e["id"].as_str()?.to_string(),
                            version: e["version"].as_u64().unwrap_or(0) as u32,
                            name: e["name"].as_str().unwrap_or("?").to_string(),
                            activation: e["activation"].as_str().unwrap_or("?").to_string(),
                            security: e["security"].as_str().unwrap_or("?").to_string(),
                        })
                    })
                    .collect();
                self.ext_rows = rows;
                self.exts = v;
            }
            Err(e) => {
                self.exts = vec![json!({ "error": e.to_string() })];
                self.ext_rows.clear();
            }
        }
    }

    /// Selected extension row (through the active filter).
    pub fn ext_selected(&self) -> Option<&crate::agent::state::ExtRow> {
        let idx = self.filtered_ext().get(self.sel).copied()?;
        self.ext_rows.get(idx)
    }

    /// Run one extension operator action through the ONE runtime asset
    /// store — spawned so installs/scans never block the draw loop.
    /// `Install` accepts a local package path or an https URL (GitHub raw
    /// links work); the URL leg is gated by mode + netpol BEFORE any socket.
    pub fn ext_spawn(&mut self, op: crate::agent::state::ExtOp) {
        use crate::agent::state::ExtOp;
        if self.op_edit {
            self.status = "finish editing the operator id first (Enter saves, Esc cancels)".into();
            return;
        }
        if self.op_join.is_some() {
            self.status = "an operator action is running — it reports here when done".into();
            return;
        }
        let rt = self.runtime.clone();
        let operator = self.operator_id.trim().to_string();
        let task: tokio::task::JoinHandle<Result<(String, Option<Vec<String>>), String>> = match op
        {
            ExtOp::Install { source } => {
                let source = source.trim().to_string();
                if source.is_empty() {
                    self.status =
                        "type a local package path or an https URL, then Enter (Esc cancels)"
                            .into();
                    return;
                }
                self.status = format!("installing extension from {source} …");
                tokio::spawn(async move {
                    let text = fetch_package_text(&rt, &source).await?;
                    let pkg: openrt_workspace_assets::ExtensionPackage =
                        serde_json::from_str(&text)
                            .map_err(|e| format!("not an extension package JSON: {e}"))?;
                    let (eid, ever) = (pkg.extension.id.0.clone(), pkg.extension.version.0);
                    let skills = rt.extension_install(pkg).await.map_err(|e| e.to_string())?;
                    let mut msg =
                        format!("extension {eid}@{ever} installed → Registered/unscanned");
                    if !skills.is_empty() {
                        msg.push_str(&format!(" · payload skills: {}", skills.join(", ")));
                    }
                    msg.push_str(" · next: v validate → s scan → a activate");
                    Ok((msg, None))
                })
            }
            ExtOp::Validate | ExtOp::Scan | ExtOp::Inspect => {
                let Some(row) = self.ext_selected().cloned() else {
                    self.status = "no extension selected — ↑/↓ first".into();
                    return;
                };
                let (id, ver) = (row.id, Some(row.version));
                let label = match op {
                    ExtOp::Validate => "validate",
                    ExtOp::Scan => "scan",
                    _ => "inspect",
                };
                self.status = format!("extension {label} `{id}` …");
                tokio::spawn(async move {
                    match op {
                        ExtOp::Validate => {
                            let v = rt
                                .extension_validate(&id, ver)
                                .await
                                .map_err(|e| e.to_string())?;
                            Ok((format!("extension {id}@{v} validated"), None))
                        }
                        ExtOp::Scan => {
                            let r = rt
                                .extension_scan(&id, ver)
                                .await
                                .map_err(|e| e.to_string())?;
                            let mut lines = vec![
                                format!("action   {}", r.action.as_str()),
                                format!("findings {}", r.findings.len()),
                                String::new(),
                            ];
                            for f in &r.findings {
                                lines.push(format!(
                                    "{}: {} — {}",
                                    f.category.as_str(),
                                    f.severity.as_str(),
                                    f.reason
                                ));
                            }
                            Ok((
                                format!(
                                    "extension `{id}` scan → {} · {} finding(s)",
                                    r.action.as_str(),
                                    r.findings.len()
                                ),
                                Some(lines),
                            ))
                        }
                        ExtOp::Inspect => {
                            let v = rt
                                .extension_inspect(&id, ver)
                                .await
                                .map_err(|e| e.to_string())?;
                            let pretty =
                                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
                            Ok((
                                format!("extension `{id}` manifest"),
                                Some(pretty.lines().map(str::to_string).collect()),
                            ))
                        }
                        // Not `unreachable!()`. This runs inside a spawned task,
                        // so a panic here would kill only the task and leave the
                        // status line reading "extension scan …" forever: a silent
                        // hang rather than a visible failure. A missing arm is an
                        // operator-visible error, not something to crash on.
                        other => Err(format!("unsupported extension operation {other:?}")),
                    }
                })
            }
            ExtOp::Activate | ExtOp::Deactivate | ExtOp::Invalidate => {
                if operator.is_empty() {
                    self.status = "set an operator id first (o), then retry".into();
                    return;
                }
                let Some(row) = self.ext_selected().cloned() else {
                    self.status = "no extension selected — ↑/↓ first".into();
                    return;
                };
                let id = row.id.clone();
                let label = match op {
                    ExtOp::Activate => "activate",
                    ExtOp::Deactivate => "deactivate",
                    _ => "invalidate",
                };
                self.status = format!("extension {label} `{id}` …");
                tokio::spawn(async move {
                    match op {
                        ExtOp::Activate => {
                            let v = rt
                                .extension_activate(&id, Some(row.version), &operator)
                                .await
                                .map_err(|e| e.to_string())?;
                            Ok((
                                format!("extension {id}@{v} → Active (bindings on next build)"),
                                None,
                            ))
                        }
                        ExtOp::Deactivate => {
                            let v = rt
                                .extension_deactivate(&id, &operator)
                                .await
                                .map_err(|e| e.to_string())?;
                            Ok((format!("extension {id}@{v} → Inactive"), None))
                        }
                        ExtOp::Invalidate => {
                            let v = rt
                                .extension_invalidate(&id, &operator)
                                .await
                                .map_err(|e| e.to_string())?;
                            Ok((format!("extension {id}@{v} invalidated (tombstoned)"), None))
                        }
                        // Not `unreachable!()`. This runs inside a spawned task,
                        // so a panic here would kill only the task and leave the
                        // status line reading "extension scan …" forever: a silent
                        // hang rather than a visible failure. A missing arm is an
                        // operator-visible error, not something to crash on.
                        other => Err(format!("unsupported extension operation {other:?}")),
                    }
                })
            }
        };
        self.op_join = Some(task);
    }

    pub async fn load_security(&mut self) -> Result<(), AppError> {
        let mut rows: Vec<(String, String)> = Vec::new();
        rows.push(("mode".into(), self.fetch_mode().await));
        rows.push((
            "kill switch".into(),
            match self.runtime.kill_switch.engaged() {
                Some(reason) => format!("HALTED: {reason}"),
                None => "clear".into(),
            },
        ));
        match self.runtime.policy_document().await {
            Ok(doc) => rows.push((
                "policy document".into(),
                format!(
                    "v{} · {} rules · {} · {}",
                    doc.version,
                    doc.rules.len(),
                    doc.author,
                    doc.note
                ),
            )),
            Err(e) => rows.push(("policy document".into(), format!("error: {e}"))),
        }
        for r in &self.runtime.permission_policy.rules {
            let scope = r.scope.as_deref().unwrap_or("*");
            rows.push((
                format!("rule {}", r.pattern),
                format!("{:?} · scope {scope}", r.action),
            ));
        }
        self.policy_rows = rows;
        self.findings = openrt_shield::store::list_findings(&*self.runtime.store, None)
            .await?
            .into_iter()
            .map(|f| crate::app::FindingRow {
                id: f.id,
                severity: f.severity.as_str().to_string(),
                category: f.category.as_str().to_string(),
                reason: f.reason,
                status: f.status.as_str().to_string(),
            })
            .collect();
        self.recovery_rows = self
            .runtime
            .recovery_records()
            .await?
            .iter()
            .map(|r| crate::app::RecoveryRow {
                record_id: r.record_id.clone(),
                interrupted: r.interrupted_tasks.len(),
                incomplete: r.incomplete_runs.len(),
                stale_sandboxes: r.stale_sandboxes.len(),
                clean: r.is_clean(),
            })
            .collect();
        Ok(())
    }
}

// ----------------------------------------------------------------------
// filesystem / git helpers (bounded, read-only)
// ----------------------------------------------------------------------

fn read_dir_rows(root: &Path) -> Vec<FileRow> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut rows: Vec<FileRow> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some(FileRow {
                name: e.file_name().to_string_lossy().into_owned(),
                is_dir: meta.is_dir(),
                size: meta.len(),
            })
        })
        .collect();
    rows.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    rows.truncate(500);
    rows
}

fn read_preview(path: &Path, diff: Option<&str>) -> Result<(String, Vec<String>), String> {
    let title = match diff {
        Some(d) => format!("{d} {}", path.display()),
        None => path.display().to_string(),
    };
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let bytes = if bytes.len() > 256 * 1024 {
        &bytes[..256 * 1024]
    } else {
        &bytes[..]
    };
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<String> = text.lines().take(400).map(|l| l.to_string()).collect();
    if text.lines().count() > 400 {
        lines.push("… truncated (400 lines shown)".into());
    }
    Ok((title, lines))
}

fn git_output(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() && args[0] != "status" {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_snapshot(root: &Path) -> Result<GitInfo, String> {
    // An unborn repository (zero commits) fails `rev-parse HEAD` — fall back
    // to the symbolic ref so the branch still renders honestly.
    let head = git_output(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .or_else(|_| git_output(root, &["symbolic-ref", "--short", "HEAD"]))
        .unwrap_or_else(|_| String::new());
    let head = head.trim().to_string();
    if head.is_empty() {
        return Err("not a git repository".into());
    }
    let status_raw = git_output(root, &["status", "--porcelain=v1", "-b"])?;
    let mut branch = head;
    let mut status = Vec::new();
    for line in status_raw.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            let rest = rest.split("...").next().unwrap_or(rest).trim();
            // Unborn branch: "## No commits yet on master".
            branch = rest
                .strip_prefix("No commits yet on ")
                .unwrap_or(rest)
                .to_string();
        } else if !line.trim().is_empty() {
            status.push(line.to_string());
            if status.len() >= 200 {
                break;
            }
        }
    }
    // `git log` fails on an unborn branch — show an empty history instead.
    let log: Vec<String> = git_output(root, &["log", "--oneline", "-8"])
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default();
    let mut stat = git_output(root, &["diff", "--stat"])?;
    let cached = git_output(root, &["diff", "--cached", "--stat"])?;
    stat.push_str(&cached);
    let stat = stat.lines().map(str::to_string).take(20).collect();
    Ok(GitInfo {
        branch,
        status,
        log,
        stat,
    })
}

fn git_diff_for(root: &Path, file: &str) -> Result<(String, Vec<String>), String> {
    let mut out = git_output(root, &["diff", "--", file])?;
    let cached = git_output(root, &["diff", "--cached", "--", file])?;
    out.push_str(&cached);
    if out.trim().is_empty() {
        return Ok((
            format!("no changes: {file}"),
            vec!["working tree clean for this file".into()],
        ));
    }
    let mut lines: Vec<String> = out.lines().take(400).map(str::to_string).collect();
    if out.lines().count() > 400 {
        lines.push("… truncated".into());
    }
    Ok((format!("diff {file}"), lines))
}

/// Bounded recursive content search (case-insensitive substring).
fn search_walk(root: &Path, query: &str) -> Vec<SearchHit> {
    let q = query.to_lowercase();
    let mut hits = Vec::new();
    let mut stack: Vec<(PathBuf, u8)> = vec![(root.to_path_buf(), 0)];
    let mut files_seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if hits.len() >= 200 || files_seen >= 5000 || depth > 12 {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.filter_map(|e| e.ok()) {
            if hits.len() >= 200 || files_seen >= 5000 {
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            files_seen += 1;
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            if name.to_lowercase().contains(&q) {
                hits.push(SearchHit {
                    path: rel.clone(),
                    line: 0,
                    text: "<file name match>".into(),
                });
                if hits.len() >= 200 {
                    break;
                }
            }
            if meta.len() > 512 * 1024 {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if bytes.contains(&0) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            for (i, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(&q) {
                    let mut t = line.trim().to_string();
                    t.truncate(200);
                    hits.push(SearchHit {
                        path: rel.clone(),
                        line: i + 1,
                        text: t,
                    });
                    if hits.len() >= 200 {
                        break;
                    }
                }
            }
        }
    }
    hits
}

// ----------------------------------------------------------------------
// model categories (`models/<category>` sections in the Models view)
// ----------------------------------------------------------------------

/// Category bucket for a registry card, derived ONLY from its real tag list
/// (curated rows carry their catalog tags; installed cards carry whatever
/// their manifest declared). Unknown tags fall through to `general`.
/// The honest outcome of registering one local model file.
///
/// Returned instead of a bare `String` so callers (the `i` import, the `s`
/// drop-in scan) can say *which* of the three facts held instead of flattening
/// them into prose: the artifact is catalogued, whether its bytes are ours,
/// and whether this machine can execute it right now.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelImportOutcome {
    pub artifact_id: String,
    /// Lifecycle state the registry recorded.
    pub lifecycle: &'static str,
    /// Size of the file on disk.
    pub artifact_bytes: u64,
    pub digest_prefix: String,
    /// `true` when the bytes are the operator's file, indexed but not copied.
    pub by_reference: bool,
    pub execution: openrt_models::artifact::ExecutionFit,
}

impl ModelImportOutcome {
    /// One line for the status bar. Says what happened, then what it means for
    /// execution — never "usable now" for something that has not been loaded.
    pub fn summary(&self) -> String {
        use openrt_models::artifact::{human_bytes, ExecutionFit};
        let exec = match &self.execution {
            // "FITS NOW" would read as a claim about the present: the file is
            // on disk and *would* fit, and nothing has been loaded. Say both.
            ExecutionFit::Ready { required, available } => format!(
                "execution: WOULD FIT (advisory — needs {} {}, host reports {} free; registered, not loaded, admission still applies at load)",
                required.provenance.as_str(),
                human_bytes(required.bytes),
                human_bytes(*available)
            ),
            ExecutionFit::ResourceLimited {
                required,
                available,
                ..
            } => format!(
                "execution: RESOURCE-LIMITED (advisory — needs {} {}, host reports {} free; registered, not loaded)",
                required.provenance.as_str(),
                human_bytes(required.bytes),
                human_bytes(*available)
            ),
            ExecutionFit::Unknown { reason } => {
                format!("execution: UNKNOWN ({reason} — registered, not loaded)")
            }
        };
        format!(
            "REGISTERED ON DISK `{}` · {} · {} · sha256 {}… · {} · {exec}",
            self.artifact_id,
            self.lifecycle,
            human_bytes(self.artifact_bytes),
            self.digest_prefix,
            if self.by_reference {
                "bytes not copied (registry holds a reference)"
            } else {
                "bytes stored in registry"
            }
        )
    }
}

/// Register one local model file through the ONE registry.
///
/// Registration is metadata-only by construction: the file is inspected through
/// a bounded header window and digested with a fixed 1 MiB buffer, so the peak
/// memory cost is a buffer rather than the artifact. A 9 GiB model registers on
/// a machine with a few hundred MiB free, which is the whole point — being able
/// to *catalogue* a model has never required being able to *run* it.
///
/// What this deliberately does not do is load, map, or admit anything for
/// execution. Execution is the resource gate's boundary
/// (`openrt-resources`), and the fit reported here is advisory.
///
/// Shared by `i` (import) and `s` (drop-in scan).
pub(crate) async fn import_model_file(
    rt: Arc<openrt_runtime::Runtime>,
    id: String,
    path: String,
) -> Result<ModelImportOutcome, String> {
    // Extension allowlist first: a structured refusal instead of a late
    // read/hash failure on an arbitrary file the operator mistyped.
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match &ext {
        Some(e) if SUPPORTED_MODEL_EXTS.contains(&e.as_str()) => {}
        Some(e) => {
            return Err(format!(
                "unsupported model file format `.{e}` — supported: {}",
                SUPPORTED_MODEL_EXTS.join(", ")
            ))
        }
        None => {
            return Err(format!(
                "unsupported model file format (no extension) — supported: {}",
                SUPPORTED_MODEL_EXTS.join(", ")
            ))
        }
    }
    let p = path.clone();
    let meta = tokio::task::spawn_blocking(move || std::fs::metadata(&p))
        .await
        .map_err(|e| format!("stat task crashed: {e}"))?
        .map_err(|e| format!("stat {path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{path} is not a file"));
    }
    let size = meta.len();
    if size == 0 {
        return Err("refusing to import an empty file".into());
    }

    // Header inspection and the host memory reading are both bounded and both
    // advisory. Nothing here can refuse the registration.
    let ip = path.clone();
    let (artifact_md, sys): (
        openrt_models::artifact::ArtifactMetadata,
        openrt_resources::SystemInfo,
    ) = tokio::task::spawn_blocking(move || {
        let md = openrt_models::artifact::read_metadata(std::path::Path::new(&ip))
            .map_err(|e| format!("inspect {ip}: {e}"))?;
        let sys = openrt_resources::SystemInfo::collect();
        Ok::<_, String>((md, sys))
    })
    .await
    .map_err(|e| format!("inspect task crashed: {e}"))??;
    let profile = openrt_models::artifact::ModelResourceProfile::from_metadata(&artifact_md, size);
    let execution =
        openrt_models::artifact::assess_execution_fit(&profile, Some(sys.mem.available_kb * 1024));

    // Streamed digest: bounded memory, and the digest is the same function the
    // materializing path uses, so the two cannot disagree about an artifact.
    let dp = path.clone();
    let (digest, bytes_read) =
        tokio::task::spawn_blocking(move || openrt_verify::sha256_file(std::path::Path::new(&dp)))
            .await
            .map_err(|e| format!("digest task crashed: {e}"))?
            .map_err(|e| format!("digest {path}: {e}"))?;
    if bytes_read != size {
        return Err(format!(
            "{path} changed while it was being read ({size} bytes stat'd, {bytes_read} hashed) — retry"
        ));
    }

    let manifest = openrt_registry::ManifestV1 {
        schema_version: openrt_registry::MANIFEST_SCHEMA_VERSION,
        artifact_id: id.clone(),
        kind: openrt_registry::RegistryArtifactKind::Binary,
        blobs: vec![openrt_registry::BlobRef {
            file_key: file_key_for(&path),
            digest: digest.clone(),
            size_bytes: size,
            location: openrt_registry::BlobLocation::External { path: path.clone() },
        }],
        base_digest: None,
        created_at: openrt_core::now().to_rfc3339(),
    };
    let registry = &rt.model_registry;
    match registry
        .register_local(manifest, &path, "tui.operator")
        .await
    {
        Ok(lr) => Ok(ModelImportOutcome {
            artifact_id: id,
            lifecycle: lr.state.as_str(),
            artifact_bytes: size,
            digest_prefix: digest[..digest.len().min(12)].to_string(),
            by_reference: true,
            execution,
        }),
        Err(e) => Err(format!("register `{id}` failed: {e}")),
    }
}

/// A stable logical file key, so re-registering the same artifact produces the
/// same manifest shape regardless of what the operator called the file.
fn file_key_for(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or("artifact.bin")
        .to_string()
}

pub(crate) fn model_category(tags: &[String]) -> &'static str {
    for t in tags {
        match t.as_str() {
            "vision" => return "vision",
            "chat" => return "chat",
            "code" => return "code",
            "embedding" => return "embedding",
            "light" | "small" => return "light",
            "moe" | "expert" => return "moe",
            _ => {}
        }
    }
    "general"
}

/// Stable section order for the category headers.
pub(crate) fn category_rank(cat: &str) -> u8 {
    match cat {
        "chat" => 0,
        "vision" => 1,
        "code" => 2,
        "embedding" => 3,
        "light" => 4,
        "moe" => 5,
        _ => 6,
    }
}

/// Read an extension package from a local path or an egress-gated https URL
/// (the GitHub/raw.githubusercontent browser path). The network leg runs the
/// SAME authorities as every other egress in the fabric — operating mode
/// first, then the netpol grant (default-deny) — before any socket opens.
async fn fetch_package_text(
    rt: &std::sync::Arc<openrt_runtime::Runtime>,
    source: &str,
) -> Result<String, String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        rt.modes
            .ensure_network_allowed()
            .await
            .map_err(|e| format!("network mode: {e}"))?;
        let np = rt.netpol();
        match np.ensure_egress("extension", source).await {
            Ok(openrt_netpol::EgressDecision::Allow { .. }) => {}
            Ok(openrt_netpol::EgressDecision::Deny { reason, .. }) => {
                return Err(format!("egress denied for `{source}`: {reason}"));
            }
            Err(e) => return Err(format!("egress check failed: {e}")),
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let resp = client
            .get(source)
            .send()
            .await
            .map_err(|e| format!("fetch {source}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("fetch {source}: HTTP {}", resp.status()));
        }
        if let Some(len) = resp.content_length() {
            if len > 32 * 1024 * 1024 {
                return Err(format!(
                    "response is {len} bytes — extension packages are JSON documents; refusing anything over 32 MiB"
                ));
            }
        }
        resp.text()
            .await
            .map_err(|e| format!("fetch {source}: {e}"))
    } else {
        let p = source.to_string();
        tokio::task::spawn_blocking(move || std::fs::read_to_string(&p))
            .await
            .map_err(|e| format!("read task crashed: {e}"))?
            .map_err(|e| format!("read {source}: {e}"))
    }
}

impl AgentApp {
    // ------------------------------------------------------------------
    // SECURITY control center (document / audit — all through the runtime)
    // ------------------------------------------------------------------

    /// Enter in the Security view: the versioned policy document (pretty)
    /// plus its revision history — the operator's editable record.
    pub fn security_doc(&mut self) {
        if self.op_join.is_some() {
            self.status = "another operator action is running — retry after it finishes".into();
            return;
        }
        let rt = self.runtime.clone();
        self.status = "loading policy document …".into();
        self.op_join = Some(tokio::spawn(async move {
            let doc = rt
                .policy_document()
                .await
                .map_err(|e| format!("policy document: {e}"))?;
            let pretty =
                serde_json::to_string_pretty(&doc).map_err(|e| format!("serialize: {e}"))?;
            let mut lines: Vec<String> = pretty.lines().map(str::to_string).collect();
            let versions = rt
                .policy_versions()
                .await
                .map_err(|e| format!("policy versions: {e}"))?;
            lines.push(String::new());
            lines.push(format!(
                "revisions: {}",
                versions
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            let res: Result<(String, Option<Vec<String>>), String> = Ok((
                format!(
                    "policy document v{} · {} rules · append-only",
                    doc.version,
                    doc.rules.len()
                ),
                Some(lines),
            ));
            res
        }));
    }

    /// `l`: the recent audit chain rows (actor / action / subject / result).
    pub fn security_audit(&mut self) {
        if self.op_join.is_some() {
            self.status = "another operator action is running — retry after it finishes".into();
            return;
        }
        let rt = self.runtime.clone();
        self.status = "reading audit chain …".into();
        self.op_join = Some(tokio::spawn(async move {
            let mut q = openrt_audit::AuditQuery::new();
            q.limit = Some(40);
            let recs = rt
                .audit_query(&q)
                .await
                .map_err(|e| format!("audit query: {e}"))?;
            let mut lines: Vec<String> = Vec::new();
            for r in &recs {
                lines.push(format!(
                    "{:>6} {:<14} {:<22} {:<28} {}",
                    r.seq, r.entry.actor, r.entry.action, r.entry.subject, r.entry.result
                ));
            }
            if lines.is_empty() {
                lines.push("(audit chain is empty)".into());
            }
            let res: Result<(String, Option<Vec<String>>), String> =
                Ok((format!("last {} audit row(s)", recs.len()), Some(lines)));
            res
        }));
    }
}
