//! Countries by their ISO 3166-1 alpha-2 codes, which tax and address rules key on
//! (docs/plan/localization.md, L0). People type a country (`country`, a name or a code);
//! `country_code` follows it on every save: a recognised name or a code gives the code, a bare
//! code also the country's name, anything else no code.

use crate::util::{s, set};
use async_trait::async_trait;
use ferrum_kernel::{Doc, DocHook, HookCtx, Result, Tx};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::LazyLock;
use uuid::Uuid;

const TABLE: &str = include_str!("countries.tsv");

struct Countries {
    /// code → name
    names: HashMap<&'static str, &'static str>,
    /// a name, spelling or code as [`key`] folds it → code
    codes: HashMap<String, &'static str>,
}

static COUNTRIES: LazyLock<Countries> = LazyLock::new(|| {
    let mut names = HashMap::new();
    let mut codes = HashMap::new();
    for line in TABLE.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let mut cols = line.split('\t');
        let (Some(code), Some(name)) = (cols.next(), cols.next()) else { continue };
        names.insert(code, name);
        codes.insert(code.to_ascii_lowercase(), code);
        codes.insert(key(name), code);
        for other in cols.next().unwrap_or("").split(';').filter(|o| !o.is_empty()) {
            codes.insert(other.to_string(), code);
        }
    }
    Countries { names, codes }
});

/// How names compare: lowercase, straight quotes, single spaces, no trailing dots, no leading "the".
fn key(name: &str) -> String {
    let folded = name.to_lowercase().replace(['\u{2019}', '\u{2018}'], "'");
    let folded = folded.split_whitespace().collect::<Vec<_>>().join(" ");
    let folded = folded.trim_end_matches('.');
    folded.strip_prefix("the ").unwrap_or(folded).to_string()
}

/// The code of a country's name, other name or code: "India", "Bharat", "in" → "IN".
pub fn code_for(name: &str) -> Option<&'static str> {
    COUNTRIES.codes.get(&key(name)).copied()
}

/// A code's English name: "DE" → "Germany".
pub fn name_of(code: &str) -> Option<&'static str> {
    COUNTRIES.names.get(code.to_ascii_uppercase().as_str()).copied()
}

/// Sets `country_code` from `country`; a bare code also gets the country's name.
pub fn fill(doc: &mut Doc) {
    let typed = s(doc, "country").map(str::trim).unwrap_or_default().to_string();
    let code = code_for(&typed);
    if let Some(c) = code
        && typed.eq_ignore_ascii_case(c)
    {
        set(doc, "country", name_of(c).unwrap_or(c));
    }
    set(doc, "country_code", code.map_or(Value::Null, Value::from));
}

/// For records with `country` and `country_code` (Company, Address).
pub struct CountryHook;

#[async_trait]
impl DocHook for CountryHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        fill(doc);
        Ok(())
    }
}

/// Fills the codes of `entity`'s records that have a country but no code yet (records from before
/// `country_code`); returns the countries it does not recognise. The names stay as typed.
pub async fn backfill(tx: &mut Tx, tenant: Uuid, entity: &str) -> Result<Vec<String>> {
    let table = ferrum_kernel::ddl::q(&ferrum_meta::table_for(entity));
    let typed: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select distinct country from {table} where tenant_id = $1 and country_code is null and coalesce(country, '') <> ''"
    )))
    .bind(tenant)
    .fetch_all(&mut **tx)
    .await?;
    let mut unknown = Vec::new();
    for name in typed {
        let Some(code) = code_for(&name) else {
            unknown.push(name);
            continue;
        };
        sqlx::query(sqlx::AssertSqlSafe(format!("update {table} set country_code = $3 where tenant_id = $1 and country = $2 and country_code is null")))
            .bind(tenant)
            .bind(&name)
            .bind(code)
            .execute(&mut **tx)
            .await?;
    }
    Ok(unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_table_is_whole_and_unambiguous() {
        let table = &*COUNTRIES;
        assert_eq!(table.names.len(), 250, "ISO 3166-1's 249 codes and XK");
        for (code, name) in &table.names {
            assert!(code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase()), "{code}");
            assert_eq!(code_for(name), Some(*code), "{name}");
            assert_eq!(code_for(code), Some(*code));
        }
        // Every other name is a different key; a duplicate would have been lost to another code.
        let others: usize =
            TABLE.lines().filter(|l| !l.starts_with('#')).filter_map(|l| l.split('\t').nth(2)).map(|o| o.split(';').filter(|x| !x.is_empty()).count()).sum();
        assert_eq!(table.codes.len(), 2 * table.names.len() + others);
    }

    #[test]
    fn names_people_type() {
        for (typed, code) in [
            ("India", "IN"),
            (" bharat ", "IN"),
            ("United States of America", "US"),
            ("U.S.A.", "US"),
            ("UK", "GB"),
            ("The Netherlands", "NL"),
            ("Deutschland", "DE"),
            ("Cote d'Ivoire", "CI"),
            ("Côte d’Ivoire", "CI"),
            ("Viet Nam", "VN"),
            ("Türkiye", "TR"),
            ("Turkey", "TR"),
            ("Bosnia & Herzegovina", "BA"),
            ("Kosovo", "XK"),
            ("sg", "SG"),
        ] {
            assert_eq!(code_for(typed), Some(code), "{typed}");
        }
        assert_eq!(code_for("Congo"), None, "two countries share it");
        assert_eq!(code_for(""), None);
        assert_eq!(name_of("de"), Some("Germany"));
    }

    #[test]
    fn the_code_follows_the_country() {
        let mut doc = json!({"country": "in"}).as_object().cloned().unwrap();
        fill(&mut doc);
        assert_eq!((doc["country"].as_str(), doc["country_code"].as_str()), (Some("India"), Some("IN")));
        doc.insert("country".into(), json!("Deutschland"));
        fill(&mut doc);
        assert_eq!((doc["country"].as_str(), doc["country_code"].as_str()), (Some("Deutschland"), Some("DE")));
        doc.insert("country".into(), json!("Atlantis"));
        fill(&mut doc);
        assert!(doc["country_code"].is_null());
        doc.remove("country");
        fill(&mut doc);
        assert!(doc["country_code"].is_null());
    }
}
