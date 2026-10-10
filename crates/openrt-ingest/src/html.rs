//! Genuine HTML text extraction (pure Rust, no network, no fabricated text).
//!
//! The walker parses real markup: tags, comments, doctype, and character
//! references are recognised structurally. `<script>`/`<style>` content is
//! dropped (never treated as document text), headings/lists/tables map to
//! block kinds, and only text that exists in the source is emitted.

use crate::normalize::{Block, BlockKind, BlockSource, ExtractMethod, NormalizedDocumentV1};

/// Extract a normalised document from HTML bytes.
pub fn normalize_html(
    data: &[u8],
) -> Result<NormalizedDocumentV1, crate::normalize::NormalizeError> {
    use crate::normalize::NormalizeError;
    let text = std::str::from_utf8(data.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(data))
        .map_err(|_| NormalizeError::Unsupported("non-utf8 in html path".into()))?;
    let mut w = Walker::default();
    w.run(text);
    let mut blocks = w.finish();
    if blocks.is_empty() {
        // Honest empty document (same posture as the text parser).
        blocks.push(Block {
            key: crate::block_key(0),
            kind: BlockKind::Paragraph,
            page: None,
            text: String::new(),
            table: None,
            source: BlockSource::parser(ExtractMethod::HtmlParser),
        });
    }
    let mut doc = crate::normalize::doc(
        data,
        crate::FormatKind::Html,
        ExtractMethod::HtmlParser,
        blocks,
        0,
        crate::Confidence::High,
    )?;
    doc.extractor = "openrt-html-parser/pure".into();
    Ok(doc)
}

#[derive(Default)]
struct Walker {
    segments: Vec<(BlockKind, String)>,
    cur: Option<(BlockKind, String)>,
    in_comment: bool,
}

impl Walker {
    fn flush(&mut self) {
        if let Some((kind, buf)) = self.cur.take() {
            let t = buf.trim().to_string();
            if !t.is_empty() {
                self.segments.push((kind, t));
            }
        }
    }

    fn start(&mut self, kind: BlockKind) {
        self.flush();
        self.cur = Some((kind, String::new()));
    }

    fn text(&mut self, raw: &str) {
        let decoded = decode_entities(raw);
        if decoded.trim().is_empty() && !decoded.contains(['\n']) {
            return;
        }
        let entry = self
            .cur
            .get_or_insert((BlockKind::Paragraph, String::new()));
        entry.1.push_str(&decoded);
    }

    fn run(&mut self, html: &str) {
        let bytes = html.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            // Comment / doctype / processing-instruction / CDATA openers.
            if bytes[i..].starts_with(b"<!--") {
                self.in_comment = true;
                i += 4;
                continue;
            }
            if self.in_comment {
                if let Some(rel) = html[i..].find("-->") {
                    i += rel + 3;
                    self.in_comment = false;
                } else {
                    break;
                }
                continue;
            }
            if bytes[i] == b'<' {
                if let Some(tag) = read_tag(&html[i..]) {
                    let (name, closing, self_close, rest_len) = tag;
                    let lname = name.to_ascii_lowercase();

                    // Skip script/style content until the matching close tag.
                    if !closing && matches!(lname.as_str(), "script" | "style" | "template") {
                        self.flush();
                        i += rest_len;
                        // Drop everything until the matching close tag.
                        let close = format!("</{lname}");
                        match find_ci(&html[i..], &close) {
                            Some(rel) => i += rel + close.len(),
                            None => break,
                        }
                        continue;
                    }

                    self.apply_tag(&lname, closing, self_close);
                    i += rest_len;
                    continue;
                }
                // Bare `<` that never closes a tag: treat as text.
                self.text("<");
                i += 1;
                continue;
            }
            // Plain text run until the next `<`.
            let next = html[i..].find('<').unwrap_or(html.len() - i);
            let chunk = &html[i..i + next];
            self.text(chunk);
            i += next;
        }
        self.flush();
    }

    fn apply_tag(&mut self, lname: &str, closing: bool, self_close: bool) {
        // Headings.
        if let Some(level) = heading_level(lname) {
            if closing {
                self.flush();
            } else {
                self.start(BlockKind::Heading);
                let _ = level;
            }
            return;
        }
        match lname {
            "title" => {
                if closing {
                    self.flush();
                } else {
                    self.start(BlockKind::Heading);
                }
            }
            "li" => {
                if closing {
                    self.flush();
                } else {
                    self.start(BlockKind::Bullet);
                }
            }
            "td" | "th" => {
                if !closing {
                    if self.cur.is_none() {
                        self.cur = Some((BlockKind::Table, String::new()));
                    } else if let Some(e) = self.cur.as_mut() {
                        if !e.1.is_empty() && !e.1.ends_with('\n') {
                            e.1.push_str(" | ");
                        }
                        if e.0 != BlockKind::Table {
                            e.0 = BlockKind::Table;
                        }
                    }
                }
            }
            "tr" => {
                if let Some(e) = self.cur.as_mut() {
                    e.1.push('\n');
                }
                if closing {
                    // Row boundary is a soft break inside the table segment.
                }
            }
            "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "aside"
            | "blockquote" | "pre" | "ul" | "ol" | "dl" | "dt" | "dd" | "table" | "figure"
            | "figcaption" | "hgroup" | "nav" | "form" | "body" | "html" | "head" => {
                if closing || self_close {
                    self.flush();
                } else {
                    self.flush();
                    self.cur = Some((BlockKind::Paragraph, String::new()));
                }
            }
            "br" | "hr" => {
                if let Some(e) = self.cur.as_mut() {
                    e.1.push('\n');
                }
            }
            // Inline elements and unknown tags: text flows through unchanged.
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush();
        self.segments
            .into_iter()
            .enumerate()
            .map(|(i, (kind, text))| Block {
                key: crate::block_key(i),
                kind,
                page: None,
                text,
                table: None,
                source: BlockSource::parser(ExtractMethod::HtmlParser),
            })
            .collect()
    }
}

fn heading_level(lname: &str) -> Option<u8> {
    let b = lname.as_bytes();
    if b.len() == 2 && b[0] == b'h' && (b'1'..=b'6').contains(&b[1]) {
        Some(b[1] - b'0')
    } else {
        None
    }
}

/// Reads `<name …>`, `</name …>`, `<!doctype …>`, `<?xml …>` at `s`'s start.
/// Returns `(tag name, closing, self-closing, consumed bytes)`.
fn read_tag(s: &str) -> Option<(String, bool, bool, usize)> {
    let b = s.as_bytes();
    if b.first() != Some(&b'<') {
        return None;
    }
    let mut idx = 1usize;
    let closing = b.get(idx) == Some(&b'/');
    if closing {
        idx += 1;
    }
    let rest = s.get(idx..)?;
    let name: String = if b.get(idx) == Some(&b'!') || b.get(idx) == Some(&b'?') {
        // `<!doctype …>` / `<?xml …>` / `<![CDATA[…]]>` — consume whole thing.
        let end = rest.find('>')? + idx + 1;
        let inner = &s[1..end.saturating_sub(1)];
        return Some((inner.to_string(), false, true, end));
    } else {
        rest.chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == ':' || *c == '_')
            .collect()
    };
    if name.is_empty() {
        return None;
    }
    let after_name = &s[idx + name.len()..];
    let end = after_name.find('>')? + idx + name.len() + 1;
    let consumed = &s[..end];
    let self_close = consumed.trim_end().ends_with("/>");
    Some((name, closing, self_close, end))
}

/// Case-insensitive substring search returning a byte offset.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.to_lowercase();
    let n = needle.to_lowercase();
    h.find(&n)
}

/// Decode the character references that appear in real documents. Unknown
/// references are left verbatim (never guessed).
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            // Copy the next full UTF-8 char.
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&s[i..(i + ch_len).min(s.len())]);
            i += ch_len;
            continue;
        }
        // Find the terminating `;` within a small window.
        let end = s[i..].find(';').map(|e| i + e);
        match end {
            Some(end) if end - i <= 12 => {
                let ent = &s[i + 1..end];
                let decoded: Option<char> = match ent {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some('\u{a0}'),
                    _ => {
                        if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X"))
                        {
                            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                        } else if let Some(dec) = ent.strip_prefix('#') {
                            dec.parse::<u32>().ok().and_then(char::from_u32)
                        } else {
                            None
                        }
                    }
                };
                match decoded {
                    Some(c) => {
                        out.push(c);
                        i = end + 1;
                    }
                    None => {
                        out.push('&');
                        i += 1;
                    }
                }
            }
            _ => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_and_paragraphs() {
        let html = br#"<!DOCTYPE html>
        <html><head><title>Doc Title</title>
        <style>body { color: red }</style></head>
        <body><h1>Heading One</h1><p>First para.</p><p>Second para.</p></body></html>"#;
        let d = normalize_html(html).unwrap();
        let texts: Vec<&str> = d.blocks.iter().map(|b| b.text.as_str()).collect();
        assert!(texts.contains(&"Doc Title"));
        assert!(texts.contains(&"Heading One"));
        assert!(texts.contains(&"First para."));
        assert!(texts.contains(&"Second para."));
        assert!(!d.full_text().contains("color: red"));
        assert_eq!(d.format, crate::FormatKind::Html);
        assert_eq!(d.extractor, "openrt-html-parser/pure");
    }

    #[test]
    fn script_content_dropped() {
        let html =
            br#"<html><body><p>visible</p><script>var secret = "not text";</script></body></html>"#;
        let d = normalize_html(html).unwrap();
        assert!(d.full_text().contains("visible"));
        assert!(!d.full_text().contains("secret"));
    }

    #[test]
    fn lists_and_entities() {
        let html = br#"<ul><li>one &amp; only</li><li>two &lt;3</li></ul>"#;
        let d = normalize_html(html).unwrap();
        assert_eq!(d.blocks.len(), 2);
        assert_eq!(d.blocks[0].kind, crate::normalize::BlockKind::Bullet);
        assert_eq!(d.blocks[0].text, "one & only");
        assert_eq!(d.blocks[1].text, "two <3");
    }

    #[test]
    fn comments_dropped() {
        let html = br#"<p>kept</p><!-- <p>hidden</p> -->"#;
        let d = normalize_html(html).unwrap();
        assert_eq!(d.full_text(), "kept");
    }

    #[test]
    fn table_cells_joined() {
        let html = br#"<table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table>"#;
        let d = normalize_html(html).unwrap();
        let t = d.full_text();
        assert!(t.contains('a') && t.contains('b') && t.contains('c') && t.contains('d'));
        assert!(t.contains('|'));
    }

    #[test]
    fn numeric_entities() {
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
        assert_eq!(decode_entities("raw &unknown; keep"), "raw &unknown; keep");
    }

    #[test]
    fn empty_html_honest() {
        let d = normalize_html(b"<html><head></head><body></body></html>").unwrap();
        assert_eq!(d.blocks.len(), 1);
        assert!(d.blocks[0].text.is_empty());
    }

    #[test]
    fn block_keys_are_reading_order() {
        let d = normalize_html(b"<h1>A</h1><p>B</p>").unwrap();
        assert_eq!(d.blocks[0].key, "b0");
        assert_eq!(d.blocks[1].key, "b1");
    }
}
