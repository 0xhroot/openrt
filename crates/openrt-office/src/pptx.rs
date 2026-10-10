//! Minimal PPTX (PresentationML) deck: create + read-back of our own files.
//!
//! Structure written (the minimum PowerPoint/LibreOffice accept):
//! presentation → slideMaster → slideLayout → theme, one layout shared by all
//! slides; every slide has a title + body text box. Verification of "real
//! file" status is `ppt.render`/`ppt.export_pdf` through LibreOffice when
//! available (structured UNAVAILABLE otherwise — never fake GREEN).

use serde_json::Value;

use crate::xml::{each_element, escape};
use crate::zip::{read_zip, ZipWriter};

#[derive(Debug, Clone, PartialEq)]
pub struct Slide {
    pub title: String,
    pub bullets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Deck {
    pub title: String,
    pub slides: Vec<Slide>,
}

fn sp_xml(
    shape_id: u32,
    name: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    paragraphs: &[String],
) -> String {
    let mut ps = String::new();
    for p in paragraphs {
        ps.push_str(&format!(
            r#"<a:p><a:r><a:t xml:space="preserve">{}</a:t></a:r></a:p>"#,
            escape(p)
        ));
    }
    if ps.is_empty() {
        ps.push_str(r#"<a:p><a:endParaRPr lang="en-US"/></a:p>"#);
    }
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{shape_id}" name="{name}"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{w}" cy="{h}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/>{ps}</p:txBody></p:sp>"#
    )
}

impl Deck {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            slides: Vec::new(),
        }
    }

    fn slide_xml(slide: &Slide) -> String {
        let title_p = vec![slide.title.clone()];
        let mut body: Vec<String> = slide.bullets.clone();
        if body.is_empty() {
            body.push(String::new());
        }
        let title_sp = sp_xml(2, "Title", 838_200, 365_125, 8_229_600, 1_325_563, &title_p);
        let body_sp = sp_xml(3, "Body", 838_200, 1_828_800, 8_229_600, 4_405_031, &body);
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>{title_sp}{body_sp}</p:spTree></p:cSld><p:clrMapOvr><a:overrideClrMapping bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/></p:clrMapOvr></p:sld>"#
        )
    }

    /// Deterministic PPTX bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = ZipWriter::new();
        let n = self.slides.len().max(1);

        let mut ct = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/><Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/><Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>"#,
        );
        for i in 1..=n {
            ct.push_str(&format!(
                r#"<Override PartName="/ppt/slides/slide{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#
            ));
        }
        ct.push_str(r#"<Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>"#);
        ct.push_str("</Types>");
        w.add_str("[Content_Types].xml", &ct);

        w.add_str(
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/></Relationships>"#,
        );

        // presentation.xml: master id + slide ids + size.
        let mut sld_id_lst = String::new();
        let mut pres_rels = String::new();
        pres_rels.push_str(
            r#"<Relationship Id="rIdMaster" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>"#,
        );
        for i in 1..=n {
            let sid = 255 + i as u32;
            sld_id_lst.push_str(&format!(r#"<p:sldId id="{sid}" r:id="rId{i}"/>"#));
            pres_rels.push_str(&format!(
                r#"<Relationship Id="rId{i}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{i}.xml"/>"#
            ));
        }
        w.add_str(
            "ppt/presentation.xml",
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rIdMaster"/></p:sldMasterIdLst><p:sldIdLst>{sld_id_lst}</p:sldIdLst><p:sldSz cx="9144000" cy="6858000"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>"#
            ),
        );
        w.add_str(
            "ppt/_rels/presentation.xml.rels",
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{pres_rels}</Relationships>"#
            ),
        );

        w.add_str(
            "ppt/slideMasters/slideMaster1.xml",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst><p:txStyles><p:titleStyle/><p:bodyStyle/><p:otherStyle/></p:txStyles></p:sldMaster>"#,
        );
        w.add_str(
            "ppt/slideMasters/_rels/slideMaster1.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/></Relationships>"#,
        );
        w.add_str(
            "ppt/slideLayouts/slideLayout1.xml",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="blank" preserve="1"><p:cSld name="Blank"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#,
        );
        w.add_str(
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#,
        );
        w.add_str("ppt/theme/theme1.xml", THEME_XML);

        let mut slides_xml = String::new();
        let mut slide_rels = String::new();
        for (i, s) in self.slides.iter().enumerate() {
            let idx = i + 1;
            slides_xml.push_str(&format!(
                r#"<Override PartName="/ppt/slides/slide{idx}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#
            ));
            w.add_str(&format!("ppt/slides/slide{idx}.xml"), &Self::slide_xml(s));
            w.add_str(
                &format!("ppt/slides/_rels/slide{idx}.xml.rels"),
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/></Relationships>"#,
            );
            let _ = &mut slide_rels;
        }
        let _ = slides_xml;
        w.add_str(
            "docProps/core.xml",
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>{}</dc:title></cp:coreProperties>"#,
                escape(&self.title)
            ),
        );
        w.build()
    }

    /// Read back a deck (our own files; third-party text is extracted best-effort).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let entries = read_zip(bytes)?;
        let mut slide_files: Vec<(usize, String)> = Vec::new();
        for (name, data) in &entries {
            if let Some(rest) = name.strip_prefix("ppt/slides/slide") {
                if let Some(num) = rest.strip_suffix(".xml") {
                    if let Ok(i) = num.parse::<usize>() {
                        slide_files.push((i, String::from_utf8_lossy(data).into_owned()));
                    }
                }
            }
        }
        if slide_files.is_empty() {
            return Err("no slides found (not a PPTX)".into());
        }
        slide_files.sort_by_key(|(i, _)| *i);
        let mut title = entries
            .iter()
            .find(|(n, _)| n == "docProps/core.xml")
            .and_then(|(_, d)| {
                let core = String::from_utf8_lossy(d);
                core.split("<dc:title>")
                    .nth(1)
                    .and_then(|t| t.split("</dc:title>").next())
                    .map(|t| crate::xml::unescape(t))
            })
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| String::from("Untitled"));
        let mut slides = Vec::new();
        for (_, xml) in slide_files {
            let mut shapes: Vec<Vec<String>> = Vec::new();
            each_element(&xml, "p:sp", |_a, inner| {
                let mut texts = Vec::new();
                each_element(inner, "a:t", |_ta, t| {
                    texts.push(unescape_basic(t));
                });
                if !texts.is_empty() {
                    shapes.push(texts);
                }
            });
            let stitle = shapes
                .first()
                .map(|s| s.join(""))
                .unwrap_or_else(|| String::new());
            let bullets = shapes
                .iter()
                .skip(1)
                .flat_map(|s| s.iter().cloned())
                .filter(|t| !t.is_empty())
                .collect();
            if slides.is_empty() && title == "Untitled" && !stitle.is_empty() {
                title = stitle.clone();
            }
            slides.push(Slide {
                title: stitle,
                bullets,
            });
        }
        Ok(Self { title, slides })
    }

    pub fn text_summary(&self) -> Value {
        Value::Array(
            self.slides
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "title": s.title,
                        "bullets": s.bullets,
                    })
                })
                .collect(),
        )
    }
}

fn unescape_basic(s: &str) -> String {
    crate::xml::unescape(s)
}

const THEME_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="OpenRT"><a:themeElements><a:clrScheme name="OpenRT"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme name="OpenRT"><a:majorFont><a:latin typeface="Calibri Light"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="OpenRT"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deck_roundtrip() {
        let mut d = Deck::new("Q3 Report");
        d.slides.push(Slide {
            title: "Summary".into(),
            bullets: vec!["up".into(), "to the right".into()],
        });
        d.slides.push(Slide {
            title: "Risks".into(),
            bullets: vec![],
        });
        let bytes = d.to_bytes();
        let back = Deck::from_bytes(&bytes).unwrap();
        assert_eq!(back.title, "Q3 Report");
        assert_eq!(back.slides.len(), 2);
        assert_eq!(back.slides[0].title, "Summary");
        assert_eq!(back.slides[0].bullets, vec!["up", "to the right"]);
        assert_eq!(back.slides[1].title, "Risks");
    }

    #[test]
    fn xml_escaping_in_deck() {
        let mut d = Deck::new("A < B & C");
        d.slides.push(Slide {
            title: "x".into(),
            bullets: vec!["<script>".into()],
        });
        let bytes = d.to_bytes();
        // Raw XML must not contain unescaped angle brackets from content.
        let entries = read_zip(&bytes).unwrap();
        let slide = entries
            .iter()
            .find(|(n, _)| n == "ppt/slides/slide1.xml")
            .unwrap();
        let text = String::from_utf8_lossy(&slide.1);
        assert!(!text.contains("<script>"));
        let back = Deck::from_bytes(&bytes).unwrap();
        assert_eq!(back.slides[0].bullets[0], "<script>");
        assert_eq!(back.title, "A < B & C");
    }
}
