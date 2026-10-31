//! Interface releases (docs/plan/apps-ui.md §10.1, §12.1): an app's built files checked, named by
//! the digest of their contents, and its `index.html` turned into an entry template the server
//! completes for every page load with the bootstrap, the theme, the bar runtime and the bar.
//!
//! Rules for `index.html`: one `<head>` and one `<body>`; scripts only as files of its own
//! (`<script type="module" src="./…">`), never inline or from elsewhere; stylesheets and other
//! files it links to are its own; no `<base>` (the shell decides addresses) and no
//! Content-Security-Policy of its own (the platform sets it). Relative references are rewritten
//! to `/_ui/{app}/{digest}/…`, so the same page works at any depth of any route.

use crate::AppError;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Most files in one release, most bytes in all, and most in one file.
pub const MAX_UI_FILES: usize = 2_000;
pub const MAX_UI_BYTES: usize = 40 << 20;
pub const MAX_UI_FILE: usize = 16 << 20;

/// A checked release, ready to store under `ui/{app}/{digest}/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedUi {
    pub digest: String,
    /// Path under the release → its bytes and content type. `index.html` is not served as a
    /// file; the entry template replaces it.
    pub files: BTreeMap<String, (Vec<u8>, &'static str)>,
    pub entry: EntryTemplate,
}

/// `index.html` split where the shell goes: `head` ends with the `<head>` tag (the bootstrap,
/// theme and runtime follow it), `body` ends with the `<body>` tag (the bar follows it).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntryTemplate {
    pub head: String,
    pub body: String,
    pub tail: String,
}

impl EntryTemplate {
    /// The page: the template with the shell's parts in their places.
    pub fn render(&self, in_head: &str, in_body: &str) -> String {
        let mut out = String::with_capacity(self.head.len() + self.body.len() + self.tail.len() + in_head.len() + in_body.len());
        out.push_str(&self.head);
        out.push_str(in_head);
        out.push_str(&self.body);
        out.push_str(in_body);
        out.push_str(&self.tail);
        out
    }
}

/// The content type a release may serve a file as, by extension; `None` for files releases may
/// not hold.
pub fn content_type(path: &str) -> Option<&'static str> {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    Some(match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "wasm" => "application/wasm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => return None,
    })
}

fn bad<T>(msg: impl Into<String>) -> Result<T, AppError> {
    Err(AppError::Bundle(format!("ui: {}", msg.into())))
}

/// A path inside a release: plain segments only.
fn check_rel(path: &str) -> Result<(), AppError> {
    let ok = !path.is_empty()
        && path.len() <= 300
        && path.split('/').all(|s| !s.is_empty() && s != "." && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.@~+".contains(&b)));
    if !ok {
        return bad(format!("'{path}' is not a plain file path"));
    }
    Ok(())
}

/// The digest naming a release: SHA-256 over every path and the SHA-256 of its contents.
pub fn digest_of<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> String {
    let mut listed: Vec<(&str, String)> = files.into_iter().map(|(p, b)| (p, hex::encode(Sha256::digest(b)))).collect();
    listed.sort();
    let mut h = Sha256::new();
    h.update(b"ferrum-ui-v1\n");
    for (p, d) in listed {
        h.update(p.as_bytes());
        h.update(b"\0");
        h.update(d.as_bytes());
        h.update(b"\n");
    }
    hex::encode(h.finalize())
}

/// Checks a release's files (paths relative to its `dist` directory) and prepares it.
pub fn prepare(app: &str, files: BTreeMap<String, Vec<u8>>) -> Result<PreparedUi, AppError> {
    if files.len() > MAX_UI_FILES {
        return bad(format!("at most {MAX_UI_FILES} files"));
    }
    let mut total = 0usize;
    for (path, bytes) in &files {
        check_rel(path)?;
        if content_type(path).is_none() {
            return bad(format!("{path}: releases hold web files only (html, js, css, json, images, fonts, wasm, audio, video)"));
        }
        if bytes.len() > MAX_UI_FILE {
            return bad(format!("{path} is larger than {} MB", MAX_UI_FILE >> 20));
        }
        total += bytes.len();
    }
    if total > MAX_UI_BYTES {
        return bad(format!("the files add up to more than {} MB", MAX_UI_BYTES >> 20));
    }
    let Some(index) = files.get("index.html") else { return bad("there is no index.html") };
    let index = std::str::from_utf8(index).map_err(|_| AppError::Bundle("ui: index.html is not UTF-8".into()))?.to_string();
    let digest = digest_of(files.iter().map(|(p, b)| (p.as_str(), b.as_slice())));
    let entry = entry_template(&index, &files, &format!("/_ui/{app}/{digest}/"))?;
    let files = files.into_iter().filter(|(p, _)| p != "index.html").map(|(p, b)| (p.clone(), (b, content_type(&p).expect("checked")))).collect();
    Ok(PreparedUi { digest, files, entry })
}

/// A bundle's interface, checked and prepared (`None` when it has none): the files under its
/// `ui.dist`. The platform refuses a bundle whose interface fails here at upload and install, and
/// the store at submission.
pub fn from_bundle(b: &crate::Bundle) -> Result<Option<PreparedUi>, AppError> {
    let Some(ui) = &b.manifest.ui else { return Ok(None) };
    let prefix = format!("{}/", ui.dist.trim_end_matches('/'));
    let files: BTreeMap<String, Vec<u8>> =
        b.paths().filter_map(|p| p.strip_prefix(prefix.as_str()).map(|rel| (rel.to_string(), b.file(p).unwrap_or_default().to_vec()))).collect();
    if files.is_empty() {
        return bad(format!("{} holds no files", ui.dist));
    }
    prepare(&b.manifest.id, files).map(Some)
}

/// What an automatic look at a bundle's interface finds (docs/plan/apps-ui.md U8), for the people
/// who review or install it: what its page misses for accessibility, and how much a first visit
/// downloads. Hard limits (size, the page's rules) are [`prepare`]'s.
pub fn review(b: &crate::Bundle) -> Vec<String> {
    let Some(ui) = &b.manifest.ui else { return vec![] };
    let prefix = format!("{}/", ui.dist.trim_end_matches('/'));
    let mut bytes = 0usize;
    let mut index = String::new();
    for p in b.paths() {
        let Some(rel) = p.strip_prefix(prefix.as_str()) else { continue };
        let file = b.file(p).unwrap_or_default();
        bytes += file.len();
        if rel == "index.html" {
            index = String::from_utf8_lossy(file).into_owned();
        }
    }
    review_page(&index, bytes)
}

/// The same look at an interface's page (`index.html`) and the size of all its files.
pub fn review_page(index: &str, bytes: usize) -> Vec<String> {
    let index = index.to_ascii_lowercase();
    let index = index.as_str();
    let mut out = Vec::new();
    let tag = |name: &str| index.find(&format!("<{name}")).map(|i| &index[i..i + index[i..].find('>').unwrap_or(0)]);
    if !tag("html").is_some_and(|t| t.contains(" lang=")) {
        out.push("its page names no language (<html lang>), so screen readers guess how to read it".to_string());
    }
    let titled = index.find("<title>").is_some_and(|i| index[i + 7..].split("</title>").next().is_some_and(|t| !t.trim().is_empty()));
    if !titled {
        out.push("its page has no title (<title>), which tabs, history and screen readers show".to_string());
    }
    if !index.contains("name=\"viewport\"") && !index.contains("name=viewport") {
        out.push("its page sets no viewport, so phones show it zoomed out".to_string());
    }
    let mut rest = index;
    while let Some(i) = rest.find("<img") {
        let end = rest[i..].find('>').map_or(rest.len(), |e| i + e);
        if !rest[i..end].contains(" alt=") {
            out.push("an image on its page has no text alternative (alt)".to_string());
            break;
        }
        rest = &rest[end..];
    }
    if bytes > 2 << 20 {
        out.push(format!("its interface is {:.1} MB, which a first visit downloads", bytes as f64 / f64::from(1 << 20)));
    }
    out
}

/// One tag of `index.html`: where it starts and ends, its name, and its attributes with the
/// byte ranges of their values.
struct Tag<'a> {
    start: usize,
    end: usize,
    name: String,
    closing: bool,
    attrs: Vec<(String, Option<(usize, usize, &'a str)>)>,
}

impl Tag<'_> {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v.map_or("", |v| v.2))
    }
}

/// The tags of an HTML document, skipping comments, doctype and the contents of raw-text
/// elements (script, style, textarea, title). Enough for the pages bundlers write; anything
/// it cannot read is refused rather than guessed.
fn tags(html: &str) -> Result<Vec<Tag<'_>>, AppError> {
    let b = html.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = html[i..].find('<') {
        let start = i + off;
        if html[start..].starts_with("<!--") {
            let Some(e) = html[start + 4..].find("-->") else { return bad("index.html has an unclosed comment") };
            i = start + 4 + e + 3;
            continue;
        }
        if html[start..].starts_with("<!") || html[start..].starts_with("<?") {
            let Some(e) = html[start..].find('>') else { return bad("index.html is cut short") };
            i = start + e + 1;
            continue;
        }
        let mut j = start + 1;
        let closing = b.get(j) == Some(&b'/');
        if closing {
            j += 1;
        }
        let name_start = j;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'-') {
            j += 1;
        }
        if j == name_start {
            // A lone '<' in text.
            i = start + 1;
            continue;
        }
        let name = html[name_start..j].to_ascii_lowercase();
        let mut attrs = Vec::new();
        loop {
            while j < b.len() && (b[j].is_ascii_whitespace() || b[j] == b'/') {
                j += 1;
            }
            if j >= b.len() {
                return bad("index.html is cut short");
            }
            if b[j] == b'>' {
                j += 1;
                break;
            }
            let an = j;
            while j < b.len() && !b[j].is_ascii_whitespace() && !b"=>/\"'".contains(&b[j]) {
                j += 1;
            }
            if j == an {
                return bad(format!("index.html: cannot read the attributes of <{name}>"));
            }
            let aname = html[an..j].to_ascii_lowercase();
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if b.get(j) == Some(&b'=') {
                j += 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                let (vs, ve) = match b.get(j) {
                    Some(q @ (b'"' | b'\'')) => {
                        let Some(e) = html[j + 1..].find(*q as char) else { return bad("index.html has an unclosed attribute") };
                        (j + 1, j + 1 + e)
                    }
                    Some(_) => {
                        let mut e = j;
                        while e < b.len() && !b[e].is_ascii_whitespace() && b[e] != b'>' {
                            e += 1;
                        }
                        (j, e)
                    }
                    None => return bad("index.html is cut short"),
                };
                j = if b.get(ve).is_some_and(|c| *c == b'"' || *c == b'\'') { ve + 1 } else { ve };
                attrs.push((aname, Some((vs, ve, &html[vs..ve]))));
            } else {
                attrs.push((aname, None));
            }
        }
        let end = j;
        let raw = !closing && matches!(name.as_str(), "script" | "style" | "textarea" | "title");
        out.push(Tag { start, end, name: name.clone(), closing, attrs });
        i = end;
        if raw {
            let close = format!("</{name}");
            let Some(e) = html[i..].to_ascii_lowercase().find(&close) else { return bad(format!("index.html has an unclosed <{name}>")) };
            i += e;
        }
    }
    Ok(out)
}

/// A reference in `index.html` → the file of the release it names, or why it is refused.
/// `None` for references that are not to files (fragments, `data:` images, empty).
fn local_file(value: &str) -> Result<Option<String>, String> {
    let v = value.trim();
    if v.is_empty() || v.starts_with('#') || v.starts_with("data:") {
        return Ok(None);
    }
    if v.starts_with("//") || v.contains("://") || v.starts_with('/') || v.contains(':') {
        return Err(format!("'{v}' is not a file of this release; build with a relative base"));
    }
    let path = v.split(['?', '#']).next().unwrap_or_default().trim_start_matches("./");
    Ok(Some(path.to_string()))
}

fn entry_template(html: &str, files: &BTreeMap<String, Vec<u8>>, base: &str) -> Result<EntryTemplate, AppError> {
    let tags = tags(html)?;
    let mut head = None;
    let mut body = None;
    // (start, end, replacement) of attribute values to rewrite.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut i = 0;
    while i < tags.len() {
        let t = &tags[i];
        if t.closing {
            i += 1;
            continue;
        }
        match t.name.as_str() {
            "head" if head.replace(t.end).is_some() => return bad("index.html has more than one <head>"),
            "body" if body.replace(t.end).is_some() => return bad("index.html has more than one <body>"),
            "base" => return bad("index.html must not have a <base> element: the shell decides addresses"),
            "meta" if t.attr("http-equiv").is_some_and(|v| v.eq_ignore_ascii_case("content-security-policy")) => {
                return bad("index.html must not set a Content-Security-Policy: the platform sets it");
            }
            "script" => {
                let Some(src) = t.attr("src") else { return bad("index.html has an inline script; scripts must be files of the release") };
                let module = t.attr("type").is_some_and(|v| v.eq_ignore_ascii_case("module")) || t.attr("nomodule").is_some();
                if !module {
                    return bad(format!("index.html: script {src} must be a module (type=\"module\")"));
                }
                let closes = tags.get(i + 1).is_some_and(|n| n.closing && n.name == "script" && html[t.end..n.start].trim().is_empty());
                if !closes {
                    return bad("index.html has an inline script; scripts must be files of the release");
                }
            }
            _ => {}
        }
        for (aname, value) in &t.attrs {
            let Some((vs, ve, v)) = value else { continue };
            let rewrite = matches!(
                (t.name.as_str(), aname.as_str()),
                ("script" | "img" | "source" | "audio" | "video" | "track" | "input", "src") | ("link", "href") | ("video", "poster")
            );
            if aname.starts_with("on") {
                return bad(format!("index.html: <{} {aname}> is an inline script; scripts must be files of the release", t.name));
            }
            if (aname == "href" || aname == "src" || aname == "action") && v.trim().to_ascii_lowercase().starts_with("javascript:") {
                return bad("index.html has a javascript: address");
            }
            if !rewrite {
                continue;
            }
            let rel = t.attr("rel").unwrap_or_default().to_ascii_lowercase();
            // A link to a page elsewhere (a canonical address, an author's site) is not loaded.
            let loads = t.name != "link"
                || rel
                    .split_whitespace()
                    .any(|r| matches!(r, "stylesheet" | "modulepreload" | "preload" | "prefetch" | "icon" | "apple-touch-icon" | "manifest" | "mask-icon"));
            match local_file(v) {
                Ok(Some(path)) if loads => {
                    if !files.contains_key(&path) {
                        return bad(format!("index.html refers to {path}, which is not in the release"));
                    }
                    edits.push((*vs, *ve, format!("{base}{path}")));
                }
                Ok(_) => {}
                Err(why) if loads => return bad(format!("index.html: {why}")),
                Err(_) => {}
            }
        }
        i += 1;
    }
    let (Some(head_at), Some(body_at)) = (head, body) else {
        return bad("index.html must have a <head> and a <body> element");
    };
    if body_at < head_at {
        return bad("index.html: <body> comes before <head>");
    }
    // Apply the rewrites, keeping track of where the two insertion points move.
    edits.sort_by_key(|e| e.0);
    let mut out = String::with_capacity(html.len() + edits.len() * base.len());
    let (mut head_out, mut body_out) = (None, None);
    let mut at = 0;
    for (vs, ve, replacement) in edits {
        for (mark, slot) in [(head_at, &mut head_out), (body_at, &mut body_out)] {
            if slot.is_none() && mark <= vs {
                out.push_str(&html[at..mark]);
                at = mark;
                *slot = Some(out.len());
            }
        }
        out.push_str(&html[at..vs]);
        out.push_str(&replacement.replace('"', "%22").replace('\'', "%27").replace('<', "%3C").replace('>', "%3E"));
        at = ve;
    }
    for (mark, slot) in [(head_at, &mut head_out), (body_at, &mut body_out)] {
        if slot.is_none() {
            out.push_str(&html[at..mark]);
            at = mark;
            *slot = Some(out.len());
        }
    }
    out.push_str(&html[at..]);
    let (h, b) = (head_out.expect("set"), body_out.expect("set"));
    Ok(EntryTemplate { head: out[..h].to_string(), body: out[h..b].to_string(), tail: out[b..].to_string() })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_review_reads_the_page() {
        let good = r#"<!doctype html><html lang="en"><head><meta name="viewport" content="width=device-width"><title>Board</title></head><body><img src="a.png" alt=""></body></html>"#;
        assert!(review_page(good, 1000).is_empty(), "{:?}", review_page(good, 1000));
        let bare = "<HTML><head><title> </title></head><body><IMG src=a.png></body></HTML>";
        let found = review_page(bare, 3 << 20);
        assert_eq!(found.len(), 5, "{found:?}");
        assert!(
            found[0].contains("language")
                && found[1].contains("title")
                && found[2].contains("viewport")
                && found[3].contains("alt")
                && found[4].contains("3.0 MB")
        );
    }

    use super::*;

    const VITE: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <link rel="icon" type="image/svg+xml" href="./favicon.svg" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>CRM &lt;beta&gt;</title>
    <script type="module" crossorigin src="./assets/index-B1x2.js"></script>
    <link rel="modulepreload" crossorigin href="./assets/vendor-C3d4.js">
    <link rel="stylesheet" crossorigin href="./assets/index-D5e6.css">
    <link rel="canonical" href="https://crm.example.com/">
    <!-- <script>alert(1)</script> is only a comment -->
  </head>
  <body>
    <div id="root"></div>
    <a href="https://help.example.com">Help</a>
  </body>
</html>
"#;

    fn files(index: &str) -> BTreeMap<String, Vec<u8>> {
        let mut f = BTreeMap::new();
        f.insert("index.html".to_string(), index.as_bytes().to_vec());
        for p in ["favicon.svg", "assets/index-B1x2.js", "assets/vendor-C3d4.js", "assets/index-D5e6.css"] {
            f.insert(p.to_string(), format!("/* {p} */").into_bytes());
        }
        f
    }

    #[test]
    fn a_vite_build_becomes_an_entry_template() {
        let ui = prepare("crm", files(VITE)).expect("prepared");
        assert_eq!(ui.digest.len(), 64);
        assert!(!ui.files.contains_key("index.html"), "the template replaces it");
        assert_eq!(ui.files["assets/index-B1x2.js"].1, "text/javascript; charset=utf-8");
        let base = format!("/_ui/crm/{}/", ui.digest);
        let page = ui.entry.render("<!--HEAD-->", "<!--BAR-->");
        assert!(page.contains(&format!(r#"src="{base}assets/index-B1x2.js""#)), "{page}");
        assert!(page.contains(&format!(r#"href="{base}assets/index-D5e6.css""#)));
        assert!(page.contains(&format!(r#"href="{base}favicon.svg""#)));
        assert!(page.contains(r#"href="https://crm.example.com/""#), "links to pages elsewhere stay");
        assert!(page.contains(r#"<a href="https://help.example.com">"#));
        let head = page.find("<head>").unwrap();
        assert_eq!(&page[head + 6..head + 17], "<!--HEAD-->", "the shell's head goes first in <head>");
        let body = page.find("<body>").unwrap();
        assert_eq!(&page[body + 6..body + 16], "<!--BAR-->", "the bar goes first in <body>");
        // The same files, the same digest; any change, another.
        assert_eq!(prepare("crm", files(VITE)).unwrap().digest, ui.digest);
        let mut changed = files(VITE);
        changed.insert("assets/index-B1x2.js".into(), b"changed".to_vec());
        assert_ne!(prepare("crm", changed).unwrap().digest, ui.digest);
    }

    #[test]
    fn unsafe_or_unreadable_pages_are_refused() {
        let swap = |from: &str, to: &str| VITE.replacen(from, to, 1);
        let refused = |html: String, says: &str| {
            let e = prepare("crm", files(&html)).expect_err(says).to_string();
            assert!(e.contains(says), "{e}\n-- expected: {says}");
        };
        refused(swap("<div id=\"root\"></div>", "<div id=\"root\"></div><script>alert(1)</script>"), "inline script");
        refused(swap("</head>", "<script type=\"module\">import('./x.js')</script></head>"), "inline script");
        refused(swap("<div id=\"root\">", "<div id=\"root\" onclick=\"go()\">"), "inline script");
        refused(swap("./assets/index-B1x2.js", "https://cdn.example.com/x.js"), "not a file of this release");
        refused(swap("./assets/index-B1x2.js", "/assets/index-B1x2.js"), "relative base");
        refused(swap("./assets/index-D5e6.css", "//cdn.example.com/x.css"), "not a file of this release");
        refused(swap("./assets/index-B1x2.js", "./assets/missing.js"), "not in the release");
        refused(swap("type=\"module\" crossorigin src", "src"), "must be a module");
        refused(swap("<head>", "<head><base href=\"/\">"), "<base>");
        refused(swap("<head>", "<head><meta http-equiv=\"Content-Security-Policy\" content=\"default-src *\">"), "Content-Security-Policy");
        refused(swap("<body>", ""), "<head> and a <body>");
        refused(swap("<body>", "<body><body>"), "more than one <body>");
        refused(swap("<a href=\"https://help.example.com\">", "<a href=\"javascript:alert(1)\">"), "javascript:");
        let mut odd = files(VITE);
        odd.insert("server.exe".into(), vec![0]);
        assert!(prepare("crm", odd).unwrap_err().to_string().contains("web files only"));
        let mut deep = files(VITE);
        deep.insert("../escape.js".into(), vec![0]);
        assert!(prepare("crm", deep).unwrap_err().to_string().contains("plain file path"));
        let mut none = files(VITE);
        none.remove("index.html");
        assert!(prepare("crm", none).unwrap_err().to_string().contains("no index.html"));
    }

    #[test]
    fn uppercase_tags_unquoted_attributes_and_raw_text() {
        let html = "<!DOCTYPE html><HTML><HEAD><TITLE>a <b> c</TITLE><SCRIPT TYPE=module SRC=./main.js></SCRIPT><style>body > p { }</style></HEAD><BODY class=x>hi</BODY></HTML>";
        let mut f = BTreeMap::new();
        f.insert("index.html".to_string(), html.as_bytes().to_vec());
        f.insert("main.js".to_string(), b"x".to_vec());
        let ui = prepare("notes", f).expect("prepared");
        let page = ui.entry.render("[H]", "[B]");
        assert!(page.contains("<HEAD>[H]<TITLE>"), "{page}");
        assert!(page.contains("<BODY class=x>[B]hi"), "{page}");
        assert!(page.contains(&format!("SRC=/_ui/notes/{}/main.js>", ui.digest)), "{page}");
    }
}
