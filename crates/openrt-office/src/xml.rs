//! Tiny XML helpers for OOXML generation and read-back. Not a general XML
//! parser — it covers the subset these documents use (elements with
//! double-quoted attributes, text nodes) and fails structured on anything it
//! cannot understand.

/// Escape text for XML content and attribute values.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Spreadsheet-style column index → letters (0 → `A`, 26 → `AA`).
pub fn col_name(mut idx: usize) -> String {
    let mut s = Vec::new();
    loop {
        let rem = idx % 26;
        s.push(b'A' + rem as u8);
        if idx < 26 {
            break;
        }
        idx = idx / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).expect("ascii")
}

/// `A1` → (col_index, row_index) both 0-based.
pub fn parse_cell_ref(r: &str) -> Option<(usize, usize)> {
    let mut col = 0usize;
    let mut i = 0;
    let b = r.as_bytes();
    while i < b.len() && b[i].is_ascii_alphabetic() {
        col = col * 26 + (b[i].to_ascii_uppercase() - b'A' + 1) as usize;
        i += 1;
    }
    if col == 0 || i == 0 {
        return None;
    }
    let row: usize = r[i..].parse().ok()?;
    Some((col - 1, row.checked_sub(1)?))
}

/// Extract the value of `key="…"` (or `'…'`) from an attributes string.
pub fn attr<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = attrs;
    let pat = format!("{key}=");
    while let Some(pos) = rest.find(&pat) {
        let after = &rest[pos + pat.len()..];
        let quote = after.chars().next()?;
        if quote != '"' && quote != '\'' {
            rest = &after[1..];
            continue;
        }
        let end = after[1..].find(quote)?;
        return Some(&after[1..1 + end]);
    }
    None
}

/// Call `f(attrs, inner)` for each `<name …>inner</name>` (self-closing
/// elements are reported with an empty inner). Nesting of the *same* name is
/// not supported (none of the OOXML fragments we parse nest like that).
pub fn each_element(xml: &str, name: &str, mut f: impl FnMut(&str, &str)) {
    let mut rest = xml;
    let open_pat_a = format!("<{name} ");
    let open_pat_b = format!("<{name}>");
    loop {
        let pos = match rest.find(&open_pat_a) {
            Some(p) => p,
            None => match rest.find(&open_pat_b) {
                Some(p) => p,
                None => return,
            },
        };
        let after = &rest[pos + 1 + name.len()..];
        // Attribute end: '>' or '/>'
        let (self_closing, after_open) = match after.find('>') {
            Some(g) => (after[..g].ends_with('/'), &after[g + 1..]),
            None => return,
        };
        let attrs = {
            let g = after.find('>').expect("checked above");
            let mut s = &after[..g];
            if self_closing {
                s = &s[..s.len() - 1];
            }
            s
        };
        if self_closing {
            f(attrs, "");
            rest = after_open;
            continue;
        }
        let close = format!("</{name}>");
        match after_open.find(&close) {
            Some(end) => {
                f(attrs, &after_open[..end]);
                rest = &after_open[end + close.len()..];
            }
            None => return,
        }
    }
}

/// Concatenate all `<t>…</t>` text contents (respecting escapes).
pub fn text_content(fragment: &str) -> String {
    let mut out = String::new();
    each_element(fragment, "t", |_a, inner| {
        out.push_str(&unescape(inner));
    });
    out
}

/// Unescape XML entities produced by [`escape`].
pub fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_roundtrip() {
        let s = "a<b>&'\"";
        assert_eq!(unescape(&escape(s)), s);
    }

    #[test]
    fn col_names() {
        assert_eq!(col_name(0), "A");
        assert_eq!(col_name(25), "Z");
        assert_eq!(col_name(26), "AA");
        assert_eq!(col_name(27), "AB");
        assert_eq!(col_name(701), "ZZ");
        assert_eq!(col_name(702), "AAA");
    }

    #[test]
    fn parse_refs() {
        assert_eq!(parse_cell_ref("A1"), Some((0, 0)));
        assert_eq!(parse_cell_ref("B3"), Some((1, 2)));
        assert_eq!(parse_cell_ref("AA10"), Some((26, 9)));
        assert_eq!(parse_cell_ref("1A"), None);
    }

    #[test]
    fn attrs_and_elements() {
        let xml = r#"<row r="3" spans="1:2"><c r="A3" t="inlineStr"><is><t>hi</t></is></c><c r="B3" t="str"><v>7</v></c></row>"#;
        let mut hits = Vec::new();
        each_element(xml, "c", |a, inner| {
            hits.push((attr(a, "r").unwrap().to_string(), inner.to_string()));
        });
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, "A3");
        assert_eq!(text_content(&hits[0].1), "hi");
        let mut selfclosing = 0;
        each_element(
            r#"<sheet name="S" sheetId="1" r:id="rId1"/>"#,
            "sheet",
            |a, _i| {
                assert_eq!(attr(a, "name").unwrap(), "S");
                selfclosing += 1;
            },
        );
        assert_eq!(selfclosing, 1);
    }
}
