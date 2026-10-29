//! Helpers shared by the finance hooks, upkeep and reports.

use chrono::NaiveDate;
use ferrum_domain::util::{id, s};
use ferrum_kernel::value::parse_date;
use ferrum_kernel::{Ctx, Doc, HookCtx, Result, Tx, notify};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

pub const PACKAGE: &str = "finance";

/// Whether the tenant installed this package. Hooks on other packages' documents (invoices,
/// orders, payments, journals) must leave tenants without it alone: their schema lacks our fields.
pub fn installed(ctx: &Ctx) -> bool {
    ctx.schema.packages.iter().any(|p| p == PACKAGE)
}

/// Whether the record being saved is not stored yet (its first save).
pub async fn is_new(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<bool> {
    let Some(rid) = id(doc, "id") else { return Ok(true) };
    let sql = format!("select exists(select 1 from {} where tenant_id = $1 and id = $2)", ferrum_kernel::ddl::q(&hc.entity.table));
    let found: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).bind(hc.ctx.tenant).bind(rid).fetch_one(&mut **hc.tx).await?;
    Ok(!found)
}

/// Child rows a hook builds, as the kernel stores them: each with an id (kept when it has one)
/// and its position.
pub fn child_rows(rows: Vec<Doc>) -> serde_json::Value {
    serde_json::Value::Array(
        rows.into_iter()
            .enumerate()
            .map(|(i, mut r)| {
                if r.get("id").and_then(serde_json::Value::as_str).is_none() {
                    r.insert("id".into(), json!(Uuid::now_v7().to_string()));
                }
                r.insert("idx".into(), json!(i));
                serde_json::Value::Object(r)
            })
            .collect(),
    )
}

pub fn opt_date(doc: &Doc, k: &str) -> Option<NaiveDate> {
    s(doc, k).and_then(|v| parse_date(&v[..v.len().min(10)]).ok())
}

pub fn int(doc: &Doc, k: &str) -> i64 {
    doc.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0)
}

/// `1,234.50`-style display of an amount.
pub fn fmt(v: Decimal, precision: u32) -> String {
    ferrum_kernel::print::format_money(&json!(v.to_string()), precision)
}

#[derive(Debug, Clone)]
pub struct FiscalYear {
    pub id: Uuid,
    pub name: String,
    pub start: NaiveDate,
    pub end: NaiveDate,
}

type FyRow = (Uuid, String, NaiveDate, NaiveDate);

/// The company's fiscal year containing `on` (fiscal years of a company never overlap).
pub async fn fiscal_year_of(tx: &mut Tx, tenant: Uuid, company: Uuid, on: NaiveDate) -> Result<Option<FiscalYear>> {
    let row: Option<FyRow> = sqlx::query_as(
        "select id, year_name, start_date, end_date from e_fiscal_year where tenant_id = $1 and company = $2 and $3 between start_date and end_date limit 1",
    )
    .bind(tenant)
    .bind(company)
    .bind(on)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|(id, name, start, end)| FiscalYear { id, name, start, end }))
}

pub async fn fiscal_year(tx: &mut Tx, tenant: Uuid, fy: Uuid) -> Result<Option<(Uuid, FiscalYear)>> {
    let row: Option<(Uuid, Uuid, String, NaiveDate, NaiveDate)> =
        sqlx::query_as("select company, id, year_name, start_date, end_date from e_fiscal_year where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(fy)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(row.map(|(company, id, name, start, end)| (company, FiscalYear { id, name, start, end })))
}

/// A cost center and every cost center below it.
pub async fn cost_center_subtree(tx: &mut Tx, tenant: Uuid, root: Uuid) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "with recursive t(id, depth) as (select id, 0 from e_cost_center where tenant_id = $1 and id = $2 \
         union all select c.id, t.depth + 1 from e_cost_center c join t on c.parent_cost_center = t.id where c.tenant_id = $1 and t.depth < 64) \
         select id from t",
    )
    .bind(tenant)
    .bind(root)
    .fetch_all(&mut **tx)
    .await?)
}

/// A cost center and every cost center above it.
pub async fn cost_center_ancestors(tx: &mut Tx, tenant: Uuid, cc: Uuid) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "with recursive t(id, parent, depth) as (select id, parent_cost_center, 0 from e_cost_center where tenant_id = $1 and id = $2 \
         union all select c.id, c.parent_cost_center, t.depth + 1 from e_cost_center c join t on c.id = t.parent where c.tenant_id = $1 and t.depth < 64) \
         select id from t",
    )
    .bind(tenant)
    .bind(cc)
    .fetch_all(&mut **tx)
    .await?)
}

/// Tells a user in the app: the active user with `email` when there is one, else `fallback`
/// (typically whoever created the record). Delivered at commit.
#[allow(clippy::too_many_arguments)]
pub async fn notify_user(
    tx: &mut Tx,
    tenant: Uuid,
    email: Option<&str>,
    fallback: Option<Uuid>,
    title: &str,
    body: &str,
    entity: &str,
    record: Option<Uuid>,
) -> Result<bool> {
    let by_email: Option<Uuid> = match email.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => {
            sqlx::query_scalar("select id from app_user where tenant_id = $1 and email = $2 and is_active")
                .bind(tenant)
                .bind(e)
                .fetch_optional(&mut **tx)
                .await?
        }
        None => None,
    };
    match by_email.or(fallback.filter(|u| !u.is_nil())) {
        Some(user) => {
            notify::send(tx, tenant, user, title, Some(body), Some(entity), record).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Runs a daily task at most once per workspace-local day: returns whether this call claimed today.
pub async fn claim_daily(tx: &mut Tx, tenant: Uuid, task: &str, today: NaiveDate) -> Result<bool> {
    let claimed: Option<bool> = sqlx::query_scalar(
        "insert into fin_daily_run (tenant_id, task, day) values ($1, $2, $3) \
         on conflict (tenant_id, task) do update set day = excluded.day where fin_daily_run.day < excluded.day returning true",
    )
    .bind(tenant)
    .bind(task)
    .bind(today)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(claimed.unwrap_or(false))
}

/// Active tenants that installed this package, with a system context each.
pub async fn tenants(k: &ferrum_kernel::Kernel) -> Result<Vec<Ctx>> {
    let mut sys = k.db.system_tx().await?;
    let ids: Vec<Uuid> =
        sqlx::query_scalar("select t.id from tenant t join installed_package p on p.tenant_id = t.id and p.name = $1 where t.status = 'active' order by t.id")
            .bind(PACKAGE)
            .fetch_all(&mut *sys)
            .await?;
    sys.commit().await?;
    let mut out = Vec::new();
    for t in ids {
        let schema = k.registry.schema(t).await?;
        if schema.packages.iter().any(|p| p == PACKAGE) {
            out.push(Ctx::system(t, schema));
        }
    }
    Ok(out)
}
