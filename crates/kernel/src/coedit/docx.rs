//! A document as a Word file (`.docx`, Office Open XML) for downloads, carrying what the editor
//! stores (Docs' stored format 2): paragraphs and headings with their alignment, spacing, indents
//! and styles (Title, Subtitle, Heading 1-6, defined in `styles.xml` so Word's navigation and its
//! own contents work); lists with their bullets and numbers; checklists, quotes and callouts, code,
//! dividers; tables with their column widths, merged cells, cell colours and repeating header
//! rows; images at their size and place, with their caption and description; the character
//! formatting (fonts with a family to fall back on, sizes, colours, any highlight colour, sub- and
//! superscript); links to the web, mail, the document's headings (bookmarks) and other documents;
//! the page's size, orientation and margins; page breaks; the header and footer with their page
//! number, page count, date and title fields (a different first page too); footnotes as Word's
//! own; the contents as Word's TOC field holding the current headings; suggestions as tracked
//! changes. Every value is checked (`format`) before it is written.

use super::comments::{self, CommentThread, Edge, ptr};
use super::format::{self, Href, Outline, Page, Slot, any, mark, mark_field};
use super::{FRAGMENT, IMAGE, number, string, truthy};
use crate::{Error, Result};
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::Write as _;
use yrs::branch::BranchPtr;
use yrs::types::text::YChange;
use yrs::{Any, Doc, GetString, Out, ReadTxn, Text, Transact, Xml, XmlElementRef, XmlFragment, XmlOut};

/// An image's bytes, for a Word file.
pub struct ImageData {
    pub bytes: Vec<u8>,
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub content_type: String,
    /// In pixels: drawn this size (unless the document gives one), or smaller to fit the page.
    pub width: u32,
    pub height: u32,
}

const EMU_PER_PX: u64 = 9525;
/// The editor's line height is a multiple of the font size (CSS); Word's `auto` spacing is a
/// multiple of the font's own line (Arial's is 1.117 of its size). A ratio as Word writes it.
fn line(ratio: f64) -> u32 {
    (ratio / 1.117 * 240.0).round() as u32
}
/// The editor's list indents (`padding-left` 1.6em for bullets, 1.8em for numbers, at 11 pt), and
/// where the bullet or number hangs from it.
const BULLET_STEP: u32 = 352;
const NUMBER_STEP: u32 = 396;
const HANGING: u32 = 260;
/// A list item's paragraphs (0.15em above and below), and a table cell's.
const TIGHT: (u32, u32) = (0, 33);
const CELL: (u32, u32) = (33, 33);
const EMU_PER_TWIP: u64 = 635;
const NS: &str = "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
                  xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
                  xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
                  xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
                  xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const TYPE: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml";

/// How a block is written inside what holds it.
#[derive(Clone, Default)]
struct Place {
    /// The paragraph that starts a list item: its numbering (num id) and level.
    num: Option<(usize, u32)>,
    /// A list item's later paragraphs line up with its first one.
    indent: u32,
    style: Option<&'static str>,
    /// Header cells.
    bold: bool,
    /// Runs before the paragraph's first words (a checklist's box, a footnote's number).
    prefix: Option<String>,
    /// A callout's colour.
    shade: Option<&'static str>,
    /// The space before and after its paragraphs, in twips, where the editor's differs from a
    /// paragraph's own (list items, table cells); a paragraph's `spaceBefore`/`spaceAfter` win.
    gap: Option<(u32, u32)>,
}

/// A list's numbering: bullets or numbers, its level, the glyph or number format, its start.
#[derive(Clone, PartialEq)]
struct List {
    ordered: bool,
    level: u32,
    format: &'static str,
    start: i64,
}

struct Writer<'a> {
    images: &'a dyn Fn(&str) -> Option<ImageData>,
    origin: Option<&'a str>,
    title: &'a str,
    date: String,
    page: Page,
    outline: Outline,
    /// The part being written's hyperlinks and images: (id, type, target, external).
    rels: Vec<(String, &'static str, String, bool)>,
    /// Image files under `word/`.
    media: Vec<(String, Vec<u8>)>,
    lists: Vec<List>,
    /// Fonts named in the text: (name, family).
    fonts: BTreeSet<(String, &'static str)>,
    drawings: u32,
    changes: u32,
    bookmarks: u32,
    headings: usize,
    notes: usize,
    /// Writing a footnote's body: references inside it are not the text's.
    in_note: bool,
    lost: BTreeSet<String>,
    /// Comment threads, the Word id of each one's first comment, where their ranges go (per text),
    /// and which ranges have started and ended.
    threads: &'a [CommentThread],
    first: Vec<usize>,
    marks: HashMap<BranchPtr, Vec<(u32, Edge, usize)>>,
    started: Vec<bool>,
    ended: Vec<bool>,
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

/// Word's highlight colours, by their value: a highlight of another colour is a shading.
const HIGHLIGHTS: [(&str, &str); 16] = [
    ("#ffff00", "yellow"),
    ("#00ff00", "green"),
    ("#00ffff", "cyan"),
    ("#ff00ff", "magenta"),
    ("#0000ff", "blue"),
    ("#ff0000", "red"),
    ("#000080", "darkBlue"),
    ("#008080", "darkCyan"),
    ("#008000", "darkGreen"),
    ("#800080", "darkMagenta"),
    ("#800000", "darkRed"),
    ("#808000", "darkYellow"),
    ("#808080", "darkGray"),
    ("#c0c0c0", "lightGray"),
    ("#000000", "black"),
    ("#ffffff", "white"),
];

/// `#1f2a44` as Word writes it: `1F2A44`.
fn hex(c: &str) -> String {
    c.trim_start_matches('#').to_ascii_uppercase()
}

impl Writer<'_> {
    fn rel(&mut self, kind: &'static str, target: String, external: bool) -> String {
        let id = format!("rId{}", self.rels.len() + 1);
        self.rels.push((id.clone(), kind, target, external));
        id
    }

    /// Writes a part of its own (a header, the footnotes): its hyperlinks and images are its own.
    fn part(&mut self, write: impl FnOnce(&mut Self, &mut String)) -> (String, Vec<(String, &'static str, String, bool)>) {
        let saved = std::mem::take(&mut self.rels);
        let mut out = String::new();
        write(self, &mut out);
        (out, std::mem::replace(&mut self.rels, saved))
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
        let attr = |k: &str| any(e.get_attribute(txn, k));
        match &*tag {
            "paragraph" => {
                let style = match format::role(attr("role").as_ref()) {
                    Some("title") => Some("Title"),
                    Some(_) => Some("Subtitle"),
                    None => place.style,
                };
                self.paragraph(e, txn, place, style, None, out);
            }
            "heading" => {
                let level = number(e.get_attribute(txn, "level")).unwrap_or(1).clamp(1, 6) as usize;
                let n = self.headings;
                self.headings += 1;
                let name = bookmark(self.outline.headings.get(n).and_then(|h| h.anchor.as_deref()), n);
                self.paragraph(e, txn, place, Some(["Heading1", "Heading2", "Heading3", "Heading4", "Heading5", "Heading6"][level - 1]), Some(&name), out);
            }
            "blockquote" => {
                let shade = format::callout(attr("callout").as_ref()).map(|k| match k {
                    "note" => "E7F0FB",
                    "tip" => "E6F4EA",
                    "warning" => "FFF4E0",
                    _ => "FDE8E8",
                });
                let inner = Place { style: Some("Quote"), shade, ..place.clone() };
                self.blocks(e.children(txn), txn, &inner, depth, out);
            }
            "codeBlock" => {
                let mut code = String::new();
                // Comments on code cover the whole block: its lines are written as one text.
                let mut marks = Vec::new();
                for child in e.children(txn) {
                    if let XmlOut::Text(t) = child {
                        code.push_str(&t.get_string(txn));
                        marks.extend(self.marks.remove(&ptr(&t)).unwrap_or_default());
                    }
                }
                out.push_str("<w:p><w:pPr><w:pStyle w:val=\"Code\"/></w:pPr>");
                for &(_, edge, n) in marks.iter().filter(|m| m.1 == Edge::Start) {
                    self.comment_mark(edge, n, out);
                }
                for (n, line) in code.split('\n').enumerate() {
                    if n > 0 {
                        out.push_str("<w:r><w:br/></w:r>");
                    }
                    text_run(line, "", "w:t", out);
                }
                for &(_, edge, n) in marks.iter().filter(|m| m.1 != Edge::Start) {
                    self.comment_mark(edge, n, out);
                }
                out.push_str("</w:p>");
            }
            "horizontalRule" => {
                out.push_str("<w:p><w:pPr><w:pBdr><w:bottom w:val=\"single\" w:sz=\"6\" w:space=\"1\" w:color=\"D0D7DE\"/></w:pBdr></w:pPr></w:p>")
            }
            "pageBreak" => out.push_str("<w:p><w:r><w:br w:type=\"page\"/></w:r></w:p>"),
            "tableOfContents" => self.contents(e, txn, out),
            // Bodies go to the footnotes part, in the order of their references.
            "footnote" => {}
            list @ ("bulletList" | "orderedList" | "taskList") => {
                let level = depth.min(8);
                let num = match list {
                    "bulletList" => {
                        let glyph = match format::bullet(attr("bullet").as_ref()) {
                            Some("circle") => "\u{25E6}",
                            Some(_) => "\u{25AA}",
                            None => ["\u{2022}", "\u{25E6}", "\u{25AA}"][level as usize % 3],
                        };
                        self.lists.push(List { ordered: false, level, format: glyph, start: 1 });
                        Some(self.lists.len())
                    }
                    "orderedList" => {
                        let fmt = match format::numbering(attr("type").as_ref()) {
                            "a" => "lowerLetter",
                            "i" => "lowerRoman",
                            _ => "decimal",
                        };
                        let start = number(e.get_attribute(txn, "start")).unwrap_or(1).clamp(0, 32767);
                        self.lists.push(List { ordered: true, level, format: fmt, start });
                        Some(self.lists.len())
                    }
                    _ => None,
                };
                for item in e.children(txn) {
                    let XmlOut::Element(li) = item else { continue };
                    let prefix = (list == "taskList").then(|| if truthy(li.get_attribute(txn, "checked")) { "\u{2611} " } else { "\u{2610} " });
                    let indent = if list == "orderedList" { NUMBER_STEP } else { BULLET_STEP } * (level + 1);
                    let mut first = true;
                    for child in li.children(txn) {
                        let XmlOut::Element(c) = child else { continue };
                        let nested = matches!(&**c.tag(), "bulletList" | "orderedList" | "taskList");
                        let item_place = Place {
                            num: if first && !nested { num.map(|n| (n, level)) } else { None },
                            indent: if first && num.is_some() && !nested { 0 } else { indent },
                            prefix: if first && !nested {
                                prefix.map(|p| {
                                    let mut r = String::new();
                                    text_run(p, "", "w:t", &mut r);
                                    r
                                })
                            } else {
                                None
                            },
                            gap: place.gap.or(Some(TIGHT)),
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

    fn paragraph<T: ReadTxn>(&mut self, e: &XmlElementRef, txn: &T, place: &Place, style: Option<&str>, mark_as: Option<&str>, out: &mut String) {
        let attr = |k: &str| any(e.get_attribute(txn, k));
        let mut ppr = String::new();
        if let Some(style) = style {
            let _ = write!(ppr, "<w:pStyle w:val=\"{style}\"/>");
        }
        if let Some((num, level)) = place.num {
            let _ = write!(ppr, "<w:numPr><w:ilvl w:val=\"{level}\"/><w:numId w:val=\"{num}\"/></w:numPr>");
        }
        if let Some(fill) = place.shade {
            let _ = write!(ppr, "<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"{fill}\"/>");
        }
        let mut spacing = String::new();
        if let Some(tw) = format::space(attr("spaceBefore").as_ref()).map(|pt| pt * 20).or(place.gap.map(|g| g.0)) {
            let _ = write!(spacing, " w:before=\"{tw}\"");
        }
        if let Some(tw) = format::space(attr("spaceAfter").as_ref()).map(|pt| pt * 20).or(place.gap.map(|g| g.1)) {
            let _ = write!(spacing, " w:after=\"{tw}\"");
        }
        if let Some(lh) = format::line_height(attr("lineHeight").as_ref()) {
            let _ = write!(spacing, " w:line=\"{}\" w:lineRule=\"auto\"", line(lh));
        }
        if !spacing.is_empty() {
            let _ = write!(ppr, "<w:spacing{spacing}/>");
        }
        let left = if place.num.is_some() { 0 } else { place.indent + format::indent(attr("indent").as_ref()).unwrap_or(0) * 720 };
        if left > 0 {
            let _ = write!(ppr, "<w:ind w:left=\"{left}\"/>");
        }
        let jc = match format::text_align(attr("textAlign").as_ref()) {
            Some("center") => Some("center"),
            Some("right") => Some("right"),
            Some("justify") => Some("both"),
            _ => None,
        };
        if let Some(jc) = jc {
            let _ = write!(ppr, "<w:jc w:val=\"{jc}\"/>");
        }
        out.push_str("<w:p>");
        if !ppr.is_empty() {
            let _ = write!(out, "<w:pPr>{ppr}</w:pPr>");
        }
        let bookmark = mark_as.map(|name| {
            self.bookmarks += 1;
            let _ = write!(out, "<w:bookmarkStart w:id=\"{}\" w:name=\"{name}\"/>", self.bookmarks);
            self.bookmarks
        });
        if let Some(prefix) = &place.prefix {
            out.push_str(prefix);
        }
        self.inline(e, txn, place.bold, out);
        if let Some(id) = bookmark {
            let _ = write!(out, "<w:bookmarkEnd w:id=\"{id}\"/>");
        }
        out.push_str("</w:p>");
    }

    /// A paragraph's content: its text and the nodes inside its lines.
    fn inline<T: ReadTxn>(&mut self, e: &XmlElementRef, txn: &T, bold: bool, out: &mut String) {
        for child in e.children(txn) {
            match child {
                XmlOut::Text(t) => self.runs(&t, txn, bold, out),
                XmlOut::Element(c) => match &**c.tag() {
                    "hardBreak" => out.push_str("<w:r><w:br/></w:r>"),
                    "footnoteRef" => {
                        let n = self.notes;
                        if !self.in_note {
                            self.notes += 1;
                        }
                        if !self.in_note && n < self.outline.notes.len() {
                            let _ = write!(out, "<w:r><w:rPr><w:rStyle w:val=\"FootnoteReference\"/></w:rPr><w:footnoteReference w:id=\"{}\"/></w:r>", n + 1);
                        }
                    }
                    "pageField" => {
                        let (instr, shown) = match format::field(any(c.get_attribute(txn, "kind")).as_ref()) {
                            "page" => (" PAGE ".to_string(), "1".to_string()),
                            "pages" => (" NUMPAGES ".into(), "1".into()),
                            "date" => (" DATE \\@ \"d MMMM yyyy\" ".into(), self.date.clone()),
                            _ => (" TITLE ".into(), self.title.to_string()),
                        };
                        let _ = write!(out, "<w:fldSimple w:instr=\"{}\">", attr(&instr));
                        text_run(if shown.is_empty() { " " } else { &shown }, if bold { "<w:b/>" } else { "" }, "w:t", out);
                        out.push_str("</w:fldSimple>");
                    }
                    // A mention of a record links to it.
                    "mention" => match format::mention_href(&c, txn, self.origin) {
                        Some(href) => {
                            let mut runs = String::new();
                            self.inline(&c, txn, bold, &mut runs);
                            let id = self.rel("http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink", href, true);
                            let _ = write!(out, "<w:hyperlink r:id=\"{id}\" w:history=\"1\">{runs}</w:hyperlink>");
                        }
                        None => self.inline(&c, txn, bold, out),
                    },
                    // Nodes the editor may add inside a line (others): their words.
                    _ => self.inline(&c, txn, bold, out),
                },
                XmlOut::Fragment(_) => {}
            }
        }
    }

    /// A comment range's edge, once.
    fn comment_mark(&mut self, edge: Edge, n: usize, out: &mut String) {
        match edge {
            Edge::Start if !self.started[n] => {
                self.started[n] = true;
                comments::starts(self.threads, &self.first, n, out);
            }
            Edge::End | Edge::EmptyEnd if self.started[n] && !self.ended[n] => {
                self.ended[n] = true;
                comments::ends(self.threads, &self.first, n, out);
            }
            _ => {}
        }
    }

    /// A text's runs, each chunk with its marks: links as hyperlinks, suggestions as tracked changes;
    /// cut where a comment's range starts or ends.
    fn runs<T: ReadTxn>(&mut self, t: &yrs::XmlTextRef, txn: &T, bold: bool, out: &mut String) {
        // A footnote's comments go to the document's start: a range cannot cross into the notes.
        let marks = if self.in_note { Vec::new() } else { self.marks.remove(&ptr(&t)).unwrap_or_default() };
        let mut next = 0;
        let mut pos = 0u32;
        for chunk in t.diff(txn, YChange::identity) {
            let Out::Any(Any::String(s)) = chunk.insert else {
                self.flush(&marks, &mut next, pos, out);
                pos += 1;
                continue;
            };
            let attrs = chunk.attributes.as_deref();
            let mut rest: &str = &s;
            loop {
                self.flush(&marks, &mut next, pos, out);
                let len = rest.encode_utf16().count() as u32;
                match marks.get(next).map(|m| m.0).filter(|&o| o < pos + len) {
                    Some(cut) => {
                        let at = utf16_boundary(rest, (cut - pos) as usize);
                        self.piece(&rest[..at], attrs, bold, out);
                        rest = &rest[at..];
                        pos = cut;
                    }
                    None => {
                        self.piece(rest, attrs, bold, out);
                        pos += len;
                        break;
                    }
                }
            }
        }
        self.flush(&marks, &mut next, u32::MAX, out);
    }

    /// The comment edges of `marks` from `next` that are at or before `upto`.
    fn flush(&mut self, marks: &[(u32, Edge, usize)], next: &mut usize, upto: u32, out: &mut String) {
        while let Some(&(_, edge, n)) = marks.get(*next).filter(|m| m.0 <= upto) {
            self.comment_mark(edge, n, out);
            *next += 1;
        }
    }

    /// Words with one set of marks.
    fn piece(&mut self, s: &str, attrs: Option<&yrs::types::Attrs>, bold: bool, out: &mut String) {
        if s.is_empty() {
            return;
        }
        {
            let get = |name: &str| mark(attrs, name);
            let link = match format::href(get("link")) {
                Some(Href::Web(h)) => Some((h, true)),
                Some(Href::Heading(a)) => Some((format!("h_{a}"), false)),
                Some(Href::Doc(id)) => match self.origin {
                    Some(o) => Some((format!("{o}/docs/d/{id}"), true)),
                    None => {
                        self.lost.insert(super::html::LOST_DOC_LINKS.into());
                        None
                    }
                },
                None => None,
            };
            // Run properties, in the order the schema has them.
            let mut rpr = String::new();
            if link.is_some() {
                rpr.push_str("<w:rStyle w:val=\"Hyperlink\"/>");
            }
            let style = get("textStyle");
            match format::font_family(mark_field(style, "fontFamily")) {
                Some(f) => {
                    let (name, family) = format::word_font(&f);
                    let n = attr(&name);
                    let _ = write!(rpr, "<w:rFonts w:ascii=\"{n}\" w:hAnsi=\"{n}\" w:eastAsia=\"{n}\" w:cs=\"{n}\"/>");
                    self.fonts.insert((name, family));
                }
                None if get("code").is_some() => rpr.push_str("<w:rFonts w:ascii=\"Consolas\" w:hAnsi=\"Consolas\" w:cs=\"Consolas\"/>"),
                None => {}
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
            if let Some(c) = format::color(mark_field(style, "color")) {
                let _ = write!(rpr, "<w:color w:val=\"{}\"/>", hex(&c));
            }
            if let Some(pt) = format::font_size(mark_field(style, "fontSize")) {
                let half = (pt * 2.0) as u32;
                let _ = write!(rpr, "<w:sz w:val=\"{half}\"/><w:szCs w:val=\"{half}\"/>");
            }
            let mut shading = None;
            if let Some(h) = get("highlight") {
                match format::color(mark_field(Some(h), "color")) {
                    None => rpr.push_str("<w:highlight w:val=\"yellow\"/>"),
                    Some(c) => match HIGHLIGHTS.iter().find(|(v, _)| *v == c) {
                        Some((_, name)) => {
                            let _ = write!(rpr, "<w:highlight w:val=\"{name}\"/>");
                        }
                        None => shading = Some(hex(&c)),
                    },
                }
            }
            if get("underline").is_some() {
                rpr.push_str("<w:u w:val=\"single\"/>");
            }
            if let Some(fill) = shading {
                let _ = write!(rpr, "<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"{fill}\"/>");
            }
            if get("subscript").is_some() {
                rpr.push_str("<w:vertAlign w:val=\"subscript\"/>");
            } else if get("superscript").is_some() {
                rpr.push_str("<w:vertAlign w:val=\"superscript\"/>");
            }
            let deleted = get("deletion");
            let mut run = String::new();
            text_run(s, &rpr, if deleted.is_some() { "w:delText" } else { "w:t" }, &mut run);
            match link {
                Some((target, true)) => {
                    let id = self.rel("http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink", target, true);
                    run = format!("<w:hyperlink r:id=\"{id}\" w:history=\"1\">{run}</w:hyperlink>");
                }
                Some((anchor, false)) => run = format!("<w:hyperlink w:anchor=\"{anchor}\" w:history=\"1\">{run}</w:hyperlink>"),
                None => {}
            }
            let change = deleted.map(|v| ("w:del", v)).or_else(|| get("insertion").map(|v| ("w:ins", v)));
            if let Some((tag, v)) = change {
                self.changes += 1;
                let text = |k: &str| match mark_field(Some(v), k) {
                    Some(Any::String(s)) => Some(s.to_string()),
                    _ => None,
                };
                let author = text("name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Someone".into());
                let date = text("at").filter(|d| d.len() >= 10 && d.is_ascii()).map(|d| format!(" w:date=\"{}\"", attr(&d))).unwrap_or_default();
                let _ = write!(out, "<{tag} w:id=\"{}\" w:author=\"{}\"{date}>{run}</{tag}>", self.changes, attr(&author));
            } else {
                out.push_str(&run);
            }
        }
    }

    fn table<T: ReadTxn>(&mut self, e: &XmlElementRef, txn: &T, place: &Place, out: &mut String) {
        let (rows, cols) = format::table_map(e, txn);
        if cols == 0 {
            return;
        }
        let text_w = self.page.text_width();
        let (grid, fixed): (Vec<u32>, bool) = match format::grid(&rows, txn, cols, text_w / 15) {
            Some(px) => (px.iter().map(|w| w * 15).collect(), true),
            None => (vec![text_w / cols as u32; cols], false),
        };
        let width = |col: usize, span: usize| grid[col.min(cols)..(col + span).min(cols)].iter().sum::<u32>();
        let tbl_w =
            if fixed { format!("<w:tblW w:w=\"{}\" w:type=\"dxa\"/>", grid.iter().sum::<u32>()) } else { "<w:tblW w:w=\"5000\" w:type=\"pct\"/>".into() };
        let _ = write!(out, "<w:tbl><w:tblPr><w:tblStyle w:val=\"TableGrid\"/>{tbl_w}<w:tblLayout w:type=\"fixed\"/></w:tblPr><w:tblGrid>");
        for w in &grid {
            let _ = write!(out, "<w:gridCol w:w=\"{w}\"/>");
        }
        out.push_str("</w:tblGrid>");
        for row in &rows {
            out.push_str("<w:tr>");
            if !row.is_empty() && row.iter().all(|s| matches!(s, Slot::Cell { header: true, .. } | Slot::Covered { .. })) {
                out.push_str("<w:trPr><w:tblHeader/></w:trPr>");
            }
            for slot in row {
                match slot {
                    Slot::Covered { col, span } => {
                        let grid_span = if *span > 1 { format!("<w:gridSpan w:val=\"{span}\"/>") } else { String::new() };
                        let _ = write!(out, "<w:tc><w:tcPr><w:tcW w:w=\"{}\" w:type=\"dxa\"/>{grid_span}<w:vMerge/></w:tcPr><w:p/></w:tc>", width(*col, *span));
                    }
                    Slot::Cell { col, span, down, el, header } => {
                        let mut content = String::new();
                        let inner = Place { bold: *header || place.bold, gap: Some(CELL), ..Place::default() };
                        self.blocks(el.children(txn), txn, &inner, 0, &mut content);
                        if !content.contains("<w:p") {
                            content.push_str("<w:p/>");
                        }
                        let _ = write!(out, "<w:tc><w:tcPr><w:tcW w:w=\"{}\" w:type=\"dxa\"/>", width(*col, *span));
                        if *span > 1 {
                            let _ = write!(out, "<w:gridSpan w:val=\"{span}\"/>");
                        }
                        if *down > 1 {
                            out.push_str("<w:vMerge w:val=\"restart\"/>");
                        }
                        // A header cell is shaded as the editor draws it, unless it has a colour of its own.
                        match format::color(any(el.get_attribute(txn, "background")).as_ref()) {
                            Some(c) => {
                                let _ = write!(out, "<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"{}\"/>", hex(&c));
                            }
                            None if *header => out.push_str("<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"EFF1F5\"/>"),
                            None => {}
                        }
                        let _ = write!(out, "</w:tcPr>{content}</w:tc>");
                    }
                }
            }
            out.push_str("</w:tr>");
        }
        // The editor's 1em under a table: a paragraph of that height (Word needs one between tables).
        out.push_str("</w:tbl><w:p><w:pPr><w:spacing w:before=\"0\" w:after=\"0\" w:line=\"220\" w:lineRule=\"exact\"/></w:pPr></w:p>");
    }

    fn image<T: ReadTxn>(&mut self, e: &XmlElementRef, txn: &T, out: &mut String) {
        let get = |k: &str| any(e.get_attribute(txn, k));
        let alt = string(e.get_attribute(txn, "alt")).unwrap_or_default();
        let caption = format::caption(get("caption").as_ref());
        let align = format::image_align(get("align").as_ref());
        let jc = match align {
            "left" | "wrapLeft" => "left",
            "right" | "wrapRight" => "right",
            _ => "center",
        };
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
            self.lost.insert(super::html::LOST_IMAGES.into());
            let words = [alt.trim(), caption.as_str()].into_iter().filter(|w| !w.is_empty()).collect::<Vec<_>>().join(": ");
            if !words.is_empty() {
                out.push_str("<w:p>");
                text_run(&format!("[{words}]"), "<w:i/>", "w:t", out);
                out.push_str("</w:p>");
            }
            return;
        };
        self.drawings += 1;
        let n = self.drawings;
        let path = format!("media/image{n}.{ext}");
        let rid = self.rel("http://schemas.openxmlformats.org/officeDocument/2006/relationships/image", path.clone(), false);
        self.media.push((path, img.bytes));
        // The size the document draws it at (its height follows its width), else the picture's own.
        let (pw, ph) = (u64::from(img.width.max(1)), u64::from(img.height.max(1)));
        let (w, h) = match (format::image_px(get("width").as_ref()), format::image_px(get("height").as_ref())) {
            (Some(w), Some(h)) => (u64::from(w), u64::from(h)),
            (Some(w), None) => (u64::from(w), (u64::from(w) * ph / pw).max(1)),
            _ => (pw, ph),
        };
        let (mut cx, mut cy) = (w * EMU_PER_PX, h * EMU_PER_PX);
        let max = u64::from(self.page.text_width()) * EMU_PER_TWIP;
        if cx > max {
            cy = cy * max / cx;
            cx = max;
        }
        let alt = attr(alt.trim());
        let graphic = format!(
            "<wp:docPr id=\"{n}\" name=\"Image {n}\" descr=\"{alt}\"/><wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect=\"1\"/></wp:cNvGraphicFramePr>\
             <a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><pic:pic>\
             <pic:nvPicPr><pic:cNvPr id=\"{n}\" name=\"image{n}.{ext}\" descr=\"{alt}\"/><pic:cNvPicPr/></pic:nvPicPr>\
             <pic:blipFill><a:blip r:embed=\"{rid}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
             <pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr>\
             </pic:pic></a:graphicData></a:graphic>"
        );
        if align.starts_with("wrap") {
            // Beside the text that follows, which wraps round it.
            let _ = write!(
                out,
                "<w:p><w:r><w:drawing><wp:anchor distT=\"0\" distB=\"114300\" distL=\"114300\" distR=\"114300\" simplePos=\"0\" relativeHeight=\"{}\" \
                 behindDoc=\"0\" locked=\"0\" layoutInCell=\"1\" allowOverlap=\"1\"><wp:simplePos x=\"0\" y=\"0\"/>\
                 <wp:positionH relativeFrom=\"column\"><wp:align>{jc}</wp:align></wp:positionH>\
                 <wp:positionV relativeFrom=\"paragraph\"><wp:posOffset>0</wp:posOffset></wp:positionV>\
                 <wp:extent cx=\"{cx}\" cy=\"{cy}\"/><wp:effectExtent l=\"0\" t=\"0\" r=\"0\" b=\"0\"/><wp:wrapSquare wrapText=\"bothSides\"/>{graphic}</wp:anchor></w:drawing></w:r></w:p>",
                251_658_240 + n
            );
        } else {
            let _ = write!(
                out,
                "<w:p><w:pPr><w:jc w:val=\"{jc}\"/></w:pPr><w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\">\
                 <wp:extent cx=\"{cx}\" cy=\"{cy}\"/>{graphic}</wp:inline></w:drawing></w:r></w:p>"
            );
        }
        if !caption.is_empty() {
            let _ = write!(out, "<w:p><w:pPr><w:pStyle w:val=\"Caption\"/><w:jc w:val=\"{jc}\"/></w:pPr>");
            text_run(&caption, "", "w:t", out);
            out.push_str("</w:p>");
        }
    }

    /// The contents as Word's TOC field, its result the headings as they are now (linked to them;
    /// Word adds the page numbers when it updates the field, which it is asked to on opening).
    fn contents<T: ReadTxn>(&mut self, e: &XmlElementRef, txn: &T, out: &mut String) {
        let levels = number(e.get_attribute(txn, "levels")).filter(|l| (1..=6).contains(l)).unwrap_or(3);
        out.push_str("<w:p><w:pPr><w:pStyle w:val=\"TOCHeading\"/></w:pPr><w:r><w:t>Contents</w:t></w:r></w:p>");
        let shown: Vec<(usize, &format::Heading)> =
            self.outline.headings.iter().enumerate().filter(|(_, h)| i64::from(h.level) <= levels && !h.text.is_empty()).collect();
        let begin = format!(
            "<w:r><w:fldChar w:fldCharType=\"begin\"/></w:r><w:r><w:instrText xml:space=\"preserve\"> TOC \\o &quot;1-{levels}&quot; \\h \\z \\u </w:instrText></w:r><w:r><w:fldChar w:fldCharType=\"separate\"/></w:r>"
        );
        let end = "<w:r><w:fldChar w:fldCharType=\"end\"/></w:r>";
        if shown.is_empty() {
            let _ = write!(out, "<w:p>{begin}{end}</w:p>");
            return;
        }
        let last = shown.len() - 1;
        for (i, (n, h)) in shown.iter().enumerate() {
            let _ = write!(out, "<w:p><w:pPr><w:pStyle w:val=\"TOC{}\"/></w:pPr>", h.level);
            if i == 0 {
                out.push_str(&begin);
            }
            let _ = write!(out, "<w:hyperlink w:anchor=\"{}\" w:history=\"1\">", bookmark(h.anchor.as_deref(), *n));
            text_run(&h.text, "", "w:t", out);
            out.push_str("</w:hyperlink>");
            if i == last {
                out.push_str(end);
            }
            out.push_str("</w:p>");
        }
    }
}

/// A heading's bookmark: `h_<anchor>` when something links to it, else a hidden one by its place.
fn bookmark(anchor: Option<&str>, n: usize) -> String {
    match anchor {
        Some(a) => format!("h_{a}"),
        None => format!("_Heading{}", n + 1),
    }
}

/// Puts `marks` at the start of the body's first paragraph (after its properties); a body without
/// one gets one.
fn into_first_paragraph(body: &mut String, marks: &str) {
    if marks.is_empty() {
        return;
    }
    let open = body.find("<w:p>");
    let empty = body.find("<w:p/>");
    match (open, empty) {
        (Some(o), e) if e.is_none_or(|e| o < e) => {
            let after = o + "<w:p>".len();
            let at = if body[after..].starts_with("<w:pPr>") { body[after..].find("</w:pPr>").map_or(after, |i| after + i + "</w:pPr>".len()) } else { after };
            body.insert_str(at, marks);
        }
        (_, Some(e)) => body.replace_range(e..e + "<w:p/>".len(), &format!("<w:p>{marks}</w:p>")),
        _ => body.insert_str(0, &format!("<w:p>{marks}</w:p>")),
    }
}

/// Puts `marks` at the end of the body's last paragraph.
fn into_last_paragraph(body: &mut String, marks: &str) {
    if marks.is_empty() {
        return;
    }
    if body.ends_with("<w:p/>") {
        let at = body.len() - "<w:p/>".len();
        body.replace_range(at.., &format!("<w:p>{marks}</w:p>"));
    } else if let Some(at) = body.rfind("</w:p>") {
        body.insert_str(at, marks);
    } else {
        body.push_str(&format!("<w:p>{marks}</w:p>"));
    }
}

/// The byte index in `s` of its `units`th UTF-16 unit (a character's start; within one, after it).
fn utf16_boundary(s: &str, units: usize) -> usize {
    let mut n = 0;
    for (i, c) in s.char_indices() {
        if n >= units {
            return i;
        }
        n += c.len_utf16();
    }
    s.len()
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
<w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial" w:eastAsia="Arial" w:cs="Arial"/><w:sz w:val="22"/><w:szCs w:val="22"/><w:lang w:val="en-US"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:before="0" w:after="121" w:line="344" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults>
<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style>
<w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:spacing w:before="0" w:after="121" w:line="258" w:lineRule="auto"/></w:pPr><w:rPr><w:b/><w:sz w:val="52"/><w:szCs w:val="52"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Subtitle"><w:name w:val="Subtitle"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:spacing w:before="0" w:after="210" w:line="279" w:lineRule="auto"/></w:pPr><w:rPr><w:color w:val="57606A"/><w:sz w:val="30"/><w:szCs w:val="30"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="295" w:after="151" w:line="269" w:lineRule="auto"/><w:outlineLvl w:val="0"/></w:pPr><w:rPr><w:b/><w:sz w:val="38"/><w:szCs w:val="38"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="202" w:after="121" w:line="279" w:lineRule="auto"/><w:outlineLvl w:val="1"/></w:pPr><w:rPr><w:b/><w:sz w:val="29"/><w:szCs w:val="29"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading3"><w:name w:val="heading 3"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="145" w:after="121"/><w:outlineLvl w:val="2"/></w:pPr><w:rPr><w:b/><w:sz w:val="24"/><w:szCs w:val="24"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading4"><w:name w:val="heading 4"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="122" w:after="121"/><w:outlineLvl w:val="3"/></w:pPr><w:rPr><w:b/><w:sz w:val="22"/><w:szCs w:val="22"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading5"><w:name w:val="heading 5"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="110" w:after="121"/><w:outlineLvl w:val="4"/></w:pPr><w:rPr><w:b/><w:sz w:val="21"/><w:szCs w:val="21"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading6"><w:name w:val="heading 6"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="110" w:after="121"/><w:outlineLvl w:val="5"/></w:pPr><w:rPr><w:b/><w:i/><w:sz w:val="21"/><w:szCs w:val="21"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Quote"><w:name w:val="Quote"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:pBdr><w:left w:val="single" w:sz="18" w:space="8" w:color="D0D7DE"/></w:pBdr><w:ind w:left="284"/></w:pPr><w:rPr><w:color w:val="57606A"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Code"><w:name w:val="Code"/><w:basedOn w:val="Normal"/><w:pPr><w:shd w:val="clear" w:color="auto" w:fill="F6F8FA"/><w:spacing w:after="120" w:line="240" w:lineRule="auto"/></w:pPr><w:rPr><w:rFonts w:ascii="Consolas" w:hAnsi="Consolas" w:cs="Consolas"/><w:sz w:val="20"/><w:szCs w:val="20"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Caption"><w:name w:val="caption"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:spacing w:before="60" w:after="200"/></w:pPr><w:rPr><w:i/><w:color w:val="57606A"/><w:sz w:val="18"/><w:szCs w:val="18"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Header"><w:name w:val="header"/><w:basedOn w:val="Normal"/><w:pPr><w:spacing w:before="0" w:after="0" w:line="301" w:lineRule="auto"/></w:pPr><w:rPr><w:color w:val="555555"/><w:sz w:val="18"/><w:szCs w:val="18"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Footer"><w:name w:val="footer"/><w:basedOn w:val="Normal"/><w:pPr><w:spacing w:before="0" w:after="0" w:line="301" w:lineRule="auto"/></w:pPr><w:rPr><w:color w:val="555555"/><w:sz w:val="18"/><w:szCs w:val="18"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="FootnoteText"><w:name w:val="footnote text"/><w:basedOn w:val="Normal"/><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/></w:pPr><w:rPr><w:sz w:val="18"/><w:szCs w:val="18"/></w:rPr></w:style>
<w:style w:type="character" w:styleId="FootnoteReference"><w:name w:val="footnote reference"/><w:rPr><w:vertAlign w:val="superscript"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="TOCHeading"><w:name w:val="TOC Heading"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="240" w:after="120"/></w:pPr><w:rPr><w:b/><w:sz w:val="28"/><w:szCs w:val="28"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="TOC1"><w:name w:val="toc 1"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:pPr><w:spacing w:after="60"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="TOC2"><w:name w:val="toc 2"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:pPr><w:spacing w:after="60"/><w:ind w:left="220"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="TOC3"><w:name w:val="toc 3"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:pPr><w:spacing w:after="60"/><w:ind w:left="440"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="TOC4"><w:name w:val="toc 4"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:pPr><w:spacing w:after="60"/><w:ind w:left="660"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="TOC5"><w:name w:val="toc 5"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:pPr><w:spacing w:after="60"/><w:ind w:left="880"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="TOC6"><w:name w:val="toc 6"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:pPr><w:spacing w:after="60"/><w:ind w:left="1100"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="CommentText"><w:name w:val="annotation text"/><w:basedOn w:val="Normal"/><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/></w:pPr><w:rPr><w:sz w:val="20"/><w:szCs w:val="20"/></w:rPr></w:style>
<w:style w:type="character" w:styleId="CommentReference"><w:name w:val="annotation reference"/><w:rPr><w:sz w:val="16"/><w:szCs w:val="16"/></w:rPr></w:style>
<w:style w:type="character" w:styleId="Hyperlink"><w:name w:val="Hyperlink"/><w:rPr><w:color w:val="0969DA"/><w:u w:val="single"/></w:rPr></w:style>
<w:style w:type="table" w:styleId="TableGrid"><w:name w:val="Table Grid"/><w:tblPr><w:tblBorders><w:top w:val="single" w:sz="6" w:space="0" w:color="C8CFD9"/><w:left w:val="single" w:sz="6" w:space="0" w:color="C8CFD9"/><w:bottom w:val="single" w:sz="6" w:space="0" w:color="C8CFD9"/><w:right w:val="single" w:sz="6" w:space="0" w:color="C8CFD9"/><w:insideH w:val="single" w:sz="6" w:space="0" w:color="C8CFD9"/><w:insideV w:val="single" w:sz="6" w:space="0" w:color="C8CFD9"/></w:tblBorders><w:tblCellMar><w:top w:w="88" w:type="dxa"/><w:left w:w="132" w:type="dxa"/><w:bottom w:w="88" w:type="dxa"/><w:right w:w="132" w:type="dxa"/></w:tblCellMar></w:tblPr></w:style>
</w:styles>"#;

/// The numbering of each list: one definition per kind, level and format, one instance per list
/// (numbered ones restart where the document starts them).
fn numbering(lists: &[List]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:numbering xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">",
    );
    let mut kinds: Vec<&List> = Vec::new();
    for l in lists {
        if !kinds.iter().any(|k| k.ordered == l.ordered && k.level == l.level && k.format == l.format) {
            kinds.push(l);
        }
    }
    for (a, k) in kinds.iter().enumerate() {
        let _ = write!(out, "<w:abstractNum w:abstractNumId=\"{a}\"><w:multiLevelType w:val=\"hybridMultilevel\"/>");
        for l in 0..9u32 {
            let (fmt, text) = match (k.ordered, l == k.level) {
                (true, true) => (k.format, format!("%{}.", l + 1)),
                (true, false) => ("decimal", format!("%{}.", l + 1)),
                (false, true) => ("bullet", k.format.to_string()),
                (false, false) => ("bullet", ["\u{2022}", "\u{25E6}", "\u{25AA}"][l as usize % 3].to_string()),
            };
            let _ = write!(
                out,
                "<w:lvl w:ilvl=\"{l}\"><w:start w:val=\"1\"/><w:numFmt w:val=\"{fmt}\"/><w:lvlText w:val=\"{text}\"/><w:lvlJc w:val=\"left\"/><w:pPr><w:ind w:left=\"{}\" w:hanging=\"{HANGING}\"/></w:pPr></w:lvl>",
                if k.ordered { NUMBER_STEP } else { BULLET_STEP } * (l + 1)
            );
        }
        out.push_str("</w:abstractNum>");
    }
    for (n, l) in lists.iter().enumerate() {
        let a = kinds.iter().position(|k| k.ordered == l.ordered && k.level == l.level && k.format == l.format).unwrap_or(0);
        let _ = write!(out, "<w:num w:numId=\"{}\"><w:abstractNumId w:val=\"{a}\"/>", n + 1);
        if l.ordered {
            let _ = write!(out, "<w:lvlOverride w:ilvl=\"{}\"><w:startOverride w:val=\"{}\"/></w:lvlOverride>", l.level, l.start);
        }
        out.push_str("</w:num>");
    }
    out.push_str("</w:numbering>");
    out
}

/// The document as a Word file titled `title`; `images` gives an image's bytes by its id.
pub fn docx(doc: &Doc, title: &str, images: &dyn Fn(&str) -> Option<ImageData>) -> Result<Vec<u8>> {
    Ok(write(doc, title, None, images, &[])?.0)
}

type Rels = Vec<(String, &'static str, String, bool)>;

fn rels_xml(rels: &[(String, &'static str, String, bool)], fixed: &str) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    out.push_str(fixed);
    for (id, kind, target, external) in rels {
        let mode = if *external { " TargetMode=\"External\"" } else { "" };
        let _ = write!(out, "<Relationship Id=\"{id}\" Type=\"{kind}\" Target=\"{}\"{mode}/>", attr(target));
    }
    out.push_str("</Relationships>");
    out
}

/// See [`super::docx_export`]: the file and what it could not carry.
pub(super) fn write(
    doc: &Doc,
    title: &str,
    origin: Option<&str>,
    images: &dyn Fn(&str) -> Option<ImageData>,
    threads: &[CommentThread],
) -> Result<(Vec<u8>, BTreeSet<String>)> {
    let txn = doc.transact();
    let placed = comments::place(threads, &txn);
    let mut w = Writer {
        images,
        origin: format::origin(origin),
        title,
        date: chrono::Utc::now().format("%-d %B %Y").to_string(),
        page: format::page(&txn),
        outline: format::outline(&txn),
        rels: Vec::new(),
        media: Vec::new(),
        lists: Vec::new(),
        fonts: BTreeSet::new(),
        drawings: 0,
        changes: 0,
        bookmarks: 0,
        headings: 0,
        notes: 0,
        in_note: false,
        lost: BTreeSet::new(),
        threads,
        first: comments::ids(threads),
        marks: placed.marks,
        started: vec![false; threads.len()],
        ended: vec![false; threads.len()],
    };
    let mut body = String::new();
    if let Some(fragment) = txn.get_xml_fragment(FRAGMENT) {
        w.blocks(fragment.children(&txn), &txn, &Place::default(), 0, &mut body);
    }
    let any_comments = threads.iter().any(|t| !t.comments.is_empty());
    if any_comments {
        // Threads with no place met in the text (on the whole document, their paragraph gone, in a
        // footnote) at the start; a range whose end was not met ends with the text.
        let (mut at_start, mut at_end) = (String::new(), String::new());
        for n in 0..threads.len() {
            if threads[n].comments.is_empty() {
                continue;
            }
            if !w.started[n] {
                comments::starts(threads, &w.first, n, &mut at_start);
                comments::ends(threads, &w.first, n, &mut at_start);
            } else if !w.ended[n] {
                comments::ends(threads, &w.first, n, &mut at_end);
            }
        }
        if !placed.removed.is_empty() {
            w.lost.insert(comments::removed_words(placed.removed.len()));
        }
        into_first_paragraph(&mut body, &at_start);
        into_last_paragraph(&mut body, &at_end);
    }
    // Parts: (path, content type, xml, its relationships).
    let mut parts: Vec<(String, String, String, Rels)> = Vec::new();
    let mut doc_rels = format!(
        "<Relationship Id=\"rIdStyles\" Type=\"{REL}/styles\" Target=\"styles.xml\"/>\
         <Relationship Id=\"rIdNumbering\" Type=\"{REL}/numbering\" Target=\"numbering.xml\"/>\
         <Relationship Id=\"rIdSettings\" Type=\"{REL}/settings\" Target=\"settings.xml\"/>\
         <Relationship Id=\"rIdFonts\" Type=\"{REL}/fontTable\" Target=\"fontTable.xml\"/>"
    );
    // The header and footer: pages 2 on (or all), and the first page's own.
    let p = w.page;
    let mut refs = String::new();
    for (region, kind, which) in
        [("header", "header", "default"), ("footer", "footer", "default"), ("firstHeader", "header", "first"), ("firstFooter", "footer", "first")]
    {
        let paras = format::region(&txn, region);
        if paras.is_none() && !(which == "first" && p.different_first) {
            continue;
        }
        let style = if kind == "header" { "Header" } else { "Footer" };
        let (xml_body, rels) = w.part(|w, out| {
            for para in paras.iter().flatten() {
                w.paragraph(para, &txn, &Place::default(), Some(style), None, out);
            }
            if paras.is_none() {
                let _ = write!(out, "<w:p><w:pPr><w:pStyle w:val=\"{style}\"/></w:pPr></w:p>");
            }
        });
        let n = parts.len() + 1;
        let tag = if kind == "header" { "w:hdr" } else { "w:ftr" };
        let path = format!("word/{kind}{n}.xml");
        let _ = write!(doc_rels, "<Relationship Id=\"rIdPart{n}\" Type=\"{REL}/{kind}\" Target=\"{kind}{n}.xml\"/>");
        let _ = write!(refs, "<w:{kind}Reference w:type=\"{which}\" r:id=\"rIdPart{n}\"/>");
        parts.push((
            path,
            format!("{TYPE}.{kind}+xml"),
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<{tag} {NS}>{xml_body}</{tag}>"),
            rels,
        ));
    }
    // Footnotes: Word's own, numbered by their references, on the page of their reference.
    if !w.outline.notes.is_empty() {
        let (notes, rels) = w.part(|w, out| {
            w.in_note = true;
            out.push_str(
                "<w:footnote w:type=\"separator\" w:id=\"-1\"><w:p><w:pPr><w:spacing w:after=\"0\" w:line=\"240\" w:lineRule=\"auto\"/></w:pPr><w:r><w:separator/></w:r></w:p></w:footnote>\
                 <w:footnote w:type=\"continuationSeparator\" w:id=\"0\"><w:p><w:pPr><w:spacing w:after=\"0\" w:line=\"240\" w:lineRule=\"auto\"/></w:pPr><w:r><w:continuationSeparator/></w:r></w:p></w:footnote>",
            );
            let mark = "<w:r><w:rPr><w:rStyle w:val=\"FootnoteReference\"/></w:rPr><w:footnoteRef/></w:r><w:r><w:t xml:space=\"preserve\"> </w:t></w:r>";
            for (n, body) in w.outline.notes.clone().into_iter().enumerate() {
                let _ = write!(out, "<w:footnote w:id=\"{}\">", n + 1);
                let paras: Vec<XmlElementRef> = body.map(|b| b.children(&txn).filter_map(|c| if let XmlOut::Element(e) = c { Some(e) } else { None }).collect()).unwrap_or_default();
                if paras.is_empty() {
                    let _ = write!(out, "<w:p><w:pPr><w:pStyle w:val=\"FootnoteText\"/></w:pPr>{mark}</w:p>");
                }
                for (i, para) in paras.iter().enumerate() {
                    let place = Place { style: Some("FootnoteText"), prefix: (i == 0).then(|| mark.to_string()), ..Place::default() };
                    w.block(para, &txn, &place, 0, out);
                }
                out.push_str("</w:footnote>");
            }
            w.in_note = false;
        });
        let _ = write!(doc_rels, "<Relationship Id=\"rIdFootnotes\" Type=\"{REL}/footnotes\" Target=\"footnotes.xml\"/>");
        parts.push((
            "word/footnotes.xml".into(),
            format!("{TYPE}.footnotes+xml"),
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:footnotes {NS}>{notes}</w:footnotes>"),
            rels,
        ));
    }
    if any_comments {
        let (list, extended) = comments::parts(threads, &placed.removed, attr);
        let _ = write!(
            doc_rels,
            "<Relationship Id=\"rIdComments\" Type=\"{REL}/comments\" Target=\"comments.xml\"/>\
             <Relationship Id=\"rIdCommentsEx\" Type=\"http://schemas.microsoft.com/office/2011/relationships/commentsExtended\" Target=\"commentsExtended.xml\"/>"
        );
        parts.push(("word/comments.xml".into(), format!("{TYPE}.comments+xml"), list, Vec::new()));
        parts.push(("word/commentsExtended.xml".into(), format!("{TYPE}.commentsExtended+xml"), extended, Vec::new()));
    }
    let orient = if p.landscape { " w:orient=\"landscape\"" } else { "" };
    let title_pg = if p.different_first { "<w:titlePg/>" } else { "" };
    let document = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:document {NS}><w:body>{body}\
         <w:sectPr>{refs}<w:pgSz w:w=\"{}\" w:h=\"{}\"{orient}/><w:pgMar w:top=\"{}\" w:right=\"{}\" w:bottom=\"{}\" w:left=\"{}\" \
         w:header=\"{edge}\" w:footer=\"{edge}\" w:gutter=\"0\"/>{title_pg}</w:sectPr></w:body></w:document>",
        p.width,
        p.height,
        p.top,
        p.right,
        p.bottom,
        p.left,
        edge = format::EDGE
    );
    // Word fills the contents' page numbers when it opens the file.
    let update = if body.contains(" TOC \\o ") { "<w:updateFields w:val=\"true\"/>" } else { "" };
    let notes_pr = if w.outline.notes.is_empty() { "" } else { "<w:footnotePr><w:footnote w:id=\"-1\"/><w:footnote w:id=\"0\"/></w:footnotePr>" };
    let settings = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:settings xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
         <w:defaultTabStop w:val=\"720\"/>{update}{notes_pr}<w:compat><w:compatSetting w:name=\"compatibilityMode\" w:uri=\"http://schemas.microsoft.com/office/word\" w:val=\"15\"/></w:compat></w:settings>"
    );
    let mut fonts = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:fonts xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">",
    );
    w.fonts.insert(("Arial".into(), "swiss"));
    w.fonts.insert(("Consolas".into(), "modern"));
    for (name, family) in &w.fonts {
        let _ = write!(fonts, "<w:font w:name=\"{}\"><w:family w:val=\"{family}\"/></w:font>", attr(name));
    }
    fonts.push_str("</w:fonts>");
    drop(txn);

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
    let _ = write!(
        types,
        "<Override PartName=\"/word/document.xml\" ContentType=\"{TYPE}.document.main+xml\"/>\
         <Override PartName=\"/word/styles.xml\" ContentType=\"{TYPE}.styles+xml\"/>\
         <Override PartName=\"/word/numbering.xml\" ContentType=\"{TYPE}.numbering+xml\"/>\
         <Override PartName=\"/word/settings.xml\" ContentType=\"{TYPE}.settings+xml\"/>\
         <Override PartName=\"/word/fontTable.xml\" ContentType=\"{TYPE}.fontTable+xml\"/>\
         <Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\
         <Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/>"
    );
    for (path, ctype, ..) in &parts {
        let _ = write!(types, "<Override PartName=\"/{path}\" ContentType=\"{ctype}\"/>");
    }
    types.push_str("</Types>");
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
    let document_rels = rels_xml(&w.rels, &doc_rels);

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let failed = |e: &dyn std::fmt::Display| Error::internal(format!("writing the Word file: {e}"));
    let numbering = numbering(&w.lists);
    let mut files: Vec<(String, &[u8])> = vec![
        ("[Content_Types].xml".into(), types.as_bytes()),
        ("_rels/.rels".into(), root_rels.as_bytes()),
        ("docProps/core.xml".into(), core.as_bytes()),
        ("docProps/app.xml".into(), app.as_bytes()),
        ("word/document.xml".into(), document.as_bytes()),
        ("word/styles.xml".into(), STYLES.as_bytes()),
        ("word/numbering.xml".into(), numbering.as_bytes()),
        ("word/settings.xml".into(), settings.as_bytes()),
        ("word/fontTable.xml".into(), fonts.as_bytes()),
        ("word/_rels/document.xml.rels".into(), document_rels.as_bytes()),
    ];
    let part_rels: Vec<(String, String)> =
        parts.iter().filter(|p| !p.3.is_empty()).map(|(path, _, _, rels)| (path.replacen("word/", "word/_rels/", 1) + ".rels", rels_xml(rels, ""))).collect();
    for (path, _, xml, _) in &parts {
        files.push((path.clone(), xml.as_bytes()));
    }
    for (path, xml) in &part_rels {
        files.push((path.clone(), xml.as_bytes()));
    }
    for (name, data) in files {
        zip.start_file(name, opts).map_err(|e| failed(&e))?;
        zip.write_all(data).map_err(|e| failed(&e))?;
    }
    for (path, bytes) in &w.media {
        // Images are compressed already.
        zip.start_file(format!("word/{path}"), stored).map_err(|e| failed(&e))?;
        zip.write_all(bytes).map_err(|e| failed(&e))?;
    }
    Ok((zip.finish().map_err(|e| failed(&e))?.into_inner(), w.lost))
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
        assert!(body.contains("<w:pStyle w:val=\"Heading2\"/></w:pPr><w:bookmarkStart w:id=\"1\" w:name=\"_Heading1\"/><w:r><w:t xml:space=\"preserve\">Plan &lt;A&amp;B&gt;</w:t></w:r><w:bookmarkEnd w:id=\"1\"/>"), "{body}");
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
        assert!(body.contains("<w:numId w:val=\"1\"/>") && body.contains("<w:numId w:val=\"2\"/>") && body.contains("<w:numId w:val=\"3\"/>"), "{body}");
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
            numbering.contains("<w:num w:numId=\"1\"><w:abstractNumId w:val=\"0\"/><w:lvlOverride w:ilvl=\"0\"><w:startOverride w:val=\"3\"/>"),
            "{numbering}"
        );
        assert!(
            numbering.contains("<w:num w:numId=\"2\"><w:abstractNumId w:val=\"0\"/><w:lvlOverride w:ilvl=\"0\"><w:startOverride w:val=\"1\"/>"),
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

    fn images(id: &str) -> Option<ImageData> {
        use super::super::format::fixtures::{BLUE_PNG, GREEN_PNG, png};
        let hex = match id {
            "img1" => BLUE_PNG,
            "img2" => GREEN_PNG,
            _ => return None,
        };
        Some(ImageData { bytes: png(hex), content_type: "image/png".into(), width: 4, height: 2 })
    }

    #[test]
    fn a_formatted_document_keeps_its_formatting_in_word() {
        let doc = super::super::format::fixtures::everything();
        let (file, lost) = write(&doc, "Annual report 2026", Some("https://acme.example.com/"), &images, &[]).unwrap();
        assert!(lost.is_empty(), "{lost:?}");
        let body = part(&file, "word/document.xml");
        for want in [
            // Letter landscape, 1 inch margins, a different first page with its own header and footer.
            "<w:pgSz w:w=\"15840\" w:h=\"12240\" w:orient=\"landscape\"/><w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\" w:header=\"720\" w:footer=\"720\" w:gutter=\"0\"/><w:titlePg/></w:sectPr>",
            "<w:headerReference w:type=\"default\" r:id=\"rIdPart1\"/><w:footerReference w:type=\"default\" r:id=\"rIdPart2\"/><w:headerReference w:type=\"first\" r:id=\"rIdPart3\"/><w:footerReference w:type=\"first\" r:id=\"rIdPart4\"/>",
            "<w:pStyle w:val=\"Title\"/></w:pPr><w:r><w:t xml:space=\"preserve\">Annual report 2026</w:t>",
            "<w:pStyle w:val=\"Subtitle\"/>",
            // Spacing in twentieths of a point, line height in 240ths of Arial's line (1.5 of the size), indent in twips, justified.
            "<w:pPr><w:spacing w:before=\"120\" w:after=\"240\" w:line=\"322\" w:lineRule=\"auto\"/><w:ind w:left=\"720\"/><w:jc w:val=\"both\"/></w:pPr>",
            "<w:rFonts w:ascii=\"Georgia\" w:hAnsi=\"Georgia\" w:eastAsia=\"Georgia\" w:cs=\"Georgia\"/></w:rPr><w:t xml:space=\"preserve\">Georgia</w:t>",
            "<w:rFonts w:ascii=\"Georgia\" w:hAnsi=\"Georgia\" w:eastAsia=\"Georgia\" w:cs=\"Georgia\"/></w:rPr><w:t xml:space=\"preserve\">, serif</w:t>",
            "<w:rFonts w:ascii=\"Courier New\" w:hAnsi=\"Courier New\" w:eastAsia=\"Courier New\" w:cs=\"Courier New\"/></w:rPr><w:t xml:space=\"preserve\">, mono</w:t>",
            "<w:sz w:val=\"36\"/><w:szCs w:val=\"36\"/></w:rPr><w:t xml:space=\"preserve\">, 18 pt</w:t>",
            "<w:color w:val=\"CF222E\"/></w:rPr><w:t xml:space=\"preserve\">, red</w:t>",
            // Any highlight colour: Word's own where it has it, a shading otherwise.
            "<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"A0E7FF\"/></w:rPr><w:t xml:space=\"preserve\">, sky highlight</w:t>",
            "<w:highlight w:val=\"yellow\"/></w:rPr><w:t xml:space=\"preserve\">, yellow highlight</w:t>",
            "<w:highlight w:val=\"green\"/></w:rPr><w:t xml:space=\"preserve\">, green highlight</w:t>",
            "<w:vertAlign w:val=\"subscript\"/></w:rPr><w:t xml:space=\"preserve\">2</w:t>",
            "<w:vertAlign w:val=\"superscript\"/></w:rPr><w:t xml:space=\"preserve\">2</w:t>",
            "<w:rPr><w:b/><w:i/></w:rPr><w:t xml:space=\"preserve\">, bold italic</w:t>",
            "<w:footnoteReference w:id=\"1\"/>",
            "<w:footnoteReference w:id=\"2\"/>",
            // Headings: styles Word's navigation knows, bookmarks links and the contents go to.
            "<w:pStyle w:val=\"Heading1\"/></w:pPr><w:bookmarkStart w:id=\"1\" w:name=\"h_intro01\"/>",
            "<w:pStyle w:val=\"Heading6\"/>",
            "<w:hyperlink w:anchor=\"h_intro01\" w:history=\"1\"><w:r><w:rPr><w:rStyle w:val=\"Hyperlink\"/></w:rPr><w:t xml:space=\"preserve\">introduction</w:t>",
            // The contents: Word's TOC field, its result today's headings, linked.
            "<w:instrText xml:space=\"preserve\"> TOC \\o &quot;1-3&quot; \\h \\z \\u </w:instrText>",
            "<w:pStyle w:val=\"TOC1\"/></w:pPr><w:r><w:fldChar w:fldCharType=\"begin\"/>",
            "<w:hyperlink w:anchor=\"h_intro01\" w:history=\"1\"><w:r><w:t xml:space=\"preserve\">Introduction</w:t></w:r></w:hyperlink></w:p>",
            "<w:pStyle w:val=\"TOC2\"/></w:pPr><w:hyperlink w:anchor=\"_Heading2\" w:history=\"1\">",
            "<w:t xml:space=\"preserve\">After the page break</w:t></w:r></w:hyperlink><w:r><w:fldChar w:fldCharType=\"end\"/></w:r></w:p>",
            // A table: its columns' widths (px × 15), a header row that repeats, merged cells both ways, colours.
            "<w:tblW w:w=\"7200\" w:type=\"dxa\"/><w:tblLayout w:type=\"fixed\"/></w:tblPr><w:tblGrid><w:gridCol w:w=\"3000\"/><w:gridCol w:w=\"1800\"/><w:gridCol w:w=\"2400\"/></w:tblGrid>",
            "<w:tr><w:trPr><w:tblHeader/></w:trPr>",
            "<w:tcW w:w=\"4200\" w:type=\"dxa\"/><w:gridSpan w:val=\"2\"/><w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"EFF1F5\"/></w:tcPr>",
            "<w:tcW w:w=\"3000\" w:type=\"dxa\"/><w:vMerge w:val=\"restart\"/><w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"FDE8E8\"/></w:tcPr>",
            "<w:tcW w:w=\"3000\" w:type=\"dxa\"/><w:vMerge/></w:tcPr><w:p/>",
            "<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"E6F4EA\"/>",
            // Images at their size and place, described, captioned; one the text wraps round.
            "<w:pPr><w:jc w:val=\"right\"/></w:pPr><w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\"><wp:extent cx=\"2286000\" cy=\"1143000\"/><wp:docPr id=\"1\" name=\"Image 1\" descr=\"A blue chart\"/>",
            "<w:pStyle w:val=\"Caption\"/><w:jc w:val=\"right\"/></w:pPr><w:r><w:t xml:space=\"preserve\">Figure 1: sales by region</w:t>",
            "<wp:positionH relativeFrom=\"column\"><wp:align>left</wp:align></wp:positionH>",
            "<wp:extent cx=\"914400\" cy=\"457200\"/><wp:effectExtent l=\"0\" t=\"0\" r=\"0\" b=\"0\"/><wp:wrapSquare wrapText=\"bothSides\"/><wp:docPr id=\"2\" name=\"Image 2\" descr=\"A green square\"/>",
            "<w:p><w:r><w:br w:type=\"page\"/></w:r></w:p>",
            "<w:pStyle w:val=\"Quote\"/><w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"FFF4E0\"/>",
        ] {
            assert!(body.contains(want), "missing {want}\nin {body}");
        }
        assert_eq!(body.matches("<w:pStyle w:val=\"TOC").count(), 5, "its title, and the headings down to level 3 (Title and Subtitle are not headings)");
        let rels = part(&file, "word/_rels/document.xml.rels");
        assert!(rels.contains("Target=\"https://acme.example.com/docs/d/0192f0c4-1c2b-7c3d-8e4f-5a6b7c8d9e0f\" TargetMode=\"External\""), "{rels}");
        assert!(rels.contains("Target=\"footnotes.xml\"") && rels.contains("Target=\"header1.xml\"") && rels.contains("Target=\"footer4.xml\""), "{rels}");
        assert!(
            rels.contains("Target=\"https://acme.example.com/open/Customer/c-42\" TargetMode=\"External\""),
            "a record mention links to the record: {rels}"
        );
        assert!(body.contains("<w:t xml:space=\"preserve\">@Priya Nair</w:t>") && !rels.contains("/u1"), "a person mention is its words");
        let notes = part(&file, "word/footnotes.xml");
        assert!(
            notes.contains("<w:footnote w:type=\"separator\" w:id=\"-1\">") && notes.contains("<w:footnote w:type=\"continuationSeparator\" w:id=\"0\">"),
            "{notes}"
        );
        assert!(notes.contains("<w:footnote w:id=\"1\"><w:p><w:pPr><w:pStyle w:val=\"FootnoteText\"/></w:pPr><w:r><w:rPr><w:rStyle w:val=\"FootnoteReference\"/></w:rPr><w:footnoteRef/></w:r>"), "{notes}");
        assert!(notes.contains("The first note, with ") && notes.contains("<w:hyperlink r:id=\"rId1\"") && notes.contains("The second note."), "{notes}");
        assert!(part(&file, "word/_rels/footnotes.xml.rels").contains("Target=\"https://example.com/note\""), "a note's link is the notes' own relationship");
        let header = part(&file, "word/header1.xml");
        assert!(header.contains("<w:pStyle w:val=\"Header\"/><w:jc w:val=\"right\"/>") && header.contains("<w:fldSimple w:instr=\" TITLE \">"), "{header}");
        let footer = part(&file, "word/footer2.xml");
        for f in ["<w:fldSimple w:instr=\" PAGE \">", "<w:fldSimple w:instr=\" NUMPAGES \">", "<w:fldSimple w:instr=\" DATE \\@ &quot;d MMMM yyyy&quot; \">"] {
            assert!(footer.contains(f), "{f} in {footer}");
        }
        assert!(part(&file, "word/header3.xml").contains("CONFIDENTIAL"));
        assert!(part(&file, "word/footer4.xml").contains("<w:p><w:pPr><w:pStyle w:val=\"Footer\"/></w:pPr></w:p>"), "an empty first footer");
        let settings = part(&file, "word/settings.xml");
        assert!(settings.contains("<w:updateFields w:val=\"true\"/><w:footnotePr>"), "{settings}");
        let numbering = part(&file, "word/numbering.xml");
        assert!(numbering.contains("<w:lvl w:ilvl=\"0\"><w:start w:val=\"1\"/><w:numFmt w:val=\"bullet\"/><w:lvlText w:val=\"\u{25AA}\"/>"), "{numbering}");
        assert!(numbering.contains("<w:lvl w:ilvl=\"1\"><w:start w:val=\"1\"/><w:numFmt w:val=\"bullet\"/><w:lvlText w:val=\"\u{25E6}\"/>"), "{numbering}");
        assert!(numbering.contains("<w:numFmt w:val=\"lowerLetter\"/>") && numbering.contains("<w:numFmt w:val=\"lowerRoman\"/>"), "{numbering}");
        assert!(numbering.contains("<w:startOverride w:val=\"3\"/>"), "{numbering}");
        let styles = part(&file, "word/styles.xml");
        for id in ["Title", "Subtitle", "Heading1", "Heading6", "Caption", "TOC1", "TOC6", "FootnoteText", "FootnoteReference", "Header", "Footer"] {
            assert!(styles.contains(&format!("w:styleId=\"{id}\"")), "{id}");
        }
        let fonts = part(&file, "word/fontTable.xml");
        assert!(
            fonts.contains("<w:font w:name=\"Georgia\"><w:family w:val=\"roman\"/>")
                && fonts.contains("<w:font w:name=\"Courier New\"><w:family w:val=\"modern\"/>"),
            "{fonts}"
        );
        let types = part(&file, "[Content_Types].xml");
        for p in ["/word/footnotes.xml", "/word/header1.xml", "/word/footer2.xml", "/word/settings.xml", "/word/fontTable.xml"] {
            assert!(types.contains(&format!("PartName=\"{p}\"")), "{p}");
        }
        // Without the workspace's address a link to another document is its words, and says so.
        let (_, lost) = write(&doc, "x", None, &|_| None, &[]).unwrap();
        assert!(lost.contains(super::super::html::LOST_DOC_LINKS) && lost.contains(super::super::html::LOST_IMAGES), "{lost:?}");
    }

    #[test]
    fn hostile_values_never_reach_the_word_file() {
        let doc = super::super::format::fixtures::hostile();
        let (file, lost) = super::super::docx_export(&doc, "x", None, &images).unwrap();
        for l in ["unknown node chart3d", "unknown mark sparkle", "content from a newer version of Docs (format 3)"] {
            assert!(lost.iter().any(|x| x == l), "{l} in {lost:?}");
        }
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(&file[..])).unwrap();
        let names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().unwrap().to_string()).collect();
        for name in names.iter().filter(|n| n.ends_with(".xml") || n.ends_with(".rels")) {
            let mut s = String::new();
            z.by_name(name).unwrap().read_to_string(&mut s).unwrap();
            for bad in ["<script", "url(", "expression(", "javascript:", "data:text", "x:y", "onclick", "onmouseover", "/docs/d/.."] {
                assert!(!s.contains(bad), "{bad} in {name}: {s}");
            }
        }
        let body = part(&file, "word/document.xml");
        // A page out of bounds is the legacy page.
        assert!(body.contains("<w:pgSz w:w=\"11906\" w:h=\"16838\"/><w:pgMar w:top=\"1134\" w:right=\"1134\" w:bottom=\"1134\" w:left=\"1134\""), "{body}");
        assert!(body.contains("<w:pStyle w:val=\"Heading6\"/></w:pPr><w:bookmarkStart w:id=\"1\" w:name=\"_Heading1\"/>"), "a bad anchor is no anchor: {body}");
        assert!(body.contains("words of an unknown node") && body.contains(">styled<"), "unknown content keeps its words: {body}");
        assert!(
            body.contains("<w:p><w:pPr><w:jc w:val=\"left\"/></w:pPr><w:r><w:drawing><wp:inline")
                || body.contains("<w:jc w:val=\"center\"/></w:pPr><w:r><w:drawing>"),
            "{body}"
        );
        assert!(body.contains("<w:startOverride") || part(&file, "word/numbering.xml").contains("<w:startOverride w:val=\"32767\"/>"));
        assert!(part(&file, "word/header1.xml").contains("&lt;/style&gt;&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    /// Comment threads as Word's comments, anchored as the editor anchors them (Yjs relative
    /// positions as JSON). `FERRUM_DOCX_COMMENTS=/tmp/c.docx` keeps the file to open it.
    #[test]
    fn comment_threads_as_word_comments() {
        use super::super::{Comment, CommentThread};
        use yrs::{Assoc, IndexedSequence, Text as _};
        let doc = new_doc();
        let fragment = doc.get_or_insert_xml_fragment(FRAGMENT);
        let at = |t: &yrs::XmlTextRef, i: u32, assoc| serde_json::to_value(t.sticky_index(&doc.transact(), i, assoc).unwrap()).unwrap();
        let (first, second, gone, code) = {
            let mut txn = doc.transact_mut();
            let mut para = |words: &str| {
                let p = fragment.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
                p.push_back(&mut txn, XmlTextPrelim::new(words))
            };
            let first = para("Hello world, and 日本 🎉 too.");
            let second = para("The second paragraph.");
            let gone = para("A paragraph that goes.");
            let c = fragment.push_back(&mut txn, XmlElementPrelim::empty("codeBlock"));
            let code = c.push_back(&mut txn, XmlTextPrelim::new("let x = 1;"));
            (first, second, gone, code)
        };
        let anchor = |from: serde_json::Value, to: serde_json::Value| Some(serde_json::json!({"from": from, "to": to}));
        let when = |s: &str| Some(s.parse::<chrono::DateTime<chrono::Utc>>().unwrap());
        let said = |email: &str, name: Option<&str>, body: &str, at: &str| Comment {
            author_email: email.into(),
            author_name: name.map(Into::into),
            at: when(at),
            body: body.into(),
        };
        // "world" (6..11), and "🎉" after the CJK (UTF-16: 20..22) with a reply, and across two paragraphs, resolved.
        let threads = vec![
            CommentThread {
                anchor: anchor(at(&first, 6, Assoc::After), at(&first, 11, Assoc::Before)),
                quote: Some("world".into()),
                resolved: false,
                comments: vec![
                    said("priya.nair@demo.test", Some("Priya Nair"), "Which world?\nThis one.", "2026-11-11T10:00:00Z"),
                    said("omar.haddad@demo.test", None, "This & <that> one.", "2026-11-11T10:05:00Z"),
                ],
            },
            CommentThread {
                anchor: anchor(at(&first, 20, Assoc::After), at(&second, 3, Assoc::Before)),
                quote: Some("🎉 too. The".into()),
                resolved: true,
                comments: vec![said("ana@demo.test", None, "Spans two.", "2026-11-11T11:00:00Z")],
            },
            // Its words are deleted below: it stays where they were.
            CommentThread {
                anchor: anchor(at(&second, 4, Assoc::After), at(&second, 10, Assoc::Before)),
                quote: Some("second".into()),
                resolved: false,
                comments: vec![said("ana@demo.test", Some("Ana"), "Was here.", "2026-11-11T12:00:00Z")],
            },
            // Its paragraph goes: at the start.
            CommentThread {
                anchor: anchor(at(&gone, 2, Assoc::After), at(&gone, 11, Assoc::Before)),
                quote: Some("paragraph".into()),
                resolved: false,
                comments: vec![said("ana@demo.test", Some("Ana"), "Gone with it.", "2026-11-11T12:30:00Z")],
            },
            CommentThread {
                anchor: anchor(serde_json::json!({"item": {"client": 1, "clock": 999_999}, "assoc": 0}), serde_json::json!("junk")),
                quote: Some("?".into()),
                resolved: false,
                comments: vec![said("x@demo.test", None, "Unreadable.", "2026-11-11T13:00:00Z")],
            },
            CommentThread {
                anchor: None,
                quote: None,
                resolved: false,
                comments: vec![said("x@demo.test", None, "On the whole file.", "2026-11-11T13:30:00Z")],
            },
            CommentThread {
                anchor: anchor(at(&code, 4, Assoc::After), at(&code, 5, Assoc::Before)),
                quote: Some("x".into()),
                resolved: false,
                comments: vec![said("x@demo.test", None, "On code.", "2026-11-11T14:00:00Z")],
            },
            // A thread with no comments is no thread.
            CommentThread { anchor: None, quote: None, resolved: false, comments: vec![] },
        ];
        {
            let mut txn = doc.transact_mut();
            second.remove_range(&mut txn, 4, 7);
            fragment.remove_range(&mut txn, 2, 1);
        }
        let (file, lost) = super::super::docx_export_with_comments(&doc, "Comments", None, &|_| None, &threads).unwrap();
        if let Ok(path) = std::env::var("FERRUM_DOCX_COMMENTS") {
            std::fs::write(path, &file).unwrap();
        }
        assert!(lost.contains(&"3 comments whose text was removed".to_string()), "deleted words, a gone paragraph, an unreadable anchor: {lost:?}");
        let body = part(&file, "word/document.xml");
        // Ids 0-1 (a thread and its reply) around "world"; the 🎉 thread (2) from mid-text to "The".
        assert!(
            body.contains("<w:t xml:space=\"preserve\">Hello </w:t></w:r><w:commentRangeStart w:id=\"0\"/><w:commentRangeStart w:id=\"1\"/><w:r><w:t xml:space=\"preserve\">world</w:t></w:r><w:commentRangeEnd w:id=\"0\"/>"),
            "{body}"
        );
        assert!(body.contains("日本 </w:t></w:r><w:commentRangeStart w:id=\"2\"/><w:r><w:t xml:space=\"preserve\">🎉 too.</w:t>"), "UTF-16 offsets: {body}");
        assert!(body.contains("<w:t xml:space=\"preserve\">The</w:t></w:r><w:commentRangeEnd w:id=\"2\"/>"), "{body}");
        // The deleted words' thread (3) sits where they were, empty.
        assert!(body.contains("<w:t xml:space=\"preserve\"> </w:t></w:r><w:commentRangeStart w:id=\"3\"/><w:commentRangeEnd w:id=\"3\"/><w:r><w:rPr><w:rStyle w:val=\"CommentReference\"/></w:rPr><w:commentReference w:id=\"3\"/></w:r><w:r><w:t xml:space=\"preserve\">paragraph.</w:t>"), "{body}");
        // The gone paragraph's (4), the unreadable (5) and the whole file's (6) at the start; code's (7) round its block.
        let start = body.find("<w:p>").unwrap();
        for id in 4..=6 {
            let s = body.find(&format!("<w:commentRangeStart w:id=\"{id}\"/>")).unwrap_or_else(|| panic!("{id}: {body}"));
            assert!(s < body.find("Hello").unwrap() && s > start, "{id} at the start: {body}");
        }
        assert!(body.contains("<w:pStyle w:val=\"Code\"/></w:pPr><w:commentRangeStart w:id=\"7\"/>"), "{body}");
        // Every comment: one start, one end, one reference.
        for id in 0..8 {
            for tag in ["commentRangeStart", "commentRangeEnd", "commentReference"] {
                assert_eq!(body.matches(&format!("<w:{tag} w:id=\"{id}\"/>")).count(), 1, "{tag} {id}: {body}");
            }
        }
        assert!(!body.contains("w:id=\"8\""));
        let list = part(&file, "word/comments.xml");
        assert!(list.contains("<w:comment w:id=\"0\" w:author=\"Priya Nair\" w:initials=\"PN\" w:date=\"2026-11-11T10:00:00Z\">"), "{list}");
        assert!(list.contains("w:author=\"omar.haddad\" w:initials=\"OH\""), "the email's local part, never more: {list}");
        assert!(!list.contains("demo.test"), "{list}");
        assert!(list.contains("This &amp; &lt;that&gt; one."), "{list}");
        assert!(
            list.contains("<w:t xml:space=\"preserve\">Which world?</w:t></w:r></w:p><w:p w14:paraId=\"10000000\""),
            "two lines, the last one named: {list}"
        );
        assert!(list.contains("[On text since removed: \u{201C}second\u{201D}]") && list.contains("\u{201C}paragraph\u{201D}"), "{list}");
        assert!(!list.contains("since removed: \u{201C}x\u{201D}"), "code is still there: {list}");
        let ex = part(&file, "word/commentsExtended.xml");
        assert!(ex.contains("<w15:commentEx w15:paraId=\"10000001\" w15:paraIdParent=\"10000000\" w15:done=\"0\"/>"), "the reply in its thread: {ex}");
        assert!(ex.contains("<w15:commentEx w15:paraId=\"10000002\" w15:done=\"1\"/>"), "resolved: {ex}");
        let types = part(&file, "[Content_Types].xml");
        assert!(
            types.contains(
                "<Override PartName=\"/word/comments.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml\"/>"
            ),
            "{types}"
        );
        assert!(
            types.contains("/word/commentsExtended.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.commentsExtended+xml\""),
            "{types}"
        );
        let rels = part(&file, "word/_rels/document.xml.rels");
        assert!(
            rels.contains("relationships/comments\" Target=\"comments.xml\"")
                && rels.contains("2011/relationships/commentsExtended\" Target=\"commentsExtended.xml\""),
            "{rels}"
        );
        // Without threads, nothing of comments.
        let (plain, lost) = super::super::docx_export(&doc, "Comments", None, &|_| None).unwrap();
        assert!(!part(&plain, "[Content_Types].xml").contains("comments") && !part(&plain, "word/document.xml").contains("comment"));
        assert!(!lost.iter().any(|l| l.contains("comment")));
    }

    /// Real documents' stored states (`*.ystate` in `FERRUM_PARITY_STATES`) as Word files in
    /// `FERRUM_PARITY_OUT`, to render them with an office suite beside the editor's print.
    #[test]
    fn word_files_of_stored_states() {
        let (Ok(dir), Ok(out)) = (std::env::var("FERRUM_PARITY_STATES"), std::env::var("FERRUM_PARITY_OUT")) else { return };
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "ystate") {
                let doc = super::super::decode(&std::fs::read(&path).unwrap());
                let name = path.file_stem().unwrap().to_string_lossy().to_string();
                let (file, lost) = super::super::docx_export(&doc, &name, Some("https://acme.example.com"), &|_| None).unwrap();
                std::fs::write(format!("{out}/{name}.docx"), file).unwrap();
                eprintln!("{name}: {} words; not carried: {lost:?}", super::super::plain_text(&doc).split_whitespace().count());
            }
        }
    }
}
