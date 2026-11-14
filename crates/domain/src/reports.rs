//! The report framework: apps contribute reports ([`ReportSource`], registered with
//! [`add_source`]), and [`catalog_for`] and [`run`] serve them to callers who may run them, each
//! inside one read-only, tenant-isolated transaction. ERP's reports (the books and stock) are one
//! such source, in ferrum-app-erp.

use chrono::{Datelike, NaiveDate};
use ferrum_kernel::{Action, Ctx, Error, Kernel, Result, Tx};
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

const REPORT_TIMEOUT_MS: u64 = 120_000;

/// A standard parameter definition (`company`, `from_date`, `to_date`, `as_of`, or a link to
/// `account`, `customer`, `supplier`, `warehouse`, `item`).
pub fn param_def(name: &str) -> Value {
    match name {
        // Optional: the server uses the user's default company, or the only one.
        "company" => json!({"name": "company", "label": "Company", "type": "link", "to": "Company"}),
        "from_date" => json!({"name": "from_date", "label": "From Date", "type": "date", "default": "fiscal_year_start"}),
        "to_date" => json!({"name": "to_date", "label": "To Date", "type": "date", "default": "today"}),
        "as_of" => json!({"name": "as_of", "label": "As Of", "type": "date", "default": "today"}),
        other => {
            let to = match other {
                "account" => "Account",
                "customer" => "Customer",
                "supplier" => "Supplier",
                "warehouse" => "Warehouse",
                _ => "Item",
            };
            json!({"name": other, "label": to, "type": "link", "to": to})
        }
    }
}

/// A read-only, tenant-isolated transaction with the report timeout, for report queries: on the read address when
/// there is one fit to read from (`Db::read_tx`). A report reads only: nothing it writes needs to be seen here.
pub async fn read_tx(k: &Kernel, ctx: &Ctx) -> Result<Tx> {
    k.db.read_tx(ctx.tenant, REPORT_TIMEOUT_MS).await
}

/// Reports contributed by packages outside this crate.
#[async_trait::async_trait]
pub trait ReportSource: Send + Sync {
    /// `[{name, label, module, entity, params, scoped?}]`: `entity` is the one the caller must be
    /// able to read. A package report is expected to apply the caller's row scope to its rows
    /// (`ferrum_kernel::row_scope(..).push_sql`), as `"scoped": true` (the default) says. One that
    /// only narrows to the caller's own records says `"scoped": "owner"`, and one that sums up every
    /// record says `"scoped": false`: callers who may read only some records are then refused it.
    fn catalog(&self) -> Vec<Value>;
    /// Runs `name` (already permission-checked against its `entity`).
    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, params: &HashMap<String, String>) -> Result<Value>;
}

static SOURCES: std::sync::RwLock<Vec<std::sync::Arc<dyn ReportSource>>> = std::sync::RwLock::new(Vec::new());

/// Registers a package's reports (at startup).
pub fn add_source(source: std::sync::Arc<dyn ReportSource>) {
    if let Ok(mut s) = SOURCES.write() {
        s.push(source);
    }
}

/// Sets every app's reports at once, each with who gives it, in place of any set before: the server's set-up may run
/// more than once in a process (loco runs `after_context` twice on `start`), and a report is still listed and run once.
/// Two registrants giving one report name is an error naming both.
pub fn set_sources(sources: Vec<(String, std::sync::Arc<dyn ReportSource>)>) -> std::result::Result<(), String> {
    let mut by: HashMap<String, &str> = HashMap::new();
    for (who, s) in &sources {
        for name in s.catalog().iter().filter_map(|e| e["name"].as_str()) {
            if let Some(other) = by.insert(name.to_string(), who) {
                return Err(format!("the report {name} is registered by both {other} and {who}"));
            }
        }
    }
    *SOURCES.write().map_err(|_| "the report sources are poisoned".to_string())? = sources.into_iter().map(|(_, s)| s).collect();
    Ok(())
}

/// Each report's app's own packages (report name → packages), as the server registers them.
static OWNERS: std::sync::RwLock<Option<HashMap<String, Vec<String>>>> = std::sync::RwLock::new(None);

/// Sets which packages give each report (its app's own), in place of any set before. Zones (level 2) use it: a report
/// runs its app's own SQL, so where a zone leaves every one of those packages out it is not listed and not run,
/// whatever entity it is gated on.
pub fn set_owners(owners: HashMap<String, Vec<String>>) {
    if let Ok(mut o) = OWNERS.write() {
        *o = Some(owners);
    }
}

/// Whether `ctx`'s zone reaches report `name`'s app (crate::zones in the kernel, level 2): always where the zone
/// limits nothing; where it does, only a report whose app is known and has a package the zone shows.
fn app_seen(ctx: &Ctx, name: &str) -> bool {
    if ctx.perms.limit.is_none() {
        return true;
    }
    OWNERS.read().ok().and_then(|o| o.as_ref().and_then(|o| o.get(name).cloned())).is_some_and(|ps| ps.iter().any(|p| ctx.sees_package(p)))
}

fn sources() -> Vec<std::sync::Arc<dyn ReportSource>> {
    SOURCES.read().map(|s| s.clone()).unwrap_or_default()
}

pub fn catalog() -> Value {
    let mut list: Vec<Value> = Vec::new();
    for s in sources() {
        list.extend(s.catalog().into_iter().map(|mut e| {
            if let Value::Object(m) = &mut e {
                m.remove("entity");
                m.remove("scoped");
            }
            e
        }));
    }
    Value::Array(list)
}

/// How a report narrows its rows to the caller (its catalog entry's `scoped`).
#[derive(Clone, Copy, PartialEq)]
enum Narrows {
    Nothing,
    Owner,
    RowScope,
}

/// What a package report's catalog entry says about narrowing (see [`ReportSource::catalog`]).
fn narrows(entry: &Value) -> Narrows {
    match &entry["scoped"] {
        Value::Bool(false) => Narrows::Nothing,
        Value::String(s) if s == "owner" => Narrows::Owner,
        _ => Narrows::RowScope,
    }
}

/// Whether `ctx` may run a report over `entity`, which sums up its records. Unless the report
/// applies the caller's row scope itself, the caller must read all of them: a company scope is
/// fine, as reports run per company and check it ([`resolve_company`]); own-records access only
/// when the report narrows to the caller's records; any other scope never.
fn may_run(ctx: &Ctx, entity: &str, narrows: Narrows) -> Result<()> {
    let Some(ent) = ctx.schema.entity(entity) else { return Err(Error::NotFound(format!("{entity} is not installed"))) };
    let scope = ferrum_kernel::row_scope(ctx, ent, Action::Read)?;
    if narrows == Narrows::RowScope {
        return Ok(());
    }
    let other_scopes = scope.conds.iter().any(|c| c.field.as_deref() != Some("company"));
    // Narrowing to the caller's own records covers grants of exactly that, no conditions.
    // Records reached through a path (BD-02) are no report's to narrow to: it knows who made them, not
    // whose they are.
    let covered = scope.grants.is_empty() || (narrows == Narrows::Owner && scope.grants.iter().all(|g| g.owner && g.fields.is_empty() && g.via.is_none()));
    if other_scopes || !covered {
        return Err(Error::Forbidden(format!("this report sums up every {}; it needs access to all of them, not only some", ent.def.label)));
    }
    Ok(())
}

/// [`catalog`] for one caller: only reports of installed packages that the caller may run.
pub fn catalog_for(ctx: &Ctx) -> Value {
    let allowed = |entity: &str, n: Narrows| may_run(ctx, entity, n).is_ok();
    let mut list: Vec<Value> = Vec::new();
    for s in sources() {
        list.extend(
            s.catalog()
                .into_iter()
                .filter(|e| e.get("entity").and_then(Value::as_str).is_some_and(|ent| allowed(ent, narrows(e))))
                .filter(|e| app_seen(ctx, e["name"].as_str().unwrap_or_default()))
                .map(|mut e| {
                    if let Value::Object(m) = &mut e {
                        m.remove("entity");
                        m.remove("scoped");
                    }
                    e
                }),
        );
    }
    Value::Array(list)
}

/// Runs a report from a package source after checking the caller may run it.
async fn run_source(k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
    for s in sources() {
        let Some(entry) = s.catalog().into_iter().find(|e| e["name"] == json!(name)) else { continue };
        let entity = entry["entity"].as_str().unwrap_or_default().to_string();
        // Of an app the caller's zone leaves out: as a report that is not there.
        if ctx.schema.entity(&entity).is_none() || !app_seen(ctx, name) {
            return Err(Error::NotFound(format!("report {name} (its package is not installed)")));
        }
        may_run(ctx, &entity, narrows(&entry))?;
        let mut out = s.run(k, ctx, name, raw).await?;
        // Where the workspace records every read of its entity, recorded before the answer is given (on the primary,
        // wherever the report read), with the parameters' names and never their values.
        if ctx.reads_recorded(&entity) == Some(true) {
            let params: Vec<ferrum_kernel::query::Filter> =
                raw.keys().map(|p| ferrum_kernel::query::Filter { field: p.clone(), op: ferrum_kernel::query::Op::Eq, value: Value::Null }).collect();
            let rows = out.get("rows").and_then(Value::as_array).map_or(0, |r| r.len() as i64);
            let read = ferrum_kernel::controls::Read::Many { how: "report", filters: &params, search: false, rows, to: None };
            ferrum_kernel::controls::record_read_alone(k, ctx, &entity, read).await?;
        }
        if let Value::Object(m) = &mut out {
            m.entry("name").or_insert_with(|| json!(name));
            m.entry("label").or_insert_with(|| entry["label"].clone());
        }
        return Ok(out);
    }
    Err(Error::NotFound(format!("report {name}")))
}

pub fn col(name: &str, label: &str, ty: &str) -> Value {
    json!({"name": name, "label": label, "type": ty})
}

/// A report's parameters as sent by the caller (all strings).
pub struct Params<'a> {
    pub raw: &'a HashMap<String, String>,
}

impl Params<'_> {
    pub fn date(&self, k: &str) -> Result<Option<NaiveDate>> {
        match self.raw.get(k).map(|v| v.trim()).filter(|v| !v.is_empty()) {
            None => Ok(None),
            Some(v) => NaiveDate::parse_from_str(v, "%Y-%m-%d").map(Some).map_err(|_| Error::field(k, "use the format YYYY-MM-DD")),
        }
    }

    pub fn uuid(&self, k: &str) -> Result<Option<Uuid>> {
        match self.raw.get(k).map(|v| v.trim()).filter(|v| !v.is_empty()) {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(|_| Error::field(k, "is not a valid id")),
        }
    }

    pub fn str(&self, k: &str) -> Option<&str> {
        self.raw.get(k).map(|v| v.trim()).filter(|v| !v.is_empty())
    }
}

/// The company a report runs for (from the `company` parameter, the user's default, or the only one).
pub struct ReportCompany {
    pub id: Uuid,
    pub name: String,
    pub currency: String,
    pub fy_month: u32,
    pub precision: u32,
}

tokio::task_local! {
    /// Today where the workspace running the report is; set by `run`.
    static TODAY: NaiveDate;
}

/// The workspace's local date while a report runs (UTC outside one).
pub fn today() -> NaiveDate {
    TODAY.try_with(|d| *d).unwrap_or_else(|_| chrono::Utc::now().date_naive())
}

pub async fn resolve_company(tx: &mut Tx, ctx: &Ctx, p: &Params<'_>) -> Result<ReportCompany> {
    let cid = match p.uuid("company")?.or(ctx.default_company) {
        Some(c) => c,
        None => {
            let ids: Vec<Uuid> = sqlx::query_scalar("select id from e_company where tenant_id = $1 limit 2").bind(ctx.tenant).fetch_all(&mut **tx).await?;
            match ids.as_slice() {
                [only] => *only,
                _ => return Err(Error::field("company", "is required")),
            }
        }
    };
    if !ctx.is_admin
        && !ctx.is_system
        && let Some(allowed) = ctx.scopes.get("Company")
        && !allowed.contains(&cid)
    {
        return Err(Error::Forbidden("you do not have access to this company".into()));
    }
    let row: Option<(String, Option<i32>, Option<String>, Option<i32>)> = sqlx::query_as(
        "select c.company_name, c.fy_start_month::int4, cur.code, cur.precision::int4 from e_company c \
         left join e_currency cur on cur.tenant_id = c.tenant_id and cur.id = c.default_currency where c.tenant_id = $1 and c.id = $2",
    )
    .bind(ctx.tenant)
    .bind(cid)
    .fetch_optional(&mut **tx)
    .await?;
    let (name, fy, currency, precision) = row.ok_or_else(|| Error::NotFound("Company".into()))?;
    Ok(ReportCompany {
        id: cid,
        name,
        currency: currency.unwrap_or_default(),
        fy_month: fy.unwrap_or(4).clamp(1, 12) as u32,
        precision: precision.unwrap_or(2).clamp(0, 6) as u32,
    })
}

/// Start of the fiscal year containing `on`.
pub fn fy_start(on: NaiveDate, month: u32) -> NaiveDate {
    let year = if on.month() >= month { on.year() } else { on.year() - 1 };
    NaiveDate::from_ymd_opt(year, month, 1).unwrap_or(on)
}

pub fn period(p: &Params<'_>, co: &ReportCompany) -> Result<(NaiveDate, NaiveDate)> {
    let to = p.date("to_date")?.unwrap_or_else(today);
    let from = p.date("from_date")?.unwrap_or_else(|| fy_start(to, co.fy_month));
    if from > to {
        return Err(Error::field("from_date", "must not be after the to date"));
    }
    Ok((from, to))
}

pub async fn run(k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
    // Periods and "as of" dates default to the workspace's today. A report that fails on a replica runs once more on
    // the primary.
    k.db.reading(|| TODAY.scope(ctx.today(), run_source(k, ctx, name, raw))).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fiscal_year_start() {
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        assert_eq!(fy_start(d(2026, 10, 28), 4), d(2026, 4, 1));
        assert_eq!(fy_start(d(2026, 2, 1), 4), d(2025, 4, 1));
        assert_eq!(fy_start(d(2026, 2, 1), 1), d(2026, 1, 1));
    }

    struct One;

    #[async_trait::async_trait]
    impl ReportSource for One {
        fn catalog(&self) -> Vec<Value> {
            vec![json!({"name": "zz_one", "label": "One", "module": "test", "entity": "Customer", "scoped": false, "params": [param_def("company")]})]
        }
        async fn run(&self, _k: &Kernel, _ctx: &Ctx, _name: &str, _params: &HashMap<String, String>) -> Result<Value> {
            Ok(json!({}))
        }
    }

    #[test]
    fn one_report_name_has_one_registrant() {
        let e = set_sources(vec![("crm".into(), std::sync::Arc::new(One)), ("hr".into(), std::sync::Arc::new(One))]).unwrap_err();
        assert_eq!(e, "the report zz_one is registered by both crm and hr");
    }

    #[test]
    fn the_catalog_lists_sources_without_their_access_rules() {
        add_source(std::sync::Arc::new(One));
        let all = catalog();
        let one = all.as_array().unwrap().iter().find(|e| e["name"] == "zz_one").expect("listed");
        assert!(one.get("entity").is_none() && one.get("scoped").is_none(), "{one}");
        assert_eq!(one["params"][0]["name"], json!("company"));
    }

    struct Named(&'static str);

    #[async_trait::async_trait]
    impl ReportSource for Named {
        fn catalog(&self) -> Vec<Value> {
            vec![json!({"name": self.0, "label": self.0, "module": "test", "entity": "Customer", "scoped": false, "params": []})]
        }
        async fn run(&self, _k: &Kernel, _ctx: &Ctx, _name: &str, _params: &HashMap<String, String>) -> Result<Value> {
            Ok(json!({}))
        }
    }

    #[test]
    fn two_apps_registering_one_after_the_other_both_stay() {
        add_source(std::sync::Arc::new(Named("zz_first_app")));
        add_source(std::sync::Arc::new(Named("zz_second_app")));
        let listed: Vec<String> = catalog().as_array().unwrap().iter().filter_map(|e| e["name"].as_str().map(str::to_string)).collect();
        assert!(listed.contains(&"zz_first_app".to_string()) && listed.contains(&"zz_second_app".to_string()), "{listed:?}");
        // Only a deliberate set replaces what was registered; a name given twice in one set stops it before anything changes.
        let e = set_sources(vec![("a".into(), std::sync::Arc::new(Named("zz_same"))), ("b".into(), std::sync::Arc::new(Named("zz_same")))]).unwrap_err();
        assert_eq!(e, "the report zz_same is registered by both a and b");
        let still: Vec<String> = catalog().as_array().unwrap().iter().filter_map(|e| e["name"].as_str().map(str::to_string)).collect();
        assert!(still.contains(&"zz_first_app".to_string()), "{still:?}");
    }
}
