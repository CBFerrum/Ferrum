//! Rich text people wrote in the interface's editor (the kit's `RichTextEditor`), as the server accepts it: an allow-list as
//! strict as the kit's own (`src/ui/richtext/sanitize.ts` in Ferrum-Frontend), applied here whatever the browser sent, and the
//! plain-text form a mail's text part carries. Elements: paragraphs, headings 1 to 4, quote, code, the inline marks, lists,
//! links, images, tables, rules. Attributes: `dir` everywhere, `style` as validated colour, size, family, alignment and width
//! only, a link's address (http, https, mailto) and `target="_blank"`, an image's https address, size and description, list
//! and cell numbers. No classes, ids, data attributes, event handlers, relative addresses or `data:` images.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

const TAGS: &str =
    "a b blockquote br code col colgroup del div em h1 h2 h3 h4 hr i img li mark ol p pre s span strike strong sub sup table tbody td tfoot th thead tr u ul";
/// Removed with everything inside them (anything else not allowed is unwrapped, its text kept).
const DROP: &str = "script style title textarea select option template noscript iframe frame frameset object embed applet svg math button form input video audio canvas head meta link base xml";

static COLOR_HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$").unwrap());
static COLOR_RGB: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^rgba?\(\s*(\d{1,3})\s*[,\s]\s*(\d{1,3})\s*[,\s]\s*(\d{1,3})\s*(?:[,/]\s*(\d*\.?\d+%?)\s*)?\)$").unwrap());
static SIZE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{1,3}(?:\.\d+)?)\s*(px|pt)$").unwrap());
static FONT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\p{L}\p{N} _-]{1,40}$").unwrap());
static WIDTH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{1,4}(\.\d+)?(px|%)$").unwrap());
static MIN_WIDTH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{1,4}(\.\d+)?px$").unwrap());
const NAMED: &[&str] = &[
    "black", "white", "silver", "gray", "grey", "maroon", "red", "purple", "fuchsia", "green", "lime", "olive", "yellow", "navy", "blue", "teal", "aqua",
    "orange",
];

/// A colour kept as written when it is a hex value, an opaque-enough `rgb()`/`rgba()` or a basic name; `None` otherwise.
fn color(v: &str) -> Option<String> {
    let v = v.trim().to_ascii_lowercase();
    if COLOR_HEX.is_match(&v) {
        return (!(v.len() == 9 && &v[7..] == "00")).then_some(v);
    }
    if let Some(c) = COLOR_RGB.captures(&v) {
        let ok = (1..=3).all(|i| c[i].parse::<u16>().is_ok_and(|n| n <= 255));
        let alpha = c
            .get(4)
            .map(|a| a.as_str())
            .map(|a| a.strip_suffix('%').map_or_else(|| a.parse::<f32>().unwrap_or(0.0), |p| p.parse::<f32>().unwrap_or(0.0) / 100.0))
            .unwrap_or(1.0);
        return (ok && alpha > 0.0).then_some(v);
    }
    NAMED.contains(&v.as_str()).then_some(v)
}

/// One style declaration kept, its value checked against what the property may hold.
fn declaration(name: &str, value: &str) -> Option<String> {
    let value = value.trim();
    let value = value.strip_suffix("!important").unwrap_or(value).trim();
    let lower = value.to_ascii_lowercase();
    if value.contains(['\\', '<', '>', '@']) || lower.contains("/*") || lower.contains("url(") || lower.contains("expression") {
        return None;
    }
    let kept = match name {
        "color" | "background-color" => color(value)?,
        "font-size" => {
            let c = SIZE.captures(&lower)?;
            let n: f64 = c[1].parse().ok()?;
            let px = (if &c[2] == "pt" { n * 4.0 / 3.0 } else { n }).round();
            if !(8.0..=96.0).contains(&px) {
                return None;
            }
            format!("{px}px")
        }
        "font-family" => {
            let names: Vec<&str> = value.split(',').map(|n| n.trim().trim_matches(|c| c == '"' || c == '\'').trim()).collect();
            if names.is_empty()
                || names.len() > 4
                || !names
                    .iter()
                    .all(|n| FONT.is_match(n) && !matches!(n.to_ascii_lowercase().as_str(), "url" | "expression" | "import" | "inherit" | "initial"))
            {
                return None;
            }
            names.iter().map(|n| if n.contains(' ') { format!("\"{n}\"") } else { (*n).to_string() }).collect::<Vec<_>>().join(", ")
        }
        "text-align" => matches!(lower.as_str(), "left" | "right" | "center" | "justify" | "start" | "end").then_some(lower)?,
        "font-weight" => (matches!(lower.as_str(), "normal" | "bold")
            || (lower.len() == 3 && lower.ends_with("00") && lower.starts_with(|c: char| ('1'..='9').contains(&c))))
        .then_some(lower)?,
        "font-style" => matches!(lower.as_str(), "normal" | "italic").then_some(lower)?,
        "text-decoration" => matches!(lower.as_str(), "none" | "underline" | "line-through").then_some(lower)?,
        "width" => WIDTH.is_match(&lower).then_some(lower)?,
        "min-width" => MIN_WIDTH.is_match(&lower).then_some(lower)?,
        _ => return None,
    };
    Some(format!("{name}: {kept}"))
}

/// A style attribute as only the declarations [`declaration`] keeps; empty when none is.
pub fn clean_style(style: &str) -> String {
    style
        .split(';')
        .filter_map(|d| {
            let (name, value) = d.split_once(':')?;
            declaration(&name.trim().to_ascii_lowercase(), value)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn number(v: &str, min: u32, max: u32) -> Option<Cow<'_, str>> {
    let n: u32 = v.trim().parse().ok().filter(|_| v.trim().len() <= 5)?;
    (min..=max).contains(&n).then(|| Cow::Owned(n.to_string()))
}

static CLEANER: LazyLock<ammonia::Builder<'static>> = LazyLock::new(|| {
    let mut b = ammonia::Builder::default();
    let attrs: HashMap<&str, HashSet<&str>> = [
        ("a", &["href", "target", "title"][..]),
        ("img", &["src", "alt", "title", "width", "height"]),
        ("ol", &["start", "type"]),
        ("td", &["colspan", "rowspan"]),
        ("th", &["colspan", "rowspan"]),
    ]
    .into_iter()
    .map(|(t, a)| (t, a.iter().copied().collect()))
    .collect();
    b.tags(TAGS.split(' ').collect())
        .clean_content_tags(DROP.split(' ').collect())
        .generic_attributes(["dir", "style"].into_iter().collect())
        .tag_attributes(attrs)
        .url_schemes(["http", "https", "mailto"].into_iter().collect())
        .url_relative(ammonia::UrlRelative::Deny)
        .link_rel(Some("noopener noreferrer nofollow"))
        .strip_comments(true)
        .attribute_filter(|element, attribute, value| match attribute {
            "style" => Some(clean_style(value)).filter(|s| !s.is_empty()).map(Cow::Owned),
            "dir" => matches!(value.to_ascii_lowercase().as_str(), "ltr" | "rtl" | "auto").then(|| Cow::Owned(value.to_ascii_lowercase())),
            "target" => (value == "_blank").then_some(Cow::Borrowed(value)),
            "src" => (element == "img" && value.trim().get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://"))).then_some(Cow::Borrowed(value)),
            "width" | "height" => number(value, 1, 4000),
            "colspan" | "rowspan" => number(value, 1, 100),
            "start" => number(value, 1, 99_999),
            "type" => matches!(value, "1" | "a" | "A" | "i" | "I").then_some(Cow::Borrowed(value)),
            "title" | "alt" => Some(Cow::Owned(value.chars().take(300).collect())),
            _ => Some(Cow::Borrowed(value)),
        });
    b
});

/// Rich text from a browser as the server keeps and sends it (see the module's documentation).
pub fn sanitize_html(html: &str) -> String {
    CLEANER.clean(html).to_string()
}

static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?s)<(/?)([a-zA-Z][a-zA-Z0-9]*)((?:[^>"']|"[^"]*"|'[^']*')*)>|<!--.*?-->"#).unwrap());
static HREF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\bhref\s*=\s*"([^"]*)""#).unwrap());
static ALT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\balt\s*=\s*"([^"]*)""#).unwrap());
static START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\bstart\s*=\s*"(\d{1,5})""#).unwrap());
static ENTITY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[a-zA-Z]{2,8});").unwrap());

fn decode(s: &str) -> String {
    ENTITY
        .replace_all(s, |c: &regex::Captures| {
            let e = &c[1];
            let ch = if let Some(hex) = e.strip_prefix("#x").or_else(|| e.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(dec) = e.strip_prefix('#') {
                dec.parse().ok().and_then(char::from_u32)
            } else {
                match e {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    _ => None,
                }
            };
            ch.map(String::from).unwrap_or_else(|| c[0].to_string())
        })
        .into_owned()
}

/// The plain-text part of a mail made from its HTML: paragraphs and line breaks kept, lists as `- ` and `1. ` lines, a link's
/// address after its words, a quote's lines with `> `, an image by its description, a rule as a line of dashes. Read from
/// [`sanitize_html`]'s output (or the server's own escaped markup); markup it does not know is dropped, its text kept.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut lists: Vec<Option<u32>> = Vec::new(); // a number for an ordered list, None for bullets
    let mut links: Vec<(usize, String)> = Vec::new();
    let mut quote = 0usize;
    let mut skip = 0usize;
    let mut pre = 0usize;
    let mut at = 0;
    let newline = |out: &mut String, quote: usize, n: usize| {
        // A line that holds only a quote's mark is already the start of a line.
        if quote > 0 && out.ends_with(&"> ".repeat(quote)) && out[..out.len() - 2 * quote].ends_with('\n') {
            return;
        }
        while out.ends_with(' ') {
            out.pop();
        }
        let have = out.chars().rev().take_while(|c| *c == '\n').count();
        if out.is_empty() {
            return;
        }
        for _ in have..n {
            out.push('\n');
        }
        if quote > 0 {
            out.push_str(&"> ".repeat(quote));
        }
    };
    let mut text = |out: &mut String, s: &str, pre: bool| {
        let s = decode(s);
        if pre {
            out.push_str(&s);
            return;
        }
        let mut space = out.is_empty() || out.ends_with([' ', '\n']);
        for ch in s.chars() {
            if ch.is_whitespace() && ch != '\u{a0}' {
                if !space {
                    out.push(' ');
                    space = true;
                }
            } else {
                out.push(if ch == '\u{a0}' { ' ' } else { ch });
                space = false;
            }
        }
    };
    for m in TAG.captures_iter(html) {
        let whole = m.get(0).unwrap();
        if skip == 0 {
            text(&mut out, &html[at..whole.start()], pre > 0);
        }
        at = whole.end();
        let Some(name) = m.get(2) else { continue }; // a comment
        let name = name.as_str().to_ascii_lowercase();
        let closing = &m[1] == "/";
        let attrs = m.get(3).map_or("", |a| a.as_str());
        if DROP.split(' ').any(|d| d == name) {
            if closing {
                skip = skip.saturating_sub(1);
            } else if !attrs.trim_end().ends_with('/') {
                skip += 1;
            }
            continue;
        }
        if skip > 0 {
            continue;
        }
        match (name.as_str(), closing) {
            ("br", _) => newline(&mut out, quote, 1),
            // A list item's paragraph is the item's line: no break after its marker, none of its own at its end.
            ("p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6", _) if !lists.is_empty() => {
                if !closing && !out.ends_with(". ") && !out.ends_with("- ") {
                    newline(&mut out, quote, 1);
                }
            }
            ("p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6", _) => newline(&mut out, quote, if quote > 0 { 1 } else { 2 }),
            ("div" | "tr" | "table" | "pre", _) => {
                newline(&mut out, quote, 1);
                if name == "pre" {
                    pre = if closing { pre.saturating_sub(1) } else { pre + 1 };
                }
            }
            ("hr", _) => {
                newline(&mut out, quote, 1);
                out.push_str("----------");
                newline(&mut out, quote, 1);
            }
            ("blockquote", false) => {
                newline(&mut out, quote, 2);
                quote += 1;
                if !out.ends_with("> ") {
                    out.push_str(&"> ".repeat(quote));
                }
            }
            ("blockquote", true) => {
                if out.ends_with("\n> ") {
                    out.truncate(out.len() - 2);
                }
                quote = quote.saturating_sub(1);
                newline(&mut out, quote, 2);
            }
            ("ul", false) => lists.push(None),
            ("ol", false) => lists.push(Some(START.captures(attrs).and_then(|c| c[1].parse().ok()).unwrap_or(1))),
            ("ul" | "ol", true) => {
                lists.pop();
                newline(&mut out, quote, 1);
            }
            ("li", false) => {
                newline(&mut out, quote, 1);
                out.push_str(&"  ".repeat(lists.len().saturating_sub(1)));
                match lists.last_mut() {
                    Some(Some(n)) => {
                        out.push_str(&format!("{n}. "));
                        *n += 1;
                    }
                    _ => out.push_str("- "),
                }
            }
            ("td" | "th", true) => out.push('\t'),
            ("a", false) => links.push((out.len(), HREF.captures(attrs).map(|c| decode(&c[1])).unwrap_or_default())),
            ("a", true) => {
                if let Some((from, href)) = links.pop() {
                    let words = out.get(from..).unwrap_or("").trim().to_string();
                    let shown = href.strip_prefix("mailto:").unwrap_or(&href).to_string();
                    if !shown.is_empty() && words != shown && words != href {
                        out.push_str(&format!(" <{shown}>"));
                    }
                }
            }
            ("img", _) => {
                if let Some(alt) = ALT.captures(attrs).map(|c| decode(&c[1])).filter(|a| !a.trim().is_empty()) {
                    out.push_str(&format!("[{}]", alt.trim()));
                }
            }
            _ => {}
        }
    }
    if skip == 0 {
        text(&mut out, &html[at..], pre > 0);
    }
    // Trailing spaces and tabs off each line, no run of more than one empty line, none at either end.
    let lines: Vec<&str> = out.lines().map(|l| l.trim_end_matches([' ', '\t'])).collect();
    let mut tidy = String::new();
    let mut empty = 0;
    for l in lines {
        let blank = l.trim().is_empty() || l.trim() == ">";
        empty = if blank { empty + 1 } else { 0 };
        if empty <= 1 {
            tidy.push_str(if blank { l.trim_end_matches(' ') } else { l });
            tidy.push('\n');
        }
    }
    tidy.trim_matches('\n').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_what_the_editor_writes() {
        let html = r#"<h2 dir="auto">Hi</h2><p dir="auto">A <strong>bold</strong> <em>word</em>, <u>under</u>, <s>gone</s>, <span style="color: #c5221f; font-size: 18px">red</span> <mark style="background-color: #fff475">lit</mark>.</p><ol start="3"><li><p>one</p></li></ol><p style="text-align: center"><a href="https://example.com/x?a=1&amp;b=2" target="_blank">link</a></p><table><tbody><tr><td colspan="2"><p>cell</p></td></tr></tbody></table><hr><img src="https://example.com/a.png" alt="Logo" width="120">"#;
        let out = sanitize_html(html);
        for kept in [
            r#"<h2 dir="auto">Hi</h2>"#,
            "<strong>bold</strong>",
            "<em>word</em>",
            "<u>under</u>",
            "<s>gone</s>",
            r#"style="color: #c5221f; font-size: 18px""#,
            r#"style="background-color: #fff475""#,
            r#"<ol start="3">"#,
            r#"style="text-align: center""#,
            r#"href="https://example.com/x?a=1&amp;b=2""#,
            r#"target="_blank""#,
            r#"rel="noopener noreferrer nofollow""#,
            r#"<td colspan="2">"#,
            "<hr>",
            r#"src="https://example.com/a.png""#,
            r#"width="120""#,
        ] {
            assert!(out.contains(kept), "{kept} missing from {out}");
        }
        assert_eq!(sanitize_html(&out), out, "cleaning clean HTML gives it back");
    }

    #[test]
    fn refuses_hostile_input() {
        let cases = [
            (r#"<p>a<script>alert(1)</script>b</p>"#, "alert"),
            (r#"<img src=x onerror=alert(1)>"#, "onerror"),
            (r#"<a href="javascript:alert(1)">x</a>"#, "javascript"),
            (r#"<a href="JaVaScRiPt:alert(1)">x</a>"#, "alert"),
            (r#"<a href="&#106;avascript:alert(1)">x</a>"#, "alert"),
            (r#"<a href="vbscript:msgbox(1)">x</a>"#, "vbscript"),
            (r#"<a href="data:text/html,<script>alert(1)</script>">x</a>"#, "data:"),
            (r#"<img src="data:image/png;base64,AAAA">"#, "data:"),
            (r#"<img src="http://tracker.example/p.gif">"#, "tracker"),
            (r#"<a href="/api/admin/users">x</a>"#, "/api/admin"),
            (r#"<p style="background-image: url(https://evil.example/x)">x</p>"#, "url("),
            (r#"<p style="color: red; background: url(x)">x</p>"#, "background:"),
            (r#"<p style="color: expression(alert(1))">x</p>"#, "expression"),
            (r#"<p style="font-family: x;}</style><script>alert(1)</script>">x</p>"#, "<script"),
            (r#"<p style="position: fixed; top: 0">x</p>"#, "position"),
            (r#"<span style="font-size: 900px">x</span>"#, "900"),
            (r#"<p class="x" id="y" data-x="1">x</p>"#, "class"),
            (r#"<style>p{color:red}</style><p>x</p>"#, "color:red"),
            (r#"<iframe src="https://evil.example"></iframe>"#, "iframe"),
            (r#"<svg><script>alert(1)</script></svg>"#, "alert"),
            (r#"<math><mi xlink:href="javascript:alert(1)">x</mi></math>"#, "javascript"),
            (r#"<form action="https://evil.example"><input name=p></form>"#, "form"),
            (r#"<object data="x.swf"></object>"#, "object"),
            (r#"<base href="https://evil.example/">"#, "base"),
            (r#"<meta http-equiv="refresh" content="0;url=https://evil.example">"#, "refresh"),
            (r#"<p dir="rtl;color:red">x</p>"#, "color"),
            (r#"<ol type="disc" start="-1"><li>x</li></ol>"#, "disc"),
            (r#"<td colspan="1000000">x</td>"#, "1000000"),
            (r#"<!-- <img src=x onerror=alert(1)> --><p>x</p>"#, "onerror"),
            (r#"<noscript><p title="</noscript><img src=x onerror=alert(1)>"></p></noscript>"#, "onerror"),
            (r#"<a href="https://ok.example" target="_top">x</a>"#, "_top"),
        ];
        for (input, banned) in cases {
            let out = sanitize_html(input);
            assert!(!out.to_ascii_lowercase().contains(&banned.to_ascii_lowercase()), "{input} → {out} still holds {banned}");
        }
        // What was allowed in the same attribute survives a refused neighbour.
        assert_eq!(sanitize_html(r#"<p style="color: red; background: url(x)">x</p>"#), r#"<p style="color: red">x</p>"#);
        assert_eq!(sanitize_html(r#"<span style="font-size: 12pt">x</span>"#), r#"<span style="font-size: 16px">x</span>"#);
        assert_eq!(sanitize_html("<p>a<script>b</script>c</p>"), "<p>ac</p>");
        assert_eq!(sanitize_html("<p>a<blink>b</blink>c</p>"), "<p>abc</p>");
    }

    #[test]
    fn clean_style_validates_each_value() {
        assert_eq!(
            clean_style("color: RGB(1, 2, 3); font-family: Georgia, 'Times New Roman', serif"),
            r#"color: rgb(1, 2, 3); font-family: Georgia, "Times New Roman", serif"#
        );
        assert_eq!(clean_style("color: #00000000; color: transparent; background-color: inherit"), "");
        assert_eq!(clean_style("font-family: \"a\\\"b\""), "");
        assert_eq!(clean_style("text-align: center !important; width: 50%"), "text-align: center; width: 50%");
    }

    #[test]
    fn plain_text_of_a_message() {
        let html = sanitize_html(
            r#"<p>Hello <strong>Priya</strong>,</p><p>Invoice &amp; terms:</p><ol><li><p>pay</p></li><li><p>reply</p></li></ol><ul><li><p>one</p><ul><li><p>nested</p></li></ul></li></ul><blockquote><p>quoted</p></blockquote><p>See <a href="https://example.com">our terms</a> or <a href="mailto:ap@example.com">ap@example.com</a>.</p><hr><p>Bye<br>Ana</p><img src="https://example.com/l.png" alt="Logo">"#,
        );
        assert_eq!(
            html_to_text(&html),
            "Hello Priya,\n\nInvoice & terms:\n\n1. pay\n2. reply\n- one\n  - nested\n\n> quoted\n\nSee our terms <https://example.com> or ap@example.com.\n\n----------\n\nBye\nAna\n\n[Logo]"
        );
        assert_eq!(html_to_text("<p>Please find Sales Invoice &lt;INV-1&gt; attached.</p>"), "Please find Sales Invoice <INV-1> attached.");
        assert_eq!(html_to_text("<p>a<script>secret()</script>b</p>"), "ab");
    }
}
