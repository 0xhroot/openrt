//! Deterministic chunking engine (Phase 4 P4.5).
//!
//! * Strategies: `Fixed`, `Paragraph`, `Heading`, `Page`, `Table`, and
//!   `Overlap` (fixed with overlap).
//! * Chunk ids are stable and deterministic: they derive from the content
//!   hash, document version, the strategy, and the chunk's ordinal — identical
//!   input + config always yields identical ids.
//! * Provenance at chunk granularity: every chunk records its document id,
//!   document version, source block keys, page (when known), and its own
//!   content hash.
//! * Characters are never labelled as tokens; the model just splits on
//!   character ranges inside verified text boundaries.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::normalize::{BlockKind, NormalizedDocumentV1};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkStrategy {
    Fixed,
    Paragraph,
    Heading,
    Page,
    Table,
    Overlap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkConfig {
    pub strategy: ChunkStrategy,
    /// Max characters per chunk (Fixed/Overlap) or per unit (others).
    pub max_chars: usize,
    /// Overlap characters (only for `Overlap`; must be < max_chars).
    pub overlap_chars: usize,
    /// Minimum characters; smaller final tails are still emitted as honest
    /// tail chunks rather than silently dropped.
    pub min_chars: usize,
    /// Emit each table block as its own chunk regardless of size.
    pub table_aware: bool,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            strategy: ChunkStrategy::Paragraph,
            max_chars: 2000,
            overlap_chars: 0,
            min_chars: 1,
            table_aware: true,
        }
    }
}

impl ChunkConfig {
    pub fn validate(&self) -> Result<(), ChunkError> {
        if self.max_chars == 0 {
            return Err(ChunkError::Invalid("max_chars must be > 0".into()));
        }
        if self.overlap_chars >= self.max_chars {
            return Err(ChunkError::Invalid(
                "overlap_chars must be < max_chars".into(),
            ));
        }
        if self.strategy != ChunkStrategy::Overlap && self.overlap_chars != 0 {
            return Err(ChunkError::Invalid(
                "overlap_chars is only valid for the overlap strategy".into(),
            ));
        }
        if self.min_chars > self.max_chars {
            return Err(ChunkError::Invalid("min_chars must be <= max_chars".into()));
        }
        Ok(())
    }
}

// `Eq` is dropped in M9.4: temporal provenance (`t_start`/`t_end`) is `f64`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub chunk_id: String,
    pub doc_id: String,
    pub document_version: u32,
    pub block_keys: Vec<String>,
    pub page: Option<u32>,
    pub text: String,
    pub sha256: String,
    /// Ordinal within the document (0-based), stable.
    pub index: u32,
    /// Byte offset of this chunk's text inside the document's full text
    /// (reading order, blocks joined by `\n`) — `None` when the chunk does
    /// not map to a contiguous run of blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub char_start: Option<usize>,
    /// Byte offset one past this chunk's text inside the document's full
    /// text (see `char_start`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub char_end: Option<usize>,
    /// Media timeline start (seconds) — propagated from the constituent
    /// blocks' provenance (M9.4); `None` for page-bound or non-temporal
    /// content. Never invented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_start: Option<f64>,
    /// Media timeline end (seconds) — see `t_start`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_end: Option<f64>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChunkError {
    #[error("invalid chunk config: {0}")]
    Invalid(String),
    #[error("document has no text blocks to chunk")]
    Empty,
}

/// Deterministic chunk id = `ck/<strategy>/<doc-hash>-v<ver>-<index>` with the
/// strategy folded into the key so a config change forces new ids (index
/// rebuilds detect the change via chunk id + config hash).
pub fn chunk_id(doc_id: &str, version: u32, strategy: ChunkStrategy, index: u32) -> String {
    let s = match strategy {
        ChunkStrategy::Fixed => "fixed",
        ChunkStrategy::Paragraph => "para",
        ChunkStrategy::Heading => "head",
        ChunkStrategy::Page => "page",
        ChunkStrategy::Table => "table",
        ChunkStrategy::Overlap => "overlap",
    };
    format!("ck/{s}/{doc_id}-v{version}-{index}")
}

/// Chunk a normalised document with the given configuration.
pub fn chunk_normalized(
    doc: &NormalizedDocumentV1,
    cfg: &ChunkConfig,
) -> Result<Vec<Chunk>, ChunkError> {
    cfg.validate()?;
    if doc.blocks.is_empty() {
        return Err(ChunkError::Empty);
    }
    let mut chunks = Vec::new();
    // Byte offsets of each block's text inside `full_text()` (blocks joined
    // with a single `\n` — mirrors `NormalizedDocumentV1::full_text`).
    let mut starts: Vec<usize> = Vec::with_capacity(doc.blocks.len());
    {
        let mut off = 0usize;
        for (i, b) in doc.blocks.iter().enumerate() {
            if i > 0 {
                off += 1;
            }
            starts.push(off);
            off += b.text.len();
        }
    }
    match cfg.strategy {
        ChunkStrategy::Page => {
            for page in 1..=doc.pages.max(1) {
                let blocks: Vec<_> = doc
                    .blocks
                    .iter()
                    .filter(|b| b.page.map(|p| p == page).unwrap_or(true))
                    .collect();
                if blocks.is_empty() {
                    continue;
                }
                push_chunk(doc, cfg, &mut chunks, &blocks, &starts);
            }
        }
        ChunkStrategy::Heading => {
            // Group blocks under headings; text before the first heading is one
            // "preamble" chunk.
            let mut group: Vec<&crate::normalize::Block> = Vec::new();
            for b in &doc.blocks {
                if b.kind == BlockKind::Heading && !group.is_empty() {
                    push_chunk(doc, cfg, &mut chunks, &group, &starts);
                    group = Vec::new();
                }
                group.push(b);
            }
            if !group.is_empty() {
                push_chunk(doc, cfg, &mut chunks, &group, &starts);
            }
        }
        ChunkStrategy::Paragraph => {
            for b in &doc.blocks {
                if cfg.table_aware && b.kind == BlockKind::Table {
                    push_chunk(doc, cfg, &mut chunks, &[b], &starts);
                    continue;
                }
                push_chunk(doc, cfg, &mut chunks, &[b], &starts);
            }
        }
        ChunkStrategy::Table => {
            for b in &doc.blocks {
                if b.kind == BlockKind::Table || cfg.table_aware {
                    push_chunk(doc, cfg, &mut chunks, &[b], &starts);
                }
            }
            if chunks.is_empty() {
                for b in &doc.blocks {
                    push_chunk(doc, cfg, &mut chunks, &[b], &starts);
                }
            }
        }
        ChunkStrategy::Fixed | ChunkStrategy::Overlap => {
            let text = doc.full_text();
            let overlap = if cfg.strategy == ChunkStrategy::Overlap {
                cfg.overlap_chars
            } else {
                0
            };
            let step = cfg.max_chars.saturating_sub(overlap).max(1);
            let chars: Vec<char> = text.chars().collect();
            if chars.is_empty() {
                return Err(ChunkError::Empty);
            }
            // Byte offset of every char boundary in `full_text`.
            let mut byte_at: Vec<usize> = Vec::with_capacity(chars.len() + 1);
            {
                let mut acc = 0usize;
                byte_at.push(0);
                for c in &chars {
                    acc += c.len_utf8();
                    byte_at.push(acc);
                }
            }
            let mut pos = 0usize;
            let mut idx = 0u32;
            while pos < chars.len() {
                let end = (pos + cfg.max_chars).min(chars.len());
                let seg: String = chars[pos..end].iter().collect();
                let page = seg.contains('\n').then_some(doc.pages.max(1));
                let sha256 = crate::sha256_hex_for(seg.as_bytes());
                chunks.push(Chunk {
                    chunk_id: chunk_id(&doc.doc_id, doc.version, cfg.strategy, idx),
                    doc_id: doc.doc_id.clone(),
                    document_version: doc.version,
                    block_keys: vec!["fixed".into()],
                    page,
                    text: seg,
                    sha256,
                    index: idx,
                    char_start: Some(byte_at[pos]),
                    char_end: Some(byte_at[end]),
                    // Character windows are not block-aligned, so temporal
                    // provenance cannot be attributed honestly.
                    t_start: None,
                    t_end: None,
                });
                idx += 1;
                if end >= chars.len() {
                    break;
                }
                pos += step;
            }
        }
    }
    if chunks.is_empty() {
        return Err(ChunkError::Empty);
    }
    Ok(chunks)
}

fn push_chunk(
    doc: &NormalizedDocumentV1,
    _cfg: &ChunkConfig,
    chunks: &mut Vec<Chunk>,
    blocks: &[&crate::normalize::Block],
    starts: &[usize],
) {
    if blocks.is_empty() {
        return;
    }
    let text: String = blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return;
    }
    let page = blocks.iter().find_map(|b| b.page);
    let index = chunks.len() as u32;
    let sha256 = crate::sha256_hex_for(text.as_bytes());
    // Offsets only when the contributing blocks form a contiguous run in
    // reading order (so `full_text[start..end] == text` is exact).
    let (char_start, char_end) = contiguous_offsets(doc, blocks, starts);
    // Temporal provenance: the span covered by the constituent blocks'
    // backend-reported media times — only what the sources actually carry.
    let starts_t: Vec<f64> = blocks.iter().filter_map(|b| b.source.t_start).collect();
    let ends_t: Vec<f64> = blocks.iter().filter_map(|b| b.source.t_end).collect();
    chunks.push(Chunk {
        chunk_id: chunk_id(&doc.doc_id, doc.version, _cfg.strategy, index),
        doc_id: doc.doc_id.clone(),
        document_version: doc.version,
        block_keys: blocks.iter().map(|b| b.key.clone()).collect(),
        page,
        text,
        sha256,
        index,
        char_start,
        char_end,
        t_start: (!starts_t.is_empty()).then(|| starts_t.into_iter().fold(f64::INFINITY, f64::min)),
        t_end: (!ends_t.is_empty()).then(|| ends_t.into_iter().fold(f64::NEG_INFINITY, f64::max)),
    });
}

/// `(start, end)` byte offsets of a contiguous block run inside the
/// document's full text; `None` when the run is not contiguous.
fn contiguous_offsets(
    doc: &NormalizedDocumentV1,
    blocks: &[&crate::normalize::Block],
    starts: &[usize],
) -> (Option<usize>, Option<usize>) {
    let positions: Option<Vec<usize>> = blocks
        .iter()
        .map(|b| doc.blocks.iter().position(|d| d.key == b.key))
        .collect();
    let Some(positions) = positions else {
        return (None, None);
    };
    let first = positions[0];
    for (k, &p) in positions.iter().enumerate() {
        if p != first + k {
            return (None, None);
        }
    }
    let last_key = &blocks[blocks.len() - 1].key;
    let last_idx = doc
        .blocks
        .iter()
        .position(|d| &d.key == last_key)
        .unwrap_or(first);
    let start = starts.get(first).copied();
    let end = starts
        .get(last_idx)
        .map(|s| s + doc.blocks[last_idx].text.len());
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::normalize_text;

    #[test]
    fn config_validation() {
        let bad = ChunkConfig {
            strategy: ChunkStrategy::Overlap,
            max_chars: 100,
            overlap_chars: 100,
            min_chars: 1,
            table_aware: true,
        };
        assert!(bad.validate().is_err());
        let overlap_nonstrategy = ChunkConfig {
            strategy: ChunkStrategy::Fixed,
            max_chars: 100,
            overlap_chars: 5,
            min_chars: 1,
            table_aware: true,
        };
        assert!(overlap_nonstrategy.validate().is_err());
        assert!(ChunkConfig::default().validate().is_ok());
    }

    #[test]
    fn fixed_chunks_are_stable_ids() {
        let doc = normalize_text(b"aaa bbb ccc ddd eee fff").unwrap();
        let cfg = ChunkConfig {
            strategy: ChunkStrategy::Fixed,
            max_chars: 8,
            overlap_chars: 0,
            min_chars: 1,
            table_aware: true,
        };
        let a = chunk_normalized(&doc, &cfg).unwrap();
        let b = chunk_normalized(&doc, &cfg).unwrap();
        assert_eq!(a.len(), b.len());
        assert_eq!(a[0].chunk_id, b[0].chunk_id);
        assert!(a[0].chunk_id.starts_with("ck/fixed/"));
    }

    #[test]
    fn paragraph_strategy_respects_blocks() {
        let doc = normalize_text(b"one two\n\nthree four").unwrap();
        let cfg = ChunkConfig {
            strategy: ChunkStrategy::Paragraph,
            max_chars: 10,
            overlap_chars: 0,
            min_chars: 1,
            table_aware: true,
        };
        let chunks = chunk_normalized(&doc, &cfg).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].block_keys, vec!["b0"]);
        assert_eq!(chunks[1].block_keys, vec!["b1"]);
    }

    #[test]
    fn never_labels_chars_as_tokens() {
        let doc = normalize_text(b"hello world").unwrap();
        let cfg = ChunkConfig {
            strategy: ChunkStrategy::Fixed,
            max_chars: 4,
            overlap_chars: 0,
            min_chars: 1,
            table_aware: true,
        };
        for c in chunk_normalized(&doc, &cfg).unwrap() {
            assert!(!c.text.contains("token"));
            assert_eq!(c.sha256, crate::sha256_hex_for(c.text.as_bytes()));
        }
    }

    #[test]
    fn overlap_adjacent_chunks() {
        let doc = normalize_text(b"abcdefghijk").unwrap();
        let cfg = ChunkConfig {
            strategy: ChunkStrategy::Overlap,
            max_chars: 5,
            overlap_chars: 2,
            min_chars: 1,
            table_aware: true,
        };
        let chunks = chunk_normalized(&doc, &cfg).unwrap();
        let joined: String = chunks
            .iter()
            .map(|c| c.text.clone())
            .collect::<Vec<_>>()
            .join("|");
        assert!(joined.contains("abcde") && joined.contains("defg"));
    }

    #[test]
    fn empty_document_errors() {
        let doc = normalize_text(b"").unwrap();
        // Empty text block is filtered as empty text, so the doc chunks empty.
        let cfg = ChunkConfig::default();
        let r = chunk_normalized(&doc, &cfg);
        assert!(r.is_err() || r.unwrap_or_default().is_empty());
    }

    #[test]
    fn paragraph_chunks_carry_byte_offsets() {
        let doc = normalize_text(b"alpha\n\nbeta").unwrap();
        let cfg = ChunkConfig::default();
        let chunks = chunk_normalized(&doc, &cfg).unwrap();
        let full = doc.full_text();
        for c in &chunks {
            let s = c.char_start.expect("offset present");
            let e = c.char_end.expect("offset present");
            assert_eq!(&full[s..e], c.text);
        }
    }

    #[test]
    fn fixed_chunks_offsets_slice_full_text() {
        let doc = normalize_text(b"abcdefghijkl").unwrap();
        let cfg = ChunkConfig {
            strategy: ChunkStrategy::Fixed,
            max_chars: 5,
            overlap_chars: 0,
            min_chars: 1,
            table_aware: true,
        };
        let full = doc.full_text();
        for c in chunk_normalized(&doc, &cfg).unwrap() {
            let s = c.char_start.unwrap();
            let e = c.char_end.unwrap();
            assert_eq!(&full[s..e], c.text);
        }
    }

    #[test]
    fn heading_strategy_groups() {
        let doc =
            crate::normalize::normalize_markdown(b"# Intro\npreamble\n\n# Body\ndetail").unwrap();
        let cfg = ChunkConfig {
            strategy: ChunkStrategy::Heading,
            max_chars: 1000,
            overlap_chars: 0,
            min_chars: 1,
            table_aware: true,
        };
        let chunks = chunk_normalized(&doc, &cfg).unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].text.contains("preamble"));
        assert!(chunks[1].text.contains("detail"));
    }
}
