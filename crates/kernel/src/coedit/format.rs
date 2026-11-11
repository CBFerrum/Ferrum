//! A document's formatting as the exporters read it (Docs' stored format 2, the editor's
//! `format.ts`, `page.ts`, `pages.ts`, `regions.ts`): every stored value goes through the same check
//! the editor applies when it draws one, so no exporter writes a value it has not validated (a
//! hostile client can store anything); and what the exporters share: the page, the footnotes in
//! the order of their references, the headings, and what a document uses that an export may not
//! be able to carry.

use super::{FRAGMENT, IMAGE, number};
use std::collections::BTreeSet;
use yrs::types::Attrs;
use yrs::types::text::YChange;
use yrs::{Any, GetString, Map, Out, ReadTxn, Text, Xml, XmlElementRef, XmlFragment, XmlOut};

/// The header and footer fragments: pages 2 on (or all), and the first page's own.
pub const REGIONS: [&str; 4] = ["header", "footer", "firstHeader", "firstFooter"];
/// The newest stored format these exporters know.
pub const FORMAT: i64 = 2;

/// A value as a node keeps it (node attributes are `Out`s).
pub(super) fn any(v: Option<Out>) -> Option<Any> {
    match v? {
        Out::Any(a) => Some(a),
        _ => None,
    }
}

/// A mark's value on a run, by name: `name`, or `name--hash` (y-prosemirror's key for marks that
/// overlap); null and false are no mark.
pub(super) fn mark<'a>(attrs: Option<&'a Attrs>, name: &str) -> Option<&'a Any> {
    attrs?
        .iter()
        .find(|(k, v)| {
            (&***k == name || k.strip_prefix(name).is_some_and(|r| r.starts_with("--"))) && !matches!(v, Any::Null | Any::Undefined | Any::Bool(false))
        })
        .map(|(_, v)| v)
}

/// A mark's attribute (`{"color": "#ff0000"}`).
pub(super) fn mark_field<'a>(v: Option<&'a Any>, key: &str) -> Option<&'a Any> {
    match v? {
        Any::Map(m) => m.get(key),
        _ => None,
    }
}

fn float(v: &Any) -> Option<f64> {
    match v {
        Any::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// `12`, `12.5pt`, ` 9 px `: the number and its unit (lower case, maybe empty).
fn with_unit(s: &str) -> Option<(f64, String)> {
    let s = s.trim();
    let end = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (n, unit) = s.split_at(end);
    if n.is_empty() || n.len() > 12 || n.matches('.').count() > 1 || n.ends_with('.') {
        return None;
    }
    let n: f64 = n.parse().ok()?;
    Some((n, unit.trim().to_ascii_lowercase()))
}

// ── colours ──

const NAMED: [(&str, &str); 20] = [
    ("black", "#000000"),
    ("white", "#ffffff"),
    ("silver", "#c0c0c0"),
    ("gray", "#808080"),
    ("grey", "#808080"),
    ("maroon", "#800000"),
    ("red", "#ff0000"),
    ("purple", "#800080"),
    ("fuchsia", "#ff00ff"),
    ("magenta", "#ff00ff"),
    ("green", "#008000"),
    ("lime", "#00ff00"),
    ("olive", "#808000"),
    ("yellow", "#ffff00"),
    ("navy", "#000080"),
    ("blue", "#0000ff"),
    ("teal", "#008080"),
    ("aqua", "#00ffff"),
    ("cyan", "#00ffff"),
    ("orange", "#ffa500"),
];

/// A colour as `#rrggbb` (lower case), or None: what the editor's `cleanColor` takes from a
/// stored value (hex of 3, 6 or 8 digits, the basic names; fully transparent is none).
pub fn color(v: Option<&Any>) -> Option<String> {
    let Some(Any::String(s)) = v else { return None };
    let v = s.trim().to_ascii_lowercase();
    if v.len() > 40 {
        return None;
    }
    if let Some(h) = v.strip_prefix('#').filter(|h| matches!(h.len(), 3 | 6 | 8) && h.chars().all(|c| c.is_ascii_hexdigit())) {
        return match h.len() {
            3 => Some(h.chars().fold(String::from("#"), |mut o, c| {
                o.push(c);
                o.push(c);
                o
            })),
            8 if &h[6..] == "00" => None,
            _ => Some(format!("#{}", &h[..6])),
        };
    }
    NAMED.iter().find(|(n, _)| *n == v).map(|(_, h)| (*h).to_string())
}

// ── fonts ──

/// The families the editor offers, by the name it keeps: (name, CSS stack, Word font, Word family).
const FONTS: [(&str, &str, &str, &str); 8] = [
    ("Serif", "ui-serif, Georgia, 'Times New Roman', serif", "Georgia", "roman"),
    ("Monospace", "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace", "Courier New", "modern"),
    ("Arial", "Arial, Helvetica, sans-serif", "Arial", "swiss"),
    ("Georgia", "Georgia, 'Times New Roman', serif", "Georgia", "roman"),
    ("Times New Roman", "'Times New Roman', Times, serif", "Times New Roman", "roman"),
    ("Courier New", "'Courier New', Courier, monospace", "Courier New", "modern"),
    ("Verdana", "Verdana, Geneva, sans-serif", "Verdana", "swiss"),
    ("Trebuchet MS", "'Trebuchet MS', Helvetica, sans-serif", "Trebuchet MS", "swiss"),
];

/// A family name to use, as the editor's `cleanFontFamily` takes one: the first of a list, one of
/// the offered names in its spelling, or a plain name (letters, digits, spaces, `_.-`, at most
/// 60); None for the default and for anything that could be CSS.
pub fn font_family(v: Option<&Any>) -> Option<String> {
    let Some(Any::String(s)) = v else { return None };
    if s.len() > 300 {
        return None;
    }
    let first = s.split(',').next()?.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    let lower = first.to_lowercase();
    match lower.as_str() {
        "" | "sans-serif" | "system-ui" | "ui-sans-serif" | "-apple-system" | "inherit" | "initial" => return None,
        "serif" | "ui-serif" => return Some("Serif".into()),
        "monospace" | "ui-monospace" => return Some("Monospace".into()),
        _ => {}
    }
    if let Some(f) = FONTS.iter().find(|f| f.0.to_lowercase() == lower) {
        return Some(f.0.into());
    }
    let plain = (1..=60).contains(&first.chars().count()) && first.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '.' | '-'));
    (plain && !matches!(lower.as_str(), "url" | "expression" | "import")).then(|| first.to_string())
}

/// The generic family a name falls back to: `serif`, `monospace` or `sans-serif`.
fn generic(name: &str) -> &'static str {
    let l = name.to_lowercase();
    if ["mono", "courier", "consol"].iter().any(|k| l.contains(k)) {
        "monospace"
    } else if !l.contains("sans") && ["serif", "georgia", "times", "cambria", "garamond", "book", "palatino"].iter().any(|k| l.contains(k)) {
        "serif"
    } else {
        "sans-serif"
    }
}

/// The CSS `font-family` for a checked name (single quotes: it goes in a double-quoted attribute).
pub fn font_stack(name: &str) -> String {
    match FONTS.iter().find(|f| f.0 == name) {
        Some(f) => f.1.to_string(),
        None => format!("'{name}', {}", generic(name)),
    }
}

/// Word's font for a checked name, and its family (`w:family`, what a reader without the font
/// substitutes from: `roman`, `swiss`, `modern`).
pub fn word_font(name: &str) -> (String, &'static str) {
    match FONTS.iter().find(|f| f.0 == name) {
        Some(f) => (f.2.to_string(), f.3),
        None => (
            name.to_string(),
            match generic(name) {
                "serif" => "roman",
                "monospace" => "modern",
                _ => "swiss",
            },
        ),
    }
}

/// A size in points, rounded to a half point, 6 to 200 (a number, `12pt` or `16px`).
pub fn font_size(v: Option<&Any>) -> Option<f64> {
    let pt = match v? {
        Any::String(s) => match with_unit(s)? {
            (n, u) if u == "px" => n * 0.75,
            (n, u) if u.is_empty() || u == "pt" => n,
            _ => return None,
        },
        other => float(other)?,
    };
    let pt = (pt * 2.0).round() / 2.0;
    (6.0..=200.0).contains(&pt).then_some(pt)
}

// ── paragraphs ──

/// A line height as a ratio of the text size, 0.8 to 4 (`1.5`, `150%`).
pub fn line_height(v: Option<&Any>) -> Option<f64> {
    let n = match v? {
        Any::String(s) => match with_unit(s)? {
            (n, u) if u == "%" => n / 100.0,
            (n, u) if u.is_empty() => n,
            _ => return None,
        },
        other => float(other)?,
    };
    let n = (n * 100.0).round() / 100.0;
    (0.8..=4.0).contains(&n).then_some(n)
}

/// Space above or below a paragraph, in whole points, 0 to 200.
pub fn space(v: Option<&Any>) -> Option<u32> {
    let pt = match v? {
        Any::String(s) => {
            let (n, u) = with_unit(s)?;
            n * match u.as_str() {
                "" | "pt" => 1.0,
                "px" => 0.75,
                "in" => 72.0,
                "cm" => 28.346,
                "mm" => 2.8346,
                _ => return None,
            }
        }
        other => float(other)?,
    };
    let pt = pt.round();
    (0.0..=200.0).contains(&pt).then_some(pt as u32)
}

/// An indent in levels of 36 pt, 1 to 8.
pub fn indent(v: Option<&Any>) -> Option<u32> {
    let n = match v? {
        Any::String(s) => s.trim().parse::<f64>().ok()?,
        other => float(other)?,
    };
    (n.fract() == 0.0 && (1.0..=8.0).contains(&n)).then_some(n as u32)
}

/// `left`, `center`, `right` or `justify`.
pub fn text_align(v: Option<&Any>) -> Option<&'static str> {
    match v? {
        Any::String(s) => ["left", "center", "right", "justify"].into_iter().find(|a| **s == **a),
        _ => None,
    }
}

/// A footnote's or a heading anchor's id: 6 to 24 letters and digits.
pub fn id(v: Option<&Any>) -> Option<String> {
    match v? {
        Any::String(s) if (6..=24).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric()) => Some(s.to_string()),
        _ => None,
    }
}

/// `title`, `subtitle` or none.
pub fn role(v: Option<&Any>) -> Option<&'static str> {
    match v? {
        Any::String(s) => ["title", "subtitle"].into_iter().find(|r| **s == **r),
        _ => None,
    }
}

/// A callout's kind: `note`, `tip`, `warning`, `alert`.
pub fn callout(v: Option<&Any>) -> Option<&'static str> {
    match v? {
        Any::String(s) => ["note", "tip", "warning", "alert"].into_iter().find(|k| **s == **k),
        _ => None,
    }
}

/// A list's bullet (`circle`, `square`; None is the disc).
pub fn bullet(v: Option<&Any>) -> Option<&'static str> {
    match v? {
        Any::String(s) => ["circle", "square"].into_iter().find(|b| **s == **b),
        _ => None,
    }
}

/// A numbered list's numbers: `1` (decimal), `a`, `i`; anything else is decimal.
pub fn numbering(v: Option<&Any>) -> &'static str {
    match v {
        Some(Any::String(s)) if &**s == "a" => "a",
        Some(Any::String(s)) if &**s == "i" => "i",
        _ => "1",
    }
}

/// An image's alignment; anything else is `center`.
pub fn image_align(v: Option<&Any>) -> &'static str {
    match v {
        Some(Any::String(s)) => ["left", "center", "right", "wrapLeft", "wrapRight"].into_iter().find(|a| **s == **a).unwrap_or("center"),
        _ => "center",
    }
}

/// An image's width or height in pixels, 1 to 4000.
pub fn image_px(v: Option<&Any>) -> Option<u32> {
    let n = float(v?)?;
    (1.0..=4000.0).contains(&n).then(|| n.round() as u32)
}

/// A caption: one line of words, at most 300 characters.
pub fn caption(v: Option<&Any>) -> String {
    let Some(Any::String(s)) = v else { return String::new() };
    let one: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    one.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect()
}

/// A table cell's column widths in pixels (one per spanned column; 0 for one not set).
pub fn colwidths(v: Option<&Any>) -> Vec<u32> {
    match v {
        Some(Any::Array(a)) => a.iter().map(|w| float(w).filter(|w| (1.0..=10000.0).contains(w)).map_or(0, |w| w.round() as u32)).collect(),
        _ => Vec::new(),
    }
}

/// A header or footer field: `page`, `pages`, `date`, `title`; anything else is `page`.
pub fn field(v: Option<&Any>) -> &'static str {
    match v {
        Some(Any::String(s)) => ["page", "pages", "date", "title"].into_iter().find(|f| **s == **f).unwrap_or("page"),
        _ => "page",
    }
}

/// A link's target, as the editor's `cleanHref` keeps one.
#[derive(Debug, PartialEq)]
pub enum Href {
    Web(String),
    /// A heading of this document, by its anchor.
    Heading(String),
    /// Another document, by its id.
    Doc(String),
}

pub fn href(v: Option<&Any>) -> Option<Href> {
    let Some(Any::String(s)) = mark_field(v, "href") else { return None };
    let h = s.trim();
    if let Some(a) = h.strip_prefix('#') {
        return id(Some(&Any::from(a))).map(Href::Heading);
    }
    if let Some(d) = h.strip_prefix("/docs/d/") {
        return uuid::Uuid::try_parse(d).ok().filter(|_| d.len() == 36).map(|u| Href::Doc(u.to_string()));
    }
    super::safe_href(h).then(|| Href::Web(h.to_string()))
}

/// An origin a link to another document may start with: `http(s)://host[:port]`, nothing else.
pub fn origin(o: Option<&str>) -> Option<&str> {
    let o = o?.trim_end_matches('/');
    let rest = o.strip_prefix("https://").or_else(|| o.strip_prefix("http://"))?;
    (!rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))).then_some(o)
}

// ── the page ──

/// A document's page, in twips, as the editor's `cleanSetup` reads it: anything missing or out
/// of bounds is the legacy page (A4 portrait, 2 cm margins).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Page {
    pub width: u32,
    pub height: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
    pub left: u32,
    pub landscape: bool,
    pub different_first: bool,
    /// The stored format (1 when absent).
    pub format: i64,
}

pub const LEGACY_MARGIN: u32 = 1134;
/// Header and footer distance from the paper's edge.
pub const EDGE: u32 = 720;

impl Page {
    pub fn text_width(&self) -> u32 {
        self.width - self.left - self.right
    }
}

impl Default for Page {
    fn default() -> Self {
        Page {
            width: 11906,
            height: 16838,
            top: LEGACY_MARGIN,
            right: LEGACY_MARGIN,
            bottom: LEGACY_MARGIN,
            left: LEGACY_MARGIN,
            landscape: false,
            different_first: false,
            format: 1,
        }
    }
}

pub fn page<T: ReadTxn>(txn: &T) -> Page {
    let mut p = Page::default();
    let Some(m) = txn.get_map("page") else { return p };
    let get = |k: &str| any(m.get(txn, k));
    let (w, h) = match get("size") {
        Some(Any::String(s)) => match &*s {
            "Letter" => (12240, 15840),
            "Legal" => (12240, 20160),
            "A3" => (16838, 23811),
            "A5" => (8391, 11906),
            _ => (11906, 16838),
        },
        _ => (11906, 16838),
    };
    p.landscape = matches!(get("orientation"), Some(Any::String(s)) if &*s == "landscape");
    (p.width, p.height) = if p.landscape { (h, w) } else { (w, h) };
    p.different_first = matches!(get("differentFirst"), Some(Any::Bool(true)));
    let margin = |k: &str| {
        let n = match get(k)? {
            Any::String(s) => s.trim().parse::<f64>().ok()?,
            other => float(&other)?,
        };
        (283.0..=4320.0).contains(&n).then(|| n.round() as u32)
    };
    p.top = margin("top").unwrap_or(LEGACY_MARGIN);
    p.right = margin("right").unwrap_or(LEGACY_MARGIN);
    p.bottom = margin("bottom").unwrap_or(LEGACY_MARGIN);
    p.left = margin("left").unwrap_or(LEGACY_MARGIN);
    // Margins that leave less than 2 inches for the text are the legacy ones.
    if p.width < p.left + p.right + 2880 {
        (p.left, p.right) = (LEGACY_MARGIN, LEGACY_MARGIN);
    }
    if p.height < p.top + p.bottom + 2880 {
        (p.top, p.bottom) = (LEGACY_MARGIN, LEGACY_MARGIN);
    }
    p.format = get("format").and_then(|v| float(&v)).map_or(1, |f| f as i64);
    p
}

// ── the text's structure: headings, footnotes ──

/// A heading: its level, its words (spaces collapsed) and its anchor.
pub struct Heading {
    pub level: u32,
    pub text: String,
    pub anchor: Option<String>,
}

/// The headings of the text and its footnotes, in order. A writer walking the text meets
/// headings and footnote references in this same order: the n-th of each is `headings[n]` and
/// note n + 1.
pub struct Outline {
    pub headings: Vec<Heading>,
    /// Each reference's body, by the reference's place (a reference without one: an empty note).
    pub notes: Vec<Option<XmlElementRef>>,
}

pub(super) fn text_of<T: ReadTxn>(e: &XmlElementRef, txn: &T) -> String {
    let mut out = String::new();
    for c in e.children(txn) {
        match c {
            XmlOut::Text(t) => out.push_str(&t.get_string(txn)),
            XmlOut::Element(e) => out.push_str(&text_of(&e, txn)),
            XmlOut::Fragment(_) => {}
        }
    }
    out
}

pub fn outline<T: ReadTxn>(txn: &T) -> Outline {
    fn walk<T: ReadTxn>(e: &XmlElementRef, txn: &T, headings: &mut Vec<Heading>, refs: &mut Vec<Option<String>>) {
        match &**e.tag() {
            "heading" => {
                headings.push(Heading {
                    level: number(e.get_attribute(txn, "level")).unwrap_or(1).clamp(1, 6) as u32,
                    text: text_of(e, txn).split_whitespace().collect::<Vec<_>>().join(" "),
                    anchor: id(any(e.get_attribute(txn, "anchor")).as_ref()),
                });
                // References in a heading count too.
                for c in e.children(txn) {
                    if let XmlOut::Element(c) = c {
                        walk(&c, txn, &mut Vec::new(), refs);
                    }
                }
            }
            "footnoteRef" => refs.push(id(any(e.get_attribute(txn, "id")).as_ref())),
            _ => {
                for c in e.children(txn) {
                    if let XmlOut::Element(c) = c {
                        walk(&c, txn, headings, refs);
                    }
                }
            }
        }
    }
    let mut headings = Vec::new();
    let mut refs = Vec::new();
    let mut bodies: Vec<(String, XmlElementRef)> = Vec::new();
    if let Some(f) = txn.get_xml_fragment(FRAGMENT) {
        for c in f.children(txn) {
            let XmlOut::Element(e) = c else { continue };
            if &**e.tag() == "footnote" {
                if let Some(i) = id(any(e.get_attribute(txn, "id")).as_ref()) {
                    bodies.push((i, e));
                }
                continue;
            }
            walk(&e, txn, &mut headings, &mut refs);
        }
    }
    let notes = refs.into_iter().map(|r| r.and_then(|r| bodies.iter().find(|(i, _)| *i == r).map(|(_, e)| e.clone()))).collect();
    Outline { headings, notes }
}

/// The paragraphs of a header or footer region (None when it has no words or fields).
pub fn region<T: ReadTxn>(txn: &T, name: &str) -> Option<Vec<XmlElementRef>> {
    let f = txn.get_xml_fragment(name)?;
    let paras: Vec<XmlElementRef> = f.children(txn).filter_map(|c| if let XmlOut::Element(e) = c { Some(e) } else { None }).collect();
    let used =
        paras.iter().any(|p| p.children(txn).any(|c| matches!(c, XmlOut::Element(_)) || matches!(&c, XmlOut::Text(t) if !t.get_string(txn).trim().is_empty())));
    used.then_some(paras)
}

// ── tables ──

/// A place in a table's row: a cell, or a column a merged cell from a row above still covers.
pub enum Slot {
    Cell { col: usize, span: usize, down: i64, el: XmlElementRef, header: bool },
    Covered { col: usize, span: usize },
}

/// A table's rows as slots in column order, and how many columns it has.
pub fn table_map<T: ReadTxn>(table: &XmlElementRef, txn: &T) -> (Vec<Vec<Slot>>, usize) {
    // By column: (rows the merged cell above still covers, its width).
    let mut covered: Vec<Option<(i64, usize)>> = Vec::new();
    let mut rows = Vec::new();
    let mut width = 0usize;
    let skip = |col: &mut usize, covered: &mut Vec<Option<(i64, usize)>>, slots: &mut Vec<Slot>| {
        while let Some(Some((left, span))) = covered.get(*col).copied() {
            slots.push(Slot::Covered { col: *col, span });
            covered[*col] = if left > 1 { Some((left - 1, span)) } else { None };
            *col += span;
        }
    };
    for row in table.children(txn) {
        let XmlOut::Element(row) = row else { continue };
        let mut slots = Vec::new();
        let mut col = 0usize;
        for cell in row.children(txn) {
            let XmlOut::Element(cell) = cell else { continue };
            skip(&mut col, &mut covered, &mut slots);
            let span = number(cell.get_attribute(txn, "colspan")).unwrap_or(1).clamp(1, 63) as usize;
            let down = number(cell.get_attribute(txn, "rowspan")).unwrap_or(1).clamp(1, 1000);
            if down > 1 {
                if covered.len() < col + 1 {
                    covered.resize(col + 1, None);
                }
                covered[col] = Some((down - 1, span));
            }
            let header = &**cell.tag() == "tableHeader";
            slots.push(Slot::Cell { col, span, down, el: cell, header });
            col += span;
        }
        skip(&mut col, &mut covered, &mut slots);
        width = width.max(col);
        rows.push(slots);
    }
    (rows, width.min(63))
}

/// The table's column widths in pixels, fitted to `text_px` when wider; None when no column has a
/// width (the columns share the text's width). A column without one shares what the others leave.
pub fn grid<T: ReadTxn>(rows: &[Vec<Slot>], txn: &T, cols: usize, text_px: u32) -> Option<Vec<u32>> {
    let mut px = vec![0u32; cols];
    for row in rows {
        for slot in row {
            if let Slot::Cell { col, el, .. } = slot {
                for (i, w) in colwidths(any(el.get_attribute(txn, "colwidth")).as_ref()).into_iter().enumerate() {
                    if let Some(c) = px.get_mut(col + i).filter(|c| **c == 0) {
                        *c = w;
                    }
                }
            }
        }
    }
    if px.iter().all(|w| *w == 0) {
        return None;
    }
    let set: u32 = px.iter().sum();
    let unset = px.iter().filter(|w| **w == 0).count() as u32;
    if unset > 0 {
        let share = (text_px.saturating_sub(set) / unset).max(25);
        px.iter_mut().filter(|w| **w == 0).for_each(|w| *w = share);
    }
    let total: u32 = px.iter().sum();
    if total > text_px {
        px.iter_mut().for_each(|w| *w = (u64::from(*w) * u64::from(text_px) / u64::from(total)).max(1) as u32);
    }
    Some(px)
}

// ── what a document uses ──

const NODES: [&str; 23] = [
    "paragraph",
    "heading",
    "blockquote",
    "bulletList",
    "orderedList",
    "listItem",
    "taskList",
    "taskItem",
    "codeBlock",
    "horizontalRule",
    "table",
    "tableRow",
    "tableCell",
    "tableHeader",
    IMAGE,
    "hardBreak",
    "mention",
    "pageBreak",
    "tableOfContents",
    "footnote",
    "footnoteRef",
    "pageField",
    "text",
];
const MARKS: [&str; 12] =
    ["bold", "italic", "underline", "strike", "code", "subscript", "superscript", "textStyle", "highlight", "link", "insertion", "deletion"];

/// A stored name as it may be shown and sent in a header: ASCII letters, digits, `-` and `_`.
fn shown(name: &str) -> String {
    let base = name.split("--").next().unwrap_or(name);
    let s: String = base.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).take(40).collect();
    if s.is_empty() { "unnamed".into() } else { s }
}

/// What a document uses that some export may not carry, in words a person reads (`fonts`,
/// `page setup`), and what no exporter knows (`unknown node x`: its words are kept).
pub fn features<T: ReadTxn>(txn: &T) -> BTreeSet<String> {
    fn runs<T: ReadTxn>(t: &yrs::XmlTextRef, txn: &T, out: &mut BTreeSet<String>) {
        for chunk in t.diff(txn, YChange::identity) {
            let Some(attrs) = chunk.attributes.as_deref() else { continue };
            for k in attrs.keys() {
                let base = k.split("--").next().unwrap_or(k);
                if !MARKS.contains(&base) {
                    out.insert(format!("unknown mark {}", shown(k)));
                }
            }
            let a = Some(attrs);
            let style = mark(a, "textStyle");
            if font_family(mark_field(style, "fontFamily")).is_some() {
                out.insert("fonts".into());
            }
            if font_size(mark_field(style, "fontSize")).is_some() {
                out.insert("text sizes".into());
            }
            if color(mark_field(style, "color")).is_some() {
                out.insert("text colours".into());
            }
            if mark(a, "highlight").is_some() {
                out.insert("highlights".into());
            }
            for (m, f) in [("underline", "underline"), ("subscript", "subscript and superscript"), ("superscript", "subscript and superscript")] {
                if mark(a, m).is_some() {
                    out.insert(f.into());
                }
            }
            if mark(a, "insertion").is_some() || mark(a, "deletion").is_some() {
                out.insert("suggested changes".into());
            }
            match href(mark(a, "link")) {
                Some(Href::Heading(_)) => out.insert("links to headings".into()),
                Some(Href::Doc(_)) => out.insert("links to other documents".into()),
                _ => false,
            };
        }
    }
    fn walk<T: ReadTxn>(node: XmlOut, txn: &T, out: &mut BTreeSet<String>) {
        let e = match node {
            XmlOut::Text(t) => return runs(&t, txn, out),
            XmlOut::Fragment(f) => {
                for c in f.children(txn) {
                    walk(c, txn, out);
                }
                return;
            }
            XmlOut::Element(e) => e,
        };
        let tag = e.tag().clone();
        let attr = |k: &str| any(e.get_attribute(txn, k));
        if !NODES.contains(&&*tag) {
            out.insert(format!("unknown node {}", shown(&tag)));
        }
        let mut uses = |f: &str| {
            out.insert(f.to_string());
        };
        match &*tag {
            "paragraph" | "heading" => {
                if text_align(attr("textAlign").as_ref()).is_some_and(|a| a != "left") {
                    uses("alignment");
                }
                if line_height(attr("lineHeight").as_ref()).is_some() {
                    uses("line spacing");
                }
                if space(attr("spaceBefore").as_ref()).is_some() || space(attr("spaceAfter").as_ref()).is_some() {
                    uses("paragraph spacing");
                }
                if indent(attr("indent").as_ref()).is_some() {
                    uses("indents");
                }
                if role(attr("role").as_ref()).is_some() {
                    uses("title and subtitle styles");
                }
            }
            "blockquote" if callout(attr("callout").as_ref()).is_some() => uses("callouts"),
            "bulletList" if bullet(attr("bullet").as_ref()).is_some() => uses("list styles"),
            "orderedList" if numbering(attr("type").as_ref()) != "1" => uses("list styles"),
            "tableCell" | "tableHeader" => {
                if color(attr("background").as_ref()).is_some() {
                    uses("cell colours");
                }
                if colwidths(attr("colwidth").as_ref()).iter().any(|w| *w > 0) {
                    uses("column widths");
                }
                if number(e.get_attribute(txn, "colspan")).unwrap_or(1) > 1 || number(e.get_attribute(txn, "rowspan")).unwrap_or(1) > 1 {
                    uses("merged cells");
                }
            }
            IMAGE => {
                if image_px(attr("width").as_ref()).is_some() {
                    uses("image sizes");
                }
                if image_align(attr("align").as_ref()) != "center" {
                    uses("image alignment");
                }
                if !caption(attr("caption").as_ref()).is_empty() {
                    uses("image captions");
                }
            }
            "pageBreak" => uses("page breaks"),
            "tableOfContents" => uses("contents"),
            "footnoteRef" => uses("footnotes"),
            _ => {}
        }
        for c in e.children(txn) {
            walk(c, txn, out);
        }
    }
    let mut out = BTreeSet::new();
    if let Some(f) = txn.get_xml_fragment(FRAGMENT) {
        walk(XmlOut::Fragment(f), txn, &mut out);
    }
    for r in REGIONS {
        if let Some(paras) = region(txn, r) {
            out.insert("header and footer".into());
            let mut inner = BTreeSet::new();
            for p in paras {
                walk(XmlOut::Element(p), txn, &mut inner);
            }
            // What a region's words use is the region's: only what nothing knows counts here.
            out.extend(inner.into_iter().filter(|f| f.starts_with("unknown ")));
        }
    }
    if txn.get_map("page").is_some_and(|m| ["size", "orientation", "top", "right", "bottom", "left"].iter().any(|k| m.get(txn, k).is_some())) {
        out.insert("page setup".into());
    }
    let p = page(txn);
    if p.format > FORMAT {
        out.insert(format!("content from a newer version of Docs (format {})", p.format.min(9999)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<Any> {
        Some(Any::from(v))
    }

    #[test]
    fn stored_values_are_checked_as_the_editor_checks_them() {
        assert_eq!(color(s(" #ABC ").as_ref()).as_deref(), Some("#aabbcc"));
        assert_eq!(color(s("#1f2a4480").as_ref()).as_deref(), Some("#1f2a44"));
        assert_eq!(color(s("#1f2a4400").as_ref()), None, "transparent is no colour");
        assert_eq!(color(s("Red").as_ref()).as_deref(), Some("#ff0000"));
        for bad in ["red; background: url(https://x.test/a)", "url(x)", "expression(alert(1))", "#12345g", "</style>", "rgb(1,2,3)"] {
            assert_eq!(color(s(bad).as_ref()), None, "{bad}");
        }
        assert_eq!(font_family(s("Georgia").as_ref()).as_deref(), Some("Georgia"));
        assert_eq!(font_family(s("\"times new roman\", serif").as_ref()).as_deref(), Some("Times New Roman"));
        assert_eq!(font_family(s("ui-monospace, Menlo").as_ref()).as_deref(), Some("Monospace"));
        assert_eq!(font_family(s("system-ui").as_ref()), None);
        assert_eq!(font_family(s("Noto Sans JP").as_ref()).as_deref(), Some("Noto Sans JP"));
        for bad in ["a\"; x:y", "a};x{", "url", "x'</style><script>", "Comic(Sans)", &"x".repeat(61)] {
            assert_eq!(font_family(s(bad).as_ref()), None, "{bad}");
        }
        assert_eq!(font_size(Some(&Any::from(14))), Some(14.0));
        assert_eq!(font_size(s("16px").as_ref()), Some(12.0));
        assert_eq!(font_size(Some(&Any::from(10.3))), Some(10.5));
        for bad in [s("9999pt"), Some(Any::from(9999)), Some(Any::from(f64::NAN)), s("12em"), s("1e3"), Some(Any::from(5))] {
            assert_eq!(font_size(bad.as_ref()), None, "{bad:?}");
        }
        assert_eq!(line_height(s("150%").as_ref()), Some(1.5));
        assert_eq!(line_height(Some(&Any::from(1.15))), Some(1.15));
        assert_eq!(line_height(Some(&Any::from(40))), None);
        assert_eq!(space(s("0.5in").as_ref()), Some(36));
        assert_eq!(space(Some(&Any::from(201))), None);
        assert_eq!(indent(Some(&Any::from(3))), Some(3));
        assert_eq!(indent(Some(&Any::from(9))), None);
        assert_eq!(indent(Some(&Any::from(1.5))), None);
        assert_eq!(id(s("abc123").as_ref()).as_deref(), Some("abc123"));
        assert_eq!(id(s("ab\"><x").as_ref()), None);
        assert_eq!(image_px(Some(&Any::from(1e9))), None);
        assert_eq!(caption(s(" A\nchart\u{0}  of </x> ").as_ref()), "A chart of </x>");
        assert_eq!(colwidths(Some(&Any::from(vec![Any::from(120), Any::Null, Any::from(1e12)]))), vec![120, 0, 0]);
        let link = |h: &str| href(Some(&Any::from(std::collections::HashMap::from([("href".to_string(), Any::from(h))]))));
        assert_eq!(link("#abc123"), Some(Href::Heading("abc123".into())));
        assert_eq!(link("/docs/d/0192f0c4-1c2b-7c3d-8e4f-5a6b7c8d9e0f"), Some(Href::Doc("0192f0c4-1c2b-7c3d-8e4f-5a6b7c8d9e0f".into())));
        assert_eq!(link("https://x.test/"), Some(Href::Web("https://x.test/".into())));
        for bad in ["javascript:alert(1)", "#x", "/docs/d/../../admin", "data:text/html,x", "//evil.test"] {
            assert_eq!(link(bad), None, "{bad}");
        }
        assert_eq!(origin(Some("https://acme.example.com/")), Some("https://acme.example.com"));
        assert_eq!(origin(Some("https://x.test/\"><script>")), None);
        assert_eq!(origin(Some("javascript:alert(1)")), None);
    }
}

/// Stored documents for the exporters' tests, built as the editor stores them (DOCS-STORED-FORMAT.md
/// and the editor's own tests): one using everything format 2 has, one full of hostile values.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::super::{FRAGMENT, IMAGE, new_doc};
    use std::collections::HashMap;
    use yrs::types::Attrs;
    use yrs::{Any, Doc, Map, Text, Transact, TransactionMut, Xml, XmlElementPrelim, XmlElementRef, XmlFragment, XmlTextPrelim};

    pub const BLUE_PNG: &str =
        "89504e470d0a1a0a0000000d4948445200000004000000020802000000f0caea340000001049444154789c63e0ccbc05470cc81c0080720a614dabfcc20000000049454e44ae426082";
    pub const GREEN_PNG: &str =
        "89504e470d0a1a0a0000000d4948445200000004000000020802000000f0caea340000001049444154789c6390aa37872306640e0053b2068171abd9e00000000049454e44ae426082";
    pub const DOC_ID: &str = "0192f0c4-1c2b-7c3d-8e4f-5a6b7c8d9e0f";

    pub fn map(pairs: &[(&str, Any)]) -> Any {
        Any::from(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<HashMap<String, Any>>())
    }

    pub fn marks(pairs: &[(&str, Any)]) -> Attrs {
        pairs.iter().map(|(k, v)| (std::sync::Arc::from(*k), v.clone())).collect()
    }

    pub fn el<P: XmlFragment>(txn: &mut TransactionMut, parent: &P, tag: &str, attrs: &[(&str, Any)]) -> XmlElementRef {
        let e = parent.push_back(txn, XmlElementPrelim::empty(tag));
        for (k, v) in attrs {
            e.insert_attribute(txn, *k, v.clone());
        }
        e
    }

    /// Runs of text, each with its marks, in a paragraph-like element.
    pub fn runs(txn: &mut TransactionMut, parent: &XmlElementRef, parts: &[(&str, Attrs)]) {
        // The words, then their marks by range: text inserted after marked text takes its marks.
        let all: String = parts.iter().map(|p| p.0).collect();
        let t = parent.push_back(txn, XmlTextPrelim::new(all.as_str()));
        let mut at = 0u32;
        for (text, m) in parts {
            let len = text.encode_utf16().count() as u32;
            if !m.is_empty() {
                t.format(txn, at, len, m.clone());
            }
            at += len;
        }
    }

    pub fn para<P: XmlFragment>(txn: &mut TransactionMut, parent: &P, attrs: &[(&str, Any)], text: &str) -> XmlElementRef {
        let p = el(txn, parent, "paragraph", attrs);
        if !text.is_empty() {
            p.push_back(txn, XmlTextPrelim::new(text));
        }
        p
    }

    fn cell(txn: &mut TransactionMut, row: &XmlElementRef, tag: &str, attrs: &[(&str, Any)], text: &str) {
        let c = el(txn, row, tag, attrs);
        para(txn, &c, &[], text);
    }

    /// Everything format 2 stores: the "Annual report" (Letter landscape) of the editor's tests.
    pub fn everything() -> Doc {
        let doc = new_doc();
        let fragment = doc.get_or_insert_xml_fragment(FRAGMENT);
        let page = doc.get_or_insert_map("page");
        let header = doc.get_or_insert_xml_fragment("header");
        let footer = doc.get_or_insert_xml_fragment("footer");
        let first_header = doc.get_or_insert_xml_fragment("firstHeader");
        let mut txn = doc.transact_mut();
        for (k, v) in [
            ("size", Any::from("Letter")),
            ("orientation", Any::from("landscape")),
            ("top", Any::from(1440)),
            ("right", Any::from(1440)),
            ("bottom", Any::from(1440)),
            ("left", Any::from(1440)),
            ("differentFirst", Any::from(true)),
            ("format", Any::from(2)),
        ] {
            page.insert(&mut txn, k, v);
        }
        // Header (pages 2 on): the title at the right; footer: page n of m in the middle, the date at the left.
        let h = el(&mut txn, &header, "paragraph", &[("textAlign", Any::from("right"))]);
        runs(&mut txn, &h, &[("Annual report: ", marks(&[("bold", map(&[]))]))]);
        el(&mut txn, &h, "pageField", &[("kind", Any::from("title"))]);
        let f = el(&mut txn, &footer, "paragraph", &[("textAlign", Any::from("center"))]);
        runs(&mut txn, &f, &[("Page ", Attrs::new())]);
        el(&mut txn, &f, "pageField", &[("kind", Any::from("page"))]);
        runs(&mut txn, &f, &[(" of ", Attrs::new())]);
        el(&mut txn, &f, "pageField", &[("kind", Any::from("pages"))]);
        let f2 = el(&mut txn, &footer, "paragraph", &[]);
        el(&mut txn, &f2, "pageField", &[("kind", Any::from("date"))]);
        let fh = el(&mut txn, &first_header, "paragraph", &[("textAlign", Any::from("center"))]);
        runs(&mut txn, &fh, &[("CONFIDENTIAL", marks(&[("textStyle", map(&[("color", Any::from("#cf222e")), ("fontSize", Any::from(12))]))]))]);

        para(&mut txn, &fragment, &[("role", Any::from("title"))], "Annual report 2026");
        para(&mut txn, &fragment, &[("role", Any::from("subtitle"))], "Prepared for the board");
        el(&mut txn, &fragment, "tableOfContents", &[("levels", Any::from(3))]);
        let h1 = el(&mut txn, &fragment, "heading", &[("level", Any::from(1)), ("anchor", Any::from("intro01"))]);
        h1.push_back(&mut txn, XmlTextPrelim::new("Introduction"));
        let p = el(
            &mut txn,
            &fragment,
            "paragraph",
            &[
                ("textAlign", Any::from("justify")),
                ("lineHeight", Any::from(1.5)),
                ("spaceBefore", Any::from(6)),
                ("spaceAfter", Any::from(12)),
                ("indent", Any::from(1)),
            ],
        );
        let style = |pairs: &[(&str, Any)]| marks(&[("textStyle", map(pairs))]);
        runs(
            &mut txn,
            &p,
            &[
                ("Fonts: ", Attrs::new()),
                ("Georgia", style(&[("fontFamily", Any::from("Georgia")), ("color", Any::Null)])),
                (", serif", style(&[("fontFamily", Any::from("Serif"))])),
                (", mono", style(&[("fontFamily", Any::from("Monospace"))])),
                (", 18 pt", style(&[("fontSize", Any::from(18))])),
                (", red", style(&[("color", Any::from("#cf222e"))])),
                (", sky highlight", marks(&[("highlight", map(&[("color", Any::from("#a0e7ff"))]))])),
                (", yellow highlight", marks(&[("highlight", map(&[]))])),
                (", green highlight", marks(&[("highlight", map(&[("color", Any::from("#00ff00"))]))])),
                (", H", Attrs::new()),
                ("2", marks(&[("subscript", map(&[]))])),
                ("O, x", Attrs::new()),
                ("2", marks(&[("superscript", map(&[]))])),
                (", underlined", marks(&[("underline", map(&[]))])),
                (", bold italic", marks(&[("bold", map(&[])), ("italic", map(&[]))])),
            ],
        );
        el(&mut txn, &p, "footnoteRef", &[("id", Any::from("fn0001"))]);
        runs(&mut txn, &p, &[(" and the text goes on after the note.", Attrs::new())]);
        for level in 2..=6 {
            let h = el(&mut txn, &fragment, "heading", &[("level", Any::from(level))]);
            h.push_back(&mut txn, XmlTextPrelim::new(format!("Heading level {level}").as_str()));
        }
        let links = el(&mut txn, &fragment, "paragraph", &[]);
        let link = |h: &str| marks(&[("link", map(&[("href", Any::from(h))]))]);
        runs(
            &mut txn,
            &links,
            &[
                ("Back to the ", Attrs::new()),
                ("introduction", link("#intro01")),
                (", the ", Attrs::new()),
                ("budget document", link(&format!("/docs/d/{DOC_ID}"))),
                (" and ", Attrs::new()),
                ("the web", link("https://example.com/a?b=1&c=2")),
                (".", Attrs::new()),
            ],
        );
        let ul = el(&mut txn, &fragment, "bulletList", &[("bullet", Any::from("square"))]);
        let li = el(&mut txn, &ul, "listItem", &[]);
        para(&mut txn, &li, &[], "A square bullet");
        let nested = el(&mut txn, &li, "bulletList", &[("bullet", Any::from("circle"))]);
        let li2 = el(&mut txn, &nested, "listItem", &[]);
        para(&mut txn, &li2, &[], "A circle inside it");
        let ol = el(&mut txn, &fragment, "orderedList", &[("type", Any::from("a")), ("start", Any::from(3))]);
        for item in ["Letter c", "Letter d"] {
            let li = el(&mut txn, &ol, "listItem", &[]);
            para(&mut txn, &li, &[], item);
        }
        let ol2 = el(&mut txn, &fragment, "orderedList", &[("type", Any::from("i"))]);
        let li = el(&mut txn, &ol2, "listItem", &[]);
        para(&mut txn, &li, &[], "Roman one");
        let q = el(&mut txn, &fragment, "blockquote", &[("callout", Any::from("warning"))]);
        para(&mut txn, &q, &[], "A warning callout.");
        // A table: a header row (one cell spans two columns), a cell two rows tall with a colour.
        let table = el(&mut txn, &fragment, "table", &[]);
        let r1 = el(&mut txn, &table, "tableRow", &[]);
        cell(&mut txn, &r1, "tableHeader", &[("colwidth", Any::from(vec![Any::from(200)]))], "Region");
        cell(
            &mut txn,
            &r1,
            "tableHeader",
            &[("colspan", Any::from(2)), ("colwidth", Any::from(vec![Any::from(120), Any::from(160)]))],
            "Revenue (two columns)",
        );
        let r2 = el(&mut txn, &table, "tableRow", &[]);
        cell(&mut txn, &r2, "tableCell", &[("rowspan", Any::from(2)), ("background", Any::from("#fde8e8"))], "North (two rows)");
        cell(&mut txn, &r2, "tableCell", &[("background", Any::from("#e6f4ea"))], "12");
        cell(&mut txn, &r2, "tableCell", &[], "14");
        let r3 = el(&mut txn, &table, "tableRow", &[]);
        cell(&mut txn, &r3, "tableCell", &[], "13");
        cell(&mut txn, &r3, "tableCell", &[], "15");
        el(
            &mut txn,
            &fragment,
            IMAGE,
            &[
                ("image", Any::from("img1")),
                ("alt", Any::from("A blue chart")),
                ("width", Any::from(240)),
                ("height", Any::from(120)),
                ("align", Any::from("right")),
                ("caption", Any::from("Figure 1: sales by region")),
            ],
        );
        el(
            &mut txn,
            &fragment,
            IMAGE,
            &[("image", Any::from("img2")), ("alt", Any::from("A green square")), ("width", Any::from(96)), ("align", Any::from("wrapLeft"))],
        );
        para(&mut txn, &fragment, &[], &"This paragraph wraps round the green picture at its left. ".repeat(6));
        el(&mut txn, &fragment, "pageBreak", &[]);
        let h = el(&mut txn, &fragment, "heading", &[("level", Any::from(2))]);
        h.push_back(&mut txn, XmlTextPrelim::new("After the page break"));
        let p2 = para(&mut txn, &fragment, &[], "A second note");
        el(&mut txn, &p2, "footnoteRef", &[("id", Any::from("fn0002"))]);
        runs(&mut txn, &p2, &[(".", Attrs::new())]);
        para(&mut txn, &fragment, &[], "");
        let n1 = el(&mut txn, &fragment, "footnote", &[("id", Any::from("fn0001"))]);
        let np = el(&mut txn, &n1, "paragraph", &[]);
        runs(&mut txn, &np, &[("The first note, with ", Attrs::new()), ("a link", link("https://example.com/note"))]);
        let n2 = el(&mut txn, &fragment, "footnote", &[("id", Any::from("fn0002"))]);
        para(&mut txn, &n2, &[], "The second note.");
        drop(txn);
        doc
    }

    /// Values no editor writes, as a hostile client could store them, and content from a newer Docs.
    pub fn hostile() -> Doc {
        let doc = new_doc();
        let fragment = doc.get_or_insert_xml_fragment(FRAGMENT);
        let page = doc.get_or_insert_map("page");
        let header = doc.get_or_insert_xml_fragment("header");
        let mut txn = doc.transact_mut();
        for (k, v) in [
            ("size", Any::from("Huge")),
            ("orientation", Any::from("sideways")),
            ("top", Any::from(-5)),
            ("left", Any::from("1in; color:red")),
            ("format", Any::from(3)),
        ] {
            page.insert(&mut txn, k, v);
        }
        let h = el(&mut txn, &header, "paragraph", &[("textAlign", Any::from("center;color:red"))]);
        runs(
            &mut txn,
            &h,
            &[(
                "</style><script>alert(1)</script>\"'\\",
                marks(&[("textStyle", map(&[("color", Any::from("red;}body{display:none")), ("fontFamily", Any::from("x';}</style>"))]))]),
            )],
        );
        let p = el(
            &mut txn,
            &fragment,
            "paragraph",
            &[
                ("textAlign", Any::from("center;color:red")),
                ("lineHeight", Any::from("1;color:red")),
                ("spaceBefore", Any::from(1e12)),
                ("indent", Any::from("2\" onmouseover=\"x")),
            ],
        );
        runs(
            &mut txn,
            &p,
            &[
                (
                    "styled",
                    marks(&[
                        (
                            "textStyle",
                            map(&[
                                ("color", Any::from("red; background: url(https://x.test/a)")),
                                ("fontFamily", Any::from("a\"; x:y")),
                                ("fontSize", Any::from("9999pt")),
                            ]),
                        ),
                        ("highlight", map(&[("color", Any::from("expression(alert(1))"))])),
                    ]),
                ),
                (" js", marks(&[("link", map(&[("href", Any::from("javascript:alert(1)"))]))])),
                (" data", marks(&[("link", map(&[("href", Any::from("data:text/html,<script>alert(1)</script>"))]))])),
                (" up", marks(&[("link", map(&[("href", Any::from("/docs/d/../../admin"))]))])),
                (" sparkle", marks(&[("sparkle--abc", map(&[("glow", Any::from("url(x)"))]))])),
            ],
        );
        let hd = el(&mut txn, &fragment, "heading", &[("level", Any::from(99)), ("anchor", Any::from("x\"><script>"))]);
        hd.push_back(&mut txn, XmlTextPrelim::new("Heading"));
        let ul = el(&mut txn, &fragment, "bulletList", &[("bullet", Any::from("url(x)"))]);
        let li = el(&mut txn, &ul, "listItem", &[]);
        para(&mut txn, &li, &[], "item");
        let ol = el(&mut txn, &fragment, "orderedList", &[("type", Any::from("\" onclick=\"x")), ("start", Any::from(1e15))]);
        let li = el(&mut txn, &ol, "listItem", &[]);
        para(&mut txn, &li, &[], "item");
        let table = el(&mut txn, &fragment, "table", &[]);
        let r = el(&mut txn, &table, "tableRow", &[]);
        cell(
            &mut txn,
            &r,
            "tableCell",
            &[("background", Any::from("expression(alert(1))")), ("colwidth", Any::from(vec![Any::from(1e12)])), ("colspan", Any::from(1e9))],
            "cell",
        );
        el(
            &mut txn,
            &fragment,
            IMAGE,
            &[
                ("image", Any::from("img1")),
                ("alt", Any::from("\"><script>alert(1)</script>")),
                ("width", Any::from("1e9")),
                ("height", Any::from(-4)),
                ("align", Any::from("left;position:fixed")),
                ("caption", Any::from("</figcaption><script>alert(1)</script>")),
            ],
        );
        el(&mut txn, &fragment, "tableOfContents", &[("levels", Any::from("all"))]);
        let unknown = el(&mut txn, &fragment, "chart3d", &[("spec", Any::from("<svg onload=alert(1)>"))]);
        unknown.push_back(&mut txn, XmlTextPrelim::new("words of an unknown node"));
        let p = para(&mut txn, &fragment, &[], "a stray reference");
        el(&mut txn, &p, "footnoteRef", &[("id", Any::from("nobody"))]);
        el(&mut txn, &p, "footnoteRef", &[]);
        let note = el(&mut txn, &fragment, "footnote", &[("id", Any::from("orphan1"))]);
        para(&mut txn, &note, &[], "a note nothing refers to");
        drop(txn);
        doc
    }

    pub fn png(hex_bytes: &str) -> Vec<u8> {
        hex::decode(hex_bytes).expect("hex")
    }
}
