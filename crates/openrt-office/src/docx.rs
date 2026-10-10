//! Minimal DOCX (WordprocessingML) document: create + read-back of our own
//! files (headings, paragraphs, bullets, tables).

use serde_json::Value;

use crate::xml::{attr, each_element, escape};
use crate::zip::{read_zip, ZipWriter};

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading {
        level: u8,
        text: String,
    },
    Paragraph(String),
    Bullet(String),
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub title: Option<String>,
    pub blocks: Vec<Block>,
}

fn run(text: &str) -> String {
    format!(
        r#"<w:r><w:t xml:space="preserve">{}</w:t></w:r>"#,
        escape(text)
    )
}

impl Document {
    pub fn new(title: Option<String>) -> Self {
        Self {
            title,
            blocks: Vec::new(),
        }
    }

    fn paragraph(style: Option<&str>, num: bool, text: &str) -> String {
        let mut ppr = String::new();
        let mut need_ppr = false;
        if let Some(s) = style {
            ppr.push_str(&format!(r#"<w:pStyle w:val="{s}"/>"#));
            need_ppr = true;
        }
        if num {
            ppr.push_str(r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr>"#);
            need_ppr = true;
        }
        let ppr = if need_ppr {
            format!("<w:pPr>{ppr}</w:pPr>")
        } else {
            String::new()
        };
        format!(r#"<w:p>{ppr}{}</w:p>"#, run(text))
    }

    fn table_xml(header: &[String], rows: &[Vec<String>]) -> String {
        let mut out = String::from(
            r#"<w:tbl><w:tblPr><w:tblW w:w="0" w:type="auto"/><w:tblBorders><w:top w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:left w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:bottom w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:right w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:insideH w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:insideV w:val="single" w:sz="4" w:space="0" w:color="auto"/></w:tblBorders></w:tblPr>"#,
        );
        let mut trs = String::new();
        let mut hdr = String::from(r#"<w:tr>"#);
        for h in header {
            hdr.push_str(&format!(
                r#"<w:tc><w:p><w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">{}</w:t></w:r></w:p></w:tc>"#,
                escape(h)
            ));
        }
        hdr.push_str("</w:tr>");
        trs.push_str(&hdr);
        for row in rows {
            trs.push_str(r#"<w:tr>"#);
            for c in row {
                trs.push_str(&format!(r#"<w:tc><w:p>{}</w:p></w:tc>"#, run(c)));
            }
            trs.push_str("</w:tr>");
        }
        out.push_str(&trs);
        out.push_str("</w:tbl>");
        out
    }

    /// Deterministic DOCX bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = ZipWriter::new();
        w.add_str(
            "[Content_Types].xml",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/><Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/></Types>"#,
        );
        w.add_str(
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#,
        );
        w.add_str(
            "word/_rels/document.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdStyles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rIdNumbering" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/></Relationships>"#,
        );
        w.add_str("word/styles.xml", STYLES_XML);
        w.add_str("word/numbering.xml", NUMBERING_XML);

        let mut body = String::new();
        if let Some(t) = &self.title {
            body.push_str(&Self::paragraph(Some("Title"), false, t));
        }
        for b in &self.blocks {
            match b {
                Block::Heading { level, text } => {
                    let style = format!("Heading{}", (*level).clamp(1, 6));
                    body.push_str(&Self::paragraph(Some(&style), false, text));
                }
                Block::Paragraph(text) => body.push_str(&Self::paragraph(None, false, text)),
                Block::Bullet(text) => {
                    body.push_str(&Self::paragraph(Some("ListParagraph"), true, text))
                }
                Block::Table { header, rows } => body.push_str(&Self::table_xml(header, rows)),
            }
        }
        body.push_str(
            r#"<w:sectPr><w:pgSz w:w="11906" w:h="16838"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr>"#,
        );
        w.add_str(
            "word/document.xml",
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
            ),
        );
        w.build()
    }

    /// Read back a document (our own files; foreign files best-effort).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let entries = read_zip(bytes)?;
        let doc = entries
            .iter()
            .find(|(n, _)| n == "word/document.xml")
            .map(|(_, d)| d.clone())
            .ok_or_else(|| "word/document.xml missing (not a DOCX file)".to_string())?;
        let xml = String::from_utf8_lossy(&doc).into_owned();
        let mut title = None;
        let blocks = rebuild_blocks(&xml, &mut title);
        Ok(Self { title, blocks })
    }

    pub fn text_summary(&self) -> Value {
        let blocks: Vec<Value> = self
            .blocks
            .iter()
            .map(|b| match b {
                Block::Heading { level, text } => {
                    serde_json::json!({"type": "heading", "level": level, "text": text})
                }
                Block::Paragraph(t) => serde_json::json!({"type": "paragraph", "text": t}),
                Block::Bullet(t) => serde_json::json!({"type": "bullet", "text": t}),
                Block::Table { header, rows } => {
                    serde_json::json!({"type": "table", "header": header, "rows": rows})
                }
            })
            .collect();
        serde_json::json!({ "title": self.title, "blocks": blocks })
    }
}

/// Re-walk the body in document order, consuming paragraphs, headings,
/// bullets, and tables exactly once. Title-style paragraphs go to `title`.
fn rebuild_blocks(xml: &str, title: &mut Option<String>) -> Vec<Block> {
    let mut out = Vec::new();
    let body = match xml.find("<w:body") {
        Some(p) => &xml[p..],
        None => return out,
    };
    let mut rest = body;
    loop {
        let next_p = rest.find("<w:p ").or_else(|| rest.find("<w:p>"));
        let next_tbl = rest.find("<w:tbl>");
        let take_table = match (next_p, next_tbl) {
            (None, None) => break,
            (Some(p), Some(t)) => t < p,
            (None, Some(_)) => true,
            (Some(_), None) => false,
        };
        if take_table {
            let t = next_tbl.expect("checked");
            let after = &rest[t + 7..];
            let end = match after.find("</w:tbl>") {
                Some(e) => e,
                None => break,
            };
            let inner = &after[..end];
            let mut header = Vec::new();
            let mut rows = Vec::new();
            let mut row_i = 0;
            each_element(inner, "w:tr", |_ra, row_inner| {
                let mut cells = Vec::new();
                each_element(row_inner, "w:tc", |_ca, cell_inner| {
                    cells.push(text_content_named(cell_inner));
                });
                if row_i == 0 {
                    header = cells;
                } else {
                    rows.push(cells);
                }
                row_i += 1;
            });
            out.push(Block::Table { header, rows });
            rest = &after[end + 8..];
        } else {
            let p = next_p.expect("checked");
            let after = &rest[p..];
            let open_end = match after.find('>') {
                Some(e) => e,
                None => break,
            };
            let content = &after[open_end + 1..];
            let end = match content.find("</w:p>") {
                Some(e) => e,
                None => break,
            };
            let inner = &content[..end];
            let mut style = None;
            each_element(inner, "w:pStyle", |a, _| {
                style = attr(a, "w:val").map(str::to_string);
            });
            let mut numbered = false;
            each_element(inner, "w:numId", |_a, _| numbered = true);
            let text = text_content_named(inner);
            if !text.is_empty() {
                match style.as_deref() {
                    Some("Title") => {
                        if title.is_none() {
                            *title = Some(text);
                        }
                    }
                    Some(s) if s.starts_with("Heading") => {
                        let level: u8 = s
                            .trim_start_matches("Heading")
                            .parse()
                            .unwrap_or(1)
                            .clamp(1, 6);
                        out.push(Block::Heading { level, text });
                    }
                    Some("ListParagraph") if numbered => out.push(Block::Bullet(text)),
                    _ => out.push(Block::Paragraph(text)),
                }
            }
            rest = &content[end + 6..];
        }
    }
    out
}

/// Concatenate `w:t` text within a fragment.
fn text_content_named(fragment: &str) -> String {
    let mut out = String::new();
    each_element(fragment, "w:t", |_a, inner| {
        out.push_str(&crate::xml::unescape(inner));
    });
    out
}

const STYLES_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:cs="Calibri"/><w:sz w:val="22"/></w:rPr></w:rPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/><w:pPr><w:spacing w:after="120"/></w:pPr><w:rPr><w:b/><w:sz w:val="48"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:outlineLvl w:val="0"/><w:spacing w:before="240" w:after="120"/></w:pPr><w:rPr><w:b/><w:sz w:val="32"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:outlineLvl w:val="1"/><w:spacing w:before="200" w:after="100"/></w:pPr><w:rPr><w:b/><w:sz w:val="28"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading3"><w:name w:val="heading 3"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:outlineLvl w:val="2"/><w:spacing w:before="160" w:after="80"/></w:pPr><w:rPr><w:b/><w:sz w:val="24"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="ListParagraph"><w:name w:val="List Paragraph"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:ind w:left="720"/></w:pPr></w:style></w:styles>"#;

const NUMBERING_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:multiLevelType w:val="hybridMultilevel"/><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="•"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr><w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol" w:hint="default"/></w:rPr></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_roundtrip() {
        let mut d = Document::new(Some("Report".into()));
        d.blocks.push(Block::Heading {
            level: 1,
            text: "Intro".into(),
        });
        d.blocks.push(Block::Paragraph("Hello world.".into()));
        d.blocks.push(Block::Bullet("first".into()));
        d.blocks.push(Block::Table {
            header: vec!["k".into(), "v".into()],
            rows: vec![vec!["a".into(), "1".into()]],
        });
        let bytes = d.to_bytes();
        let back = Document::from_bytes(&bytes).unwrap();
        assert_eq!(back.title.as_deref(), Some("Report"));
        assert_eq!(
            back.blocks,
            vec![
                Block::Heading {
                    level: 1,
                    text: "Intro".into()
                },
                Block::Paragraph("Hello world.".into()),
                Block::Bullet("first".into()),
                Block::Table {
                    header: vec!["k".into(), "v".into()],
                    rows: vec![vec!["a".into(), "1".into()]],
                },
            ]
        );
    }

    #[test]
    fn escaping_and_no_tables_path() {
        let mut d = Document::new(None);
        d.blocks.push(Block::Paragraph("5 < 6 & 7 > 2".into()));
        let back = Document::from_bytes(&d.to_bytes()).unwrap();
        assert_eq!(back.blocks[0], Block::Paragraph("5 < 6 & 7 > 2".into()));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Document::from_bytes(b"nope").is_err());
    }
}
