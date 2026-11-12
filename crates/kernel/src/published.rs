//! Published lists (`publish_list`, BD-15): what a workspace chose to show anyone, read without a
//! session. Only the listed fields leave: never a record's id, owner or timestamps, and a listed link
//! as the linked record's title. A submittable entity's list shows submitted records only. Anything
//! not published (an unknown or inactive workspace, an unknown list, one that no longer builds) is
//! `None`, so that callers answer all of them alike.

use crate::Kernel;
use crate::ctx::RowScope;
use crate::ddl::q;
use crate::error::{Error, Result};
use crate::query::{order_by, parse_filters, push_from_where};
use crate::store::{entity_predicate, field_expr, row_to_doc, select_list};
use ferrum_meta::published::DEFAULT_ROWS;
use ferrum_meta::{EffEntity, FieldDef, FieldType, PublishedList, Schema, SortDirection};
use serde_json::{Map, Value, json};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// A public read waits at most this long for the database.
pub const TIMEOUT_MS: u64 = 5_000;

/// A cell of CSV: quoted when it must be, and text a spreadsheet would run as a formula (beginning
/// `=`, `+`, `-`, `@`, a tab or a carriage return) made plain with a leading `'`; a number stays one.
/// Every CSV the platform hands out of a workspace uses this guard.
pub fn csv_cell(v: &Value) -> String {
    let raw = match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let safe = if raw.starts_with(['=', '+', '-', '@', '\t', '\r']) && raw.parse::<f64>().is_err() { format!("'{raw}") } else { raw };
    if safe.contains([',', '"', '\n', '\r']) { format!("\"{}\"", safe.replace('"', "\"\"")) } else { safe }
}

/// A published list as it reads now.
#[derive(Debug, Clone)]
pub struct Published {
    pub tenant: Uuid,
    /// The workspace's name.
    pub workspace: String,
    pub label: String,
    pub description: Option<String>,
    /// `{name, label, type}` of each listed field, in order.
    pub fields: Vec<Value>,
    /// Each with the listed fields and nothing else.
    pub rows: Vec<Map<String, Value>>,
    /// More records match than the list shows (on this page).
    pub more: bool,
    pub csv: bool,
    /// The page shown, from 1.
    pub page: u32,
    /// Each row's address (its slug's value) when the list has a slug, in the rows' order.
    pub addresses: Vec<String>,
    /// The field giving a detail page's description, and the origins that may frame its pages.
    pub summary: Option<String>,
    pub frame_origins: Vec<String>,
    /// Whether search engines are offered it: a list with a slug.
    pub indexed: bool,
}

/// What a public read asks of a list: words its text fields must hold, a page, or one record by its address.
#[derive(Debug, Clone, Default)]
pub struct Find {
    pub q: Option<String>,
    pub page: u32,
    pub address: Option<String>,
}

/// Pages a visitor may turn to at most, and the longest search.
pub const MAX_PAGE: u32 = 100;
pub const MAX_QUERY: usize = 100;
/// Addresses a sitemap lists at most, over every list.
pub const MAX_SITEMAP: usize = 10_000;

impl Published {
    pub fn json(&self) -> Value {
        let mut out = json!({"label": self.label, "description": self.description, "fields": self.fields, "rows": self.rows, "more": self.more});
        if self.page > 1 {
            out["page"] = json!(self.page);
        }
        if self.indexed {
            out["addresses"] = json!(self.addresses);
        }
        out
    }

    /// The rows as CSV (CRLF), headed by the fields' labels, every cell through [`csv_cell`].
    pub fn csv(&self) -> String {
        let names: Vec<&str> = self.fields.iter().map(|f| f["name"].as_str().unwrap_or_default()).collect();
        let mut out = self.fields.iter().map(|f| csv_cell(&f["label"])).collect::<Vec<_>>().join(",");
        out.push_str("\r\n");
        for r in &self.rows {
            out.push_str(&names.iter().map(|n| csv_cell(r.get(*n).unwrap_or(&Value::Null))).collect::<Vec<_>>().join(","));
            out.push_str("\r\n");
        }
        out
    }
}

/// The linked record's title as SQL: its title field, or its number; never its id.
fn title(schema: &Schema, ent: &EffEntity, f: &FieldDef) -> String {
    let built = || {
        let to = schema.entity(f.to.as_deref()?)?;
        let link = field_expr(ent, &f.name, "a")?;
        let own = to.def.title_field.as_deref().and_then(|t| field_expr(to, t, "lt")).map(|e| format!("nullif({e}::text, ''), ")).unwrap_or_default();
        Some(format!(
            "(select coalesce({own}lt.number) from {} lt where lt.tenant_id = a.tenant_id and lt.id = {link}{})",
            q(&to.table),
            entity_predicate(to, "lt")
        ))
    };
    built().unwrap_or_else(|| "null".into())
}

/// The query of `l`: its rows (one more than its limit) as the workspace, with the filter, submitted
/// records only where the entity is submittable, in its order.
fn query(schema: &Schema, ent: &EffEntity, l: &PublishedList, tenant: Uuid) -> Result<QueryBuilder<Postgres>> {
    query_with(schema, ent, l, tenant, &Find::default())
}

/// [`query`] narrowed by what a visitor asks: the words `q` in any of the list's text fields (as typed, any case),
/// page `page` (the list's limit a page), or the one record whose slug is `address`, its detail fields too.
fn query_with(schema: &Schema, ent: &EffEntity, l: &PublishedList, tenant: Uuid, find: &Find) -> Result<QueryBuilder<Postgres>> {
    let mut cols = select_list(ent, "a");
    let shown = l.list.fields.iter().chain(&l.list.detail);
    for f in shown.filter_map(|n| ent.field(n)).filter(|f| f.ftype == FieldType::Link) {
        cols.push_str(&format!(", {} as \"_t_{}\"", title(schema, ent, f), f.name));
    }
    let mut qb = QueryBuilder::new(format!("select to_jsonb(x) from (select {cols}"));
    let filters = parse_filters(&serde_json::to_value(&l.list.filter).map_err(Error::internal)?)?;
    push_from_where(&mut qb, schema, ent, tenant, &RowScope::default(), &filters, None)?;
    if ent.is_submittable() {
        // Whatever the filter says: a draft or a cancelled document is never public.
        qb.push(" and a.docstatus = 1");
    }
    if let Some(address) = &find.address {
        let slug = l.list.slug.as_deref().and_then(|f| field_expr(ent, f, "a")).ok_or_else(|| Error::NotFound("record".into()))?;
        qb.push(format!(" and ({slug})::text = ")).push_bind(address.clone());
    }
    if let Some(q) = find.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        let texts: Vec<String> = l
            .list
            .fields
            .iter()
            .filter_map(|n| ent.field(n))
            .filter(|f| matches!(f.ftype, FieldType::Data | FieldType::Select | FieldType::Text | FieldType::Markdown))
            .filter_map(|f| field_expr(ent, &f.name, "a"))
            .collect();
        if texts.is_empty() {
            qb.push(" and false");
        } else {
            let like = format!("%{}%", q.chars().take(MAX_QUERY).collect::<String>().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
            qb.push(" and (");
            for (i, t) in texts.iter().enumerate() {
                if i > 0 {
                    qb.push(" or ");
                }
                qb.push(format!("({t})::text ilike ")).push_bind(like.clone());
            }
            qb.push(")");
        }
    }
    let (field, desc) = l.list.sort.as_ref().map_or(("created_at", true), |(f, d)| (f.as_str(), *d == SortDirection::Desc));
    let order = order_by(ent, field, desc, "a").ok_or_else(|| Error::invalid(format!("a list cannot be ordered by {field}")))?;
    let limit = l.list.limit.unwrap_or(DEFAULT_ROWS) as i64;
    // `a.id` last, so that a page's rows never shift onto the next.
    qb.push(format!(" order by {order}, a.id limit ")).push_bind(limit + 1);
    let page = find.page.clamp(1, MAX_PAGE) as i64;
    if page > 1 {
        qb.push(" offset ").push_bind((page - 1) * limit);
    }
    qb.push(") x");
    Ok(qb)
}

/// Refuses a published list `now` has, new or changed since `was`, whose filter or order does not
/// build (an operator the filters do not know, a value that does not fit its field).
pub(crate) fn check(was: &Schema, now: &Schema) -> Result<()> {
    for l in now.published_lists.iter().filter(|l| was.published_list(&l.list.name) != Some(*l)) {
        let ent = now.entity(&l.entity).ok_or_else(|| Error::internal("a published list without its entity"))?;
        query(now, ent, l, Uuid::nil()).map_err(|e| Error::invalid(format!("customization rejected: published list {}: {e}", l.list.name)))?;
    }
    Ok(())
}

impl Kernel {
    /// The list `name` the workspace at `slug` publishes, read as the workspace; `None` when there is
    /// no such workspace, it is not active, or it publishes no such list.
    pub async fn published_list(&self, slug: &str, name: &str) -> Result<Option<Published>> {
        self.published_find(slug, name, &Find::default()).await
    }

    /// [`Kernel::published_list`] as a visitor asks for it (BD-25): searched, a page of it, or one record by its address
    /// (its detail fields too). `None` for anything not published, a record included: a list without a slug, an
    /// address no published record has, a page past the last.
    pub async fn published_find(&self, slug: &str, name: &str, find: &Find) -> Result<Option<Published>> {
        let mut tx = self.db.system_tx().await?;
        let found: Option<(Uuid, String, String)> =
            sqlx::query_as("select id, status, name from tenant where slug = $1").bind(slug.trim().to_lowercase()).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let Some((tenant, _, workspace)) = found.filter(|(_, status, _)| status == "active") else { return Ok(None) };
        let schema = self.registry.schema(tenant).await?;
        let Some(l) = schema.published_list(name) else { return Ok(None) };
        let Some(ent) = schema.entity(&l.entity) else { return Ok(None) };
        if find.address.is_some() && l.list.slug.is_none() {
            return Ok(None);
        }
        let mut qb = match query_with(&schema, ent, l, tenant, find) {
            Ok(qb) => qb,
            Err(e) => {
                tracing::warn!(tenant = %tenant, list = name, "a published list does not build: {e}");
                return Ok(None);
            }
        };
        let mut tx = self.db.tenant_tx_with_timeout(tenant, TIMEOUT_MS.min(self.db.config.statement_timeout_ms)).await?;
        let mut found: Vec<Value> = qb.build_query_scalar().fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let limit = l.list.limit.unwrap_or(DEFAULT_ROWS) as usize;
        let page = find.page.clamp(1, MAX_PAGE);
        if (find.address.is_some() || page > 1) && found.is_empty() {
            return Ok(None);
        }
        let more = found.len() > limit && find.address.is_none();
        found.truncate(if find.address.is_some() { 1 } else { limit });
        let names: Vec<&String> = if find.address.is_some() { l.list.fields.iter().chain(&l.list.detail).collect() } else { l.list.fields.iter().collect() };
        let fields: Vec<(&str, Option<&FieldDef>)> = names.iter().map(|n| (n.as_str(), ent.field(n))).collect();
        let mut addresses = Vec::new();
        let rows = found
            .into_iter()
            .map(|row| {
                let titles: Map<String, Value> =
                    row.as_object().map(|r| r.iter().filter(|(k, _)| k.starts_with("_t_")).map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
                let doc = row_to_doc(ent, &schema, row);
                if let Some(s) = &l.list.slug {
                    addresses.push(doc.get(s).and_then(Value::as_str).unwrap_or_default().to_string());
                }
                fields
                    .iter()
                    .map(|(n, f)| {
                        let v = match f {
                            Some(f) if f.ftype == FieldType::Link => titles.get(&format!("_t_{n}")).cloned(),
                            _ => doc.get(*n).cloned(),
                        };
                        (n.to_string(), v.unwrap_or(Value::Null))
                    })
                    .collect()
            })
            .collect();
        let fields = fields
            .iter()
            .map(|(n, f)| match f {
                Some(f) => json!({"name": n, "label": f.label(), "type": f.ftype}),
                None => json!({"name": n, "label": "Number", "type": "data"}),
            })
            .collect();
        Ok(Some(Published {
            tenant,
            workspace,
            label: l.list.label.clone(),
            description: l.list.description.clone(),
            fields,
            rows,
            more,
            csv: l.list.csv,
            page,
            addresses,
            summary: l.list.summary.clone(),
            frame_origins: l.list.frame_origins.clone(),
            indexed: l.list.slug.is_some(),
        }))
    }

    /// Every published record's address of the workspace at `slug`, for its sitemap: `(list, address)` of each list
    /// with a slug, every page of it, [`MAX_SITEMAP`] in all; nothing a list does not publish. `None` for a
    /// workspace not active.
    pub async fn published_addresses(&self, slug: &str) -> Result<Option<Vec<(String, String)>>> {
        let mut tx = self.db.system_tx().await?;
        let found: Option<(Uuid, String)> =
            sqlx::query_as("select id, status from tenant where slug = $1").bind(slug.trim().to_lowercase()).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let Some((tenant, _)) = found.filter(|(_, status)| status == "active") else { return Ok(None) };
        let schema = self.registry.schema(tenant).await?;
        let mut out = Vec::new();
        'lists: for l in schema.published_lists.iter().filter(|l| l.list.slug.is_some()) {
            // Every page of it, as a visitor turns them.
            for page in 1..=MAX_PAGE {
                let Some(p) = self.published_find(slug, &l.list.name, &Find { page, ..Default::default() }).await? else { break };
                out.extend(p.addresses.into_iter().filter(|a| !a.is_empty()).map(|a| (l.list.name.clone(), a)));
                if out.len() >= MAX_SITEMAP {
                    out.truncate(MAX_SITEMAP);
                    break 'lists;
                }
                if !p.more {
                    break;
                }
            }
        }
        Ok(Some(out))
    }
}
