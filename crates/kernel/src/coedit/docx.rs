//! A document as a Word file (`.docx`, Office Open XML) for downloads: paragraphs and headings
//! with their alignment, lists (numbered ones start where the document starts them), checklists,
//! quotes, code, dividers, tables (merged cells included), links to the web and mail, the
//! editor's text marks, images (their bytes from the document's package) and suggestions as
//! Word's tracked changes, which Word and LibreOffice offer to accept or reject.

use super::{FRAGMENT, IMAGE, number, safe_href, string, truthy};
use crate::{Error, Result};
use std::fmt::Write as _;
use std::io::Write as _;
use yrs::types::text::YChange;
use yrs::{Any, Doc, GetString, Out, ReadTxn, Text, Transact, Xml, XmlElementRef, XmlFragment, XmlOut};

/// An image's bytes, for a Word file.
pub struct ImageData {
    pub bytes: Vec<u8>,
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub content_type: String,
    /// In pixels: drawn this size, or smaller to fit the page's width.
    pub width: u32,
    pub height: u32,
}

/// A4 with 2 cm margins, as the HTML page prints; the text is this wide, in twips.
const PAGE_W: u32 = 11906;
const PAGE_H: u32 = 16838;
const MARGIN: u32 = 1134;
const TEXT_W: u32 = PAGE_W - 2 * MARGIN;
const EMU_PER_PX: u64 = 9525;
const EMU_PER_TWIP: u64 = 635;
/// Relationship ids taken by the parts every file has.
const RID_STYLES: &str = "rIdStyles";
const RID_NUMBERING: &str = "rIdNumbering";
/// The numbering of bulleted lists; numbered lists get one each from 2 (they restart).
const BULLETS: usize = 1;

/// How a block is written inside what holds it.
#[derive(Clone, Default)]
struct Place {
    /// The paragraph that starts a list item: its numbering (num id) and level.
    num: Option<(usize, u32)>,
    /// A list item's later paragraphs line up with its first one.
    indent: u32,
    style: Option<String>,
    /// Header cells.
    bold: bool,
    /// A checklist item's box, before its first paragraph.
    prefix: Option<&'static str>,
}

struct Writer<'a> {
    images: &'a dyn Fn(&str) -> Option<ImageData>,
    /// Hyperlinks and images: (id, type, target, external).
    rels: Vec<(String, &'static str, String, bool)>,
    /// Image files under `word/`.
    media: Vec<(String, Vec<u8>)>,
    /// Numbered lists: (level, start), num ids 2, 3, ...
    ordered: Vec<(u32, i64)>,
    drawings: u32,
    changes: u32,
}

/// XML text: escaped, without the control characters XML 1.0 cannot hold.
fn xml(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
}

fn attr(s: &str) -> String {
    let mut out = String::new();
    xml(s, &mut out);
    out
}

/// A mark's attributes, as y-prosemirror keeps them (`{name: {attrs}}`).
fn mark_attr(v: &Any, key: &str) -> Option<String> {
    match v {
        Any::Map(m) => match m.get(key) {
            Some(Any::String(s)) => Some(s.to_string()),
            Some(Any::Number(n)) => Some(n.to_string()),
            _ => None,
        },
        _ => None,
    }
}

fn present(v: Option<&Any>) -> Option<&Any> {
    v.filter(|v| !matches!(v, Any::Null | Any::Undefined | Any::Bool(false)))
}

impl Writer<'_> {
    fn rel(&mut self, kind: &'static str, target: String, external: bool) -> String {
        let id = format!("rId{}", self.rels.len() + 1);
        self.rels.push((id.clone(), kind, target, external));
        id
    }

    fn blocks<T: ReadTxn>(&mut self, nodes: impl Iterator<Item = XmlOut>, txn: &T, place: &Place, depth: u32, out: &mut String) {
        for node in nodes {
            match node {
                XmlOut::Element(e) => self.block(&e, txn, place, depth, out),
                // Text straight in a container (it should not be): a paragraph of its own.
                XmlOut::Text(t) => {
                    let mut runs = String::new();
                    self.runs(&t, txn, place.bold, &mut runs);
                    if !runs.is_empty() {
                        let _ = write!(out, "<w:p>{runs}</w:p>");
                    }
                }
                XmlOut::Fragment(f) => self.blocks(f.children(txn), txn, place, depth, out),
            }
        }
    }

    fn block<T: ReadTxn>(&mut self, e: &XmlElementRef, txn: &T, place: &Place, depth: u32, out: &mut String) {
        let tag = e.tag().clone();
        match &*tag {
            "paragraph" => self.paragraph(e, txn, place, place.style.as_deref(), out),
            "heading" => {
                let level = number(e.get_attribute(txn, "level")).unwrap_or(1).clamp(1, 6);
                self.paragraph(e, txn, place, Some(&format!("Heading{level}")), out);
            }
            "blockquote" => {
                let inner = Place { style: Some("Quote".into()), ..place.clone() };
                self.blocks(e.children(txn), txn, &inner, depth, out);
            }
            "codeBlock" => {
                let mut code = String::new();
                for child in e.children(txn) {
                    if let XmlOut::Text(t) = child {
                        code.push_str(&t.get_string(txn));
                    }
                }
                out.push_str("<w:p><w:pPr><w:pStyle w:val=\"Code\"/></w:pPr>");
                for (n, line) in code.split('\n').enumerate() {
                    if n > 0 {
                        out.push_str("<w:r><w:br/></w:r>");
                    }
                    text_run(line, "", "w:t", out);
                }
                out.push_str("</w:p>");
            }
            "horizontalRule" => {
                out.push_str("<w:p><w:pPr><w:pBdr><w:bottom w:val=\"single\" w:sz=\"6\" w:space=\"1\" w:color=\"D0D7DE\"/></w:pBdr></w:pPr></w:p>")
            }
            list @ ("bulletList" | "orderedList" | "taskList") => {
                let level = depth.min(8);
                let num = match list {
                    "bulletList" => Some(BULLETS),
                    "orderedList" => {
                        self.ordered.push((level, number(e.get_attribute(txn, "start")).unwrap_or(1).max(0)));
                        Some(BULLETS + self.ordered.len())
                    }
                    _ => None,
                };
                for item in e.children(txn) {
                    let XmlOut::Element(li) = item else { continue };
                    let prefix = (list == "taskList").then(|| if truthy(li.get_attribute(txn, "checked")) { "\u{2611} " } else { "\u{2610} " });
                    let indent = 720 * (level + 1);
                    let mut first = true;
                    for child in li.children(txn) {
                        let XmlOut::Element(c) = child else { continue };
                        let nested = matches!(&**c.tag(), "bulletList" | "orderedList" | "taskList");
                        let item_place = Place {
                            num: if first && !nested { num.map(|n| (n, level)) } else { None },
                            indent: if first && num.is_some() && !nested { 0 } else { indent },
                            prefix: if first && !nested { prefix } else { None },
                            ..place.clone()
                        };
                        self.block(&c, txn, &item_place, depth + u32::from(nested), out);
                        if !nested {
                            first = false;
                        }
                    }
                }
            }
            "table" => self.table(e, txn, place, out),
            IMAGE => self.image(e, txn, out),
            "hardBreak" => out.push_str("<w:p><w:r><w:br/></w:r></w:p>"),
            _ => self.blocks(e.children(txn), txn, place, depth, out),
        }
    }

    fn paragraph<T: ReadTxn>(&mut self, e: &XmlElementRef, txn: &T, place: &Place, style: Option<&str>, out: &mut String) {
        let mut ppr = String::new();
        if let Some(style) = style {
            let _ = write!(ppr, "<w:pStyle w:val=\"{style}\"/>");
        }
        if let Some((num, level)) = place.num {
            let _ = write!(ppr, "<w:numPr><w:ilvl w:val=\"{level}\"/><w:numId w:val=\"{num}\"/></w:numPr>");
        } else if place.indent > 0 {
            let _ = write!(ppr, "<w:ind w:left=\"{}\"/>", place.indent);
        }
        if let Some(a) = string(e.get_attribute(txn, "textAlign")) {
            let jc = match a.as_str() {
                "center" => Some("center"),
                "right" => Some("right"),
                "justify" => Some("both"),
                _ => None,
            };
            if let Some(jc) = jc {
                let _ = write!(ppr, "<w:jc w:val=\"{jc}\"/>");
            }
        }
        out.push_str("<w:p>");
        if !ppr.is_empty() {
            let _ = write!(out, "<w:pPr>{ppr}</w:pPr>");
        }
        if let Some(prefix) = place.prefix {
            text_run(prefix, "", "w:t", out);
        }
        for child in e.children(txn) {
            match child {
                XmlOut::Text(t) => self.runs(&t, txn, place.bold, out),
                XmlOut::Element(c) if &**c.tag() == "hardBreak" => out.push_str("<w:r><w:br/></w:r>"),
                // Inline nodes the editor may add (mentions): their text.
                XmlOut::Element(c) => {
                    for inner in c.children(txn) {
                        if let XmlOut::Text(t) = inner {
                            self.runs(&t, txn, place.bold, out);
                        }
                    }
                }
                XmlOut::Fragment(_) => {}
            }
        }
        out.push_str("</w:p>");
    }

    /// A text's runs, each chunk with its marks: links as hyperlinks, suggestions as tracked changes.
    fn runs<T: ReadTxn>(&mut self, t: &yrs::XmlTextRef, txn: &T, bold: bool, out: &mut String) {
        for chunk in t.diff(txn, YChange::identity) {
            let Out::Any(Any::String(s)) = chunk.insert else { continue };
            let attrs = chunk.attributes.as_deref();
            let get = |name: &str| present(attrs.and_then(|a| a.get(name)));
            let mut rpr = String::new();
            let link = get("link").and_then(|v| mark_attr(v, "href")).filter(|h| safe_href(h));
            if link.is_some() {
                rpr.push_str("<w:rStyle w:val=\"Hyperlink\"/>");
            }
            if get("code").is_some() {
                rpr.push_str("<w:rFonts w:ascii=\"Consolas\" w:hAnsi=\"Consolas\" w:cs=\"Consolas\"/>");
            }
            if bold || get("bold").is_some() {
                rpr.push_str("<w:b/>");
            }
            if get("italic").is_some() {
                rpr.push_str("<w:i/>");
            }
            if get("strike").is_some() {
                rpr.push_str("<w:strike/>");
            }
            if get("underline").is_some() {
                rpr.push_str("<w:u w:val=\"single\"/>");
            }
            if get("highlight").is_some() {
                rpr.push_str("<w:highlight w:val=\"yellow\"/>");
            }
            if get("subscript").is_some() {
                rpr.push_str("<w:vertAlign w:val=\"subscript\"/>");
            } else if get("superscript").is_some() {
                rpr.push_str("<w:vertAlign w:val=\"superscript\"/>");
            }
            let deleted = get("deletion");
            let mut run = String::new();
            text_run(&s, &rpr, if deleted.is_some() { "w:delText" } else { "w:t" }, &mut run);
            if let Some(href) = link {
                let id = self.rel("http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink", href, true);
                run = format!("<w:hyperlink r:id=\"{id}\" w:history=\"1\">{run}</w:hyperlink>");
            }
            let change = deleted.map(|v| ("w:del", v)).or_else(|| get("insertion").map(|v| ("w:ins", v)));
            if let Some((tag, v)) = change {
                self.changes += 1;
                let author = mark_attr(v, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Someone".into());
                let date = mark_attr(v, "at").filter(|d| d.len() >= 10 && d.is_ascii()).map(|d| format!(" w:date=\"{}\"", attr(&d))).unwrap_or_default();
                let _ = write!(out, "<{tag} w:id=\"{}\" w:author=\"{}\"{date}>{run}</{tag}>", self.changes, attr(&author));
            } else {
                out.push_str(&run);
            }
        }
    }

    fn table<T: ReadTxn>(&mut self, e: &XmlElementRef, txn: &T, place: &Place, out: &mut String) {
        // Cells a merged cell above still covers: (rows left, its width in columns), by column.
        let mut covered: Vec<Option<(i64, usize)>> = Vec::new();
        let mut rows = Vec::new();
        let mut width = 0usize;
        for row in e.children(txn) {
            let XmlOut::Element(row) = row else { continue };
            let mut cells: Vec<(usize, String)> = Vec::new();
            let mut col = 0usize;
            let continue_cell = |col: usize, span: usize, cells: &mut Vec<(usize, String)>| {
                let grid = if span > 1 { format!("<w:gridSpan w:val=\"{span}\"/>") } else { String::new() };
                cells.push((span, format!("<w:tcPr>{{W}}{grid}<w:vMerge/></w:tcPr><w:p/>")));
                let _ = col;
            };
            let skip_covered = |col: &mut usize, covered: &mut Vec<Option<(i64, usize)>>, cells: &mut Vec<(usize, String)>| {
                while let Some(Some((left, span))) = covered.get(*col).copied() {
                    continue_cell(*col, span, cells);
                    covered[*col] = if left > 1 { Some((left - 1, span)) } else { None };
                    *col += span;
                }
            };
            for cell in row.children(txn) {
                let XmlOut::Element(cell) = cell else { continue };
                skip_covered(&mut col, &mut covered, &mut cells);
                let span = number(cell.get_attribute(txn, "colspan")).unwrap_or(1).clamp(1, 63) as usize;
                let down = number(cell.get_attribute(txn, "rowspan")).unwrap_or(1).clamp(1, 1000);
                let header = &**cell.tag() == "tableHeader";
                let mut content = String::new();
                let inner = Place { bold: header || place.bold, num: None, indent: 0, prefix: None, style: None };
                self.blocks(cell.children(txn), txn, &inner, 0, &mut content);
                if !content.contains("<w:p") {
                    content.push_str("<w:p/>");
                }
                let grid = if span > 1 { format!("<w:gridSpan w:val=\"{span}\"/>") } else { String::new() };
                let merge = if down > 1 { "<w:vMerge w:val=\"restart\"/>" } else { "" };
                cells.push((span, format!("<w:tcPr>{{W}}{grid}{merge}</w:tcPr>{content}")));
                if down > 1 {
                    if covered.len() < col + 1 {
                        covered.resize(col + 1, None);
                    }
                    covered[col] = Some((down - 1, span));
                }
                col += span;
            }
            skip_covered(&mut col, &mut covered, &mut cells);
            width = width.max(col);
            rows.push(cells);
        }
        if width == 0 {
            return;
        }
        let col_w = TEXT_W / width as u32;
        out.push_str(
            "<w:tbl><w:tblPr><w:tblStyle w:val=\"TableGrid\"/><w:tblW w:w=\"5000\" w:type=\"pct\"/><w:tblLayout w:type=\"fixed\"/></w:tblPr><w:tblGrid>",
        );
        for _ in 0..width {
            let _ = write!(out, "<w:gridCol w:w=\"{col_w}\"/>");
        }
        out.push_str("</w:tblGrid>");
        for cells in rows {
            out.push_str("<w:tr>");
            for (span, cell) in cells {
                let w = format!("<w:tcW w:w=\"{}\" w:type=\"dxa\"/>", col_w * span as u32);
                let _ = write!(out, "<w:tc>{}</w:tc>", cell.replacen("{W}", &w, 1));
            }
            out.push_str("</w:tr>");
        }
        out.push_str("</w:tbl><w:p/>");
    }

    fn image<T: ReadTxn>(&mut self, e: &XmlElementRef, txn: &T, out: &mut String) {
        let alt = string(e.get_attribute(txn, "alt")).unwrap_or_default();
        let found = string(e.get_attribute(txn, "image")).and_then(|id| (self.images)(&id));
        let ext = found.as_ref().and_then(|i| match i.content_type.as_str() {
            "image/png" => Some("png"),
            "image/jpeg" => Some("jpeg"),
            "image/gif" => Some("gif"),
            "image/webp" => Some("webp"),
            _ => None,
        });
        let (Some(img), Some(ext)) = (found, ext) else {
            // Gone, or a kind Word cannot show: its description stands in.
            if !alt.trim().is_empty() {
                out.push_str("<w:p>");
                text_run(&format!("[{}]", alt.trim()), "<w:i/>", "w:t", out);
                out.push_str("</w:p>");
            }
            return;
        };
        self.drawings += 1;
        let n = self.drawings;
        let path = format!("media/image{n}.{ext}");
        let rid = self.rel("http://schemas.openxmlformats.org/officeDocument/2006/relationships/image", path.clone(), false);
        self.media.push((path, img.bytes));
        let (w, h) = (u64::from(img.width.max(1)), u64::from(img.height.max(1)));
        let (mut cx, mut cy) = (w * EMU_PER_PX, h * EMU_PER_PX);
        let max = u64::from(TEXT_W) * EMU_PER_TWIP;
        if cx > max {
            cy = cy * max / cx;
            cx = max;
        }
        let alt = attr(alt.trim());
        let _ = write!(
            out,
            "<w:p><w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\"><wp:extent cx=\"{cx}\" cy=\"{cy}\"/>\
             <wp:docPr id=\"{n}\" name=\"Image {n}\" descr=\"{alt}\"/><wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect=\"1\"/></wp:cNvGraphicFramePr>\
             <a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><pic:pic>\
             <pic:nvPicPr><pic:cNvPr id=\"{n}\" name=\"image{n}.{ext}\" descr=\"{alt}\"/><pic:cNvPicPr/></pic:nvPicPr>\
             <pic:blipFill><a:blip r:embed=\"{rid}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
             <pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr>\
             </pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>"
        );
    }
}

/// A run of text: tabs as Word's tabs, `tag` `w:t` or (deleted text) `w:delText`.
fn text_run(s: &str, rpr: &str, tag: &str, out: &mut String) {
    if s.is_empty() {
        return;
    }
    out.push_str("<w:r>");
    if !rpr.is_empty() {
        let _ = write!(out, "<w:rPr>{rpr}</w:rPr>");
    }
    for (n, part) in s.split('\t').enumerate() {
        if n > 0 {
            out.push_str("<w:tab/>");
        }
        if !part.is_empty() {
            let _ = write!(out, "<{tag} xml:space=\"preserve\">");
            xml(&part.replace(['\n', '\r'], " "), out);
            let _ = write!(out, "</{tag}>");
        }
    }
    out.push_str("</w:r>");
}

const STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:eastAsia="Calibri" w:cs="Calibri"/><w:sz w:val="22"/><w:szCs w:val="22"/><w:lang w:val="en-US"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120" w:line="276" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults>
<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style>
<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="360" w:after="120"/><w:outlineLvl w:val="0"/></w:pPr><w:rPr><w:b/><w:sz w:val="36"/><w:szCs w:val="36"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="280" w:after="100"/><w:outlineLvl w:val="1"/></w:pPr><w:rPr><w:b/><w:sz w:val="30"/><w:szCs w:val="30"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading3"><w:name w:val="heading 3"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="240" w:after="80"/><w:outlineLvl w:val="2"/></w:pPr><w:rPr><w:b/><w:sz w:val="26"/><w:szCs w:val="26"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading4"><w:name w:val="heading 4"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:outlineLvl w:val="3"/></w:pPr><w:rPr><w:b/><w:sz w:val="24"/><w:szCs w:val="24"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading5"><w:name w:val="heading 5"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:outlineLvl w:val="4"/></w:pPr><w:rPr><w:b/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading6"><w:name w:val="heading 6"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:outlineLvl w:val="5"/></w:pPr><w:rPr><w:b/><w:i/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Quote"><w:name w:val="Quote"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:pBdr><w:left w:val="single" w:sz="18" w:space="8" w:color="D0D7DE"/></w:pBdr><w:ind w:left="284"/></w:pPr><w:rPr><w:color w:val="57606A"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Code"><w:name w:val="Code"/><w:basedOn w:val="Normal"/><w:pPr><w:shd w:val="clear" w:color="auto" w:fill="F6F8FA"/><w:spacing w:after="120" w:line="240" w:lineRule="auto"/></w:pPr><w:rPr><w:rFonts w:ascii="Consolas" w:hAnsi="Consolas" w:cs="Consolas"/><w:sz w:val="20"/><w:szCs w:val="20"/></w:rPr></w:style>
<w:style w:type="character" w:styleId="Hyperlink"><w:name w:val="Hyperlink"/><w:rPr><w:color w:val="0969DA"/><w:u w:val="single"/></w:rPr></w:style>
<w:style w:type="table" w:styleId="TableGrid"><w:name w:val="Table Grid"/><w:tblPr><w:tblBorders><w:top w:val="single" w:sz="4" w:space="0" w:color="D0D7DE"/><w:left w:val="single" w:sz="4" w:space="0" w:color="D0D7DE"/><w:bottom w:val="single" w:sz="4" w:space="0" w:color="D0D7DE"/><w:right w:val="single" w:sz="4" w:space="0" w:color="D0D7DE"/><w:insideH w:val="single" w:sz="4" w:space="0" w:color="D0D7DE"/><w:insideV w:val="single" w:sz="4" w:space="0" w:color="D0D7DE"/></w:tblBorders><w:tblCellMar><w:left w:w="108" w:type="dxa"/><w:right w:w="108" w:type="dxa"/></w:tblCellMar></w:tblPr></w:style>
</w:styles>"#;

fn numbering(ordered: &[(u32, i64)]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:numbering xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">",
    );
    out.push_str("<w:abstractNum w:abstractNumId=\"0\"><w:multiLevelType w:val=\"hybridMultilevel\"/>");
    for l in 0..9u32 {
        let bullet = ["\u{2022}", "\u{25E6}", "\u{25AA}"][l as usize % 3];
        let _ = write!(
            out,
            "<w:lvl w:ilvl=\"{l}\"><w:start w:val=\"1\"/><w:numFmt w:val=\"bullet\"/><w:lvlText w:val=\"{bullet}\"/><w:lvlJc w:val=\"left\"/><w:pPr><w:ind w:left=\"{}\" w:hanging=\"360\"/></w:pPr></w:lvl>",
            720 * (l + 1)
        );
    }
    out.push_str("</w:abstractNum><w:abstractNum w:abstractNumId=\"1\"><w:multiLevelType w:val=\"hybridMultilevel\"/>");
    for l in 0..9u32 {
        let fmt = ["decimal", "lowerLetter", "lowerRoman"][l as usize % 3];
        let _ = write!(
            out,
            "<w:lvl w:ilvl=\"{l}\"><w:start w:val=\"1\"/><w:numFmt w:val=\"{fmt}\"/><w:lvlText w:val=\"%{}.\"/><w:lvlJc w:val=\"left\"/><w:pPr><w:ind w:left=\"{}\" w:hanging=\"360\"/></w:pPr></w:lvl>",
            l + 1,
            720 * (l + 1)
        );
    }
    out.push_str("</w:abstractNum>");
    let _ = write!(out, "<w:num w:numId=\"{BULLETS}\"><w:abstractNumId w:val=\"0\"/></w:num>");
    for (n, (level, start)) in ordered.iter().enumerate() {
        let _ = write!(
            out,
            "<w:num w:numId=\"{}\"><w:abstractNumId w:val=\"1\"/><w:lvlOverride w:ilvl=\"{level}\"><w:startOverride w:val=\"{start}\"/></w:lvlOverride></w:num>",
            BULLETS + 1 + n
        );
    }
    out.push_str("</w:numbering>");
    out
}

/// The document as a Word file titled `title`; `images` gives an image's bytes by its id.
pub fn docx(doc: &Doc, title: &str, images: &dyn Fn(&str) -> Option<ImageData>) -> Result<Vec<u8>> {
    let mut w = Writer { images, rels: Vec::new(), media: Vec::new(), ordered: Vec::new(), drawings: 0, changes: 0 };
    let mut body = String::new();
    {
        let txn = doc.transact();
        if let Some(fragment) = txn.get_xml_fragment(FRAGMENT) {
            w.blocks(fragment.children(&txn), &txn, &Place::default(), 0, &mut body);
        }
    }
    let document = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
         xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
         xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
         xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
         xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><w:body>{body}\
         <w:sectPr><w:pgSz w:w=\"{PAGE_W}\" w:h=\"{PAGE_H}\"/><w:pgMar w:top=\"{MARGIN}\" w:right=\"{MARGIN}\" w:bottom=\"{MARGIN}\" w:left=\"{MARGIN}\" \
         w:header=\"709\" w:footer=\"709\" w:gutter=\"0\"/></w:sectPr></w:body></w:document>"
    );
    let mut rels = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    let _ = write!(
        rels,
        "<Relationship Id=\"{RID_STYLES}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
         <Relationship Id=\"{RID_NUMBERING}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering\" Target=\"numbering.xml\"/>"
    );
    for (id, kind, target, external) in &w.rels {
        let mode = if *external { " TargetMode=\"External\"" } else { "" };
        let _ = write!(rels, "<Relationship Id=\"{id}\" Type=\"{kind}\" Target=\"{}\"{mode}/>", attr(target));
    }
    rels.push_str("</Relationships>");
    let mut types = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
         <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
         <Default Extension=\"xml\" ContentType=\"application/xml\"/>",
    );
    for (ext, ctype) in [("png", "image/png"), ("jpeg", "image/jpeg"), ("gif", "image/gif"), ("webp", "image/webp")] {
        if w.media.iter().any(|(p, _)| p.ends_with(&format!(".{ext}"))) {
            let _ = write!(types, "<Default Extension=\"{ext}\" ContentType=\"{ctype}\"/>");
        }
    }
    types.push_str(
        "<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
         <Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
         <Override PartName=\"/word/numbering.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml\"/>\
         <Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\
         <Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/></Types>",
    );
    let root_rels = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
         <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
         <Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\
         <Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties\" Target=\"docProps/app.xml\"/></Relationships>";
    let core = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>{}</dc:title><dc:creator>Ferrum</dc:creator></cp:coreProperties>",
        attr(title)
    );
    let app = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\"><Application>Ferrum</Application></Properties>";

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let parts: [(&str, &[u8]); 7] = [
        ("[Content_Types].xml", types.as_bytes()),
        ("_rels/.rels", root_rels.as_bytes()),
        ("docProps/core.xml", core.as_bytes()),
        ("docProps/app.xml", app.as_bytes()),
        ("word/document.xml", document.as_bytes()),
        ("word/styles.xml", STYLES.as_bytes()),
        ("word/_rels/document.xml.rels", rels.as_bytes()),
    ];
    let failed = |e: &dyn std::fmt::Display| Error::internal(format!("writing the Word file: {e}"));
    for (name, data) in parts {
        zip.start_file(name, opts).map_err(|e| failed(&e))?;
        zip.write_all(data).map_err(|e| failed(&e))?;
    }
    zip.start_file("word/numbering.xml", opts).map_err(|e| failed(&e))?;
    zip.write_all(numbering(&w.ordered).as_bytes()).map_err(|e| failed(&e))?;
    for (path, bytes) in &w.media {
        // Images are compressed already.
        zip.start_file(format!("word/{path}"), stored).map_err(|e| failed(&e))?;
        zip.write_all(bytes).map_err(|e| failed(&e))?;
    }
    Ok(zip.finish().map_err(|e| failed(&e))?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::super::new_doc;
    use super::*;
    use std::collections::HashMap;
    use std::io::Read as _;
    use yrs::{XmlElementPrelim, XmlTextPrelim};

    fn part(file: &[u8], name: &str) -> String {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(file)).expect("a zip");
        let mut s = String::new();
        z.by_name(name).unwrap_or_else(|_| panic!("{name} is in the file")).read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn a_document_as_a_word_file() {
        let doc = new_doc();
        let fragment = doc.get_or_insert_xml_fragment(FRAGMENT);
        {
            let mut txn = doc.transact_mut();
            let h = fragment.push_back(&mut txn, XmlElementPrelim::empty("heading"));
            h.insert_attribute(&mut txn, "level", Any::from(2));
            h.push_back(&mut txn, XmlTextPrelim::new("Plan <A&B>"));
            let p = fragment.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.insert_attribute(&mut txn, "textAlign", "center");
            let t = p.push_back(&mut txn, XmlTextPrelim::new("Hello "));
            t.insert_with_attributes(&mut txn, 6, "world", [("bold".into(), Any::from(true))].into_iter().collect());
            let link = Any::from(HashMap::from([("href".to_string(), Any::from("https://example.com/?a=1&b=2"))]));
            t.insert_with_attributes(&mut txn, 11, " site", [("link".into(), link)].into_iter().collect());
            let bad = Any::from(HashMap::from([("href".to_string(), Any::from("javascript:alert(1)"))]));
            t.insert_with_attributes(&mut txn, 16, " trap\u{1}", [("link".into(), bad)].into_iter().collect());
            let who = |at: &str| Any::from(HashMap::from([("name".to_string(), Any::from("Ana <A>")), ("at".to_string(), Any::from(at))]));
            t.insert_with_attributes(&mut txn, 22, " new", [("insertion".into(), who("2026-10-31T10:00:00Z"))].into_iter().collect());
            t.insert_with_attributes(&mut txn, 26, " old", [("deletion".into(), who("2026-10-31T10:01:00Z"))].into_iter().collect());
            for (list, start) in [("orderedList", Some(3)), ("orderedList", None), ("bulletList", None)] {
                let l = fragment.push_back(&mut txn, XmlElementPrelim::empty(list));
                if let Some(s) = start {
                    l.insert_attribute(&mut txn, "start", Any::from(s));
                }
                let li = l.push_back(&mut txn, XmlElementPrelim::empty("listItem"));
                let lp = li.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
                lp.push_back(&mut txn, XmlTextPrelim::new("item"));
            }
            let tasks = fragment.push_back(&mut txn, XmlElementPrelim::empty("taskList"));
            let task = tasks.push_back(&mut txn, XmlElementPrelim::empty("taskItem"));
            task.insert_attribute(&mut txn, "checked", true);
            task.push_back(&mut txn, XmlElementPrelim::empty("paragraph")).push_back(&mut txn, XmlTextPrelim::new("done"));
            // A 2×2 table whose first column is one merged cell, and a header spanning two columns.
            let table = fragment.push_back(&mut txn, XmlElementPrelim::empty("table"));
            let r1 = table.push_back(&mut txn, XmlElementPrelim::empty("tableRow"));
            let c = r1.push_back(&mut txn, XmlElementPrelim::empty("tableHeader"));
            c.insert_attribute(&mut txn, "rowspan", Any::from(2));
            c.push_back(&mut txn, XmlElementPrelim::empty("paragraph")).push_back(&mut txn, XmlTextPrelim::new("tall"));
            let c = r1.push_back(&mut txn, XmlElementPrelim::empty("tableCell"));
            c.push_back(&mut txn, XmlElementPrelim::empty("paragraph")).push_back(&mut txn, XmlTextPrelim::new("b"));
            let r2 = table.push_back(&mut txn, XmlElementPrelim::empty("tableRow"));
            let c = r2.push_back(&mut txn, XmlElementPrelim::empty("tableCell"));
            c.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            for (id, alt) in [("i1", "A chart"), ("gone", "Lost picture")] {
                let img = fragment.push_back(&mut txn, XmlElementPrelim::empty(IMAGE));
                img.insert_attribute(&mut txn, "image", id);
                img.insert_attribute(&mut txn, "alt", alt);
            }
            let code = fragment.push_back(&mut txn, XmlElementPrelim::empty("codeBlock"));
            code.push_back(&mut txn, XmlTextPrelim::new("a\tb\nc"));
            fragment.push_back(&mut txn, XmlElementPrelim::empty("horizontalRule"));
        }
        let png = [&b"\x89PNG\r\n\x1a\n"[..], &[0u8; 24]].concat();
        let file =
            docx(&doc, "Q3 <draft>", &|id| (id == "i1").then(|| ImageData { bytes: png.clone(), content_type: "image/png".into(), width: 2000, height: 1000 }))
                .unwrap();
        // To look at it in an office suite: FERRUM_DOCX_SAMPLE=/tmp/sample.docx cargo test …
        if let Ok(path) = std::env::var("FERRUM_DOCX_SAMPLE") {
            std::fs::write(path, &file).unwrap();
        }
        let body = part(&file, "word/document.xml");
        assert!(body.contains("<w:pStyle w:val=\"Heading2\"/></w:pPr><w:r><w:t xml:space=\"preserve\">Plan &lt;A&amp;B&gt;</w:t>"), "{body}");
        assert!(body.contains("<w:jc w:val=\"center\"/>") && body.contains("<w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">world</w:t>"), "{body}");
        assert!(
            body.contains(
                "<w:hyperlink r:id=\"rId1\" w:history=\"1\"><w:r><w:rPr><w:rStyle w:val=\"Hyperlink\"/></w:rPr><w:t xml:space=\"preserve\"> site</w:t>"
            ),
            "{body}"
        );
        assert!(body.contains("<w:t xml:space=\"preserve\"> trap</w:t>"), "unsafe links and control characters go: {body}");
        assert!(body.contains("<w:ins w:id=\"1\" w:author=\"Ana &lt;A&gt;\" w:date=\"2026-10-31T10:00:00Z\">"), "{body}");
        assert!(
            body.contains(
                "<w:del w:id=\"2\" w:author=\"Ana &lt;A&gt;\" w:date=\"2026-10-31T10:01:00Z\"><w:r><w:delText xml:space=\"preserve\"> old</w:delText>"
            ),
            "{body}"
        );
        assert!(body.contains("<w:numId w:val=\"2\"/>") && body.contains("<w:numId w:val=\"3\"/>") && body.contains("<w:numId w:val=\"1\"/>"), "{body}");
        assert!(body.contains("\u{2611} "), "a checked task's box: {body}");
        assert!(body.contains("<w:vMerge w:val=\"restart\"/>") && body.contains("<w:vMerge/></w:tcPr><w:p/>"), "merged cells: {body}");
        assert!(body.contains("<w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">tall</w:t>"), "header cells are bold: {body}");
        assert!(body.contains("r:embed=\"rId2\"") && body.contains("descr=\"A chart\""), "only safe links take a relationship: {body}");
        assert!(body.contains("<wp:extent cx=\"6120130\" cy=\"3060065\"/>"), "a wide image fits the page: {body}");
        assert!(body.contains("<w:i/></w:rPr><w:t xml:space=\"preserve\">[Lost picture]</w:t>"), "a missing image's description: {body}");
        assert!(body.contains("a</w:t></w:r><w:r><w:tab/>") || body.contains("<w:tab/><w:t xml:space=\"preserve\">b</w:t>"), "{body}");
        assert!(!body.contains('\u{1}'));
        let numbering = part(&file, "word/numbering.xml");
        assert!(
            numbering.contains("<w:num w:numId=\"2\"><w:abstractNumId w:val=\"1\"/><w:lvlOverride w:ilvl=\"0\"><w:startOverride w:val=\"3\"/>"),
            "{numbering}"
        );
        assert!(
            numbering.contains("<w:num w:numId=\"3\"><w:abstractNumId w:val=\"1\"/><w:lvlOverride w:ilvl=\"0\"><w:startOverride w:val=\"1\"/>"),
            "lists restart: {numbering}"
        );
        let rels = part(&file, "word/_rels/document.xml.rels");
        assert!(rels.contains("Target=\"https://example.com/?a=1&amp;b=2\" TargetMode=\"External\"") && rels.contains("Target=\"media/image1.png\""), "{rels}");
        assert!(!rels.contains("javascript"), "{rels}");
        assert!(part(&file, "[Content_Types].xml").contains("<Default Extension=\"png\" ContentType=\"image/png\"/>"));
        assert!(part(&file, "docProps/core.xml").contains("<dc:title>Q3 &lt;draft&gt;</dc:title>"));
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(&file[..])).unwrap();
        let mut img = Vec::new();
        z.by_name("word/media/image1.png").unwrap().read_to_end(&mut img).unwrap();
        assert_eq!(img, png);
    }

    #[test]
    fn an_empty_document_is_a_word_file_too() {
        let file = docx(&new_doc(), "Empty", &|_| None).unwrap();
        assert!(part(&file, "word/document.xml").contains("<w:body><w:sectPr>"));
    }
}
