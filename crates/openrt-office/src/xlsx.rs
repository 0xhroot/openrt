//! Minimal XLSX (SpreadsheetML) workbook: create/modify/read.
//!
//! Writes deterministic OOXML (inline strings, stored ZIP entries); reads
//! back its own files plus common third-party files (sharedStrings, formulas
//! with cached values).

use serde_json::{json, Value};

use crate::xml::{attr, col_name, each_element, escape, parse_cell_ref, text_content};
use crate::zip::{read_zip, ZipWriter};

#[derive(Debug, Clone, PartialEq)]
pub struct Sheet {
    pub name: String,
    /// Rows of cells (0-based; ragged rows allowed; `null` = empty cell).
    pub rows: Vec<Vec<Value>>,
}

impl Sheet {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            rows: Vec::new(),
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// First row index with no data (append anchor).
    pub fn next_row(&self) -> usize {
        self.rows.len()
    }

    /// Write `rows` starting at `start_row` (0-based), growing as needed.
    pub fn write_rows(&mut self, start_row: usize, rows: &[Vec<Value>]) {
        for (i, row) in rows.iter().enumerate() {
            let r = start_row + i;
            while self.rows.len() <= r {
                self.rows.push(Vec::new());
            }
            let target = &mut self.rows[r];
            if target.len() < row.len() {
                target.resize(row.len(), Value::Null);
            }
            for (c, v) in row.iter().enumerate() {
                target[c] = v.clone();
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Workbook {
    pub sheets: Vec<Sheet>,
}

impl Workbook {
    pub fn new(default_sheet: &str) -> Self {
        Self {
            sheets: vec![Sheet::new(default_sheet)],
        }
    }

    pub fn sheet(&self, name: &str) -> Option<&Sheet> {
        self.sheets.iter().find(|s| s.name == name)
    }

    pub fn sheet_mut(&mut self, name: &str) -> Option<&mut Sheet> {
        self.sheets.iter_mut().find(|s| s.name == name)
    }

    /// Get or insert a sheet by name (create-on-write for `excel.write`).
    pub fn sheet_or_create(&mut self, name: &str) -> &mut Sheet {
        if let Some(i) = self.sheets.iter().position(|s| s.name == name) {
            &mut self.sheets[i]
        } else {
            self.sheets.push(Sheet::new(name));
            self.sheets.last_mut().expect("just pushed")
        }
    }

    fn cell_xml(r: usize, c: usize, v: &Value) -> Option<String> {
        let ref_ = format!("{}{}", col_name(c), r + 1);
        match v {
            Value::Null => None,
            Value::Bool(b) => Some(format!(
                r#"<c r="{ref_}" t="b"><v>{}</v></c>"#,
                if *b { 1 } else { 0 }
            )),
            Value::Number(n) => Some(format!(r#"<c r="{ref_}"><v>{n}</v></c>"#)),
            Value::String(s) => Some(format!(
                r#"<c r="{ref_}" t="inlineStr"><is><t xml:space="preserve">{}</t></is></c>"#,
                escape(s)
            )),
            other => Some(format!(
                r#"<c r="{ref_}" t="inlineStr"><is><t xml:space="preserve">{}</t></is></c>"#,
                escape(&other.to_string())
            )),
        }
    }

    /// Deterministic XLSX bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = ZipWriter::new();
        let mut overrides = String::new();
        overrides.push_str(
            r#"<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>"#,
        );
        for i in 1..=self.sheets.len() {
            overrides.push_str(&format!(
                r#"<Override PartName="/xl/worksheets/sheet{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#));
        }
        w.add_str(
            "[Content_Types].xml",
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>{overrides}</Types>"#
            ),
        );
        w.add_str(
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        );
        let mut sheets_xml = String::new();
        let mut wb_rels = String::new();
        for (i, s) in self.sheets.iter().enumerate() {
            let n = i + 1;
            sheets_xml.push_str(&format!(
                r#"<sheet name="{}" sheetId="{n}" r:id="rId{n}"/>"#,
                escape(&s.name)
            ));
            wb_rels.push_str(&format!(
                r#"<Relationship Id="rId{n}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{n}.xml"/>"#
            ));
        }
        wb_rels.push_str(
            r#"<Relationship Id="rIdStyles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#,
        );
        w.add_str(
            "xl/workbook.xml",
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>{sheets_xml}</sheets></workbook>"#
            ),
        );
        w.add_str(
            "xl/_rels/workbook.xml.rels",
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{wb_rels}</Relationships>"#
            ),
        );
        w.add_str(
            "xl/styles.xml",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs></styleSheet>"#,
        );
        for (i, s) in self.sheets.iter().enumerate() {
            let mut sheet_data = String::new();
            for (r, row) in s.rows.iter().enumerate() {
                if row.iter().all(|c| *c == Value::Null) {
                    continue;
                }
                sheet_data.push_str(&format!(r#"<row r="{}">"#, r + 1));
                for (c, v) in row.iter().enumerate() {
                    if let Some(cell) = Self::cell_xml(r, c, v) {
                        sheet_data.push_str(&cell);
                    }
                }
                sheet_data.push_str("</row>");
            }
            w.add_str(
                &format!("xl/worksheets/sheet{}.xml", i + 1),
                &format!(
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{sheet_data}</sheetData></worksheet>"#
                ),
            );
        }
        w.build()
    }

    /// Parse XLSX bytes (own files + common third-party layouts).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let entries = read_zip(bytes)?;
        let get = |name: &str| {
            entries
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, d)| d.clone())
        };
        // Shared strings (third-party files).
        let shared: Vec<String> = match get("xl/sharedStrings.xml") {
            Some(xml) => {
                let text = String::from_utf8_lossy(&xml).into_owned();
                let mut out = Vec::new();
                each_element(&text, "si", |_a, inner| {
                    out.push(text_content(inner));
                });
                out
            }
            None => Vec::new(),
        };
        // Sheet name → part mapping (own + third-party via rels).
        let workbook = get("xl/workbook.xml")
            .ok_or_else(|| "workbook.xml missing (not an XLSX file)".to_string())?;
        let workbook = String::from_utf8_lossy(&workbook).into_owned();
        let mut sheet_parts: Vec<(String, String)> = Vec::new(); // (name, part path)
        each_element(&workbook, "sheet", |a, _| {
            let name = attr(a, "name").unwrap_or("Sheet").to_string();
            let rid = attr(a, "r:id").unwrap_or("").to_string();
            sheet_parts.push((name, rid));
        });
        if sheet_parts.is_empty() {
            return Err("workbook has no sheets".into());
        }
        // Resolve r:id → target via workbook rels.
        let mut rels: std::collections::HashMap<String, String> = Default::default();
        if let Some(xml) = get("xl/_rels/workbook.xml.rels") {
            let text = String::from_utf8_lossy(&xml).into_owned();
            each_element(&text, "Relationship", |a, _| {
                if let (Some(id), Some(target)) = (attr(a, "Id"), attr(a, "Target")) {
                    rels.insert(id.to_string(), target.to_string());
                }
            });
        }
        let mut sheets = Vec::new();
        for (i, (name, rid)) in sheet_parts.iter().enumerate() {
            let part = match rels.get(rid) {
                Some(t) if t.starts_with('/') => t.trim_start_matches('/').to_string(),
                Some(t) => format!("xl/{t}"),
                // Fallback: sheet order matches sheetN.xml.
                None => format!("xl/worksheets/sheet{}.xml", i + 1),
            };
            let data = get(&part).ok_or_else(|| format!("worksheet part `{part}` missing"))?;
            let text = String::from_utf8_lossy(&data).into_owned();
            sheets.push(parse_sheet(name, &text, &shared)?);
        }
        Ok(Self { sheets })
    }
}

fn parse_sheet(name: &str, xml: &str, shared: &[String]) -> Result<Sheet, String> {
    let mut cells: std::collections::BTreeMap<(usize, usize), Value> = Default::default();
    each_element(xml, "c", |a, inner| {
        let Some(ref_) = attr(a, "r") else { return };
        let Some((ci, ri)) = parse_cell_ref(ref_) else {
            return;
        };
        let ty = attr(a, "t").unwrap_or("");
        let value = if ty == "inlineStr" {
            Value::String(text_content(inner))
        } else {
            let mut vtxt = None;
            each_element(inner, "v", |_va, vinner| {
                vtxt = Some(vinner.to_string());
            });
            match vtxt {
                None => Value::Null,
                Some(vtxt) => match ty {
                    "s" => shared
                        .get(vtxt.trim().parse::<usize>().unwrap_or(usize::MAX))
                        .map(|s| Value::String(s.clone()))
                        .unwrap_or(Value::Null),
                    "b" => Value::Bool(vtxt.trim() == "1"),
                    "str" | "inlineStr" => Value::String(vtxt),
                    _ => match vtxt.trim().parse::<i64>() {
                        Ok(i) => json!(i),
                        Err(_) => match vtxt.trim().parse::<f64>() {
                            Ok(f) if f.is_finite() => serde_json::Number::from_f64(f)
                                .map(Value::Number)
                                .unwrap_or(Value::String(vtxt)),
                            _ => Value::String(vtxt),
                        },
                    },
                },
            }
        };
        if value != Value::Null {
            cells.insert((ri, ci), value);
        }
    });
    if cells.is_empty() {
        return Ok(Sheet::new(name));
    }
    let max_row = cells.keys().map(|(r, _)| *r).max().unwrap_or(0);
    let mut rows = vec![Vec::new(); max_row + 1];
    for ((r, c), v) in cells {
        if rows[r].len() <= c {
            rows[r].resize(c + 1, Value::Null);
        }
        rows[r][c] = v;
    }
    Ok(Sheet {
        name: name.to_string(),
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_write_read_roundtrip() {
        let mut wb = Workbook::new("Data");
        wb.sheet_or_create("Data").write_rows(
            0,
            &[
                vec![json!("name"), json!("score")],
                vec![json!("ada"), json!(98)],
            ],
        );
        let bytes = wb.to_bytes();
        let back = Workbook::from_bytes(&bytes).unwrap();
        assert_eq!(back.sheets.len(), 1);
        assert_eq!(back.sheets[0].name, "Data");
        assert_eq!(back.sheets[0].rows[0][0], json!("name"));
        assert_eq!(back.sheets[0].rows[1][1], json!(98));
    }

    #[test]
    fn multi_sheet_and_append() {
        let mut wb = Workbook::new("One");
        wb.sheet_or_create("One").write_rows(0, &[vec![json!(1)]]);
        wb.sheet_or_create("Two")
            .write_rows(0, &[vec![json!("x")], vec![json!("y")]]);
        let bytes = wb.to_bytes();
        let mut back = Workbook::from_bytes(&bytes).unwrap();
        assert_eq!(back.sheets.len(), 2);
        back.sheet_or_create("One").write_rows(1, &[vec![json!(2)]]);
        let bytes2 = back.to_bytes();
        let back2 = Workbook::from_bytes(&bytes2).unwrap();
        assert_eq!(back2.sheet("One").unwrap().rows.len(), 2);
        assert_eq!(back2.sheet("One").unwrap().rows[1][0], json!(2));
    }

    #[test]
    fn types_survive_roundtrip() {
        let mut wb = Workbook::new("T");
        wb.sheet_or_create("T").write_rows(
            0,
            &[vec![json!("text"), json!(3.5), json!(true), Value::Null]],
        );
        let back = Workbook::from_bytes(&wb.to_bytes()).unwrap();
        let row = &back.sheets[0].rows[0];
        assert_eq!(row[0], json!("text"));
        assert_eq!(row[1], json!(3.5));
        assert_eq!(row[2], json!(true));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Workbook::from_bytes(b"zip?").is_err());
    }
}
