//! The shell's own words in a person's language (FERRUM-UX-006): what the bar, the sign-in and
//! sign-up pages, the portal's home, the consent page and the server's status pages say. The
//! catalogs are server/shell/locales/<language>.json, English → the language's. The bar's scripts
//! read them from the bootstrap (`locale.shell`), under the workspace's own words; the server's
//! pages through [`tr`]. A page with no person (sign-in, a signed-out status page) goes by the
//! browser's language ([`browser`]).

use axum::extract::Request;
use axum::http::{HeaderMap, header};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::{Map, Value};
use std::sync::OnceLock;

/// The languages the shell has words in, besides English, and their catalogs.
const CATALOGS: &[(&str, &str)] = &[("ar", include_str!("../shell/locales/ar.json")), ("hi", include_str!("../shell/locales/hi.json"))];

fn catalogs() -> &'static [(&'static str, Map<String, Value>)] {
    static PARSED: OnceLock<Vec<(&'static str, Map<String, Value>)>> = OnceLock::new();
    PARSED.get_or_init(|| CATALOGS.iter().map(|(l, s)| (*l, serde_json::from_str(s).expect("server/shell/locales: a JSON object"))).collect())
}

/// A language's base: `ar` of `ar-EG`.
fn base(language: &str) -> &str {
    language.split(['-', '_']).next().unwrap_or(language)
}

/// The shell's words in `language`; none for English or a language it has no words in.
pub fn words(language: &str) -> Option<&'static Map<String, Value>> {
    let b = base(language);
    catalogs().iter().find(|(l, _)| l.eq_ignore_ascii_case(b)).map(|(_, m)| m)
}

/// `english` in `language`: the catalog's words; for a sentence the server built with a name in
/// it, the catalog's sentence with that place (`You do not have access to {title}`) filled in as
/// the English filled it; else the English as it is.
pub fn tr(language: &str, english: &str) -> String {
    let Some(words) = words(language) else { return english.to_string() };
    if let Some(s) = words.get(english).and_then(Value::as_str) {
        return s.to_string();
    }
    // ponytail: every sentence with places is tried in turn (a few dozen); a map by first word if it grows.
    for (key, value) in words {
        let (Some(vars), Some(s)) = (key.contains('{').then(|| places(key, english)).flatten(), value.as_str()) else { continue };
        return vars.iter().fold(s.to_string(), |s, (k, v)| s.replace(&format!("{{{k}}}"), v));
    }
    english.to_string()
}

/// `template` in `language` with its places filled from `vars` as they are (already HTML, say).
/// Its own words are escaped for HTML when `html` is set.
pub fn tr_with(language: &str, template: &str, vars: &[(&str, &str)], html: bool) -> String {
    let words = tr(language, template);
    let words = if html { crate::shell::esc(&words) } else { words };
    vars.iter().fold(words, |s, (k, v)| s.replace(&format!("{{{k}}}"), v))
}

/// The values `text` gives the places of `template` (`Open {title}` and `Open Drive` → title =
/// Drive), or `None` when it is not that sentence.
fn places<'a>(template: &'a str, text: &'a str) -> Option<Vec<(&'a str, &'a str)>> {
    let mut lits = Vec::new();
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        let j = i + rest[i..].find('}')?;
        lits.push(&rest[..i]);
        names.push(&rest[i + 1..j]);
        rest = &rest[j + 1..];
    }
    lits.push(rest);
    if names.is_empty() {
        return None;
    }
    let mut t = text.strip_prefix(lits[0])?;
    let mut out = Vec::new();
    for (k, name) in names.iter().enumerate() {
        let next = lits[k + 1];
        let end = if k + 1 == names.len() {
            t.strip_suffix(next)?.len()
        } else if next.is_empty() {
            return None; // two places side by side: which part is which cannot be told
        } else {
            t.find(next)?
        };
        if end == 0 {
            return None;
        }
        out.push((*name, &t[..end]));
        t = &t[end + next.len()..];
    }
    t.is_empty().then_some(out)
}

tokio::task_local! {
    /// The language the request's browser asks for, among the shell's.
    static BROWSER: &'static str;
}

/// The first language of the browser's (`Accept-Language`) the shell has words in, or English.
pub fn browser_language(headers: &HeaderMap) -> &'static str {
    let accept = headers.get(header::ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok()).unwrap_or_default();
    for tag in accept.split(',') {
        let b = base(tag.split(';').next().unwrap_or_default().trim());
        if b.eq_ignore_ascii_case("en") {
            return "en";
        }
        if let Some((l, _)) = catalogs().iter().find(|(l, _)| l.eq_ignore_ascii_case(b)) {
            return l;
        }
    }
    "en"
}

/// Middleware: the request's browser language, for the pages drawn while it is handled.
pub async fn browser(req: Request, next: Next) -> Response {
    let language = browser_language(req.headers());
    BROWSER.scope(language, next.run(req)).await
}

/// The language a page is drawn in: its bootstrap's (the person's, else the workspace's), else the
/// browser's.
pub fn page_language(bootstrap: Option<&Value>) -> String {
    bootstrap
        .and_then(|b| b["locale"]["language"].as_str())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| BROWSER.try_with(|l| (*l).to_string()).unwrap_or_else(|_| "en".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every word the shell's scripts give tr() is in each catalog, and each catalog's words keep
    /// the English's places.
    #[test]
    fn catalogs_hold_the_scripts_words() {
        let scripts = [
            include_str!("../shell/runtime.js"),
            include_str!("../shell/login.js"),
            include_str!("../shell/signup.js"),
            include_str!("../shell/portal.js"),
            include_str!("../shell/isolated.js"),
            include_str!("../shell/assistant.js"),
        ];
        let mut asked = std::collections::BTreeSet::new();
        for src in scripts {
            let mut rest = src;
            while let Some(i) = rest.find("tr(\"") {
                let s = &rest[i + 3..];
                // The literal up to its closing quote, an escaped one aside.
                let b = s.as_bytes();
                let mut e = 1;
                while b[e] != b'"' {
                    e += if b[e] == b'\\' { 2 } else { 1 };
                }
                let lit = &s[..=e];
                asked.insert(serde_json::from_str::<String>(lit).unwrap());
                rest = &s[lit.len()..];
            }
        }
        assert!(asked.len() > 200, "{}", asked.len());
        let places_of = |s: &str| {
            let mut p: Vec<String> = s.split('{').skip(1).filter_map(|x| x.split_once('}').map(|(n, _)| n.to_string())).collect();
            p.sort();
            p
        };
        for (lang, catalog) in catalogs() {
            for w in &asked {
                assert!(catalog.contains_key(w), "{lang}: {w:?} has no words");
            }
            for (english, words) in catalog {
                let words = words.as_str().unwrap_or_else(|| panic!("{lang}: {english:?} is not text"));
                assert!(!words.trim().is_empty(), "{lang}: {english:?} is empty");
                assert_eq!(places_of(english), places_of(words), "{lang}: {english:?}");
            }
        }
    }

    #[test]
    fn sentences_with_places_are_matched() {
        assert_eq!(places("You do not have access to {title}", "You do not have access to Drive"), Some(vec![("title", "Drive")]));
        assert_eq!(places("{title} cannot open", "Board cannot open"), Some(vec![("title", "Board")]));
        assert_eq!(
            places(
                "This server has no interface for {id} {version}. An operator can add it (settings.ui_dir).",
                "This server has no interface for crm 1.2.0. An operator can add it (settings.ui_dir)."
            ),
            Some(vec![("id", "crm"), ("version", "1.2.0")])
        );
        assert_eq!(places("{title} cannot open", "cannot open"), None);
        assert_eq!(places("Open {title}", "Close Drive"), None);
        assert_eq!(tr("ar", "You do not have access to Drive"), tr("ar", "You do not have access to {title}").replace("{title}", "Drive"));
        assert_ne!(tr("ar", "Sign in"), "Sign in");
        assert_eq!(tr("en", "Sign in"), "Sign in");
        assert_eq!(tr("fr", "Sign in"), "Sign in");
        assert_eq!(tr("hi-IN", "Not a sentence it has"), "Not a sentence it has");
    }

    #[test]
    fn the_browsers_language() {
        let mut h = HeaderMap::new();
        assert_eq!(browser_language(&h), "en");
        h.insert(header::ACCEPT_LANGUAGE, "fr-FR,ar;q=0.8,en;q=0.5".parse().unwrap());
        assert_eq!(browser_language(&h), "ar");
        h.insert(header::ACCEPT_LANGUAGE, "en-GB,hi;q=0.9".parse().unwrap());
        assert_eq!(browser_language(&h), "en");
        h.insert(header::ACCEPT_LANGUAGE, "hi-IN".parse().unwrap());
        assert_eq!(browser_language(&h), "hi");
        assert_eq!(page_language(Some(&serde_json::json!({"locale": {"language": "ar"}}))), "ar");
        assert_eq!(page_language(None), "en");
    }
}
