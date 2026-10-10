//! `excel.*` / `ppt.*` / `docx.*` tools — pure-Rust OOXML create/modify/read
//! plus real render/export through LibreOffice when available.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use openrt_fabric::artifacts::ArtifactStore;
use openrt_fabric::{fail, FabricError, FabricMeta, RiskLevel, SupportState, ToolCategory};
use openrt_tools::{Tool, ToolError, ToolOutput, ToolSpec};
use openrt_verify::sha256_hex;

use crate::docx::{Block, Document};
use crate::pptx::{Deck, Slide};
use crate::soffice::Sofice;
use crate::xlsx::Workbook;

/// Shared context for office tools (artifact provenance + render backend).
pub struct OfficeCtx {
    pub artifacts: Arc<ArtifactStore>,
    pub soffice: Option<Sofice>,
    /// Why the render backend is unavailable (when `soffice` is `None`).
    pub soffice_reason: String,
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, FabricError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            FabricError::invalid_input(
                "office tool",
                format!("missing `{key}` string"),
                &format!("pass `\"{key}\": \"...\"`"),
            )
        })
}

fn arg_path<'a>(args: &'a Value) -> Result<&'a Path, FabricError> {
    let s = arg_str(args, "path")?;
    if s.trim().is_empty() {
        return Err(FabricError::invalid_input(
            "office tool",
            "`path` must not be empty",
            "pass a file path",
        ));
    }
    Ok(Path::new(s))
}

fn ensure_parent(path: &Path) -> Result<(), FabricError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| {
                FabricError::backend_failure(
                    "office file",
                    format!("cannot create parent dir `{}`: {e}", parent.display()),
                    "office.create",
                    "use a writable directory",
                )
            })?;
        }
    }
    Ok(())
}

/// Guard for create-style operations: refuse silent overwrite of an
/// existing file (high-risk office overwrite; also gated by the write cap).
fn ensure_create(path: &Path, overwrite: bool) -> Result<(), FabricError> {
    if path.exists() && !overwrite {
        return Err(FabricError::invalid_input(
            "office create",
            format!("`{}` already exists", path.display()),
            "pass \"overwrite\": true to replace it (high-risk office overwrite, gated by artifact.*.write)",
        ));
    }
    Ok(())
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, FabricError> {
    std::fs::read(path).map_err(|e| {
        FabricError::not_found(
            "office file",
            format!("cannot read `{}`: {e}", path.display()),
            "check the path; use absolute paths",
        )
    })
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), FabricError> {
    ensure_parent(path)?;
    std::fs::write(path, bytes).map_err(|e| {
        FabricError::backend_failure(
            "office file",
            format!("cannot write `{}`: {e}", path.display()),
            "office.create/write",
            "check permissions and disk space",
        )
    })
}

async fn record(
    ctx: &OfficeCtx,
    path: &Path,
    kind: &str,
    tool: &str,
    source_task: Option<String>,
    provenance: Value,
) -> Result<Value, FabricError> {
    ctx.artifacts
        .record_file(path, kind, tool, source_task, provenance)
        .await
        .map(|r| r.to_json())
        .map_err(|e| {
            FabricError::new(
                openrt_fabric::ErrorCode::IntegrityFailure,
                "artifact provenance record",
                e,
                "fabric.artifacts",
                "the file was written but could not be recorded; fix the store and re-run, or delete the file",
            )
        })
}

fn source_desc(path: &Path) -> Value {
    let sha = std::fs::read(path)
        .map(|b| sha256_hex(&b))
        .unwrap_or_default();
    json!({ "path": path.display().to_string(), "sha256": sha })
}

fn render_backend(ctx: &OfficeCtx) -> Result<&Sofice, FabricError> {
    ctx.soffice.as_ref().ok_or_else(|| {
        FabricError::unavailable(
            "office render backend",
            ctx.soffice_reason.clone(),
            "office.render/export",
            "install LibreOffice (create/read work without it)",
        )
    })
}

fn render_one(
    ctx: &OfficeCtx,
    input: &Path,
    fmt: &str,
    overwrite: bool,
) -> Result<(PathBuf, PathBuf), FabricError> {
    let out = input.with_extension(fmt);
    if out == input {
        return Err(FabricError::invalid_input(
            "office render",
            format!("output `{}` would overwrite the input file", out.display()),
            "use a source file with a different extension, or copy the file first",
        ));
    }
    if out.exists() && !overwrite {
        return Err(FabricError::invalid_input(
            "office render",
            format!("output `{}` already exists", out.display()),
            "pass \"overwrite\": true to replace the previous render (never touches the source)",
        ));
    }
    let backend = render_backend(ctx)?;
    let parent = input.parent().unwrap_or_else(|| Path::new("."));
    let produced = backend.convert(input, parent, fmt)?;
    Ok((produced, out))
}

// ---------------------------------------------------------------- excel ---

#[derive(Clone, Copy)]
pub enum ExcelOp {
    Create,
    Write,
    Read,
}

pub struct ExcelTool {
    pub spec: ToolSpec,
    pub ctx: Arc<OfficeCtx>,
    pub op: ExcelOp,
}

fn rows_arg(args: &Value) -> Result<Vec<Vec<Value>>, FabricError> {
    let rows = args.get("rows").and_then(Value::as_array).ok_or_else(|| {
        FabricError::invalid_input(
            "excel tool",
            "missing `rows` array",
            "pass rows as [[cell, cell], ...]",
        )
    })?;
    Ok(rows
        .iter()
        .map(|r| match r {
            Value::Array(cells) => cells.clone(),
            other => vec![other.clone()],
        })
        .collect())
}

fn excel_readout(wb: &Workbook) -> Value {
    json!({
        "sheets": wb.sheets.iter().map(|s| json!({
            "name": s.name,
            "row_count": s.row_count(),
            "rows": s.rows,
        })).collect::<Vec<_>>(),
    })
}

#[async_trait]
impl Tool for ExcelTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let tool = self.spec.name.clone();
        let res = self.run_inner(&args).await;
        match res {
            Ok(v) => Ok(ToolOutput::Json(v)),
            Err(e) => fail(&tool, e),
        }
    }
}

impl ExcelTool {
    async fn run_inner(&self, args: &Value) -> Result<Value, FabricError> {
        let path = arg_path(args)?.to_path_buf();
        let source_task = args
            .get("source_task")
            .and_then(Value::as_str)
            .map(str::to_string);
        let overwrite = args
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match self.op {
            ExcelOp::Create => {
                ensure_create(&path, overwrite)?;
                let sheet = args
                    .get("sheet")
                    .and_then(Value::as_str)
                    .unwrap_or("Sheet1")
                    .to_string();
                let rows = match args.get("rows") {
                    Some(_) => rows_arg(args)?,
                    None => Vec::new(),
                };
                let mut wb = Workbook::new(&sheet);
                if !rows.is_empty() {
                    wb.sheet_or_create(&sheet).write_rows(0, &rows);
                }
                let bytes = wb.to_bytes();
                write_bytes(&path, &bytes)?;
                // Self-verification: parse our own bytes back.
                Workbook::from_bytes(&bytes).map_err(|e| {
                    FabricError::verification_failed(
                        "xlsx structure",
                        format!("written file failed read-back: {e}"),
                        "report this as an openrt-office bug",
                    )
                })?;
                let artifact = record(
                    &self.ctx,
                    &path,
                    "excel",
                    "excel.create",
                    source_task,
                    json!({ "sheet": sheet, "rows": rows.len(), "format": "xlsx" }),
                )
                .await?;
                Ok(json!({
                    "ok": true,
                    "path": path.display().to_string(),
                    "sheets": [sheet],
                    "rows": rows.len(),
                    "verification": { "ok": true, "method": "OOXML read-back of written bytes" },
                    "artifact": artifact,
                }))
            }
            ExcelOp::Write => {
                if !path.is_file() {
                    return Err(FabricError::not_found(
                        "excel file",
                        format!("`{}` does not exist", path.display()),
                        "create it first with excel.create",
                    ));
                }
                let bytes = read_bytes(&path)?;
                let mut wb = Workbook::from_bytes(&bytes).map_err(|e| {
                    FabricError::backend_failure(
                        "excel file",
                        format!("cannot parse `{}`: {e}", path.display()),
                        "excel.write",
                        "the file is not a readable XLSX",
                    )
                })?;
                let sheet = arg_str(args, "sheet")?.to_string();
                let rows = rows_arg(args)?;
                let start = args
                    .get("start_row")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize);
                let target = wb.sheet_or_create(&sheet);
                let anchor = match start {
                    Some(s) => s,
                    None => target.next_row(),
                };
                target.write_rows(anchor, &rows);
                let bytes = wb.to_bytes();
                write_bytes(&path, &bytes)?;
                let back = Workbook::from_bytes(&bytes).map_err(|e| {
                    FabricError::verification_failed(
                        "xlsx structure",
                        format!("written file failed read-back: {e}"),
                        "report this as an openrt-office bug",
                    )
                })?;
                let written = back.sheet(&sheet).map(|s| s.row_count()).unwrap_or(0);
                let artifact = record(
                    &self.ctx,
                    &path,
                    "excel",
                    "excel.write",
                    source_task,
                    json!({ "sheet": sheet, "start_row": anchor, "rows": rows.len() }),
                )
                .await?;
                Ok(json!({
                    "ok": true,
                    "path": path.display().to_string(),
                    "sheet": sheet,
                    "start_row": anchor,
                    "rows_written": rows.len(),
                    "sheet_rows_now": written,
                    "verification": { "ok": true, "method": "OOXML read-back of written bytes" },
                    "artifact": artifact,
                }))
            }
            ExcelOp::Read => {
                let bytes = read_bytes(&path)?;
                let source_sha = sha256_hex(&bytes);
                let wb = Workbook::from_bytes(&bytes).map_err(|e| {
                    FabricError::backend_failure(
                        "excel file",
                        format!("cannot parse `{}`: {e}", path.display()),
                        "excel.read",
                        "the file is not a readable XLSX",
                    )
                })?;
                let mut out = excel_readout(&wb);
                if let Some(sheet) = args.get("sheet").and_then(Value::as_str) {
                    let sel = wb.sheet(sheet).ok_or_else(|| {
                        FabricError::not_found(
                            "excel sheet",
                            format!(
                                "sheet `{sheet}` not found (have: {:?})",
                                wb.sheets.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
                            ),
                            "pass an existing sheet name",
                        )
                    })?;
                    out = json!({
                        "sheets": [{
                            "name": sel.name,
                            "row_count": sel.row_count(),
                            "rows": sel.rows,
                        }],
                    });
                }
                out["ok"] = json!(true);
                out["source"] = source_desc(&path);
                out["source"]["sha256"] = json!(source_sha);
                out["verification"] =
                    json!({ "ok": true, "method": "SHA-256 of source bytes + OOXML parse" });
                Ok(out)
            }
        }
    }
}

// ------------------------------------------------------------------ ppt ---

#[derive(Clone, Copy)]
pub enum PptOp {
    Create,
    Render,
    ExportPdf,
}

pub struct PptTool {
    pub spec: ToolSpec,
    pub ctx: Arc<OfficeCtx>,
    pub op: PptOp,
}

#[async_trait]
impl Tool for PptTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let tool = self.spec.name.clone();
        let res = self.run_inner(&args).await;
        match res {
            Ok(v) => Ok(ToolOutput::Json(v)),
            Err(e) => fail(&tool, e),
        }
    }
}

impl PptTool {
    async fn run_inner(&self, args: &Value) -> Result<Value, FabricError> {
        let path = arg_path(args)?.to_path_buf();
        let source_task = args
            .get("source_task")
            .and_then(Value::as_str)
            .map(str::to_string);
        let overwrite = args
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match self.op {
            PptOp::Create => {
                ensure_create(&path, overwrite)?;
                let title = arg_str(args, "title")?.to_string();
                let mut deck = Deck::new(title.clone());
                let slides = args
                    .get("slides")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        FabricError::invalid_input(
                            "ppt.create",
                            "missing `slides` array",
                            "pass slides as [{\"title\": \"...\", \"bullets\": [\"...\"]}]",
                        )
                    })?;
                if slides.is_empty() {
                    return Err(FabricError::invalid_input(
                        "ppt.create",
                        "`slides` must not be empty",
                        "add at least one slide",
                    ));
                }
                for (i, s) in slides.iter().enumerate() {
                    let stitle = s
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("Slide {}", i + 1));
                    let bullets: Vec<String> = s
                        .get("bullets")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|b| b.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    deck.slides.push(Slide {
                        title: stitle,
                        bullets,
                    });
                }
                let bytes = deck.to_bytes();
                write_bytes(&path, &bytes)?;
                Deck::from_bytes(&bytes).map_err(|e| {
                    FabricError::verification_failed(
                        "pptx structure",
                        format!("written file failed read-back: {e}"),
                        "report this as an openrt-office bug",
                    )
                })?;
                let artifact = record(
                    &self.ctx,
                    &path,
                    "pptx",
                    "ppt.create",
                    source_task,
                    json!({ "title": title, "slides": deck.slides.len(), "format": "pptx" }),
                )
                .await?;
                Ok(json!({
                    "ok": true,
                    "path": path.display().to_string(),
                    "slides": deck.slides.len(),
                    "verification": { "ok": true, "method": "OOXML read-back of written bytes" },
                    "artifact": artifact,
                }))
            }
            PptOp::Render | PptOp::ExportPdf => {
                if !path.is_file() {
                    return Err(FabricError::not_found(
                        "ppt file",
                        format!("`{}` does not exist", path.display()),
                        "create it first with ppt.create",
                    ));
                }
                let fmt = if matches!(self.op, PptOp::Render) {
                    "png"
                } else {
                    "pdf"
                };
                let source_sha = sha256_hex(&read_bytes(&path)?);
                let (produced, _out) = render_one(&self.ctx, &path, fmt, overwrite)?;
                let kind = if fmt == "png" { "png" } else { "pdf" };
                let artifact = record(
                    &self.ctx,
                    &produced,
                    kind,
                    &self.spec.name,
                    source_task,
                    json!({
                        "source": { "path": path.display().to_string(), "sha256": source_sha },
                        "backend": "libreoffice-soffice",
                        "format": fmt,
                    }),
                )
                .await?;
                Ok(json!({
                    "ok": true,
                    "source": path.display().to_string(),
                    "output": produced.display().to_string(),
                    "format": fmt,
                    "verification": { "ok": true, "method": "soffice conversion produced a non-empty file (real document backend)" },
                    "artifact": artifact,
                }))
            }
        }
    }
}

// ---------------------------------------------------------------- docx ---

#[derive(Clone, Copy)]
pub enum DocxOp {
    Create,
    Read,
    ExportPdf,
}

pub struct DocxTool {
    pub spec: ToolSpec,
    pub ctx: Arc<OfficeCtx>,
    pub op: DocxOp,
}

fn blocks_from_json(blocks: &[Value]) -> Result<Vec<Block>, FabricError> {
    let mut out = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        let ty = b.get("type").and_then(Value::as_str).unwrap_or("");
        let text = b.get("text").and_then(Value::as_str).unwrap_or("");
        match ty {
            "heading" => out.push(Block::Heading {
                level: b
                    .get("level")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .clamp(1, 6) as u8,
                text: text.to_string(),
            }),
            "paragraph" => out.push(Block::Paragraph(text.to_string())),
            "bullet" => out.push(Block::Bullet(text.to_string())),
            "table" => {
                let header = b
                    .get("header")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| c.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let rows = b
                    .get("rows")
                    .and_then(Value::as_array)
                    .map(|rows| {
                        rows.iter()
                            .map(|r| {
                                r.as_array()
                                    .map(|a| {
                                        a.iter()
                                            .map(|c| {
                                                c.as_str()
                                                    .map(str::to_string)
                                                    .unwrap_or_else(|| c.to_string())
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(Block::Table { header, rows });
            }
            other => {
                return Err(FabricError::invalid_input(
                    "docx.create",
                    format!("blocks[{i}].type `{other}` is unknown"),
                    "use heading | paragraph | bullet | table",
                ))
            }
        }
    }
    Ok(out)
}

#[async_trait]
impl Tool for DocxTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let tool = self.spec.name.clone();
        let res = self.run_inner(&args).await;
        match res {
            Ok(v) => Ok(ToolOutput::Json(v)),
            Err(e) => fail(&tool, e),
        }
    }
}

impl DocxTool {
    async fn run_inner(&self, args: &Value) -> Result<Value, FabricError> {
        let path = arg_path(args)?.to_path_buf();
        let source_task = args
            .get("source_task")
            .and_then(Value::as_str)
            .map(str::to_string);
        let overwrite = args
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match self.op {
            DocxOp::Create => {
                ensure_create(&path, overwrite)?;
                let title = args
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let blocks_json = args
                    .get("blocks")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        FabricError::invalid_input(
                            "docx.create",
                            "missing `blocks` array",
                            "pass blocks as [{\"type\":\"heading\",\"level\":1,\"text\":\"...\"}, ...]",
                        )
                    })?;
                let blocks = blocks_from_json(blocks_json)?;
                let doc = Document::new(title.clone());
                let mut doc = doc;
                doc.blocks = blocks;
                let bytes = doc.to_bytes();
                write_bytes(&path, &bytes)?;
                Document::from_bytes(&bytes).map_err(|e| {
                    FabricError::verification_failed(
                        "docx structure",
                        format!("written file failed read-back: {e}"),
                        "report this as an openrt-office bug",
                    )
                })?;
                let artifact = record(
                    &self.ctx,
                    &path,
                    "docx",
                    "docx.create",
                    source_task,
                    json!({ "title": title, "blocks": doc.blocks.len(), "format": "docx" }),
                )
                .await?;
                Ok(json!({
                    "ok": true,
                    "path": path.display().to_string(),
                    "blocks": doc.blocks.len(),
                    "verification": { "ok": true, "method": "OOXML read-back of written bytes" },
                    "artifact": artifact,
                }))
            }
            DocxOp::Read => {
                let bytes = read_bytes(&path)?;
                let doc = Document::from_bytes(&bytes).map_err(|e| {
                    FabricError::backend_failure(
                        "docx file",
                        format!("cannot parse `{}`: {e}", path.display()),
                        "docx.read",
                        "the file is not a readable DOCX",
                    )
                })?;
                Ok(json!({
                    "ok": true,
                    "source": source_desc(&path),
                    "document": doc.text_summary(),
                    "verification": { "ok": true, "method": "SHA-256 of source bytes + OOXML parse" },
                }))
            }
            DocxOp::ExportPdf => {
                if !path.is_file() {
                    return Err(FabricError::not_found(
                        "docx file",
                        format!("`{}` does not exist", path.display()),
                        "create it first with docx.create",
                    ));
                }
                let source_sha = sha256_hex(&read_bytes(&path)?);
                let (produced, _out) = render_one(&self.ctx, &path, "pdf", overwrite)?;
                let artifact = record(
                    &self.ctx,
                    &produced,
                    "pdf",
                    "docx.export_pdf",
                    source_task,
                    json!({
                        "source": { "path": path.display().to_string(), "sha256": source_sha },
                        "backend": "libreoffice-soffice",
                        "format": "pdf",
                    }),
                )
                .await?;
                Ok(json!({
                    "ok": true,
                    "source": path.display().to_string(),
                    "output": produced.display().to_string(),
                    "format": "pdf",
                    "verification": { "ok": true, "method": "soffice conversion produced a non-empty file (real document backend)" },
                    "artifact": artifact,
                }))
            }
        }
    }
}

// -------------------------------------------------------------- factory ---

/// All office tools over one shared context.
pub fn office_tools(ctx: Arc<OfficeCtx>) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();

    tools.push(Arc::new(ExcelTool {
        spec: ToolSpec::new(
            "excel.create",
            "Create a new XLSX workbook (real OOXML) with optional initial rows; refuses to overwrite an existing file unless `overwrite` is true.",
        )
        .with_caps(&["artifact.excel.write"])
        .with_schema(json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" },
                "sheet": { "type": "string", "default": "Sheet1" },
                "rows": { "type": "array", "items": { "type": "array" } },
                "overwrite": { "type": "boolean", "default": false },
                "source_task": { "type": "string" }
            }
        })),
        ctx: ctx.clone(),
        op: ExcelOp::Create,
    }));
    tools.push(Arc::new(ExcelTool {
        spec: ToolSpec::new(
            "excel.write",
            "Modify an existing XLSX workbook: write rows into a sheet at an anchor row (append by default).",
        )
        .with_caps(&["artifact.excel.write"])
        .with_schema(json!({
            "type": "object",
            "required": ["path", "sheet", "rows"],
            "properties": {
                "path": { "type": "string" },
                "sheet": { "type": "string" },
                "rows": { "type": "array", "items": { "type": "array" } },
                "start_row": { "type": "integer", "minimum": 0 },
                "source_task": { "type": "string" }
            }
        })),
        ctx: ctx.clone(),
        op: ExcelOp::Write,
    }));
    tools.push(Arc::new(ExcelTool {
        spec: ToolSpec::new(
            "excel.read",
            "Read an XLSX workbook: sheet list and cell values, with source SHA-256 for integrity.",
        )
        .with_caps(&["artifact.excel.read"])
        .with_schema(json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" },
                "sheet": { "type": "string" }
            }
        })),
        ctx: ctx.clone(),
        op: ExcelOp::Read,
    }));

    tools.push(Arc::new(PptTool {
        spec: ToolSpec::new(
            "ppt.create",
            "Create a real PPTX deck (title + slides with bullets); refuses silent overwrite unless `overwrite` is true.",
        )
        .with_caps(&["artifact.ppt.write"])
        .with_schema(json!({
            "type": "object",
            "required": ["path", "title", "slides"],
            "properties": {
                "path": { "type": "string" },
                "title": { "type": "string" },
                "slides": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["title"],
                        "properties": {
                            "title": { "type": "string" },
                            "bullets": { "type": "array", "items": { "type": "string" } }
                        }
                    }
                },
                "overwrite": { "type": "boolean", "default": false },
                "source_task": { "type": "string" }
            }
        })),
        ctx: ctx.clone(),
        op: PptOp::Create,
    }));
    tools.push(Arc::new(PptTool {
        spec: ToolSpec::new(
            "ppt.render",
            "Render the first slide of a PPTX to PNG through the real document backend (LibreOffice). Source file is never modified.",
        )
        .with_caps(&["artifact.ppt.read"])
        .with_schema(json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" },
                "overwrite": { "type": "boolean", "default": false },
                "source_task": { "type": "string" }
            }
        })),
        ctx: ctx.clone(),
        op: PptOp::Render,
    }));
    tools.push(Arc::new(PptTool {
        spec: ToolSpec::new(
            "ppt.export_pdf",
            "Export a PPTX to PDF through the real document backend (LibreOffice). Source file is never modified.",
        )
        .with_caps(&["artifact.ppt.read"])
        .with_schema(json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" },
                "overwrite": { "type": "boolean", "default": false },
                "source_task": { "type": "string" }
            }
        })),
        ctx: ctx.clone(),
        op: PptOp::ExportPdf,
    }));

    tools.push(Arc::new(DocxTool {
        spec: ToolSpec::new(
            "docx.create",
            "Create a real DOCX document (title + heading/paragraph/bullet/table blocks); refuses silent overwrite unless `overwrite` is true.",
        )
        .with_caps(&["artifact.docx.write"])
        .with_schema(json!({
            "type": "object",
            "required": ["path", "blocks"],
            "properties": {
                "path": { "type": "string" },
                "title": { "type": "string" },
                "blocks": { "type": "array", "items": { "type": "object" } },
                "overwrite": { "type": "boolean", "default": false },
                "source_task": { "type": "string" }
            }
        })),
        ctx: ctx.clone(),
        op: DocxOp::Create,
    }));
    tools.push(Arc::new(DocxTool {
        spec: ToolSpec::new(
            "docx.read",
            "Read a DOCX document: structured blocks (headings, paragraphs, bullets, tables) with source SHA-256.",
        )
        .with_caps(&["artifact.docx.read"])
        .with_schema(json!({
            "type": "object",
            "required": ["path"],
            "properties": { "path": { "type": "string" } }
        })),
        ctx: ctx.clone(),
        op: DocxOp::Read,
    }));
    tools.push(Arc::new(DocxTool {
        spec: ToolSpec::new(
            "docx.export_pdf",
            "Export a DOCX to PDF through the real document backend (LibreOffice). Source file is never modified.",
        )
        .with_caps(&["artifact.docx.read"])
        .with_schema(json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" },
                "overwrite": { "type": "boolean", "default": false },
                "source_task": { "type": "string" }
            }
        })),
        ctx,
        op: DocxOp::ExportPdf,
    }));

    tools
}

/// Catalog metadata for every office tool.
pub fn office_metas(host: &str) -> Vec<FabricMeta> {
    vec![
        FabricMeta::new("excel.create", ToolCategory::Office, RiskLevel::Medium)
            .with_platform(host, SupportState::Green)
            .verify()
            .side_effects(&["writes an XLSX file"])
            .rollback("delete the file (or overwrite with prior content)"),
        FabricMeta::new("excel.write", ToolCategory::Office, RiskLevel::High)
            .with_platform(host, SupportState::Green)
            .verify()
            .side_effects(&["modifies an existing XLSX file"])
            .rollback("restore from a copy (no automatic undo)"),
        FabricMeta::new("excel.read", ToolCategory::Office, RiskLevel::Low)
            .with_platform(host, SupportState::Green)
            .observe()
            .side_effects(&["none (read-only + digest)"]),
        FabricMeta::new("ppt.create", ToolCategory::Office, RiskLevel::Medium)
            .with_platform(host, SupportState::Green)
            .verify()
            .side_effects(&["writes a PPTX file"])
            .rollback("delete the file"),
        FabricMeta::new("ppt.render", ToolCategory::Office, RiskLevel::Low)
            .with_platform(host, SupportState::Partial)
            .verify()
            .side_effects(&["renders PNG next to the source (soffice)"])
            .rollback("delete the rendered PNG"),
        FabricMeta::new("ppt.export_pdf", ToolCategory::Office, RiskLevel::Low)
            .with_platform(host, SupportState::Partial)
            .verify()
            .side_effects(&["writes a derived PDF (soffice)"])
            .rollback("delete the PDF"),
        FabricMeta::new("docx.create", ToolCategory::Office, RiskLevel::Medium)
            .with_platform(host, SupportState::Green)
            .verify()
            .side_effects(&["writes a DOCX file"])
            .rollback("delete the file"),
        FabricMeta::new("docx.read", ToolCategory::Office, RiskLevel::Low)
            .with_platform(host, SupportState::Green)
            .observe()
            .side_effects(&["none (read-only + digest)"]),
        FabricMeta::new("docx.export_pdf", ToolCategory::Office, RiskLevel::Low)
            .with_platform(host, SupportState::Partial)
            .verify()
            .side_effects(&["writes a derived PDF (soffice)"])
            .rollback("delete the PDF"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn ctx() -> Arc<OfficeCtx> {
        Arc::new(OfficeCtx {
            artifacts: Arc::new(ArtifactStore::new(Arc::new(MemoryStore::new()))),
            soffice: None,
            soffice_reason: "disabled in this test".into(),
        })
    }

    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("openrt-office-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    async fn run_json(tool: &Arc<dyn Tool>, args: Value) -> Value {
        match tool.run(args).await.expect("tool must not error") {
            ToolOutput::Json(v) => v,
            ToolOutput::Text(t) => json!({ "text": t }),
        }
    }

    fn by_name(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
        tools
            .iter()
            .find(|t| t.spec().name == name)
            .unwrap_or_else(|| panic!("tool {name} missing"))
            .clone()
    }

    #[tokio::test]
    async fn excel_create_write_read_roundtrip() {
        let tools = office_tools(ctx());
        let dir = temp_dir();
        let path = dir.join("wb.xlsx").display().to_string();

        let created = run_json(
            &by_name(&tools, "excel.create"),
            json!({ "path": path, "sheet": "Data", "rows": [["a", 1], ["b", 2]] }),
        )
        .await;
        assert_eq!(created["ok"], json!(true));
        assert_eq!(created["rows"], json!(2));
        assert_eq!(created["artifact"]["sha256"].as_str().unwrap().len(), 64);

        let written = run_json(
            &by_name(&tools, "excel.write"),
            json!({ "path": path, "sheet": "Data", "rows": [["c", 3]] }),
        )
        .await;
        assert_eq!(written["start_row"], json!(2));
        assert_eq!(written["rows_written"], json!(1));

        let read = run_json(&by_name(&tools, "excel.read"), json!({ "path": path })).await;
        assert_eq!(read["ok"], json!(true));
        let rows = read["sheets"][0]["rows"].clone();
        assert_eq!(rows.as_array().unwrap().len(), 3);
        assert_eq!(read["source"]["sha256"].as_str().unwrap().len(), 64);
    }

    #[tokio::test]
    async fn create_refuses_silent_overwrite() {
        let tools = office_tools(ctx());
        let dir = temp_dir();
        let path = dir.join("deck.pptx").display().to_string();
        let args = json!({
            "path": path, "title": "T",
            "slides": [{ "title": "S1", "bullets": ["b"] }]
        });
        run_json(&by_name(&tools, "ppt.create"), args.clone()).await;
        let err = by_name(&tools, "ppt.create").run(args).await;
        assert!(matches!(err, Err(ToolError::InvalidArguments(_, _))));
        // overwrite:true succeeds
        let ok = run_json(
            &by_name(&tools, "ppt.create"),
            json!({
                "path": path, "title": "T2", "overwrite": true,
                "slides": [{ "title": "S1" }]
            }),
        )
        .await;
        assert_eq!(ok["ok"], json!(true));
    }

    #[tokio::test]
    async fn docx_create_read_roundtrip_with_blocks() {
        let tools = office_tools(ctx());
        let dir = temp_dir();
        let path = dir.join("doc.docx").display().to_string();
        run_json(
            &by_name(&tools, "docx.create"),
            json!({
                "path": path, "title": "Report",
                "blocks": [
                    { "type": "heading", "level": 1, "text": "Intro" },
                    { "type": "paragraph", "text": "Body & <tags>" },
                    { "type": "bullet", "text": "one" },
                    { "type": "table", "header": ["k", "v"], "rows": [["x", "1"]] }
                ]
            }),
        )
        .await;
        let read = run_json(&by_name(&tools, "docx.read"), json!({ "path": path })).await;
        assert_eq!(read["document"]["title"], json!("Report"));
        let blocks = read["document"]["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[1]["text"], json!("Body & <tags>"));
        assert_eq!(read["source"]["sha256"].as_str().unwrap().len(), 64);
    }

    /// The strongest claim of Phase 7B: **LibreOffice really opens the files
    /// we write** (create → real `soffice --convert-to pdf` for all three
    /// formats) and the render/export tools produce recorded artifacts.
    /// Skipped (not failed) only if LibreOffice is genuinely absent.
    #[tokio::test]
    async fn real_libreoffice_opens_all_three_formats() {
        let (so, reason) = crate::detect_soffice(None);
        let so = match so {
            Some(s) => s,
            None => {
                eprintln!("SKIP real LibreOffice check: {reason}");
                return;
            }
        };
        let tools = office_tools(Arc::new(OfficeCtx {
            artifacts: Arc::new(ArtifactStore::new(Arc::new(MemoryStore::new()))),
            soffice: Some(so.clone()),
            soffice_reason: String::new(),
        }));
        let dir = temp_dir();

        // --- XLSX opens in LibreOffice (PDF export of all sheets) ---
        let xls = dir.join("wb.xlsx");
        run_json(
            &by_name(&tools, "excel.create"),
            json!({ "path": xls.display().to_string(), "rows": [["n", 1], ["m", 2]] }),
        )
        .await;
        let pdf = so
            .convert(&xls, &dir, "pdf")
            .expect("soffice must open our xlsx");
        assert!(std::fs::metadata(&pdf).unwrap().len() > 1000);

        // --- PPTX opens in LibreOffice via the render tool ---
        let pptx = dir.join("deck.pptx");
        run_json(
            &by_name(&tools, "ppt.create"),
            json!({
                "path": pptx.display().to_string(), "title": "Q3 Report",
                "slides": [
                    { "title": "Summary", "bullets": ["up", "right"] },
                    { "title": "Risks" }
                ]
            }),
        )
        .await;
        let exported = run_json(
            &by_name(&tools, "ppt.export_pdf"),
            json!({ "path": pptx.display().to_string() }),
        )
        .await;
        assert_eq!(exported["ok"], json!(true));
        let out = PathBuf::from(exported["output"].as_str().unwrap());
        assert!(out.is_file() && std::fs::metadata(&out).unwrap().len() > 1000);
        assert_eq!(exported["artifact"]["kind"], json!("pdf"));

        // --- DOCX opens in LibreOffice via the render tool ---
        let docx = dir.join("doc.docx");
        run_json(
            &by_name(&tools, "docx.create"),
            json!({
                "path": docx.display().to_string(), "title": "Report",
                "blocks": [{ "type": "heading", "level": 1, "text": "Intro" }]
            }),
        )
        .await;
        let exported = run_json(
            &by_name(&tools, "docx.export_pdf"),
            json!({ "path": docx.display().to_string() }),
        )
        .await;
        assert_eq!(exported["ok"], json!(true));
        let out = PathBuf::from(exported["output"].as_str().unwrap());
        assert!(out.is_file() && std::fs::metadata(&out).unwrap().len() > 1000);

        // Render tool with backend disabled -> honest UNAVAILABLE (ok:false).
        let disabled = office_tools(ctx());
        let disabled_out = match by_name(&disabled, "docx.export_pdf")
            .run(json!({ "path": docx.display().to_string(), "overwrite": true }))
            .await
        {
            Ok(ToolOutput::Json(v)) => v,
            other => panic!("expected structured output, got {other:?}"),
        };
        assert_eq!(disabled_out["ok"], json!(false));
        assert_eq!(disabled_out["error"]["code"], json!("UNAVAILABLE"));
    }

    #[tokio::test]
    async fn export_pdf_missing_source_is_structured_error() {
        let tools = office_tools(ctx());
        let missing = temp_dir().join("definitely-missing.docx");
        let out = by_name(&tools, "docx.export_pdf")
            .run(json!({ "path": missing.display().to_string() }))
            .await;
        match out {
            Err(_) => {}
            Ok(ToolOutput::Json(v)) => assert_eq!(
                v["ok"],
                json!(false),
                "missing source must be reported as ok:false: {v}"
            ),
            Ok(other) => panic!("unexpected output: {other:?}"),
        }
    }

    #[tokio::test]
    async fn render_refuses_overwriting_the_source() {
        let tools = office_tools(ctx());
        let dir = temp_dir();
        let pdf = dir.join("a.pdf");
        std::fs::write(&pdf, b"pretend pdf").unwrap();
        let err = by_name(&tools, "docx.export_pdf")
            .run(json!({ "path": pdf.display().to_string(), "overwrite": true }))
            .await;
        assert!(matches!(err, Err(ToolError::InvalidArguments(_, _))));
    }
}
