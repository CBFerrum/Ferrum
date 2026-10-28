//! Human-readable document numbers from templates like `SINV-{FY}-{seq:5}`.
//!
//! Gapless series take their counter inside the submitting transaction (a short row lock that
//! commits or rolls back with the document). Other series take the counter in a separate,
//! immediately committed transaction, so concurrent drafts never wait on each other (gaps are fine).

use crate::Doc;
use crate::ctx::Ctx;
use crate::db::{Db, Tx};
use crate::error::{Error, Result};
use crate::value::parse_date;
use chrono::{Datelike, NaiveDate};
use ferrum_meta::{EffEntity, NumberingDef};
use serde_json::Value;

/// Fiscal year label: `2026` for calendar years, `2025-26` when the year starts in another month.
pub fn fiscal_year_label(date: NaiveDate, start_month: u32) -> String {
    let start_month = start_month.clamp(1, 12);
    if start_month == 1 {
        return date.year().to_string();
    }
    let start = if date.month() >= start_month { date.year() } else { date.year() - 1 };
    format!("{}-{:02}", start, (start + 1) % 100)
}

fn sanitize(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/')).take(20).collect()
}

/// Splits the rendered template into `(prefix, width, suffix)`.
/// `today` (the workspace's local date) dates the number when the document has no date field.
pub fn render(def: &NumberingDef, doc: &Doc, fy_start_month: u32, today: chrono::NaiveDate) -> Result<(String, usize, String)> {
    let date = def
        .date_field
        .as_ref()
        .and_then(|f| doc.get(f))
        .and_then(Value::as_str)
        .map(|s| parse_date(&s[..s.len().min(10)]))
        .transpose()
        .map_err(Error::invalid)?
        .unwrap_or(today);
    let mut prefix = String::new();
    let mut suffix = String::new();
    let mut width = None;
    let mut rest = def.template.as_str();
    while !rest.is_empty() {
        let out = if width.is_some() { &mut suffix } else { &mut prefix };
        match rest.find('{') {
            None => {
                out.push_str(rest);
                break;
            }
            Some(start) => {
                out.push_str(&rest[..start]);
                let end = rest[start..].find('}').map(|e| e + start).ok_or_else(|| Error::invalid("bad numbering template"))?;
                let token = &rest[start + 1..end];
                match token {
                    "YYYY" => out.push_str(&format!("{:04}", date.year())),
                    "YY" => out.push_str(&format!("{:02}", date.year() % 100)),
                    "MM" => out.push_str(&format!("{:02}", date.month())),
                    "DD" => out.push_str(&format!("{:02}", date.day())),
                    "FY" => out.push_str(&fiscal_year_label(date, fy_start_month)),
                    t if t.starts_with("seq:") => {
                        if width.is_some() {
                            return Err(Error::invalid("numbering template has two {seq} tokens"));
                        }
                        width = Some(t[4..].parse::<usize>().map_err(|_| Error::invalid("bad seq width"))?);
                    }
                    t if t.starts_with("field:") => {
                        let v = doc.get(&t[6..]).map(|v| match v {
                            Value::String(s) => s.clone(),
                            Value::Null => String::new(),
                            other => other.to_string(),
                        });
                        out.push_str(&sanitize(&v.unwrap_or_default()));
                    }
                    other => return Err(Error::invalid(format!("unknown numbering token {other}"))),
                }
                rest = &rest[end + 1..];
            }
        }
    }
    Ok((prefix, width.ok_or_else(|| Error::invalid("numbering template needs {seq:N}"))?, suffix))
}

const NEXT: &str = "insert into naming_counter (tenant_id, key, value) values ($1, $2, 1) \
                    on conflict (tenant_id, key) do update set value = naming_counter.value + 1 returning value";

pub async fn assign(db: &Db, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &Doc) -> Result<String> {
    let def = ent.def.numbering.as_ref().ok_or_else(|| Error::Internal(format!("{} has no numbering", ent.def.name)))?;
    let (prefix, width, suffix) = render(def, doc, ctx.fy_start_month, ctx.today())?;
    let key = format!("{}:{prefix}#{suffix}", ent.def.name);
    let seq: i64 = if def.gapless {
        sqlx::query_scalar(NEXT).bind(ctx.tenant).bind(&key).fetch_one(&mut **tx).await?
    } else {
        let mut own = db.side_tenant_tx(ctx.tenant).await?;
        let v = sqlx::query_scalar(NEXT).bind(ctx.tenant).bind(&key).fetch_one(&mut *own).await?;
        own.commit().await?;
        v
    };
    Ok(format!("{prefix}{seq:0width$}{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(t: &str) -> NumberingDef {
        NumberingDef { template: t.into(), gapless: false, on: Default::default(), date_field: Some("posting_date".into()) }
    }

    #[test]
    fn renders_templates() {
        let mut doc = Doc::new();
        doc.insert("posting_date".into(), json!("2026-02-10"));
        doc.insert("branch".into(), json!("BLR 1!"));
        assert_eq!(
            render(&def("SINV-{FY}-{seq:5}"), &doc, 4, chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()).unwrap(),
            ("SINV-2025-26-".into(), 5, String::new())
        );
        assert_eq!(
            render(&def("{YY}{MM}/{seq:3}/{field:branch}"), &doc, 1, chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()).unwrap(),
            ("2602/".into(), 3, "/BLR1".into())
        );
        assert!(render(&def("INV-{seq:3}-{seq:3}"), &doc, 1, chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()).is_err());
        assert_eq!(fiscal_year_label(NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(), 4), "2026-27");
        assert_eq!(fiscal_year_label(NaiveDate::from_ymd_opt(2026, 3, 31).unwrap(), 4), "2025-26");
    }
}
