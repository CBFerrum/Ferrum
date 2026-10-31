//! Documents people edit together live (docs/plan/collaboration.md, C3). The server's `coedit.rs`
//! runs the sockets and rooms; this is what anything else needs of such a document: its current
//! state from storage, its words, its HTML (exports, previews, PDFs), and forgetting documents
//! their package deleted. The package owning a document is its [`crate::DocHost`].
//!
//! Storage (`coedit_doc`, `coedit_update`): a snapshot, and the changes (Yjs updates) after it in
//! the document's own sequence; every 500 are folded into the snapshot. A change is announced on
//! the [`CHANNEL`], so every replica's open copy reads it.
//!
//! Markdown goes in ([`from_markdown`], [`append`]) and comes out ([`markdown`]) for templates and
//! AI agents.

mod markdown;

use crate::{Error, Result, Tx};
use std::fmt::Write as _;
use uuid::Uuid;
use yrs::types::text::YChange;
use yrs::updates::decoder::Decode;
use yrs::{Any, Doc, OffsetKind, Options, Out, ReadTxn, StateVector, Text, Transact, TransactionMut, Update, Xml, XmlFragment, XmlOut};

/// The fragment TipTap (y-prosemirror) keeps a document in.
pub const FRAGMENT: &str = "default";

/// Where stored changes are announced: `{"t": tenant, "p": package, "d": id, "r": who, "s": seq}`.
pub const CHANNEL: &str = "ferrum_coedit";

/// The largest a document may grow, roughly its encoded size.
pub const MAX_DOC: usize = 32 << 20;

/// A document as Yjs clients count it: in UTF-16 units.
pub fn new_doc() -> Doc {
    Doc::with_options(Options { offset_kind: OffsetKind::Utf16, ..Options::default() })
}

/// A document from a state (an update holding all of it, as [`encode`] makes). Bytes that are not
/// one make an empty document.
pub fn decode(state: &[u8]) -> Doc {
    let doc = new_doc();
    if !state.is_empty() {
        apply_stored(&mut doc.transact_mut(), Uuid::nil(), &[(0, Some(state.to_vec()))]);
    }
    doc
}

/// All of a document as one update.
pub fn encode(doc: &Doc) -> Vec<u8> {
    doc.transact().encode_state_as_update_v1(&StateVector::default())
}

/// Whether bytes are a document change (an update) Yjs can read.
pub fn is_update(data: &[u8]) -> bool {
    Update::decode_v1(data).is_ok()
}

/// The document's snapshot (when folded past `after`) and its changes after that, in that order,
/// read in one statement: one view of them, whatever is folded meanwhile. Nothing: no changes
/// after `after`, or (for -1) no such document.
pub async fn read_since(tx: &mut Tx, tenant: Uuid, package: &str, id: Uuid, after: i64) -> Result<Vec<(i64, Option<Vec<u8>>)>> {
    let rows: Vec<(i32, i64, Option<Vec<u8>>)> = sqlx::query_as(
        "select 0, snapshot_upto, snapshot from coedit_doc where tenant_id = $1 and package = $2 and id = $3 and snapshot_upto > $4 \
         union all \
         select 1, u.seq, u.data from coedit_update u where u.tenant_id = $1 and u.package = $2 and u.doc = $3 \
           and u.seq > greatest($4, (select snapshot_upto from coedit_doc where tenant_id = $1 and package = $2 and id = $3)) \
         order by 1, 2",
    )
    .bind(tenant)
    .bind(package)
    .bind(id)
    .bind(after)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows.into_iter().map(|(_, seq, data)| (seq, data)).collect())
}

/// Applies stored changes; one that does not apply (it never should: each applied before it was
/// stored) is logged and skipped rather than keeping the whole document from opening. Returns
/// their size in bytes.
pub fn apply_stored(txn: &mut TransactionMut, doc: Uuid, rows: &[(i64, Option<Vec<u8>>)]) -> usize {
    let mut size = 0;
    for (seq, data) in rows {
        let Some(data) = data else { continue };
        let applied = Update::decode_v1(data).map_err(|e| e.to_string()).and_then(|u| txn.apply_update(u).map_err(|e| e.to_string()));
        if let Err(e) = applied {
            tracing::error!(%doc, seq, "coedit: a stored change does not apply, skipped: {e}");
        }
        size += data.len();
    }
    size
}

/// A document as stored: the last change read, and roughly its size in bytes.
pub struct Loaded {
    pub doc: Doc,
    pub seen: i64,
    pub size: usize,
}

/// The document from storage; None if nobody has opened it live yet (its package's copy is all).
pub async fn load(tx: &mut Tx, tenant: Uuid, package: &str, id: Uuid) -> Result<Option<Loaded>> {
    let rows = read_since(tx, tenant, package, id, -1).await?;
    let Some(seen) = rows.last().map(|r| r.0) else { return Ok(None) };
    let doc = new_doc();
    let size = apply_stored(&mut doc.transact_mut(), id, &rows);
    Ok(Some(Loaded { doc, seen, size }))
}

/// The document's current state, as [`encode`] makes it, from storage (changes not yet stored,
/// at most 50 ms old, are not in it). None: never opened live.
pub async fn state(tx: &mut Tx, tenant: Uuid, package: &str, id: Uuid) -> Result<Option<Vec<u8>>> {
    Ok(load(tx, tenant, package, id).await?.map(|l| encode(&l.doc)))
}

/// A document made from Markdown (see the `markdown` module for the subset).
pub fn from_markdown(md: &str) -> Doc {
    let doc = new_doc();
    let fragment = doc.get_or_insert_xml_fragment(FRAGMENT);
    markdown::append(&mut doc.transact_mut(), &fragment, md);
    doc
}

/// The document as Markdown, for agents and templates.
pub fn markdown(doc: &Doc) -> String {
    markdown::read(doc)
}

/// Appends a change to the document's sequence and announces it, in the caller's transaction: the
/// row lock on `coedit_doc` makes a document's changes commit in their order. `from` names who
/// stored it (a replica, or `kernel`); `authors` are added to those the next save credits.
/// None: no such document.
pub async fn store(tx: &mut Tx, tenant: Uuid, package: &str, id: Uuid, data: &[u8], authors: &[Uuid], from: &str) -> Result<Option<i64>> {
    let seq: Option<i64> = sqlx::query_scalar(
        "update coedit_doc set seq = seq + 1, updated_at = now(), editors = array(select distinct e from unnest(editors || $4) e) \
         where tenant_id = $1 and package = $2 and id = $3 returning seq",
    )
    .bind(tenant)
    .bind(package)
    .bind(id)
    .bind(authors)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(seq) = seq else { return Ok(None) };
    sqlx::query("insert into coedit_update (tenant_id, package, doc, seq, data) values ($1, $2, $3, $4, $5)")
        .bind(tenant)
        .bind(package)
        .bind(id)
        .bind(seq)
        .bind(data)
        .execute(&mut **tx)
        .await?;
    sqlx::query("select pg_notify($1, $2)")
        .bind(CHANNEL)
        .bind(serde_json::json!({"t": tenant, "p": package, "d": id, "r": from, "s": seq}).to_string())
        .execute(&mut **tx)
        .await?;
    Ok(Some(seq))
}

/// Adds the blocks of `md` at the end of a document, as a change of its own: stored in its
/// sequence and announced, so everyone who has it open sees them at once. A document nobody has
/// opened live yet starts from `initial` (its package's content). Returns the change's number.
pub async fn append(tx: &mut Tx, tenant: Uuid, package: &str, id: Uuid, initial: Option<&[u8]>, md: &str, author: Uuid) -> Result<i64> {
    sqlx::query("insert into coedit_doc (tenant_id, package, id, snapshot) values ($1, $2, $3, $4) on conflict do nothing")
        .bind(tenant)
        .bind(package)
        .bind(id)
        .bind(initial.filter(|b| !b.is_empty() && is_update(b)))
        .execute(&mut **tx)
        .await?;
    let loaded = load(tx, tenant, package, id).await?.ok_or_else(|| Error::NotFound("document".into()))?;
    let change = {
        let fragment = loaded.doc.get_or_insert_xml_fragment(FRAGMENT);
        let mut txn = loaded.doc.transact_mut();
        if markdown::append(&mut txn, &fragment, md) == 0 {
            return Err(Error::field("markdown", "has nothing to add"));
        }
        txn.encode_update_v1()
    };
    if loaded.size + change.len() > MAX_DOC {
        return Err(Error::invalid("the document is too large to grow further"));
    }
    store(tx, tenant, package, id, &change, &[author], "kernel").await?.ok_or_else(|| Error::NotFound("document".into()))
}

/// Forgets documents their package deleted. Anyone still editing one is refused at the next change.
pub async fn forget(tx: &mut Tx, tenant: Uuid, package: &str, ids: &[Uuid]) -> Result<()> {
    sqlx::query("delete from coedit_doc where tenant_id = $1 and package = $2 and id = any($3)")
        .bind(tenant)
        .bind(package)
        .bind(ids)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn children<T: ReadTxn>(txn: &T) -> Vec<XmlOut> {
    txn.get_xml_fragment(FRAGMENT).map(|f| f.children(txn).collect()).unwrap_or_default()
}

/// The document's words, a line per block, for search, previews and AI agents.
pub fn plain_text(doc: &Doc) -> String {
    fn walk<T: ReadTxn>(node: XmlOut, txn: &T, out: &mut String) {
        match node {
            XmlOut::Text(t) => {
                for chunk in t.diff(txn, YChange::identity) {
                    if let Out::Any(Any::String(s)) = chunk.insert {
                        out.push_str(&s);
                    }
                }
            }
            XmlOut::Element(e) => {
                let tag = e.tag().clone();
                if &*tag == "hardBreak" {
                    out.push('\n');
                    return;
                }
                for child in e.children(txn) {
                    walk(child, txn, out);
                }
                if &*tag != "mention" && !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            XmlOut::Fragment(f) => {
                for child in f.children(txn) {
                    walk(child, txn, out);
                }
            }
        }
    }
    let txn = doc.transact();
    let mut out = String::new();
    for child in children(&txn) {
        walk(child, &txn, &mut out);
    }
    out.trim_end().to_string()
}

pub(crate) fn esc(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

fn number(v: Option<Out>) -> Option<i64> {
    match v? {
        Out::Any(Any::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Out::Any(Any::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn string(v: Option<Out>) -> Option<String> {
    match v? {
        Out::Any(Any::String(s)) => Some(s.to_string()),
        _ => None,
    }
}

fn truthy(v: Option<Out>) -> bool {
    match v {
        Some(Out::Any(Any::Bool(b))) => b,
        Some(Out::Any(Any::String(s))) => &*s == "true",
        _ => false,
    }
}

/// A link a reader may follow: the web and mail only.
fn safe_href(href: &str) -> bool {
    let h = href.trim().to_ascii_lowercase();
    h.starts_with("https://") || h.starts_with("http://") || h.starts_with("mailto:")
}

/// The document as HTML (a fragment for a page's body), for exports, previews and PDFs: the nodes
/// and marks the editor makes as their HTML elements, other nodes as their content; text escaped;
/// links only to the web and mail; no images, scripts or styles beyond text alignment.
pub fn html(doc: &Doc) -> String {
    fn marks(attrs: Option<&yrs::types::Attrs>) -> Vec<(&'static str, String)> {
        let Some(attrs) = attrs else { return Vec::new() };
        let mut out = Vec::new();
        for (name, tag) in [
            ("link", "a"),
            ("bold", "strong"),
            ("italic", "em"),
            ("underline", "u"),
            ("strike", "s"),
            ("highlight", "mark"),
            ("subscript", "sub"),
            ("superscript", "sup"),
            ("code", "code"),
        ] {
            let Some(v) = attrs.get(name) else { continue };
            if matches!(v, Any::Null | Any::Undefined | Any::Bool(false)) {
                continue;
            }
            if name == "link" {
                let href = match v {
                    Any::Map(m) => m.get("href").and_then(|h| if let Any::String(s) = h { Some(s.to_string()) } else { None }),
                    _ => None,
                };
                if let Some(href) = href.filter(|h| safe_href(h)) {
                    let mut open = String::from("a href=\"");
                    esc(&href, &mut open);
                    open.push_str("\" rel=\"noopener noreferrer nofollow\"");
                    out.push((tag, open));
                }
                continue;
            }
            out.push((tag, tag.to_string()));
        }
        out
    }
    fn align<T: ReadTxn>(e: &yrs::XmlElementRef, txn: &T) -> String {
        match string(e.get_attribute(txn, "textAlign")).as_deref() {
            Some(a @ ("center" | "right" | "justify")) => format!(" style=\"text-align:{a}\""),
            _ => String::new(),
        }
    }
    fn walk<T: ReadTxn>(node: XmlOut, txn: &T, out: &mut String, in_code: bool) {
        match node {
            XmlOut::Text(t) => {
                for chunk in t.diff(txn, YChange::identity) {
                    let Out::Any(Any::String(s)) = chunk.insert else { continue };
                    let m = if in_code { Vec::new() } else { marks(chunk.attributes.as_deref()) };
                    for (_, open) in &m {
                        let _ = write!(out, "<{open}>");
                    }
                    esc(&s, out);
                    for (tag, _) in m.iter().rev() {
                        let _ = write!(out, "</{tag}>");
                    }
                }
            }
            XmlOut::Element(e) => {
                let tag = e.tag().clone();
                let inner = |out: &mut String, code: bool| {
                    for child in e.children(txn) {
                        walk(child, txn, out, code);
                    }
                };
                match &*tag {
                    "paragraph" => {
                        let _ = write!(out, "<p{}>", align(&e, txn));
                        let at = out.len();
                        inner(out, false);
                        if out.len() == at {
                            out.push_str("<br>");
                        }
                        out.push_str("</p>\n");
                    }
                    "heading" => {
                        let level = number(e.get_attribute(txn, "level")).unwrap_or(1).clamp(1, 6);
                        let _ = write!(out, "<h{level}{}>", align(&e, txn));
                        inner(out, false);
                        let _ = writeln!(out, "</h{level}>");
                    }
                    "codeBlock" => {
                        out.push_str("<pre><code>");
                        inner(out, true);
                        out.push_str("</code></pre>\n");
                    }
                    "orderedList" => {
                        match number(e.get_attribute(txn, "start")).filter(|s| *s != 1) {
                            Some(start) => {
                                let _ = write!(out, "<ol start=\"{start}\">");
                            }
                            None => out.push_str("<ol>"),
                        }
                        inner(out, false);
                        out.push_str("</ol>\n");
                    }
                    "taskList" => {
                        out.push_str("<ul class=\"tasks\">");
                        inner(out, false);
                        out.push_str("</ul>\n");
                    }
                    "taskItem" => {
                        let checked = truthy(e.get_attribute(txn, "checked"));
                        let _ = write!(out, "<li><input type=\"checkbox\" disabled{}><div>", if checked { " checked" } else { "" });
                        inner(out, false);
                        out.push_str("</div></li>\n");
                    }
                    "tableHeader" | "tableCell" => {
                        let cell = if &*tag == "tableHeader" { "th" } else { "td" };
                        let _ = write!(out, "<{cell}");
                        for span in ["colspan", "rowspan"] {
                            if let Some(n) = number(e.get_attribute(txn, span)).filter(|n| *n > 1 && *n < 1000) {
                                let _ = write!(out, " {span}=\"{n}\"");
                            }
                        }
                        out.push('>');
                        inner(out, false);
                        let _ = writeln!(out, "</{cell}>");
                    }
                    "hardBreak" => out.push_str("<br>"),
                    "horizontalRule" => out.push_str("<hr>\n"),
                    _ => {
                        let html = match &*tag {
                            "blockquote" => Some("blockquote"),
                            "bulletList" => Some("ul"),
                            "listItem" => Some("li"),
                            "table" => Some("table"),
                            "tableRow" => Some("tr"),
                            _ => None,
                        };
                        if let Some(h) = html {
                            let _ = write!(out, "<{h}>");
                        }
                        inner(out, false);
                        if let Some(h) = html {
                            let _ = writeln!(out, "</{h}>");
                        }
                    }
                }
            }
            XmlOut::Fragment(f) => {
                for child in f.children(txn) {
                    walk(child, txn, out, in_code);
                }
            }
        }
    }
    let txn = doc.transact();
    let mut out = String::new();
    for child in children(&txn) {
        walk(child, &txn, &mut out, false);
    }
    out
}

/// A whole page for the document: for downloads and the PDF renderer.
pub fn html_page(title: &str, doc: &Doc) -> String {
    let mut t = String::new();
    esc(title, &mut t);
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{t}</title><style>\
         @page{{size:A4;margin:2cm}}body{{font:11pt/1.55 system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;color:#1f2328;max-width:46em;margin:0 auto}}\
         h1,h2,h3{{line-height:1.25}}pre{{background:#f6f8fa;padding:.75em;border-radius:6px;overflow-x:auto}}code{{font-family:ui-monospace,monospace}}\
         blockquote{{border-left:3px solid #d0d7de;margin-left:0;padding-left:1em;color:#57606a}}\
         table{{border-collapse:collapse;width:100%;table-layout:fixed}}th,td{{border:1px solid #d0d7de;padding:.3em .6em;vertical-align:top}}\
         th{{background:#f6f8fa;text-align:left}}td>p,th>p,li>div>p{{margin:0}}ul.tasks{{list-style:none;padding-left:.2em}}\
         ul.tasks li{{display:flex;gap:.5em;align-items:baseline}}mark{{background:#fff3b0}}@media screen{{body{{padding:1.5em}}}}\
         </style></head><body>\n{}</body></html>\n",
        html(doc)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::{XmlElementPrelim, XmlTextPrelim};

    fn sample() -> Doc {
        let doc = new_doc();
        let fragment = doc.get_or_insert_xml_fragment(FRAGMENT);
        let mut txn = doc.transact_mut();
        let h = fragment.push_back(&mut txn, XmlElementPrelim::empty("heading"));
        h.insert_attribute(&mut txn, "level", Any::from(2));
        h.push_back(&mut txn, XmlTextPrelim::new("Plan <A&B>"));
        let p = fragment.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.insert_attribute(&mut txn, "textAlign", "center");
        let t = p.push_back(&mut txn, XmlTextPrelim::new("Hello "));
        t.insert_with_attributes(&mut txn, 6, "world", [("bold".into(), Any::from(true))].into_iter().collect());
        let link = Any::from(std::collections::HashMap::from([("href".to_string(), Any::from("https://example.com/?a=1&b=2"))]));
        t.insert_with_attributes(&mut txn, 11, " site", [("link".into(), link)].into_iter().collect());
        let bad = Any::from(std::collections::HashMap::from([("href".to_string(), Any::from("javascript:alert(1)"))]));
        t.insert_with_attributes(&mut txn, 16, " trap", [("link".into(), bad)].into_iter().collect());
        p.push_back(&mut txn, XmlElementPrelim::empty("hardBreak"));
        p.push_back(&mut txn, XmlTextPrelim::new("日本 🎉"));
        let list = fragment.push_back(&mut txn, XmlElementPrelim::empty("taskList"));
        let item = list.push_back(&mut txn, XmlElementPrelim::empty("taskItem"));
        item.insert_attribute(&mut txn, "checked", true);
        let ip = item.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        ip.push_back(&mut txn, XmlTextPrelim::new("done"));
        let script = fragment.push_back(&mut txn, XmlElementPrelim::empty("script"));
        script.push_back(&mut txn, XmlTextPrelim::new("<script>alert(1)</script>"));
        fragment.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        drop(txn);
        doc
    }

    #[test]
    fn words_and_html_of_a_document() {
        let doc = sample();
        assert_eq!(plain_text(&doc), "Plan <A&B>\nHello world site trap\n日本 🎉\ndone\n<script>alert(1)</script>");
        let html = html(&doc);
        assert!(html.starts_with("<h2>Plan &lt;A&amp;B&gt;</h2>\n<p style=\"text-align:center\">Hello <strong>world</strong>"), "{html}");
        assert!(html.contains("<a href=\"https://example.com/?a=1&amp;b=2\" rel=\"noopener noreferrer nofollow\"> site</a> trap<br>日本 🎉</p>"), "{html}");
        assert!(html.contains("<li><input type=\"checkbox\" disabled checked><div><p>done</p>\n</div></li>"), "{html}");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;") && !html.contains("<script>"), "unknown nodes are their escaped text");
        assert!(html.ends_with("<p><br></p>\n"), "an empty paragraph keeps its line");
        assert!(html_page("Q3 <draft>", &doc).contains("<title>Q3 &lt;draft&gt;</title>"));
    }

    #[test]
    fn states_round_trip_and_garbage_is_empty() {
        let doc = sample();
        let state = encode(&doc);
        assert!(is_update(&state));
        assert_eq!(plain_text(&decode(&state)), plain_text(&doc));
        assert!(!is_update(b"%PDF-1.7 not a document"));
        assert_eq!(plain_text(&decode(b"%PDF-1.7 not a document")), "");
        assert_eq!(plain_text(&decode(&[])), "");
    }
}
