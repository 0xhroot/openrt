//! OOXML (docx/xlsx/pptx) → normalised document.
//!
//! Uses the real office-fabric readers (`openrt-office`): genuine ZIP + XML
//! parsing, no text fabrication. A file that fails to parse surfaces as a
//! structured `Ooxml` error — never silently ingested as something else.

use openrt_office::{Block as OoBlock, Deck, Document as OoDocument, Workbook};

use crate::normalize::{
    Block, BlockKind, BlockSource, ExtractMethod, NormalizedDocumentV1, TableRegion,
};

fn push_table(blocks: &mut Vec<Block>, header: &[String], rows: &[Vec<String>]) {
    if header.is_empty() && rows.is_empty() {
        return;
    }
    let cols = header
        .len()
        .max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
    let mut cells: Vec<String> = header.to_vec();
    while cells.len() < cols {
        cells.push(String::new());
    }
    let mut text = String::new();
    if !header.is_empty() {
        text.push_str(&header.join(", "));
    }
    for row in rows {
        let mut padded: Vec<String> = row.clone();
        while padded.len() < cols {
            padded.push(String::new());
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&padded.join(", "));
        cells.extend(padded);
    }
    blocks.push(Block {
        key: crate::block_key(blocks.len()),
        kind: BlockKind::Table,
        page: None,
        text,
        table: Some(TableRegion {
            rows: rows.len() + usize::from(!header.is_empty()),
            cols,
            cells,
        }),
        source: BlockSource::parser(ExtractMethod::Ooxml),
    });
}

/// DOCX (WordprocessingML) → normalised blocks.
pub fn normalize_docx(
    data: &[u8],
) -> Result<NormalizedDocumentV1, crate::normalize::NormalizeError> {
    use crate::normalize::NormalizeError;
    let doc =
        OoDocument::from_bytes(data).map_err(|e| NormalizeError::Ooxml(format!("docx: {e}")))?;
    let mut blocks: Vec<Block> = Vec::new();
    if let Some(title) = doc.title.filter(|t| !t.trim().is_empty()) {
        blocks.push(Block {
            key: crate::block_key(0),
            kind: BlockKind::Heading,
            page: None,
            text: title,
            table: None,
            source: BlockSource::parser(ExtractMethod::Ooxml),
        });
    }
    for b in &doc.blocks {
        let (kind, text): (BlockKind, String) = match b {
            OoBlock::Heading { level: _, text } => (BlockKind::Heading, text.clone()),
            OoBlock::Paragraph(t) => (BlockKind::Paragraph, t.clone()),
            OoBlock::Bullet(t) => (BlockKind::Bullet, t.clone()),
            OoBlock::Table { header, rows } => {
                push_table(&mut blocks, header, rows);
                continue;
            }
        };
        if text.trim().is_empty() {
            continue;
        }
        blocks.push(Block {
            key: crate::block_key(blocks.len()),
            kind,
            page: None,
            text,
            table: None,
            source: BlockSource::parser(ExtractMethod::Ooxml),
        });
    }
    if blocks.is_empty() {
        blocks.push(Block {
            key: crate::block_key(0),
            kind: BlockKind::Paragraph,
            page: None,
            text: String::new(),
            table: None,
            source: BlockSource::parser(ExtractMethod::Ooxml),
        });
    }
    let mut out = crate::normalize::doc(
        data,
        crate::FormatKind::Docx,
        ExtractMethod::Ooxml,
        blocks,
        0,
        crate::Confidence::High,
    )?;
    out.extractor = "openrt-ooxml-docx/pure".into();
    Ok(out)
}

fn cell_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// XLSX (SpreadsheetML) → normalised blocks (one heading + one table per
/// sheet, in workbook order).
pub fn normalize_xlsx(
    data: &[u8],
) -> Result<NormalizedDocumentV1, crate::normalize::NormalizeError> {
    use crate::normalize::NormalizeError;
    let wb = Workbook::from_bytes(data).map_err(|e| NormalizeError::Ooxml(format!("xlsx: {e}")))?;
    let mut blocks: Vec<Block> = Vec::new();
    for sheet in &wb.sheets {
        if !sheet.name.trim().is_empty() {
            blocks.push(Block {
                key: crate::block_key(blocks.len()),
                kind: BlockKind::Heading,
                page: None,
                text: format!("sheet: {}", sheet.name),
                table: None,
                source: BlockSource::parser(ExtractMethod::Ooxml),
            });
        }
        let rows: Vec<Vec<String>> = sheet
            .rows
            .iter()
            .map(|r| r.iter().map(cell_text).collect())
            .filter(|r: &Vec<String>| r.iter().any(|c| !c.trim().is_empty()))
            .collect();
        push_table(&mut blocks, &[], &rows);
    }
    if blocks.is_empty() {
        blocks.push(Block {
            key: crate::block_key(0),
            kind: BlockKind::Paragraph,
            page: None,
            text: String::new(),
            table: None,
            source: BlockSource::parser(ExtractMethod::Ooxml),
        });
    }
    let mut out = crate::normalize::doc(
        data,
        crate::FormatKind::Xlsx,
        ExtractMethod::Ooxml,
        blocks,
        0,
        crate::Confidence::High,
    )?;
    out.extractor = "openrt-ooxml-xlsx/pure".into();
    Ok(out)
}

/// PPTX (PresentationML) → normalised blocks (deck title + slide headings and
/// bullets, in slide order).
pub fn normalize_pptx(
    data: &[u8],
) -> Result<NormalizedDocumentV1, crate::normalize::NormalizeError> {
    use crate::normalize::NormalizeError;
    let deck = Deck::from_bytes(data).map_err(|e| NormalizeError::Ooxml(format!("pptx: {e}")))?;
    let mut blocks: Vec<Block> = Vec::new();
    if !deck.title.trim().is_empty() && deck.title != "Untitled" {
        blocks.push(Block {
            key: crate::block_key(0),
            kind: BlockKind::Heading,
            page: None,
            text: deck.title.clone(),
            table: None,
            source: BlockSource::parser(ExtractMethod::Ooxml),
        });
    }
    for slide in &deck.slides {
        let mut slide_text: Vec<(BlockKind, String)> = Vec::new();
        if !slide.title.trim().is_empty() {
            slide_text.push((BlockKind::Heading, slide.title.clone()));
        }
        for b in &slide.bullets {
            if !b.trim().is_empty() {
                slide_text.push((BlockKind::Bullet, b.clone()));
            }
        }
        for (kind, text) in slide_text {
            blocks.push(Block {
                key: crate::block_key(blocks.len()),
                kind,
                page: None,
                text,
                table: None,
                source: BlockSource::parser(ExtractMethod::Ooxml),
            });
        }
    }
    if blocks.is_empty() {
        blocks.push(Block {
            key: crate::block_key(0),
            kind: BlockKind::Paragraph,
            page: None,
            text: String::new(),
            table: None,
            source: BlockSource::parser(ExtractMethod::Ooxml),
        });
    }
    let mut out = crate::normalize::doc(
        data,
        crate::FormatKind::Pptx,
        ExtractMethod::Ooxml,
        blocks,
        0,
        crate::Confidence::High,
    )?;
    out.extractor = "openrt-ooxml-pptx/pure".into();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::BlockKind;

    #[test]
    fn docx_roundtrip_extracts_text() {
        let mut d = OoDocument::new(Some("Quarterly Report".to_string()));
        d.blocks.push(OoBlock::Heading {
            level: 1,
            text: "Summary".into(),
        });
        d.blocks
            .push(OoBlock::Paragraph("Revenue grew by 12 percent.".into()));
        d.blocks.push(OoBlock::Bullet("Hire two engineers".into()));
        d.blocks.push(OoBlock::Table {
            header: vec!["q1".into(), "q2".into()],
            rows: vec![vec!["10".into(), "12".into()]],
        });
        let bytes = d.to_bytes();
        let doc = normalize_docx(&bytes).unwrap();
        assert_eq!(doc.format, crate::FormatKind::Docx);
        assert_eq!(doc.extractor, "openrt-ooxml-docx/pure");
        let full = doc.full_text();
        assert!(full.contains("Quarterly Report"));
        assert!(full.contains("Revenue grew by 12 percent."));
        assert!(full.contains("Hire two engineers"));
        let table = doc.blocks.iter().find(|b| b.table.is_some()).unwrap();
        assert_eq!(table.table.as_ref().unwrap().cols, 2);
        assert_eq!(doc.blocks[0].kind, BlockKind::Heading);
    }

    #[test]
    fn xlsx_roundtrip_extracts_rows() {
        let mut wb = Workbook::new("Data");
        {
            let sheet = wb.sheet_or_create("Revenue");
            sheet.write_rows(
                0,
                &[
                    vec![serde_json::json!("region"), serde_json::json!("amount")],
                    vec![serde_json::json!("emea"), serde_json::json!(42)],
                ],
            );
        }
        let bytes = wb.to_bytes();
        let doc = normalize_xlsx(&bytes).unwrap();
        assert_eq!(doc.format, crate::FormatKind::Xlsx);
        let full = doc.full_text();
        assert!(full.contains("sheet: Revenue"));
        assert!(full.contains("region, amount"));
        assert!(full.contains("emea, 42"));
    }

    #[test]
    fn pptx_roundtrip_extracts_slides() {
        let mut deck = Deck::new("Roadmap Deck");
        deck.slides.push(openrt_office::Slide {
            title: "Phase 9".into(),
            bullets: vec!["Multimodal".into(), "Retrieval".into()],
        });
        let bytes = deck.to_bytes();
        let doc = normalize_pptx(&bytes).unwrap();
        assert_eq!(doc.format, crate::FormatKind::Pptx);
        let full = doc.full_text();
        assert!(full.contains("Roadmap Deck"));
        assert!(full.contains("Phase 9"));
        assert!(full.contains("Multimodal"));
    }

    #[test]
    fn malformed_docx_is_structured_error() {
        let mut zip = b"PK\x03\x04".to_vec();
        zip.extend_from_slice(&[0u8; 40]);
        zip.extend_from_slice(b"PK\x05\x06");
        zip.extend_from_slice(&[0u8; 18]);
        let err = normalize_docx(&zip).unwrap_err();
        assert!(matches!(err, crate::normalize::NormalizeError::Ooxml(_)));
    }
}
