//! An app's description (the manifest's `description`, a Markdown file in the bundle, usually its
//! README) for people who read it outside GitHub: the store's page shows it as HTML ([`to_html`]),
//! the install dialog as a plain excerpt ([`excerpt`]).
//!
//! READMEs are written for GitHub, with raw HTML, badges, pictures and diagrams. Both forms keep
//! the Markdown and drop the rest, so whatever a publisher writes, the result is safe to put in a
//! page: raw HTML never passes (only what the Markdown renderer generates, with text escaped),
//! images are dropped (the store shows the manifest's screenshots), Mermaid and other diagram
//! blocks go, and links are http(s), mailto, or the description's own headings. A relative link
//! resolves against the app's GitHub repository when its homepage is one, and is otherwise
//! dropped with its text kept.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};
use std::collections::HashMap;

/// Prefixed to the ids of headings (and to the links to them), so a description's ids never
/// clash with the page around it.
pub const ID_PREFIX: &str = "about-";

/// Fenced code languages that are diagrams drawn by a script the page does not run.
const DIAGRAMS: &[&str] = &["mermaid", "plantuml", "dot", "graphviz", "geojson", "topojson", "stl"];

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_GFM
}

/// The description as HTML for a page of its own: headings one level down (the page has its own
/// title) with ids under [`ID_PREFIX`], links checked, tables without inline styles (pages may
/// forbid them), GitHub alerts as `<blockquote class="markdown-alert-…">`.
pub fn to_html(markdown: &str, homepage: Option<&str>) -> String {
    let repository = homepage.and_then(github_repository);
    let events = unlink_missing_anchors(prune(safe_events(markdown, repository.as_deref(), true)));
    let mut out = String::with_capacity(markdown.len());
    pulldown_cmark::html::push_html(&mut out, events.into_iter());
    out
}

/// The description's opening prose as plain text, for a dialog: its paragraphs and list items,
/// separated by blank lines, until about `max` characters (cut at a word, with an ellipsis).
/// Headings, tables, code, quotes, HTML and images are skipped, and so are lines made only of
/// links (badges, a table of contents).
pub fn excerpt(markdown: &str, max: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut used = 0;
    // The block being read: its prefix, its text and how many letters or digits sit outside links.
    let mut current: Option<(&str, String, usize)> = None;
    let mut skip = 0usize;
    let mut in_link = 0usize;
    let flush = |current: &mut Option<(&str, String, usize)>, parts: &mut Vec<String>, used: &mut usize| {
        if let Some((prefix, text, outside)) = current.take() {
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if outside > 0 && !text.is_empty() {
                *used += text.chars().count() + 2;
                parts.push(format!("{prefix}{text}"));
            }
        }
    };
    for ev in safe_events(markdown, None, false) {
        if used >= max {
            break;
        }
        match ev {
            Event::Start(Tag::Heading { .. } | Tag::Table(_) | Tag::CodeBlock(_) | Tag::BlockQuote(_) | Tag::FootnoteDefinition(_)) => skip += 1,
            Event::End(TagEnd::Heading(_) | TagEnd::Table | TagEnd::CodeBlock | TagEnd::BlockQuote(_) | TagEnd::FootnoteDefinition) => {
                skip = skip.saturating_sub(1)
            }
            _ if skip > 0 => {}
            Event::Start(Tag::Item) => {
                flush(&mut current, &mut parts, &mut used);
                current = Some(("• ", String::new(), 0));
            }
            Event::Start(Tag::Paragraph) if current.is_none() => current = Some(("", String::new(), 0)),
            Event::End(TagEnd::Paragraph) => match &mut current {
                Some(("", ..)) => flush(&mut current, &mut parts, &mut used),
                Some((_, text, _)) => text.push(' '),
                None => {}
            },
            Event::End(TagEnd::Item) => flush(&mut current, &mut parts, &mut used),
            Event::Start(Tag::Link { .. }) => in_link += 1,
            Event::End(TagEnd::Link) => in_link = in_link.saturating_sub(1),
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, text, outside)) = &mut current {
                    text.push_str(&t);
                    if in_link == 0 {
                        *outside += t.chars().filter(|c| c.is_alphanumeric()).count();
                    }
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, text, _)) = &mut current {
                    text.push(' ');
                }
            }
            _ => {}
        }
    }
    flush(&mut current, &mut parts, &mut used);
    cut(&parts.join("\n\n"), max)
}

/// `https://github.com/{owner}/{repo}` when `homepage` is a GitHub repository's address (a
/// trailing `/` or `.git` allowed), else `None`.
pub fn github_repository(homepage: &str) -> Option<String> {
    let rest = homepage.trim().strip_prefix("https://github.com/")?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    let ok = |s: &str| !s.is_empty() && !s.starts_with('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (ok(owner) && ok(repo)).then(|| format!("https://github.com/{owner}/{repo}"))
}

/// The Markdown's events with everything unsafe or unshowable taken out (see the module).
/// `page`: headings get ids and move one level down, for [`to_html`].
fn safe_events<'a>(markdown: &'a str, repository: Option<&str>, page: bool) -> Vec<Event<'a>> {
    let events: Vec<Event<'a>> = Parser::new_ext(markdown, options()).collect();
    let mut out = Vec::with_capacity(events.len());
    // For each open link: whether its start was kept.
    let mut links: Vec<bool> = Vec::new();
    let mut slugs: HashMap<String, usize> = HashMap::new();
    let mut i = 0;
    while i < events.len() {
        let ev = events[i].clone();
        i += 1;
        match ev {
            Event::Html(_) | Event::InlineHtml(_) | Event::Start(Tag::HtmlBlock) | Event::End(TagEnd::HtmlBlock) => {}
            Event::Start(Tag::Image { .. }) => {
                i = past(&events, i, |e| matches!(e, Event::Start(Tag::Image { .. })), |e| matches!(e, Event::End(TagEnd::Image)))
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(ref info))) if is_diagram(info) => {
                i = past(&events, i, |_| false, |e| matches!(e, Event::End(TagEnd::CodeBlock)))
            }
            Event::Start(Tag::Link { link_type, dest_url, title, id }) => match href(link_type, &dest_url, repository) {
                Some(href) => {
                    links.push(true);
                    out.push(Event::Start(Tag::Link { link_type, dest_url: href.into(), title, id }));
                }
                None => links.push(false),
            },
            Event::End(TagEnd::Link) => {
                if links.pop().unwrap_or(false) {
                    out.push(ev);
                }
            }
            Event::Start(Tag::Heading { level, .. }) if page => {
                let text = heading_text(&events[i..]);
                let mut slug = slug(&text);
                let seen = slugs.entry(slug.clone()).or_insert(0);
                if *seen > 0 {
                    slug = format!("{slug}-{seen}");
                }
                *seen += 1;
                out.push(Event::Start(Tag::Heading { level: down(level), id: Some(format!("{ID_PREFIX}{slug}").into()), classes: vec![], attrs: vec![] }));
            }
            Event::End(TagEnd::Heading(level)) if page => out.push(Event::End(TagEnd::Heading(down(level)))),
            Event::Start(Tag::Table(alignments)) => out.push(Event::Start(Tag::Table(vec![Alignment::None; alignments.len()]))),
            ev => out.push(ev),
        }
    }
    out
}

/// The index just past the end that closes the block starting before `from` (nested starts of
/// the same kind counted).
fn past(events: &[Event], from: usize, start: impl Fn(&Event) -> bool, end: impl Fn(&Event) -> bool) -> usize {
    let mut depth = 1;
    for (k, e) in events.iter().enumerate().skip(from) {
        if start(e) {
            depth += 1;
        } else if end(e) {
            depth -= 1;
            if depth == 0 {
                return k + 1;
            }
        }
    }
    events.len()
}

fn is_diagram(info: &str) -> bool {
    let lang = info.split(|c: char| c.is_whitespace() || c == ',' || c == '{').next().unwrap_or("");
    DIAGRAMS.iter().any(|d| lang.eq_ignore_ascii_case(d))
}

/// A link's destination as the page may use it, or `None` to keep only its text.
fn href(kind: LinkType, dest: &str, repository: Option<&str>) -> Option<String> {
    let dest = dest.trim();
    if kind == LinkType::Email {
        // The renderer writes `mailto:` before an e-mail autolink's address.
        return Some(dest.to_string());
    }
    if let Some(fragment) = dest.strip_prefix('#') {
        return (!fragment.is_empty()).then(|| format!("#{ID_PREFIX}{fragment}"));
    }
    let lower = dest.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:") {
        return Some(dest.to_string());
    }
    // Any other scheme (javascript:, data:, …), a network path or a backslash trick: refused.
    let scheme = dest.split(['/', '?', '#']).next().is_some_and(|first| first.contains(':'));
    if scheme || dest.starts_with("//") || dest.contains('\\') || dest.is_empty() {
        return None;
    }
    let path = dest.trim_start_matches("./").trim_start_matches('/');
    let file = path.split(['?', '#']).next().unwrap_or("");
    if file.split('/').any(|c| c == ".." || c == ".") {
        return None;
    }
    repository.map(|repo| format!("{repo}/blob/HEAD/{path}"))
}

fn heading_text(events: &[Event]) -> String {
    let mut text = String::new();
    for e in events {
        match e {
            Event::End(TagEnd::Heading(_)) => break,
            Event::Text(t) | Event::Code(t) => text.push_str(t),
            _ => {}
        }
    }
    text
}

/// GitHub's heading anchors: lower case, letters, digits, `_` and `-` kept, spaces made `-`,
/// everything else dropped.
fn slug(text: &str) -> String {
    text.trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            c if c.is_alphanumeric() || c == '_' || c == '-' => Some(c),
            c if c.is_whitespace() => Some('-'),
            _ => None,
        })
        .collect()
}

fn down(level: HeadingLevel) -> HeadingLevel {
    match level {
        HeadingLevel::H1 => HeadingLevel::H2,
        HeadingLevel::H2 => HeadingLevel::H3,
        HeadingLevel::H3 => HeadingLevel::H4,
        HeadingLevel::H4 => HeadingLevel::H5,
        _ => HeadingLevel::H6,
    }
}

/// What dropping left empty: links with no content (a badge's), paragraphs of white space, and
/// headings with nothing under them before the next heading of their level (a section that held
/// only a diagram or pictures).
fn prune(mut events: Vec<Event>) -> Vec<Event> {
    loop {
        let before = events.len();
        let mut out: Vec<Event> = Vec::with_capacity(events.len());
        let mut k = 0;
        while k < events.len() {
            match (&events[k], events.get(k + 1)) {
                (Event::Start(Tag::Link { .. }), Some(Event::End(TagEnd::Link))) => k += 2,
                (Event::Start(Tag::Paragraph), _) => {
                    let end = past(&events, k + 1, |_| false, |e| matches!(e, Event::End(TagEnd::Paragraph)));
                    let blank = events[k + 1..end - 1].iter().all(|e| match e {
                        Event::Text(t) => t.trim().is_empty(),
                        Event::SoftBreak | Event::HardBreak => true,
                        _ => false,
                    });
                    if blank {
                        k = end;
                    } else {
                        out.push(events[k].clone());
                        k += 1;
                    }
                }
                (Event::Start(Tag::Heading { level, .. }), _) => {
                    let level = *level;
                    let end = past(&events, k + 1, |_| false, |e| matches!(e, Event::End(TagEnd::Heading(_))));
                    let empty = match events.get(end) {
                        None => true,
                        Some(Event::Start(Tag::Heading { level: next, .. })) => *next <= level,
                        _ => false,
                    };
                    if empty {
                        k = end;
                    } else {
                        out.push(events[k].clone());
                        k += 1;
                    }
                }
                _ => {
                    out.push(events[k].clone());
                    k += 1;
                }
            }
        }
        events = out;
        if events.len() == before {
            return events;
        }
    }
}

/// Links to headings that are not there (a section [`prune`] took out) keep only their text.
fn unlink_missing_anchors(events: Vec<Event>) -> Vec<Event> {
    let ids: std::collections::HashSet<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::Start(Tag::Heading { id: Some(id), .. }) => Some(id.to_string()),
            _ => None,
        })
        .collect();
    let mut kept: Vec<bool> = Vec::new();
    let mut out = Vec::with_capacity(events.len());
    for e in events {
        match &e {
            Event::Start(Tag::Link { dest_url, .. }) => {
                let keep = dest_url.strip_prefix('#').is_none_or(|id| ids.contains(id));
                kept.push(keep);
                if keep {
                    out.push(e);
                }
            }
            Event::End(TagEnd::Link) => {
                if kept.pop().unwrap_or(true) {
                    out.push(e);
                }
            }
            _ => out.push(e),
        }
    }
    out
}

/// `text` cut to about `max` characters at a word, with an ellipsis when cut.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    let head = match head.rfind(char::is_whitespace) {
        Some(at) if at > max / 2 => &head[..at],
        _ => head.as_str(),
    };
    format!("{}…", head.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '·' | '—' | '-')))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the Ferrum repositories' READMEs.
    const README: &str = r##"<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/banner-dark.svg">
  <img alt="Ferrum CRM" src="docs/assets/banner-light.svg" width="100%">
</picture>

[![CI](https://github.com/CBFerrum/ferrum-app-crm/actions/workflows/ci.yml/badge.svg)](https://github.com/CBFerrum/ferrum-app-crm/actions/workflows/ci.yml) ![Version](https://img.shields.io/badge/version-0.3.5-2f5bea)

**A native app for [Ferrum](https://github.com/CBFerrum/Ferrum), the open-source, AI-first business platform.**

[Overview](#overview) · [Screenshots](#screenshots) · [How it works](#how-it-works)

</div>

---

## Overview

Leads, a pipeline you can drag, quotations and every touchpoint, in one place beside the books.

| At a glance | |
|---|---|
| 📦 Version | `0.3.5` |

> [!NOTE]
> Its records are ordinary Ferrum records.

## Screenshots

<picture>
  <img alt="The pipeline." src="docs/assets/pipeline-light.webp" width="100%">
</picture>
<p align="center"><sub>The pipeline.</sub></p>

## How it works

### The pipeline

```mermaid
flowchart LR
    A --> B
```

### Inside Ferrum

Everything it adds is ordinary metadata. See [the changelog](CHANGELOG.md) and [the plan](../Ferrum/docs/plan.md).
"##;

    #[test]
    fn a_readme_becomes_a_safe_page() {
        let html = to_html(README, Some("https://github.com/CBFerrum/ferrum-app-crm"));
        for gone in ["<div", "<picture", "<img", "<source", "<p align", "badge.svg", "mermaid", "flowchart", "The pipeline.</sub>"] {
            assert!(!html.contains(gone), "{gone} in {html}");
        }
        assert!(html.contains("<strong>A native app for <a href=\"https://github.com/CBFerrum/Ferrum\">Ferrum</a>"), "{html}");
        // The contents line points at the headings' ids, one level down.
        assert!(html.contains("<a href=\"#about-overview\">Overview</a>"), "{html}");
        assert!(html.contains("<h3 id=\"about-overview\">Overview</h3>"), "{html}");
        assert!(html.contains("<h3 id=\"about-how-it-works\">How it works</h3>"), "{html}");
        // A section left with only a diagram goes; one with text stays.
        assert!(!html.contains("The pipeline</h4>"), "{html}");
        assert!(html.contains("<h4 id=\"about-inside-ferrum\">Inside Ferrum</h4>"), "{html}");
        // The screenshots' section keeps nothing but its heading, so it goes too, and the
        // contents line's link to it becomes text.
        assert!(!html.contains("id=\"about-screenshots\"") && !html.contains("href=\"#about-screenshots\""), "{html}");
        assert!(html.contains("· Screenshots ·"), "{html}");
        assert!(html.contains("<table>") && !html.contains("style="), "{html}");
        assert!(html.contains("markdown-alert-note"), "{html}");
        // Relative links resolve in the repository; a way out of it is text only.
        assert!(html.contains("<a href=\"https://github.com/CBFerrum/ferrum-app-crm/blob/HEAD/CHANGELOG.md\">the changelog</a>"), "{html}");
        assert!(html.contains(" and the plan."), "{html}");
        // Without a GitHub homepage, relative links are text.
        let html = to_html(README, Some("https://example.com/crm"));
        assert!(html.contains("See the changelog and the plan."), "{html}");
    }

    #[test]
    fn nothing_active_passes() {
        let md = r#"Hello <script>alert(1)</script> <img src=x onerror="alert(2)">

<iframe src="https://evil.example"></iframe>

<a href="javascript:alert(3)">raw</a> [js](javascript:alert(4)) [JS](JaVaScRiPt:alert(5)) [data](data:text/html,x)
[vb](vbscript:x) [net](//evil.example/x) [back](\\evil) [rel](a/../../etc/passwd) ![x](https://e/x.png "t")
[ok](https://example.com/a?b=1&c="2") <https://example.com/auto> <someone@example.com> [mail](mailto:a@example.com)

[t]: https://example.com "title with \"quotes\" and <tags>"
[ref][t] `<b>code</b>` and **<em>bold</em>**
"#;
        let html = to_html(md, None);
        for bad in ["<script", "onerror", "<iframe", "javascript:", "JaVaScRiPt", "data:text", "vbscript", "//evil", "\\\\evil", "<img", "<b>", "<em>"] {
            assert!(!html.to_ascii_lowercase().contains(&bad.to_ascii_lowercase()), "{bad} in {html}");
        }
        assert!(html.contains("<a href=\"https://example.com/a?b=1&amp;c=%222%22\">ok</a>"), "{html}");
        assert!(html.contains("<a href=\"https://example.com/auto\">"), "{html}");
        assert!(html.contains("href=\"mailto:someone@example.com\""), "{html}");
        assert!(html.contains("&lt;b&gt;code&lt;/b&gt;"), "{html}");
        assert!(html.contains("title=\"title with &quot;quotes&quot; and &lt;tags&gt;\""), "{html}");
        // Refused links keep their words.
        assert!(html.contains("js") && html.contains("data") && html.contains("rel"), "{html}");
    }

    #[test]
    fn headings_get_unique_ids() {
        let html = to_html("# Ferrum CRM\n\nx\n\n## Quick start (development)\n\ny\n\n## Quick start (development)\n\nz\n\n## AI agents: MCP\n\nw", None);
        assert!(html.contains("<h2 id=\"about-ferrum-crm\">"), "{html}");
        assert!(html.contains("<h3 id=\"about-quick-start-development\">"), "{html}");
        assert!(html.contains("<h3 id=\"about-quick-start-development-1\">"), "{html}");
        assert!(html.contains("<h3 id=\"about-ai-agents-mcp\">"), "{html}");
    }

    #[test]
    fn the_excerpt_is_the_opening_prose() {
        let text = excerpt(README, 600);
        assert_eq!(
            text,
            "A native app for Ferrum, the open-source, AI-first business platform.\n\nLeads, a pipeline you can drag, quotations and every touchpoint, in one place beside the books.\n\nEverything it adds is ordinary metadata. See the changelog and the plan."
        );
        let short = excerpt(README, 60);
        assert_eq!(short, "A native app for Ferrum, the open-source, AI-first business…");
        assert_eq!(excerpt("Just words,\nover two lines.", 100), "Just words, over two lines.");
        assert_eq!(excerpt("- one\n- two `code`\n  - nested\n\n1. first", 100), "• one\n\n• two code\n\n• nested\n\n• first");
        assert_eq!(excerpt("<div>only html</div>\n\n![x](y.png)", 100), "");
    }

    #[test]
    fn github_repositories() {
        assert_eq!(github_repository("https://github.com/CBFerrum/ferrum-app-crm").as_deref(), Some("https://github.com/CBFerrum/ferrum-app-crm"));
        assert_eq!(github_repository("https://github.com/a/b.git/").as_deref(), Some("https://github.com/a/b"));
        for not in [
            "http://github.com/a/b",
            "https://github.com/a",
            "https://github.com/a/b/c",
            "https://github.com.evil/a/b",
            "https://github.com/../b",
            "https://example.com/a/b",
        ] {
            assert_eq!(github_repository(not), None, "{not}");
        }
    }
}
