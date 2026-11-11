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
//! AI agents. Documents go out as Word files too ([`docx`]).
//!
//! Images ([`IMAGE`] nodes) name an image their package keeps (`image`, an id) and describe it
//! (`alt`); never an address. Whoever renders a document says where each image's bytes are
//! ([`html_with`], [`docx`]), so a document cannot point readers anywhere else.

mod docx;
pub mod format;
mod html;
mod markdown;

pub use docx::{ImageData, docx};

use crate::{Error, Result, Tx};
use uuid::Uuid;
use yrs::types::text::YChange;
use yrs::updates::decoder::Decode;
use yrs::{Any, Doc, OffsetKind, Options, Out, ReadTxn, StateVector, Text, Transact, TransactionMut, Update, Xml, XmlFragment, XmlOut};

/// The fragment TipTap (y-prosemirror) keeps a document in.
pub const FRAGMENT: &str = "default";

/// An image's node: `image` (the id its package keeps it under) and `alt` (what it shows).
pub const IMAGE: &str = "image";

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
                // An image's description and caption are among the document's words.
                if &*tag == IMAGE {
                    out.push_str(string(e.get_attribute(txn, "alt")).unwrap_or_default().trim());
                    let caption = format::caption(format::any(e.get_attribute(txn, "caption")).as_ref());
                    if !caption.is_empty() {
                        out.push('\n');
                        out.push_str(&caption);
                    }
                }
                for child in e.children(txn) {
                    walk(child, txn, out);
                }
                // Nodes inside a line (a mention, a footnote's number) do not end it.
                if !matches!(&*tag, "mention" | "footnoteRef" | "pageField") && !out.is_empty() && !out.ends_with('\n') {
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

/// The images a document shows, by the ids their package keeps them under.
pub fn image_ids(doc: &Doc) -> std::collections::BTreeSet<String> {
    fn walk<T: ReadTxn>(node: XmlOut, txn: &T, out: &mut std::collections::BTreeSet<String>) {
        match node {
            XmlOut::Element(e) => {
                if &**e.tag() == IMAGE
                    && let Some(id) = string(e.get_attribute(txn, "image")).filter(|id| !id.is_empty())
                {
                    out.insert(id);
                }
                for child in e.children(txn) {
                    walk(child, txn, out);
                }
            }
            XmlOut::Fragment(f) => {
                for child in f.children(txn) {
                    walk(child, txn, out);
                }
            }
            XmlOut::Text(_) => {}
        }
    }
    let txn = doc.transact();
    let mut out = std::collections::BTreeSet::new();
    for child in children(&txn) {
        walk(child, &txn, &mut out);
    }
    out
}

/// The document as HTML without its images (their descriptions stand in): see [`html_with`].
pub fn html(doc: &Doc) -> String {
    html_with(doc, &|_| None)
}

/// The document as HTML (a fragment for a page's body), for previews and agents: the nodes and marks
/// the editor makes as their HTML elements with their checked styles, other nodes as their content;
/// text escaped; links to the web, mail and the document's headings; suggestions as `ins` and
/// `del`; images where `images` gives their address (a `data:` URI for files that travel), else
/// their description; footnotes at the end; no scripts.
pub fn html_with(doc: &Doc, images: &dyn Fn(&str) -> Option<String>) -> String {
    html::fragment(doc, images)
}

/// A whole page for the document without its images: see [`html_page_with`].
pub fn html_page(title: &str, doc: &Doc) -> String {
    html_page_with(title, doc, &|_| None)
}

/// A whole page for the document: for downloads and the PDF renderer. See [`html_page_export`].
pub fn html_page_with(title: &str, doc: &Doc, images: &dyn Fn(&str) -> Option<String>) -> String {
    html_page_export(title, doc, None, images).0
}

/// A whole page for the document, for downloads and the PDF renderer (which prints it as the
/// editor prints: the page's size and margins, the header and footer with page numbers, page
/// breaks, footnotes at the end), and what it could not carry ([`not_carried`]). `origin` (the
/// workspace's, `https://acme.example.com`) makes links to other documents links; without it they
/// are their words.
pub fn html_page_export(title: &str, doc: &Doc, origin: Option<&str>, images: &dyn Fn(&str) -> Option<String>) -> (String, Vec<String>) {
    let (page, lost) = html::page(title, doc, origin, images);
    (page, not_carried(doc, &[], lost))
}

/// The document as a Word file and what it could not carry: see [`docx`] and [`not_carried`].
pub fn docx_export(doc: &Doc, title: &str, origin: Option<&str>, images: &dyn Fn(&str) -> Option<ImageData>) -> Result<(Vec<u8>, Vec<String>)> {
    let (file, lost) = docx::write(doc, title, origin, images)?;
    Ok((file, not_carried(doc, &[], lost)))
}

/// The document as Markdown and what Markdown could not carry: see [`markdown`]. `images` gives
/// an image's address (none: the image is left out, as [`markdown`] leaves it).
pub fn markdown_export(doc: &Doc, origin: Option<&str>, images: &dyn Fn(&str) -> Option<String>) -> (String, Vec<String>) {
    let (md, lost) = markdown::write(doc, origin, images);
    (md, not_carried(doc, markdown::CANNOT, lost))
}

/// What an export of `doc` could not carry, in words a person reads, for the interface to show
/// (each is plain ASCII, so the list may travel as a header joined by `; `): of what the document
/// uses, what the format `cannot` say; what the exporter met and dropped (`lost`); and what no
/// exporter knows (`unknown node x`, `unknown mark y`: their words are kept, their formatting is not).
fn not_carried(doc: &Doc, cannot: &[&str], lost: std::collections::BTreeSet<String>) -> Vec<String> {
    let used = format::features(&doc.transact());
    let mut out: Vec<String> =
        used.into_iter().filter(|f| cannot.contains(&f.as_str()) || f.starts_with("unknown ") || f.starts_with("content from a newer")).collect();
    out.extend(lost);
    out.sort();
    out.dedup();
    out
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
        assert!(html.starts_with("<h2 id=\"heading-1\">Plan &lt;A&amp;B&gt;</h2>\n<p style=\"text-align:center\">Hello <strong>world</strong>"), "{html}");
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

    /// The fixtures' exports, written to `FERRUM_EXPORT_SAMPLES` (a directory) to open them in an
    /// office suite and a browser: `FERRUM_EXPORT_SAMPLES=/tmp/x cargo test -p ferrum-kernel --lib coedit`.
    #[test]
    fn every_export_of_the_fixtures() {
        use format::fixtures::{BLUE_PNG, GREEN_PNG, everything, hostile, png};
        let bytes = |id: &str| match id {
            "img1" => Some(png(BLUE_PNG)),
            "img2" => Some(png(GREEN_PNG)),
            _ => None,
        };
        let data = |id: &str| bytes(id).map(|b| ImageData { bytes: b, content_type: "image/png".into(), width: 4, height: 2 });
        let uri = |id: &str| bytes(id).map(|b| format!("data:image/png;base64,{}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b)));
        let origin = Some("https://acme.example.com");
        // The same text on A4 portrait with 2.5 cm margins and one header for every page.
        let a4 = everything();
        {
            use yrs::Map;
            let page = a4.get_or_insert_map("page");
            let mut txn = a4.transact_mut();
            for (k, v) in [
                ("size", Any::from("A4")),
                ("orientation", Any::from("portrait")),
                ("top", Any::from(1417)),
                ("bottom", Any::from(1417)),
                ("differentFirst", Any::from(false)),
            ] {
                page.insert(&mut txn, k, v);
            }
        }
        for (name, doc) in [("everything", everything()), ("everything-a4", a4), ("hostile", hostile())] {
            let (file, lost_docx) = docx_export(&doc, "Annual report 2026", origin, &data).unwrap();
            let (page, lost_html) = html_page_export("Annual report 2026", &doc, origin, &uri);
            let (md, lost_md) = markdown_export(&doc, origin, &|id| (id == "img1").then(|| "https://acme.example.com/i/img1.png".to_string()));
            if let Ok(dir) = std::env::var("FERRUM_EXPORT_SAMPLES") {
                std::fs::write(format!("{dir}/{name}.docx"), &file).unwrap();
                std::fs::write(format!("{dir}/{name}.html"), &page).unwrap();
                std::fs::write(format!("{dir}/{name}.md"), &md).unwrap();
                std::fs::write(format!("{dir}/{name}.not-carried.txt"), format!("docx: {lost_docx:?}\nhtml: {lost_html:?}\nmarkdown: {lost_md:?}\n")).unwrap();
            }
            for list in [&lost_docx, &lost_html, &lost_md] {
                assert!(list.iter().all(|l| l.is_ascii() && !l.contains(['\n', '\r', ';'])), "a list that can travel as a header: {list:?}");
            }
        }
    }

    #[test]
    fn the_words_of_footnotes_captions_and_new_nodes_are_searchable() {
        let words = plain_text(&format::fixtures::everything());
        assert!(words.contains("bold italic and the text goes on after the note."), "a note's number does not break the line: {words}");
        assert!(words.contains("A blue chart\nFigure 1: sales by region"), "{words}");
        assert!(words.ends_with("The first note, with a link\nThe second note."), "{words}");
        assert!(plain_text(&format::fixtures::hostile()).contains("words of an unknown node"));
    }
}
