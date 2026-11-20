//! The visitor's language on public pages (docs/builder/decisions.md BD-87): what a browser asks for
//! (`Accept-Language`), matched against what the workspace offers its visitors (`public_languages`). The platform
//! chooses for its own pages and `shell.public` pages; an app with content in languages of its own (a store's, a
//! site's) gets the asked list and chooses among its own with [`choose`].

use crate::{Kernel, Result};
use uuid::Uuid;

/// At most this many languages are read from a header, each at most [`CODE_MAX`] characters.
pub const ASKED_MAX: usize = 20;
pub const CODE_MAX: usize = 35;

/// A language code: letters and digits in parts of at most 8 joined by `-`, the first of 2 to 8 letters (`en`,
/// `pt-br`, `zh-hant-tw`), at most [`CODE_MAX`] characters.
pub fn valid_code(code: &str) -> bool {
    let parts: Vec<&str> = code.split('-').collect();
    !code.is_empty()
        && code.len() <= CODE_MAX
        && parts.iter().all(|p| !p.is_empty() && p.len() <= 8 && p.bytes().all(|c| c.is_ascii_alphanumeric()))
        && parts[0].len() >= 2
        && parts[0].bytes().all(|c| c.is_ascii_alphabetic())
}

/// What a browser asks for, in the order it prefers (`de-AT,de;q=0.9,en;q=0.5`): codes in lower case, those it
/// refuses (`q=0`) and anything not a code left out, at most [`ASKED_MAX`].
pub fn accepted(header: &str) -> Vec<String> {
    let mut asked: Vec<(f32, usize, String)> = header
        .split(',')
        .take(ASKED_MAX * 2)
        .enumerate()
        .filter_map(|(i, part)| {
            let mut bits = part.split(';');
            let code = bits.next()?.trim().to_lowercase();
            let q = bits.find_map(|b| b.trim().strip_prefix("q=").and_then(|v| v.trim().parse::<f32>().ok())).unwrap_or(1.0);
            (valid_code(&code) && q > 0.0 && q.is_finite()).then_some((q, i, code))
        })
        .collect();
    asked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut out: Vec<String> = Vec::new();
    for (_, _, c) in asked {
        if !out.contains(&c) && out.len() < ASKED_MAX {
            out.push(c);
        }
    }
    out
}

/// The first of `asked` that `offered` has, by its code, else by its first part (`de-at` takes `de`, `de` takes
/// `de-ch`); none: `offered`'s first (English when nothing is offered).
pub fn choose(offered: &[String], asked: &[String]) -> String {
    let first = |c: &str| c.split('-').next().unwrap_or(c).to_string();
    for want in asked {
        if let Some(exact) = offered.iter().find(|o| o.eq_ignore_ascii_case(want)) {
            return exact.clone();
        }
        if let Some(near) = offered.iter().find(|o| first(&o.to_lowercase()) == first(want)) {
            return near.clone();
        }
    }
    offered.first().cloned().unwrap_or_else(|| "en".into())
}

impl Kernel {
    /// The languages the workspace offers people outside it, its default first: its `public_languages` setting, else
    /// its own language, else English.
    pub async fn public_languages(&self, tenant: Uuid) -> Result<Vec<String>> {
        let mut tx = self.db.system_tx().await?;
        let (set, language): (Option<serde_json::Value>, Option<String>) =
            sqlx::query_as("select settings->'public_languages', settings->>'language' from tenant where id = $1")
                .bind(tenant)
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or((None, None));
        tx.commit().await?;
        let set: Vec<String> =
            set.and_then(|v| serde_json::from_value::<Vec<String>>(v).ok()).unwrap_or_default().into_iter().filter(|c| valid_code(&c.to_lowercase())).collect();
        if !set.is_empty() {
            return Ok(set);
        }
        Ok(vec![language.filter(|l| !l.is_empty()).unwrap_or_else(|| "en".into())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_orders_by_q_and_caps() {
        assert_eq!(accepted("de-AT,de;q=0.9,en;q=0.5,fr;q=0"), ["de-at", "de", "en"]);
        assert_eq!(accepted("en;q=0.2, pt-BR , *;q=0.8, x;q=1, ja;q=0.7"), ["pt-br", "ja", "en"], "wildcards and non-codes left out");
        assert_eq!(accepted("en, EN, en;q=0.1"), ["en"], "once each");
        assert_eq!(accepted(""), Vec::<String>::new());
        let many: Vec<String> = (0..60).map(|i| format!("l{}", char::from(b'a' + (i % 26) as u8))).map(|c| format!("{c}x")).collect();
        assert!(accepted(&many.join(",")).len() <= ASKED_MAX);
        let long = format!("en-{}", "a".repeat(40));
        assert_eq!(accepted(&long), Vec::<String>::new(), "a code longer than 35 characters is no code");
        assert!(valid_code("zh-hant-tw") && !valid_code("e") && !valid_code("1n") && !valid_code("en_US") && !valid_code("en--us"));
    }

    #[test]
    fn choose_falls_back_by_first_part() {
        let offered = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let asked = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(choose(&offered(&["en", "de", "pt-BR"]), &asked(&["de-at", "en"])), "de");
        assert_eq!(choose(&offered(&["en", "de-CH"]), &asked(&["de"])), "de-CH");
        assert_eq!(choose(&offered(&["en", "pt-BR"]), &asked(&["pt-br"])), "pt-BR", "exact, as the workspace wrote it");
        assert_eq!(choose(&offered(&["fr", "en"]), &asked(&["ja", "ko"])), "fr", "nothing asked is offered: the default");
        assert_eq!(choose(&[], &asked(&["ja"])), "en");
    }
}
