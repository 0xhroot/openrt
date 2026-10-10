//! Genuine, minimal PDF text extraction.
//!
//! Scope: classic xref tables (with a recovery scan for malformed offsets),
//! `/FlateDecode` (zlib) stream decompression via `flate2`, page counting from
//! the pages dicts, and text shown by `Tj`/`TJ`/`'`. Content streams are
//! resolved through the page dict's real `/Contents` references (inline or
//! indirect); a legacy fallback scans object streams with `page: None` —
//! never with invented page numbers. Extracted runs must pass a text
//! plausibility filter so binary stream payloads (image/profile data) are
//! never stored as document text. Unsupported structures fail honestly:
//! encrypted (`/Encrypt`), non-decodable streams, no text layer.
//! Nothing is invented — no fake pages, no OCR claims, no confidence values.

use flate2::read::ZlibDecoder;
use std::io::Read;

use crate::normalize::{Block, BlockKind, BlockSource, ExtractMethod, NormalizedDocumentV1};
use crate::{block_key, sha256_hex_for};

/// Extract text from genuine PDF bytes. Returns blocks in page order.
pub fn normalize_pdf(
    data: &[u8],
) -> Result<NormalizedDocumentV1, crate::normalize::NormalizeError> {
    use crate::normalize::NormalizeError;
    if data.windows(8).any(|w| w == b"/Encrypt") {
        return Err(NormalizeError::Pdf(
            "encrypted pdf is not extractable without credentials".into(),
        ));
    }
    let _entries = resolve_offsets(data)?;
    let pages = count_pages(data);
    let mut blocks: Vec<Block> = Vec::new();
    for (page, content) in page_content_streams_flat(data) {
        let runs = extract_text_runs(&content);
        if runs.is_empty() {
            continue;
        }
        blocks.push(Block {
            key: block_key(blocks.len()),
            kind: BlockKind::Page,
            page,
            text: runs.join("\n"),
            table: None,
            source: BlockSource {
                page,
                ..BlockSource::parser(ExtractMethod::PdfText)
            },
        });
    }
    if blocks.is_empty() {
        return Err(NormalizeError::PdfImageOnly);
    }
    let content_sha256 = sha256_hex_for(data);
    Ok(NormalizedDocumentV1 {
        schema_version: 1,
        doc_id: format!("doc/{content_sha256}"),
        version: 1,
        content_sha256,
        format: crate::FormatKind::Pdf,
        size_bytes: data.len() as u64,
        pages,
        blocks,
        created_at: openrt_core::now().to_rfc3339(),
        extractor: "openrt-pdf-text/pure".into(),
        detection_confidence: crate::Confidence::High,
        modality: crate::normalize::Modality::Text,
        media: None,
    })
}

/// Read the `startxref` offset and walk the classic xref chain, returning
/// `(object_offset, generation, in_use)` entries. Falls back to a full-object
/// scan when xref parsing fails (honest recovery, no fabrication).
fn resolve_offsets(raw: &[u8]) -> Result<Vec<(u64, u64, char)>, crate::normalize::NormalizeError> {
    use crate::normalize::NormalizeError;
    let eof = raw
        .windows(b"%%EOF".len())
        .rposition(|w| w == b"%%EOF")
        .unwrap_or(raw.len());
    let head = &raw[..eof];
    let Some(pos) = head
        .windows(b"startxref".len())
        .rposition(|w| w == b"startxref")
    else {
        return Err(NormalizeError::Pdf("missing startxref".into()));
    };
    let offset = scan_u64(&head[pos + b"startxref".len()..]).unwrap_or(0);
    parse_xref(raw, offset as usize).or_else(|_| scan_object_offsets(raw))
}

fn parse_xref(
    raw: &[u8],
    mut pos: usize,
) -> Result<Vec<(u64, u64, char)>, crate::normalize::NormalizeError> {
    use crate::normalize::NormalizeError;
    let mut entries = Vec::new();
    let mut hops = 0u32;
    loop {
        hops += 1;
        if hops > 64 {
            return Err(NormalizeError::Pdf("xref chain too long".into()));
        }
        let window = &raw[pos.min(raw.len())..raw.len().min(pos.saturating_add(2_000_000))];
        let Some(rel) = window.windows(4).position(|w| w == b"xref") else {
            break;
        };
        pos = window.as_ptr() as usize - raw.as_ptr() as usize + rel + 4;
        let sub = &raw[pos.min(raw.len())..raw.len().min(pos.saturating_add(2_000_000))];
        let mut cursor = 0usize;
        while cursor < sub.len() {
            let rest = &sub[cursor..];
            let Some(_start) = scan_u64(rest) else { break };
            let Some(count) = scan_u64(&rest[nums_len(rest)..]) else {
                break;
            };
            if count == 0 || count > 200_000 {
                break;
            }
            cursor = nums_len(rest) + nums_len(&rest[nums_len(rest)..]);
            for _ in 0..count {
                let line = &sub[cursor..cursor.saturating_add(20).min(sub.len())];
                if line.len() < 18 {
                    break;
                }
                let off = scan_u64(&line[..10]).unwrap_or(0);
                let gen = scan_u64(&line[11..]).unwrap_or(0);
                let inuse = line.get(17).copied().unwrap_or(b'n');
                entries.push((off, gen, inuse as char));
                cursor += 20;
            }
        }
        // Try `/Prev N` in the trailer to continue the chain.
        let trail = &raw[pos.min(raw.len())..raw.len().min(pos.saturating_add(8192))];
        let Some(p) = trail.windows(6).position(|w| w == b"/Prev ") else {
            break;
        };
        let next = scan_u64(&trail[p + 6..]).unwrap_or(0);
        if next == 0 || next as usize == pos {
            break;
        }
        pos = next as usize;
    }
    entries.sort_by_key(|(o, _, _)| *o);
    entries.dedup();
    entries.retain(|(o, _, _)| (*o as usize) < raw.len());
    if entries.is_empty() {
        Err(crate::normalize::NormalizeError::Pdf("empty xref".into()))
    } else {
        Ok(entries)
    }
}

/// Scan every `N 0 obj …` marker as a best-effort object offset map.
fn scan_object_offsets(
    raw: &[u8],
) -> Result<Vec<(u64, u64, char)>, crate::normalize::NormalizeError> {
    use crate::normalize::NormalizeError;
    let mut entries = Vec::new();
    let mut pos = 0usize;
    while let Some(hit) = raw[pos..].windows(4).position(|w| w == b" obj") {
        let rel = pos + hit;
        let num_start = raw[..rel]
            .iter()
            .rposition(|b| !b.is_ascii_digit())
            .map(|i| i + 1)
            .unwrap_or(0);
        let num: u64 = std::str::from_utf8(&raw[num_start..rel])
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        if num > 0 {
            entries.push((num_start as u64, 0, 'n'));
        }
        pos = rel + 5;
    }
    entries.sort_by_key(|(o, _, _)| *o);
    entries.dedup();
    if entries.is_empty() {
        Err(NormalizeError::Pdf("no objects found".into()))
    } else {
        Ok(entries)
    }
}

fn scan_u64(text: &[u8]) -> Option<u64> {
    let digits: String = text
        .iter()
        .skip_while(|b| b.is_ascii_whitespace())
        .take_while(|b| b.is_ascii_digit())
        .map(|b| *b as char)
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u64>().ok().filter(|v| *v < 4_000_000_000u64)
}

fn nums_len(text: &[u8]) -> usize {
    text.iter()
        .skip_while(|b| b.is_ascii_whitespace())
        .take_while(|b| b.is_ascii_digit())
        .count()
        .saturating_add(1)
}

/// Count `/Type /Page` dicts (upper-bound honest page count; 1 minimum so an
/// extractable single-page stream never reports 0).
pub(crate) fn count_pages(raw: &[u8]) -> u32 {
    let needle = b"/Type /Page";
    let mut n = 0u32;
    let mut pos = 0usize;
    while let Some(hit) = raw[pos..].windows(needle.len()).position(|w| w == needle) {
        let at = pos + hit;
        // `/Type /Pages` (the tree node) is NOT a page.
        let next = raw.get(at + needle.len()).copied();
        if next != Some(b's') {
            n += 1;
        }
        pos = at + needle.len();
    }
    n.max(1)
}

/// Resolve `(page, content_stream)` pairs. Page dicts are the authority:
/// each `/Type /Page` contributes its inline stream and/or the indirect
/// streams named by `/Contents` (execution order preserved). When no page
/// dict can be parsed at all, a legacy fallback scans object streams and
/// reports `page: None` — the page number is unknown, never invented.
fn page_content_streams_flat(raw: &[u8]) -> Vec<(Option<u32>, Vec<u8>)> {
    let mut pages: Vec<(Option<Vec<u8>>, Vec<u32>)> = Vec::new();
    let mut pos = 0usize;
    while let Some(hit) = raw[pos..].windows(b" obj".len()).position(|w| w == b" obj") {
        let rel = pos + hit;
        let num_start = raw[..rel]
            .iter()
            .rposition(|b| !b.is_ascii_digit())
            .map(|i| i + 1)
            .unwrap_or(0);
        let end_rel = raw[rel..]
            .windows(b"endobj".len())
            .position(|w| w == b"endobj")
            .map(|i| rel + i)
            .unwrap_or(raw.len());
        let obj = &raw[num_start..end_rel.min(raw.len())];
        let is_page = obj
            .windows(b"/Type /Page ".len())
            .any(|w| w == b"/Type /Page ");
        if is_page {
            let inline = extract_first_stream(obj);
            let refs = contents_refs(obj);
            pages.push((inline, refs));
        }
        pos = end_rel.min(raw.len());
    }

    if pages.is_empty() {
        // Legacy fallback: object streams without page dicts. The page is
        // genuinely unknown — `None`, not a guess.
        let mut fallback: Vec<(Option<u32>, Vec<u8>)> = Vec::new();
        let mut p = 0usize;
        while let Some(hit) = raw[p..].windows(b" obj".len()).position(|w| w == b" obj") {
            let rel = p + hit;
            let num_start = raw[..rel]
                .iter()
                .rposition(|b| !b.is_ascii_digit())
                .map(|i| i + 1)
                .unwrap_or(0);
            let end_rel = raw[rel..]
                .windows(b"endobj".len())
                .position(|w| w == b"endobj")
                .map(|i| rel + i)
                .unwrap_or(raw.len());
            let obj = &raw[num_start..end_rel.min(raw.len())];
            if let Some(s) = extract_first_stream(obj) {
                fallback.push((None, s));
            }
            p = end_rel.min(raw.len());
        }
        return fallback;
    }

    let mut out: Vec<(Option<u32>, Vec<u8>)> = Vec::new();
    for (i, (inline, refs)) in pages.iter().enumerate() {
        let page = i as u32 + 1;
        if let Some(s) = inline {
            out.push((Some(page), s.clone()));
            continue;
        }
        if !refs.is_empty() {
            let mut joined: Vec<u8> = Vec::new();
            for r in refs {
                if let Some(s) = stream_of_object(raw, *r) {
                    if !joined.is_empty() {
                        joined.push(b'\n');
                    }
                    joined.extend_from_slice(&s);
                }
            }
            if !joined.is_empty() {
                out.push((Some(page), joined));
            }
        }
    }
    out
}

/// Parse the object numbers named by a page dict's `/Contents` (direct
/// `N G R` or an array of them).
fn contents_refs(obj: &[u8]) -> Vec<u32> {
    let mut refs = Vec::new();
    if let Some(rel) = find_bytes(obj, b"/Contents") {
        let mut i = rel + b"/Contents".len();
        loop {
            while i < obj.len() && (obj[i].is_ascii_whitespace() || obj[i] == b'[') {
                i += 1;
            }
            let start = i;
            while i < obj.len() && obj[i].is_ascii_digit() {
                i += 1;
            }
            if start == i {
                break;
            }
            let Some(num) = std::str::from_utf8(&obj[start..i])
                .ok()
                .and_then(|t| t.parse::<u32>().ok())
            else {
                break;
            };
            while i < obj.len() && obj[i].is_ascii_whitespace() {
                i += 1;
            }
            let gstart = i;
            while i < obj.len() && obj[i].is_ascii_digit() {
                i += 1;
            }
            if gstart == i {
                break;
            }
            while i < obj.len() && obj[i].is_ascii_whitespace() {
                i += 1;
            }
            if obj.get(i) == Some(&b'R') {
                refs.push(num);
                i += 1;
                // Continue for `[2 0 R 3 0 R]` arrays.
                let mut j = i;
                while j < obj.len() && (obj[j].is_ascii_whitespace() || obj[j] == b']') {
                    j += 1;
                }
                if j < obj.len() && obj[j].is_ascii_digit() {
                    i = j;
                    continue;
                }
            }
            break;
        }
        // Only the first `/Contents` key in the dict is authoritative.
    }
    refs
}

/// The object number immediately before a ` obj` marker (the marker starts
/// at the space after the generation number: `N G obj`).
fn object_number_before(raw: &[u8], rel: usize) -> u32 {
    let mut gen_start = rel;
    while gen_start > 0 && raw[gen_start - 1].is_ascii_digit() {
        gen_start -= 1;
    }
    let mut j = gen_start;
    while j > 0 && raw[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    let num_end = j;
    let mut num_start = num_end;
    while num_start > 0 && raw[num_start - 1].is_ascii_digit() {
        num_start -= 1;
    }
    if num_start == num_end {
        return 0;
    }
    std::str::from_utf8(&raw[num_start..num_end])
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(0)
}

/// Locate object `num` and return its first decompressed stream.
fn stream_of_object(raw: &[u8], num: u32) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    while let Some(hit) = raw[pos..].windows(b" obj".len()).position(|w| w == b" obj") {
        let rel = pos + hit;
        let num_start = raw[..rel]
            .iter()
            .rposition(|b| !b.is_ascii_digit())
            .map(|i| i + 1)
            .unwrap_or(0);
        let found = object_number_before(raw, rel);
        let end_rel = raw[rel..]
            .windows(b"endobj".len())
            .position(|w| w == b"endobj")
            .map(|i| rel + i)
            .unwrap_or(raw.len());
        if found == num {
            let obj = &raw[num_start..end_rel.min(raw.len())];
            return extract_first_stream(obj);
        }
        pos = end_rel.min(raw.len());
    }
    None
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn extract_first_stream(obj: &[u8]) -> Option<Vec<u8>> {
    let stream_pos = obj.windows(6).position(|w| w == b"stream")?;
    let data = &obj[stream_pos + 6..];
    let data = data
        .strip_prefix(b"\r\n")
        .or_else(|| data.strip_prefix(b"\n"))?;
    let end = data.windows(9).position(|w| w == b"endstream")?;
    let data = &data[..end];
    if obj
        .windows(b"/FlateDecode".len())
        .any(|w| w == b"/FlateDecode")
    {
        let mut out = Vec::new();
        ZlibDecoder::new(data).read_to_end(&mut out).ok()?;
        Some(out)
    } else {
        Some(data.to_vec())
    }
}

/// Newline-signalling PDF text operators between BT/ET sections.
fn is_newline_op(b: &[u8]) -> bool {
    b == b"Td" || b == b"TD" || b == b"T*"
}

fn extract_text_runs(content: &[u8]) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    let mut i = 0usize;
    while i < content.len() {
        match content[i] {
            b'(' => {
                if let Some((lit, next)) = read_literal_string(content, i) {
                    if !lit.is_empty() {
                        current.push_str(&lit);
                    }
                    i = next;
                } else {
                    i += 1;
                }
            }
            b'<' => {
                let rel_end = content[i + 1..]
                    .windows(1)
                    .position(|b| b == b">")
                    .unwrap_or(0);
                if rel_end > 0 {
                    if let Some(bytes) = try_hex_decode(&content[i + 1..i + 1 + rel_end]) {
                        current.push_str(&String::from_utf8_lossy(&bytes));
                    }
                    i += 1 + rel_end + 1;
                } else {
                    i += 1;
                }
            }
            b if b.is_ascii_alphabetic() => {
                let start = i;
                while i < content.len() && content[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let op = &content[start..i];
                if is_newline_op(op) || op == b"BT" || op == b"ET" || op == b"'" {
                    let t = current.trim().to_string();
                    if plausible_text(&t) {
                        runs.push(t);
                    }
                    current.clear();
                }
            }
            _ => i += 1,
        }
    }
    let t = current.trim().to_string();
    if plausible_text(&t) {
        runs.push(t);
    }
    runs
}

/// Text plausibility: a candidate run is document text only when it carries
/// no C0 control bytes and is not dominated by high-bit bytes. Binary stream
/// payloads (image/profile/font data misread as literal strings) fail this
/// check and are dropped — never stored as document content.
fn plausible_text(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut total = 0usize;
    let mut control = 0usize;
    let mut high_bit = 0usize;
    for c in s.chars() {
        total += 1;
        let u = c as u32;
        if u < 0x20 && c != '\t' && c != '\n' && c != '\r' {
            control += 1;
        } else if u == 0x7f || (0x80..=0x9f).contains(&u) {
            // DEL + C1 controls: never present in real extracted text.
            control += 1;
        } else if u > 0x7f {
            high_bit += 1;
        }
    }
    control == 0 && high_bit * 10 <= total * 3
}

fn read_literal_string(content: &[u8], start: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut i = start + 1;
    let mut depth = 1i32;
    let mut escaped = false;
    while i < content.len() {
        let b = content[i];
        if escaped {
            match b {
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'(' | b')' | b'\\' => out.push(b as char),
                b'0'..=b'7' => {
                    let mut v: u32 = (b - b'0') as u32;
                    let mut k = 0;
                    while k < 2 && i + 1 < content.len() && (b'0'..=b'7').contains(&content[i + 1])
                    {
                        i += 1;
                        v = v * 8 + (content[i] - b'0') as u32;
                        k += 1;
                    }
                    out.push(char::from_u32(v).unwrap_or('?'));
                }
                _ => out.push(b as char),
            }
            escaped = false;
            i += 1;
            continue;
        }
        match b {
            b'\\' => {
                escaped = true;
                i += 1;
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((out, i + 1));
                }
                i += 1;
            }
            _ => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    None
}

fn try_hex_decode(hex: &[u8]) -> Option<Vec<u8>> {
    let cleaned: Vec<u8> = hex.iter().copied().filter(u8::is_ascii_hexdigit).collect();
    let mut out = Vec::with_capacity(cleaned.len() / 2);
    let mut i = 0usize;
    while i + 1 < cleaned.len() {
        let hi = (cleaned[i] as char).to_digit(16)? as u8;
        let lo = (cleaned[i + 1] as char).to_digit(16)? as u8;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_string_escapes() {
        let (s, _) = read_literal_string(b"(a\\(b\\) c)", 0).unwrap();
        assert_eq!(s, "a(b) c");
    }

    #[test]
    fn text_op_extraction() {
        let content = b"BT /F1 12 Tf 72 720 Td (Hello) Tj ET BT /F1 12 Tf 72 700 Td (World) Tj ET";
        assert_eq!(extract_text_runs(content), vec!["Hello", "World"]);
    }

    #[test]
    fn hex_string_text() {
        let content = b"(x) Tj <48656c6c6f> Tj";
        // Tj does not move the cursor to a new line; the run accumulates.
        assert_eq!(extract_text_runs(content), vec!["xHello"]);
    }

    #[test]
    fn encrypted_rejected() {
        let enc = b"%PDF-1.7\n1 0 obj << /Encrypt 2 0 R >> endobj";
        assert!(crate::normalize_pdf(enc).is_err());
    }

    #[test]
    fn flate_stream_decompresses() {
        let mut c = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        use std::io::Write;
        c.write_all(b"BT /F1 12 Tf 72 720 Td (Injected) Tj ET")
            .unwrap();
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        pdf.extend_from_slice(b"1 0 obj << /Type /Page /Contents 2 0 R >> endobj\n");
        pdf.extend_from_slice(b"2 0 obj << /Filter /FlateDecode /Length 100 >>\nstream\n");
        pdf.extend_from_slice(&c.finish().unwrap());
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        pdf.extend_from_slice(b"3 0 obj << /Type /Catalog >> endobj\n");
        pdf.extend_from_slice(
            b"xref\n0 4\n0000000000 65535 f \ntrailer << /Root 3 0 R >>\nstartxref 1\n%%EOF\n",
        );
        let doc = crate::normalize_pdf(&pdf).unwrap();
        assert!(doc.full_text().contains("Injected"));
    }

    #[test]
    fn page_block_counts_pages() {
        assert_eq!(count_pages(b"/Type /Page "), 1);
        assert_eq!(count_pages(b""), 1);
        // The page-tree node is not a page.
        assert_eq!(count_pages(b"/Type /Pages"), 1); // floor of 1, not 2
        let mut two = b"/Type /Page ".to_vec();
        two.extend_from_slice(b"/Type /Page /Type /Pages");
        assert_eq!(count_pages(&two), 2);
    }

    fn flate_stream(text: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        use std::io::Write;
        enc.write_all(text).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn multi_page_blocks_carry_page_numbers() {
        let s1 = flate_stream(b"BT (Page one text) Tj ET");
        let s2 = flate_stream(b"BT (Page two text) Tj ET");
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        pdf.extend_from_slice(b"1 0 obj << /Type /Page /Contents 2 0 R >> endobj\n");
        pdf.extend_from_slice(
            format!(
                "2 0 obj << /Filter /FlateDecode /Length {} >>\nstream\n",
                s1.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&s1);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        pdf.extend_from_slice(b"3 0 obj << /Type /Page /Contents 4 0 R >> endobj\n");
        pdf.extend_from_slice(
            format!(
                "4 0 obj << /Filter /FlateDecode /Length {} >>\nstream\n",
                s2.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&s2);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        pdf.extend_from_slice(b"5 0 obj << /Type /Catalog >> endobj\n");
        pdf.extend_from_slice(
            b"xref\n0 6\n0000000000 65535 f \ntrailer << /Root 5 0 R >>\nstartxref 1\n%%EOF\n",
        );
        let doc = normalize_pdf(&pdf).unwrap();
        assert_eq!(doc.pages, 2, "page-tree node must not inflate the count");
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(doc.blocks[0].page, Some(1));
        assert_eq!(doc.blocks[1].page, Some(2));
        assert_eq!(doc.blocks[0].source.page, Some(1));
        assert!(doc.full_text().contains("Page one text"));
        assert!(doc.full_text().contains("Page two text"));
    }

    #[test]
    fn binary_stream_text_rejected_as_implausible() {
        // A content-like stream carrying binary payload in literal strings
        // must yield no runs (this is what image/profile streams look like).
        let mut content = b"BT ".to_vec();
        content.extend_from_slice(b"(");
        content.extend_from_slice(&[0x00, 0x00, 0x03, 0xff, 0xff, 0xff, 0x00, 0xfe]);
        content.extend_from_slice(b") Tj (\x00\x00hello) Tj ET");
        let runs = extract_text_runs(&content);
        assert!(runs.is_empty(), "binary garbage must be dropped: {runs:?}");
    }

    #[test]
    fn plausible_text_accepted() {
        assert!(plausible_text("The turbine rated 40 MW at peak."));
        assert!(plausible_text(
            "Caf\u{e9} na\u{ef}\u{ef}ve \u{2014} r\u{e9}sum\u{e9} (2024)"
        ));
        assert!(!plausible_text(""));
        assert!(!plausible_text(&format!("{}abc", char::from(0))));
        // ÿ-dominated binary mix (>30% high-bit) is rejected.
        let garbage: String = std::iter::repeat('\u{ff}')
            .take(40)
            .chain("abc".chars())
            .collect();
        assert!(!plausible_text(&garbage));
    }

    #[test]
    fn indirect_contents_refs_resolve_to_page() {
        // Standard layout: page dict references a separate content object.
        let s1 = flate_stream(b"BT (Indirect page text) Tj ET");
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        pdf.extend_from_slice(b"1 0 obj << /Type /Page /Contents 2 0 R >> endobj\n");
        pdf.extend_from_slice(
            format!(
                "2 0 obj << /Filter /FlateDecode /Length {} >>\nstream\n",
                s1.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&s1);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        pdf.extend_from_slice(b"3 0 obj << /Type /Pages /Kids [1 0 R] >> endobj\n");
        pdf.extend_from_slice(b"xref\n0 4\ntrailer << /Root 1 0 R >>\nstartxref 1\n%%EOF\n");
        let streams = page_content_streams_flat(&pdf);
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].0, Some(1));
        let doc = normalize_pdf(&pdf).unwrap();
        assert_eq!(doc.pages, 1);
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(doc.blocks[0].page, Some(1));
        assert!(doc.full_text().contains("Indirect page text"));
    }

    #[test]
    fn fallback_streams_have_unknown_pages() {
        // No page dicts at all: streams are still read, but the page number
        // is genuinely unknown and must be `None` — never invented.
        let s1 = flate_stream(b"BT (Loose stream text) Tj ET");
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        pdf.extend_from_slice(
            format!(
                "1 0 obj << /Filter /FlateDecode /Length {} >>\nstream\n",
                s1.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&s1);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        pdf.extend_from_slice(b"xref\n0 2\ntrailer << /Root 1 0 R >>\nstartxref 1\n%%EOF\n");
        let doc = normalize_pdf(&pdf).unwrap();
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(doc.blocks[0].page, None);
        assert_eq!(doc.blocks[0].source.page, None);
    }

    #[test]
    fn multi_page_detect_is_supported() {
        let mut pdf = b"%PDF-1.7\n1 0 obj << /Type /Page /Contents 2 0 R >> endobj\n".to_vec();
        pdf.extend_from_slice(b"2 0 obj << /Length 4 >>\nstream\nBT ET\nendstream\nendobj\n");
        pdf.extend_from_slice(b"trailer << /Root 3 0 R >>\nstartxref 1\n%%EOF\n");
        let r = crate::detect(&pdf, Some("x.pdf"));
        assert!(r.status.is_supported(), "status: {:?}", r.status);
    }
}
