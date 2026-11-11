//! A document as HTML: a fragment for agents and previews ([`super::html_with`]) and a whole page
//! for downloads and the PDF renderer ([`super::html_page_export`]). Every style is built from a
//! checked value (`format`), never copied from what is stored; text and attributes are escaped;
//! links go to the web, mail, the document's own headings and (given the workspace's origin) other
//! documents. The page prints as the editor prints: `@page` from the document's page setup, the
//! header and footer in the page's margin boxes with the page number and count, page breaks,
//! footnotes at the end, the contents as links.

use super::format::{self, Href, Outline, Page, Slot, any, mark, mark_field};
use super::{FRAGMENT, IMAGE, esc, number, string, truthy};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use yrs::types::text::YChange;
use yrs::{Any, Doc, Out, ReadTxn, Text, Transact, Xml, XmlElementRef, XmlFragment, XmlOut};

/// Links to other documents, without the workspace's address to make them links.
pub(super) const LOST_DOC_LINKS: &str = "links to other documents (kept as words)";
/// Images whose bytes the export was not given.
pub(super) const LOST_IMAGES: &str = "images that could not be read (their descriptions stand in)";

struct Html<'a, T: ReadTxn> {
    txn: &'a T,
    images: &'a dyn Fn(&str) -> Option<String>,
    origin: Option<&'a str>,
    outline: Outline,
    page: Page,
    /// Headings and footnote references met so far.
    headings: usize,
    notes: usize,
    lost: BTreeSet<String>,
}

/// The document's own font (the editor's sans).
const BODY_FONT: &str = "system-ui,-apple-system,'Segoe UI',Roboto,sans-serif";

/// A heading's id: its anchor when something links to it, else its place.
fn heading_id(anchor: Option<&str>, n: usize) -> String {
    match anchor {
        Some(a) => format!("h_{a}"),
        None => format!("heading-{}", n + 1),
    }
}

impl<T: ReadTxn> Html<'_, T> {
    /// A paragraph's or heading's own style: alignment, line height, space around, indent.
    fn block_style(&self, e: &XmlElementRef) -> String {
        let a = |k: &str| any(e.get_attribute(self.txn, k));
        let mut s = String::new();
        if let Some(al) = format::text_align(a("textAlign").as_ref()).filter(|al| *al != "left") {
            let _ = write!(s, "text-align:{al};");
        }
        if let Some(lh) = format::line_height(a("lineHeight").as_ref()) {
            let _ = write!(s, "line-height:{lh};");
        }
        if let Some(pt) = format::space(a("spaceBefore").as_ref()) {
            let _ = write!(s, "margin-top:{pt}pt;");
        }
        if let Some(pt) = format::space(a("spaceAfter").as_ref()) {
            let _ = write!(s, "margin-bottom:{pt}pt;");
        }
        if let Some(n) = format::indent(a("indent").as_ref()) {
            let _ = write!(s, "margin-inline-start:{}pt;", n * 36);
        }
        if s.is_empty() {
            return s;
        }
        let mut out = String::from(" style=\"");
        esc(s.trim_end_matches(';'), &mut out);
        out.push('"');
        out
    }

    /// A run's marks as (tag, opening tag), outermost first.
    fn marks(&mut self, attrs: Option<&yrs::types::Attrs>) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        match format::href(mark(attrs, "link")) {
            Some(Href::Web(h)) => out.push(("a", h)),
            Some(Href::Heading(a)) => out.push(("a", format!("#h_{a}"))),
            Some(Href::Doc(id)) => match self.origin {
                Some(o) => out.push(("a", format!("{o}/docs/d/{id}"))),
                None => {
                    self.lost.insert(LOST_DOC_LINKS.into());
                }
            },
            None => {}
        }
        if let Some((_, h)) = out.first_mut() {
            let mut open = String::from("a href=\"");
            esc(h, &mut open);
            open.push_str(if open.starts_with("a href=\"#") { "\"" } else { "\" rel=\"noopener noreferrer nofollow\"" });
            *h = open;
        }
        let style = mark(attrs, "textStyle");
        let mut css = String::new();
        if let Some(c) = format::color(mark_field(style, "color")) {
            let _ = write!(css, "color:{c};");
        }
        if let Some(f) = format::font_family(mark_field(style, "fontFamily")) {
            let _ = write!(css, "font-family:{};", format::font_stack(&f));
        }
        if let Some(pt) = format::font_size(mark_field(style, "fontSize")) {
            let _ = write!(css, "font-size:{pt}pt;");
        }
        if !css.is_empty() {
            let mut open = String::from("span style=\"");
            esc(css.trim_end_matches(';'), &mut open);
            open.push('"');
            out.push(("span", open));
        }
        if let Some(h) = mark(attrs, "highlight") {
            out.push(("mark", format::color(mark_field(Some(h), "color")).map_or("mark".into(), |c| format!("mark style=\"background-color:{c}\""))));
        }
        for (name, tag) in [
            ("bold", "strong"),
            ("italic", "em"),
            ("underline", "u"),
            ("strike", "s"),
            ("subscript", "sub"),
            ("superscript", "sup"),
            ("code", "code"),
            ("insertion", "ins"),
            ("deletion", "del"),
        ] {
            if mark(attrs, name).is_some() {
                out.push((tag, tag.to_string()));
            }
        }
        out
    }

    fn children(&mut self, e: &XmlElementRef, out: &mut String, code: bool) {
        for child in e.children(self.txn) {
            self.node(child, out, code);
        }
    }

    fn node(&mut self, node: XmlOut, out: &mut String, in_code: bool) {
        let txn = self.txn;
        let e = match node {
            XmlOut::Text(t) => {
                for chunk in t.diff(txn, YChange::identity) {
                    let Out::Any(Any::String(s)) = chunk.insert else { continue };
                    let m = if in_code { Vec::new() } else { self.marks(chunk.attributes.as_deref()) };
                    for (_, open) in &m {
                        let _ = write!(out, "<{open}>");
                    }
                    esc(&s, out);
                    for (tag, _) in m.iter().rev() {
                        let _ = write!(out, "</{tag}>");
                    }
                }
                return;
            }
            XmlOut::Fragment(f) => {
                for c in f.children(txn) {
                    self.node(c, out, in_code);
                }
                return;
            }
            XmlOut::Element(e) => e,
        };
        let tag = e.tag().clone();
        let attr = |k: &str| any(e.get_attribute(txn, k));
        match &*tag {
            "paragraph" => {
                let class = match format::role(attr("role").as_ref()) {
                    Some(r) => format!(" class=\"{r}\""),
                    None => String::new(),
                };
                let _ = write!(out, "<p{class}{}>", self.block_style(&e));
                let at = out.len();
                self.children(&e, out, false);
                if out.len() == at {
                    out.push_str("<br>");
                }
                out.push_str("</p>\n");
            }
            "heading" => {
                let level = number(e.get_attribute(txn, "level")).unwrap_or(1).clamp(1, 6);
                let n = self.headings;
                self.headings += 1;
                let anchor = self.outline.headings.get(n).and_then(|h| h.anchor.clone());
                let _ = write!(out, "<h{level} id=\"{}\"{}>", heading_id(anchor.as_deref(), n), self.block_style(&e));
                self.children(&e, out, false);
                let _ = writeln!(out, "</h{level}>");
            }
            "codeBlock" => {
                out.push_str("<pre><code>");
                self.children(&e, out, true);
                out.push_str("</code></pre>\n");
            }
            "blockquote" => {
                match format::callout(attr("callout").as_ref()) {
                    Some(k) => {
                        let _ = write!(out, "<blockquote class=\"callout callout-{k}\">");
                    }
                    None => out.push_str("<blockquote>"),
                }
                self.children(&e, out, false);
                out.push_str("</blockquote>\n");
            }
            "bulletList" => {
                match format::bullet(attr("bullet").as_ref()) {
                    Some(b) => {
                        let _ = write!(out, "<ul style=\"list-style-type:{b}\">");
                    }
                    None => out.push_str("<ul>"),
                }
                self.children(&e, out, false);
                out.push_str("</ul>\n");
            }
            "orderedList" => {
                out.push_str("<ol");
                let kind = format::numbering(attr("type").as_ref());
                if kind != "1" {
                    let _ = write!(out, " type=\"{kind}\"");
                }
                if let Some(start) = number(e.get_attribute(txn, "start")).filter(|s| *s != 1 && (0..=1_000_000_000).contains(s)) {
                    let _ = write!(out, " start=\"{start}\"");
                }
                out.push('>');
                self.children(&e, out, false);
                out.push_str("</ol>\n");
            }
            "taskList" => {
                out.push_str("<ul class=\"tasks\">");
                self.children(&e, out, false);
                out.push_str("</ul>\n");
            }
            "taskItem" => {
                let checked = truthy(e.get_attribute(txn, "checked"));
                let _ = write!(out, "<li><input type=\"checkbox\" disabled{}><div>", if checked { " checked" } else { "" });
                self.children(&e, out, false);
                out.push_str("</div></li>\n");
            }
            "table" => self.table(&e, out),
            "hardBreak" => out.push_str("<br>"),
            "horizontalRule" => out.push_str("<hr>\n"),
            "pageBreak" => out.push_str("<div class=\"page-break\"></div>\n"),
            "tableOfContents" => self.contents(&e, out),
            "footnoteRef" => {
                let n = self.notes;
                self.notes += 1;
                if n < self.outline.notes.len() {
                    let _ = write!(out, "<sup class=\"fn-ref\"><a href=\"#fn-{0}\" id=\"fnref-{0}\">{0}</a></sup>", n + 1);
                }
            }
            // Bodies are written at the end, in the order of their references.
            "footnote" => {}
            "mention" => match format::mention_href(&e, txn, self.origin) {
                Some(href) => {
                    out.push_str("<a href=\"");
                    esc(&href, out);
                    out.push_str("\" rel=\"noopener noreferrer nofollow\">");
                    self.children(&e, out, in_code);
                    out.push_str("</a>");
                }
                None => self.children(&e, out, in_code),
            },
            IMAGE => self.image(&e, out),
            other => {
                let html = match other {
                    "listItem" => Some("li"),
                    "tableRow" => Some("tr"),
                    _ => None,
                };
                if let Some(h) = html {
                    let _ = write!(out, "<{h}>");
                }
                self.children(&e, out, in_code);
                if let Some(h) = html {
                    let _ = writeln!(out, "</{h}>");
                }
            }
        }
    }

    fn table(&mut self, e: &XmlElementRef, out: &mut String) {
        let txn = self.txn;
        let (rows, cols) = format::table_map(e, txn);
        if cols == 0 {
            return;
        }
        out.push_str("<table");
        let grid = format::grid(&rows, txn, cols, self.page.text_width() / 15);
        if let Some(g) = &grid {
            let _ = write!(out, " style=\"width:{}px\"><colgroup>", g.iter().sum::<u32>());
            for w in g {
                let _ = write!(out, "<col style=\"width:{w}px\">");
            }
            out.push_str("</colgroup>");
        } else {
            out.push('>');
        }
        out.push('\n');
        // Rows of header cells at the top repeat on every page a table runs over.
        let heads = rows.iter().take_while(|r| !r.is_empty() && r.iter().all(|s| matches!(s, Slot::Cell { header: true, .. } | Slot::Covered { .. }))).count();
        for (n, row) in rows.iter().enumerate() {
            if n == 0 && heads > 0 {
                out.push_str("<thead>");
            }
            if n == heads {
                out.push_str("<tbody>");
            }
            out.push_str("<tr>");
            for slot in row {
                let Slot::Cell { span, down, el, header, .. } = slot else { continue };
                let cell = if *header { "th" } else { "td" };
                let _ = write!(out, "<{cell}");
                if *span > 1 {
                    let _ = write!(out, " colspan=\"{span}\"");
                }
                if *down > 1 {
                    let _ = write!(out, " rowspan=\"{down}\"");
                }
                if let Some(c) = format::color(any(el.get_attribute(txn, "background")).as_ref()) {
                    let _ = write!(out, " style=\"background-color:{c}\"");
                }
                out.push('>');
                self.children(el, out, false);
                let _ = writeln!(out, "</{cell}>");
            }
            out.push_str("</tr>\n");
            if n + 1 == heads {
                out.push_str("</thead>");
            }
        }
        if rows.len() > heads {
            out.push_str("</tbody>");
        }
        out.push_str("</table>\n");
    }

    fn image(&mut self, e: &XmlElementRef, out: &mut String) {
        let txn = self.txn;
        let attr = |k: &str| any(e.get_attribute(txn, k));
        let alt = string(e.get_attribute(txn, "alt")).unwrap_or_default();
        let caption = format::caption(attr("caption").as_ref());
        let place = match format::image_align(attr("align").as_ref()) {
            "wrapLeft" => "wrap-left",
            "wrapRight" => "wrap-right",
            "left" => "align-left",
            "right" => "align-right",
            _ => "align-center",
        };
        match string(e.get_attribute(txn, "image")).and_then(|id| (self.images)(&id)) {
            Some(src) => {
                let _ = write!(out, "<figure class=\"image {place}\"><img src=\"");
                esc(&src, out);
                out.push_str("\" alt=\"");
                esc(alt.trim(), out);
                out.push('"');
                if let Some(w) = format::image_px(attr("width").as_ref()) {
                    let _ = write!(out, " width=\"{w}\"");
                    // The height follows the width: a stored one only keeps the shape.
                    if let Some(h) = format::image_px(attr("height").as_ref()) {
                        let _ = write!(out, " height=\"{h}\"");
                    }
                }
                out.push('>');
                if !caption.is_empty() {
                    out.push_str("<figcaption>");
                    esc(&caption, out);
                    out.push_str("</figcaption>");
                }
                out.push_str("</figure>\n");
            }
            None => {
                self.lost.insert(LOST_IMAGES.into());
                let words = [alt.trim(), caption.as_str()].into_iter().filter(|w| !w.is_empty()).collect::<Vec<_>>().join(": ");
                if !words.is_empty() {
                    out.push_str("<p class=\"image-missing\">[");
                    esc(&words, out);
                    out.push_str("]</p>\n");
                }
            }
        }
    }

    /// The headings down to the block's level, as links to them (a list without page numbers: the
    /// page they fall on is the reader's layout).
    fn contents(&mut self, e: &XmlElementRef, out: &mut String) {
        let levels = number(e.get_attribute(self.txn, "levels")).filter(|l| (1..=6).contains(l)).unwrap_or(3) as u32;
        let shown: Vec<(usize, &format::Heading)> = self.outline.headings.iter().enumerate().filter(|(_, h)| h.level <= levels && !h.text.is_empty()).collect();
        out.push_str("<nav class=\"contents\" aria-label=\"Contents\"><p class=\"contents-title\">Contents</p>");
        if !shown.is_empty() {
            let top = shown.iter().map(|(_, h)| h.level).min().unwrap_or(1);
            out.push_str("<ul>");
            for (n, h) in shown {
                let _ = write!(out, "<li class=\"l{}\"><a href=\"#{}\">", (h.level - top + 1).min(5), heading_id(h.anchor.as_deref(), n));
                esc(&h.text, out);
                out.push_str("</a></li>");
            }
            out.push_str("</ul>");
        }
        out.push_str("</nav>\n");
    }

    fn footnotes(&mut self, out: &mut String) {
        if self.outline.notes.is_empty() {
            return;
        }
        out.push_str("<section class=\"footnotes\" aria-label=\"Footnotes\"><ol>\n");
        // A reference inside a note is not one of the text's: it is not numbered.
        self.notes = usize::MAX / 2;
        for (n, body) in self.outline.notes.clone().into_iter().enumerate() {
            let _ = write!(out, "<li id=\"fn-{}\">", n + 1);
            if let Some(body) = body {
                self.children(&body, out, false);
            }
            out.push_str("</li>\n");
        }
        out.push_str("</ol></section>\n");
    }
}

/// The body of the document: the text, then its footnotes.
fn body<T: ReadTxn>(txn: &T, images: &dyn Fn(&str) -> Option<String>, origin: Option<&str>, lost: &mut BTreeSet<String>) -> String {
    let mut h = Html {
        txn,
        images,
        origin: format::origin(origin),
        outline: format::outline(txn),
        page: format::page(txn),
        headings: 0,
        notes: 0,
        lost: BTreeSet::new(),
    };
    let mut out = String::new();
    if let Some(f) = txn.get_xml_fragment(FRAGMENT) {
        for c in f.children(txn) {
            h.node(c, &mut out, false);
        }
    }
    h.footnotes(&mut out);
    lost.append(&mut h.lost);
    out
}

/// See [`super::html_with`].
pub(super) fn fragment(doc: &Doc, images: &dyn Fn(&str) -> Option<String>) -> String {
    body(&doc.transact(), images, None, &mut BTreeSet::new())
}

/// A CSS string: quotes, backslashes, `<`, `>`, `&` and control characters as escapes, so it can
/// neither end itself nor the `<style>` it is in.
fn css_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\'' | '\\' | '<' | '>' | '&' => {
                let _ = write!(out, "\\{:x} ", c as u32);
            }
            '\n' => out.push_str("\\a "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A header's or footer's lines as the margin boxes of one side of the page: each paragraph in
/// the box of its alignment, the page number and count as the printer's counters, one style a
/// line (that of its first words).
fn margin_boxes<T: ReadTxn>(txn: &T, side: &str, region: Option<&str>, title: &str, date: &str, lost: &mut BTreeSet<String>) -> String {
    #[derive(PartialEq, Clone)]
    struct Style {
        bold: bool,
        italic: bool,
        underline: bool,
        size: Option<f64>,
        color: Option<String>,
        family: Option<String>,
    }
    let paras = region.and_then(|r| format::region(txn, r)).unwrap_or_default();
    // (box, content, style of the first words)
    let mut lines: Vec<(&str, String, Option<Style>)> = Vec::new();
    for p in &paras {
        let align = match format::text_align(any(p.get_attribute(txn, "textAlign")).as_ref()) {
            Some("center") => "center",
            Some("right") => "right",
            _ => "left",
        };
        let mut parts: Vec<String> = Vec::new();
        let mut styles: Vec<Style> = Vec::new();
        let none = Style { bold: false, italic: false, underline: false, size: None, color: None, family: None };
        for c in p.children(txn) {
            match c {
                XmlOut::Text(t) => {
                    for chunk in t.diff(txn, YChange::identity) {
                        let Out::Any(Any::String(s)) = chunk.insert else { continue };
                        if s.is_empty() {
                            continue;
                        }
                        let a = chunk.attributes.as_deref();
                        let st = mark(a, "textStyle");
                        styles.push(Style {
                            bold: mark(a, "bold").is_some(),
                            italic: mark(a, "italic").is_some(),
                            underline: mark(a, "underline").is_some(),
                            size: format::font_size(mark_field(st, "fontSize")),
                            color: format::color(mark_field(st, "color")),
                            family: format::font_family(mark_field(st, "fontFamily")),
                        });
                        let mut q = String::new();
                        css_string(&s, &mut q);
                        parts.push(q);
                    }
                }
                XmlOut::Element(e) if &**e.tag() == "pageField" => {
                    let mut q = String::new();
                    parts.push(match format::field(any(e.get_attribute(txn, "kind")).as_ref()) {
                        "page" => "counter(page)".into(),
                        "pages" => "counter(pages)".into(),
                        "date" => {
                            css_string(date, &mut q);
                            q
                        }
                        _ => {
                            css_string(title, &mut q);
                            q
                        }
                    });
                    styles.push(none.clone());
                }
                XmlOut::Element(e) if &**e.tag() == "hardBreak" => parts.push("\"\\a \"".into()),
                XmlOut::Element(e) => {
                    let mut q = String::new();
                    css_string(&format::text_of(&e, txn), &mut q);
                    parts.push(q);
                }
                XmlOut::Fragment(_) => {}
            }
        }
        if styles.iter().any(|s| *s != styles[0]) {
            lost.insert("styles within a header or footer line (each line prints in the style of its first words)".into());
        }
        lines.push((align, parts.join(" "), styles.into_iter().next()));
    }
    // A blank line at the end is no line.
    while lines.last().is_some_and(|l| l.1.is_empty()) {
        lines.pop();
    }
    let mut css = String::new();
    for b in ["left", "center", "right"] {
        let mine: Vec<&(&str, String, Option<Style>)> = lines.iter().filter(|l| l.0 == b).collect();
        if mine.is_empty() {
            let _ = write!(css, "@{side}-{b}{{content:none}}");
            continue;
        }
        let content = mine.iter().map(|l| if l.1.is_empty() { "\"\"".to_string() } else { l.1.clone() }).collect::<Vec<_>>().join(" \"\\a \" ");
        let first = mine.iter().find_map(|l| l.2.clone());
        let st = first.unwrap_or(Style { bold: false, italic: false, underline: false, size: None, color: None, family: None });
        let _ = write!(
            css,
            "@{side}-{b}{{content:{content};white-space:pre-wrap;vertical-align:middle;font-size:{}pt;color:{};font-weight:{};font-style:{}",
            st.size.unwrap_or(9.0),
            st.color.as_deref().unwrap_or("#555555"),
            if st.bold { 700 } else { 400 },
            if st.italic { "italic" } else { "normal" }
        );
        if st.underline {
            css.push_str(";text-decoration:underline");
        }
        // A margin box does not inherit the body's font: the text's own, else the document's.
        let family = st.family.map_or_else(|| BODY_FONT.to_string(), |f| format::font_stack(&f));
        let _ = write!(css, ";font-family:{family}");
        css.push('}');
    }
    css
}

/// See [`super::html_page_export`].
pub(super) fn page(title: &str, doc: &Doc, origin: Option<&str>, images: &dyn Fn(&str) -> Option<String>) -> (String, BTreeSet<String>) {
    let txn = doc.transact();
    let mut lost = BTreeSet::new();
    let html = body(&txn, images, origin, &mut lost);
    let p = format::page(&txn);
    let inches = |tw: u32| format!("{}in", (f64::from(tw) / 1440.0 * 10000.0).round() / 10000.0);
    let date = chrono::Utc::now().format("%-d %B %Y").to_string();
    let boxes = |header: &str, footer: &str, lost: &mut BTreeSet<String>| {
        format!("{}{}", margin_boxes(&txn, "top", Some(header), title, &date, lost), margin_boxes(&txn, "bottom", Some(footer), title, &date, lost))
    };
    let mut at_page = format!(
        "@page{{size:{} {};margin:{} {} {} {};{}}}",
        inches(p.width),
        inches(p.height),
        inches(p.top),
        inches(p.right),
        inches(p.bottom),
        inches(p.left),
        boxes("header", "footer", &mut lost)
    );
    if p.different_first {
        let _ = write!(at_page, "@page :first{{{}}}", boxes("firstHeader", "firstFooter", &mut lost));
    }
    let mut t = String::new();
    esc(title, &mut t);
    let page = format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{t}</title><style>\
         {at_page}\
         body{{font:11pt/1.55 {BODY_FONT};color:#1f2328;max-width:{}px;margin:0 auto}}\
         h1,h2,h3,h4,h5,h6{{line-height:1.25;break-after:avoid}}pre{{background:#f6f8fa;padding:.75em;border-radius:6px;overflow-x:auto}}code{{font-family:ui-monospace,monospace}}\
         .title{{font-size:26pt;line-height:1.15;margin:0 0 .2em}}.subtitle{{font-size:15pt;color:#57606a;margin:0 0 .8em}}\
         blockquote{{border-left:3px solid #d0d7de;margin-left:0;padding-left:1em;color:#57606a}}\
         .callout{{color:inherit;padding:.5em 1em;border-radius:4px}}.callout-note{{background:#e7f0fb;border-color:#0969da}}.callout-tip{{background:#e6f4ea;border-color:#1a7f37}}\
         .callout-warning{{background:#fff4e0;border-color:#bf8700}}.callout-alert{{background:#fde8e8;border-color:#cf222e}}\
         table{{border-collapse:collapse;width:100%;table-layout:fixed}}th,td{{border:1px solid #d0d7de;padding:.3em .6em;vertical-align:top}}\
         th{{background:#f6f8fa;text-align:left}}tr,figure,.footnotes li{{break-inside:avoid}}td>p,th>p,li>div>p,.footnotes li>p{{margin:0}}ul.tasks{{list-style:none;padding-left:.2em}}\
         ul.tasks li{{display:flex;gap:.5em;align-items:baseline}}mark{{background:#fff3b0;color:inherit}}\
         figure{{margin:.6em 0}}figure.align-center{{text-align:center}}figure.align-right{{text-align:right}}\
         figure.wrap-left{{float:left;margin:0 1em .5em 0;max-width:50%}}figure.wrap-right{{float:right;margin:0 0 .5em 1em;max-width:50%}}\
         figcaption{{font-size:9pt;color:#57606a;margin-top:.3em}}\
         img{{max-width:100%;height:auto}}.image-missing{{color:#57606a;font-style:italic}}\
         .page-break{{break-after:page;clear:both}}@media screen{{.page-break{{border-top:1px dashed #d0d7de;margin:1.5em 0}}}}\
         .contents{{margin:1em 0}}.contents-title{{font-weight:600;margin:0 0 .3em}}.contents ul{{list-style:none;padding:0;margin:0}}\
         .contents a{{color:inherit;text-decoration:none}}.contents .l2{{padding-left:1.5em}}.contents .l3{{padding-left:3em}}.contents .l4{{padding-left:4.5em}}.contents .l5{{padding-left:6em}}\
         .fn-ref a{{text-decoration:none}}.footnotes{{clear:both;border-top:1px solid #d0d7de;margin-top:2em;font-size:9pt}}\
         ins{{color:#1a7f37;text-decoration:underline}}del{{color:#cf222e}}@media screen{{body{{padding:1.5em}}}}\
         </style></head><body>\n{html}</body></html>\n",
        p.text_width() / 15
    );
    drop(txn);
    (page, lost)
}

#[cfg(test)]
mod tests {
    use super::super::format::fixtures::{everything, hostile};
    use super::super::html_page_export;

    fn uri(id: &str) -> Option<String> {
        (id == "img1" || id == "img2").then(|| format!("data:image/png;base64,{id}"))
    }

    #[test]
    fn a_formatted_document_prints_as_the_editor_prints() {
        let (page, lost) = html_page_export("Annual report 2026", &everything(), Some("https://acme.example.com"), &uri);
        assert_eq!(lost, vec!["styles within a header or footer line (each line prints in the style of its first words)".to_string()]);
        for want in [
            "@page{size:11in 8.5in;margin:1in 1in 1in 1in;@top-left{content:none}@top-center{content:none}@top-right{content:\"Annual report: \" \"Annual report 2026\";",
            "font-weight:700",
            "@bottom-center{content:\"Page \" counter(page) \" of \" counter(pages);",
            "@page :first{@top-left{content:none}@top-center{content:\"CONFIDENTIAL\";white-space:pre-wrap;vertical-align:middle;font-size:12pt;color:#cf222e;",
            "@page :first{@top-left{content:none}@top-center{content:\"CONFIDENTIAL\"",
            "max-width:864px",
            "<p class=\"title\">Annual report 2026</p>",
            "<p class=\"subtitle\">Prepared for the board</p>",
            "<nav class=\"contents\" aria-label=\"Contents\"><p class=\"contents-title\">Contents</p><ul><li class=\"l1\"><a href=\"#h_intro01\">Introduction</a></li><li class=\"l2\"><a href=\"#heading-2\">Heading level 2</a></li><li class=\"l3\"><a href=\"#heading-3\">Heading level 3</a></li><li class=\"l2\"><a href=\"#heading-7\">After the page break</a></li></ul></nav>",
            "<h1 id=\"h_intro01\">Introduction</h1>",
            "<h6 id=\"heading-6\">Heading level 6</h6>",
            "<p style=\"text-align:justify;line-height:1.5;margin-top:6pt;margin-bottom:12pt;margin-inline-start:36pt\">",
            "<span style=\"font-family:Georgia, &#39;Times New Roman&#39;, serif\">Georgia</span>",
            "<span style=\"font-family:ui-monospace, SFMono-Regular, Menlo, Consolas, monospace\">, mono</span>",
            "<span style=\"font-size:18pt\">, 18 pt</span>",
            "<span style=\"color:#cf222e\">, red</span>",
            "<mark style=\"background-color:#a0e7ff\">, sky highlight</mark>",
            "<mark>, yellow highlight</mark>",
            "<sub>2</sub>",
            "<sup>2</sup>",
            "<u>, underlined</u>",
            "<sup class=\"fn-ref\"><a href=\"#fn-1\" id=\"fnref-1\">1</a></sup> and the text",
            "<a href=\"#h_intro01\">introduction</a>",
            "<a href=\"https://acme.example.com/docs/d/0192f0c4-1c2b-7c3d-8e4f-5a6b7c8d9e0f\" rel=\"noopener noreferrer nofollow\">budget document</a>",
            "<a href=\"https://example.com/a?b=1&amp;c=2\" rel=\"noopener noreferrer nofollow\">the web</a>",
            "<a href=\"https://acme.example.com/open/Customer/c-42\" rel=\"noopener noreferrer nofollow\">@Acme Foods</a> asked @Priya Nair</p>",
            "<ul style=\"list-style-type:square\"><li><p>A square bullet</p>\n<ul style=\"list-style-type:circle\">",
            "<ol type=\"a\" start=\"3\">",
            "<ol type=\"i\">",
            "<blockquote class=\"callout callout-warning\">",
            "<table style=\"width:480px\"><colgroup><col style=\"width:200px\"><col style=\"width:120px\"><col style=\"width:160px\"></colgroup>",
            "<thead><tr><th><p>Region</p>\n</th>\n<th colspan=\"2\"><p>Revenue (two columns)</p>\n</th>\n</tr>\n</thead><tbody><tr><td rowspan=\"2\" style=\"background-color:#fde8e8\">",
            "<figure class=\"image align-right\"><img src=\"data:image/png;base64,img1\" alt=\"A blue chart\" width=\"240\" height=\"120\"><figcaption>Figure 1: sales by region</figcaption></figure>",
            "<figure class=\"image wrap-left\"><img src=\"data:image/png;base64,img2\" alt=\"A green square\" width=\"96\"></figure>",
            "<div class=\"page-break\"></div>",
            "<section class=\"footnotes\" aria-label=\"Footnotes\"><ol>\n<li id=\"fn-1\"><p>The first note, with <a href=\"https://example.com/note\" rel=\"noopener noreferrer nofollow\">a link</a></p>\n</li>\n<li id=\"fn-2\"><p>The second note.</p>\n</li>\n</ol></section>",
        ] {
            assert!(page.contains(want), "missing {want}\nin {page}");
        }
        assert!(!page.contains("Heading level 4</a>"), "the contents stop at its level");
        // A document nobody set a page for prints as it always has: A4, 2 cm.
        let (legacy, _) = html_page_export("x", &super::super::new_doc(), None, &|_| None);
        assert!(legacy.contains("@page{size:8.2681in 11.6931in;margin:0.7875in 0.7875in 0.7875in 0.7875in;"), "{legacy}");
        assert!(!legacy.contains("@page :first"));
    }

    #[test]
    fn hostile_values_never_reach_the_page() {
        let (page, lost) = html_page_export("</title><script>alert(1)</script>", &hostile(), None, &uri);
        for l in ["unknown node chart3d", "unknown mark sparkle", "content from a newer version of Docs (format 3)"] {
            assert!(lost.iter().any(|x| x == l), "{l} in {lost:?}");
        }
        for bad in [
            "<script",
            "url(",
            "expression(",
            "javascript:",
            "data:text",
            "x:y",
            "onclick",
            "onmouseover",
            "onload",
            "9999",
            "1e12",
            "display:none",
            "/docs/d/..",
            "position:fixed",
        ] {
            assert!(!page.contains(bad), "{bad} in {page}");
        }
        assert_eq!(page.matches("</style>").count(), 1, "nothing stored closes the style: {page}");
        assert_eq!(page.matches("</title>").count(), 1, "{page}");
        // The header's words are a CSS string with `<`, `>`, quotes and backslashes escaped.
        assert!(page.contains("@top-left{content:\"\\3c /style\\3e \\3c script\\3e alert(1)\\3c /script\\3e \\22 \\27 \\5c \";"), "{page}");
        assert!(page.contains("@page{size:8.2681in 11.6931in;margin:0.7875in 0.7875in 0.7875in 0.7875in;"), "an absurd page is the legacy one: {page}");
        assert!(page.contains("<p><mark>styled</mark> js data up sparkle"), "unchecked styles and links are dropped, the words stay: {page}");
        assert!(page.contains("<h6 id=\"heading-1\">Heading</h6>"), "{page}");
        assert!(page.contains("<ul><li>") && page.contains("<ol><li>"), "unknown list styles are the default ones: {page}");
        assert!(
            page.contains(
                "<figure class=\"image align-center\"><img src=\"data:image/png;base64,img1\" alt=\"&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;\">"
            ),
            "{page}"
        );
        assert!(page.contains("<figcaption>&lt;/figcaption&gt;&lt;script&gt;alert(1)&lt;/script&gt;</figcaption>"), "{page}");
        assert!(page.contains("words of an unknown node"), "{page}");
        assert!(page.contains("<li id=\"fn-1\"></li>") && page.contains("<li id=\"fn-2\"></li>") && !page.contains("nothing refers to"), "{page}");
    }
}
