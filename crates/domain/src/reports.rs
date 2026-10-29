//! Built-in reports, computed straight from the ledgers (journal lines, open items, stock moves)
//! inside one read-only, tenant-isolated transaction.

use crate::util::money;
use chrono::{Datelike, NaiveDate};
use ferrum_kernel::{Action, Ctx, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

const MAX_ROWS: i64 = 10_000;
const REPORT_TIMEOUT_MS: u64 = 120_000;

struct Def {
    name: &'static str,
    label: &'static str,
    module: &'static str,
    /// Reading this entity is what the report requires.
    entity: &'static str,
    params: &'static [&'static str],
}

const DEFS: &[Def] = &[
    Def { name: "trial_balance", label: "Trial Balance", module: "accounting", entity: "JournalEntry", params: &["company", "from_date", "to_date"] },
    Def {
        name: "general_ledger",
        label: "General Ledger",
        module: "accounting",
        entity: "JournalEntry",
        params: &["company", "from_date", "to_date", "account", "customer", "supplier"],
    },
    Def { name: "profit_and_loss", label: "Profit and Loss", module: "accounting", entity: "JournalEntry", params: &["company", "from_date", "to_date"] },
    Def { name: "balance_sheet", label: "Balance Sheet", module: "accounting", entity: "JournalEntry", params: &["company", "as_of"] },
    Def { name: "receivables", label: "Accounts Receivable", module: "selling", entity: "SalesInvoice", params: &["company", "as_of", "customer"] },
    Def { name: "payables", label: "Accounts Payable", module: "buying", entity: "PurchaseInvoice", params: &["company", "as_of", "supplier"] },
    Def { name: "stock_balance", label: "Stock Balance", module: "inventory", entity: "StockEntry", params: &["company", "as_of", "warehouse", "item"] },
    Def {
        name: "stock_ledger",
        label: "Stock Ledger",
        module: "inventory",
        entity: "StockEntry",
        params: &["company", "from_date", "to_date", "warehouse", "item"],
    },
    Def {
        name: "sales_register",
        label: "Sales Register",
        module: "selling",
        entity: "SalesInvoice",
        params: &["company", "from_date", "to_date", "customer"],
    },
    Def {
        name: "purchase_register",
        label: "Purchase Register",
        module: "buying",
        entity: "PurchaseInvoice",
        params: &["company", "from_date", "to_date", "supplier"],
    },
];

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

/// A read-only, tenant-isolated transaction with the report timeout, for report queries.
pub async fn read_tx(k: &Kernel, ctx: &Ctx) -> Result<Tx> {
    let mut tx = k.db.tenant_tx_with_timeout(ctx.tenant, REPORT_TIMEOUT_MS).await?;
    sqlx::query("set transaction read only").execute(&mut *tx).await?;
    Ok(tx)
}

/// Reports contributed by packages outside this crate.
#[async_trait::async_trait]
pub trait ReportSource: Send + Sync {
    /// `[{name, label, module, entity, params}]`: `entity` is the one the caller must be able to read.
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

fn sources() -> Vec<std::sync::Arc<dyn ReportSource>> {
    SOURCES.read().map(|s| s.clone()).unwrap_or_default()
}

pub fn catalog() -> Value {
    let mut list: Vec<Value> = DEFS
        .iter()
        .map(|d| json!({"name": d.name, "label": d.label, "module": d.module, "params": d.params.iter().map(|p| param_def(p)).collect::<Vec<_>>()}))
        .collect();
    for s in sources() {
        list.extend(s.catalog().into_iter().map(|mut e| {
            if let Value::Object(m) = &mut e {
                m.remove("entity");
            }
            e
        }));
    }
    Value::Array(list)
}

/// Runs a report from a package source after checking the caller may read its entity.
async fn run_source(k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
    for s in sources() {
        let Some(entry) = s.catalog().into_iter().find(|e| e["name"] == json!(name)) else { continue };
        let entity = entry["entity"].as_str().unwrap_or_default().to_string();
        if ctx.schema.entity(&entity).is_none() {
            return Err(Error::NotFound(format!("report {name} (its package is not installed)")));
        }
        ctx.require(&entity, Action::Read)?;
        let mut out = s.run(k, ctx, name, raw).await?;
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
    // Periods and "as of" dates default to the workspace's today, for packages' reports too.
    TODAY.scope(ctx.today(), run_report(k, ctx, name, raw)).await
}

async fn run_report(k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
    let Some(def) = DEFS.iter().find(|d| d.name == name) else { return run_source(k, ctx, name, raw).await };
    if ctx.schema.entity(def.entity).is_none() {
        return Err(Error::NotFound(format!("report {name} (its package is not installed)")));
    }
    ctx.require(def.entity, Action::Read)?;
    let p = Params { raw };
    let mut tx = k.db.tenant_tx_with_timeout(ctx.tenant, REPORT_TIMEOUT_MS).await?;
    sqlx::query("set transaction read only").execute(&mut *tx).await?;
    let co = resolve_company(&mut tx, ctx, &p).await?;
    let mut out = match name {
        "trial_balance" => trial_balance(&mut tx, ctx, &co, &p).await?,
        "general_ledger" => general_ledger(&mut tx, ctx, &co, &p).await?,
        "profit_and_loss" => profit_and_loss(&mut tx, ctx, &co, &p).await?,
        "balance_sheet" => balance_sheet(&mut tx, ctx, &co, &p).await?,
        "receivables" => ageing(&mut tx, ctx, &co, &p, "customer").await?,
        "payables" => ageing(&mut tx, ctx, &co, &p, "supplier").await?,
        "stock_balance" => stock_balance(&mut tx, ctx, &co, &p).await?,
        "stock_ledger" => stock_ledger(&mut tx, ctx, &co, &p).await?,
        "sales_register" => register(&mut tx, ctx, &co, &p, "customer").await?,
        "purchase_register" => register(&mut tx, ctx, &co, &p, "supplier").await?,
        _ => return Err(Error::NotFound(format!("report {name}"))),
    };
    tx.rollback().await?;
    if let Value::Object(m) = &mut out {
        m.insert("name".into(), json!(def.name));
        m.insert("label".into(), json!(def.label));
        let meta = m.entry("meta").or_insert_with(|| json!({}));
        if let Value::Object(meta) = meta {
            meta.insert("company".into(), json!(co.name));
            meta.insert("currency".into(), json!(co.currency));
        }
    }
    Ok(out)
}

// ───────────────────────────── accounts ─────────────────────────────

#[derive(sqlx::FromRow, Clone)]
struct Acc {
    id: Uuid,
    parent_account: Option<Uuid>,
    account_name: String,
    account_number: Option<String>,
    is_group: Option<bool>,
    root_type: String,
}

fn natural(root: &str, debit_minus_credit: Decimal) -> Decimal {
    if matches!(root, "Asset" | "Expense") { debit_minus_credit } else { -debit_minus_credit }
}

fn label(a: &Acc) -> String {
    match &a.account_number {
        Some(n) if !n.is_empty() => format!("{n} - {}", a.account_name),
        _ => a.account_name.clone(),
    }
}

async fn accounts(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany) -> Result<Vec<Acc>> {
    Ok(sqlx::query_as::<_, Acc>(
        "select id, parent_account, account_name, account_number, is_group, root_type from e_account where tenant_id = $1 and company = $2 \
         order by account_number nulls last, account_name",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .fetch_all(&mut **tx)
    .await?)
}

/// Debit-minus-credit per account over `[from, to]` (from = None: since the beginning).
async fn balances(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, from: Option<NaiveDate>, to: NaiveDate) -> Result<HashMap<Uuid, Decimal>> {
    balances_where(tx, ctx, co, from, to, &[]).await
}

/// Entries that close a year's income and expense into equity (the Finance module's Period
/// Closing Voucher). An income statement leaves them out, or a closed year would show no profit;
/// balance sheets and trial balances keep them.
pub const CLOSING_SOURCES: &[&str] = &["PeriodClosingVoucher"];

/// Account balances over a period, leaving out entries from the `skip` source entities.
async fn balances_where(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, from: Option<NaiveDate>, to: NaiveDate, skip: &[&str]) -> Result<HashMap<Uuid, Decimal>> {
    Ok(sqlx::query_as::<_, (Uuid, Decimal)>(
        "select l.account_id, sum(l.debit - l.credit) from journal_line l where l.tenant_id = $1 and l.company_id = $2 \
         and ($3::date is null or l.posting_date >= $3) and l.posting_date <= $4 \
         and (cardinality($5::text[]) = 0 or not exists (select 1 from journal_entry e where e.tenant_id = l.tenant_id and e.id = l.entry_id and e.source_entity = any($5))) \
         group by l.account_id",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(skip.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect())
}

/// Indented account tree for the given root types with balances rolled up to groups.
fn tree(accs: &[Acc], bal: &HashMap<Uuid, Decimal>, root: &str, precision: u32) -> (Vec<Value>, Decimal) {
    let mut children: HashMap<Option<Uuid>, Vec<&Acc>> = HashMap::new();
    for a in accs.iter().filter(|a| a.root_type == root) {
        let parent = a.parent_account.filter(|p| accs.iter().any(|x| x.id == *p && x.root_type == root));
        children.entry(parent).or_default().push(a);
    }
    fn total(a: &Acc, children: &HashMap<Option<Uuid>, Vec<&Acc>>, bal: &HashMap<Uuid, Decimal>, depth: usize) -> Decimal {
        if depth > 64 {
            return Decimal::ZERO;
        }
        let own = natural(&a.root_type, bal.get(&a.id).copied().unwrap_or_default());
        own + children.get(&Some(a.id)).map(|cs| cs.iter().map(|c| total(c, children, bal, depth + 1)).sum::<Decimal>()).unwrap_or_default()
    }
    fn walk(a: &Acc, depth: usize, children: &HashMap<Option<Uuid>, Vec<&Acc>>, bal: &HashMap<Uuid, Decimal>, precision: u32, out: &mut Vec<Value>) {
        if depth > 64 {
            return;
        }
        let t = total(a, children, bal, depth);
        let has_activity =
            bal.contains_key(&a.id) || children.get(&Some(a.id)).is_some_and(|cs| cs.iter().any(|c| total(c, children, bal, depth + 1) != Decimal::ZERO));
        if t.is_zero() && !has_activity {
            return;
        }
        out.push(
            json!({"account": label(a), "account_id": a.id, "amount": money(t.round_dp(precision)), "_indent": depth, "_bold": a.is_group.unwrap_or(false)}),
        );
        for c in children.get(&Some(a.id)).cloned().unwrap_or_default() {
            walk(c, depth + 1, children, bal, precision, out);
        }
    }
    let mut out = Vec::new();
    let mut sum = Decimal::ZERO;
    for r in children.get(&None).cloned().unwrap_or_default() {
        sum += total(r, &children, bal, 0);
        walk(r, 0, &children, bal, precision, &mut out);
    }
    (out, sum)
}

async fn trial_balance(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let year_start = fy_start(from, co.fy_month);
    let accs = accounts(tx, ctx, co).await?;
    let before = balances(tx, ctx, co, None, from.pred_opt().unwrap_or(from)).await?;
    // Income and expense restart every fiscal year; earlier years show as one equity line.
    let pl_this_year = balances(tx, ctx, co, Some(year_start), from.pred_opt().unwrap_or(from)).await?;
    let movement: HashMap<Uuid, (Decimal, Decimal)> = sqlx::query_as::<_, (Uuid, Decimal, Decimal)>(
        "select account_id, sum(debit), sum(credit) from journal_line where tenant_id = $1 and company_id = $2 and posting_date between $3 and $4 group by account_id",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|(a, dr, cr)| (a, (dr, cr)))
    .collect();
    let has_before = from > NaiveDate::MIN;
    let mut rows = Vec::new();
    let mut totals = [Decimal::ZERO; 6];
    let mut prior_pl = Decimal::ZERO;
    let mut push = |rows: &mut Vec<Value>, name: String, root: &str, opening: Decimal, dr: Decimal, cr: Decimal| {
        let closing = opening + dr - cr;
        let cells = [opening.max(Decimal::ZERO), (-opening).max(Decimal::ZERO), dr, cr, closing.max(Decimal::ZERO), (-closing).max(Decimal::ZERO)];
        for (t, c) in totals.iter_mut().zip(cells) {
            *t += c;
        }
        rows.push(json!({
            "account": name, "root_type": root,
            "opening_debit": money(cells[0]), "opening_credit": money(cells[1]),
            "debit": money(cells[2]), "credit": money(cells[3]),
            "closing_debit": money(cells[4]), "closing_credit": money(cells[5]),
        }));
    };
    for a in accs.iter().filter(|a| !a.is_group.unwrap_or(false)) {
        let pl = matches!(a.root_type.as_str(), "Income" | "Expense");
        let opening = if pl {
            let all = before.get(&a.id).copied().unwrap_or_default();
            let this = pl_this_year.get(&a.id).copied().unwrap_or_default();
            prior_pl += all - this;
            this
        } else {
            before.get(&a.id).copied().unwrap_or_default()
        };
        let (dr, cr) = movement.get(&a.id).copied().unwrap_or_default();
        if opening.is_zero() && dr.is_zero() && cr.is_zero() {
            continue;
        }
        push(&mut rows, label(a), &a.root_type, opening, dr, cr);
    }
    if has_before && !prior_pl.is_zero() {
        push(&mut rows, "Profit/loss of earlier fiscal years (not yet closed)".into(), "Equity", prior_pl, Decimal::ZERO, Decimal::ZERO);
    }
    let names = ["opening_debit", "opening_credit", "debit", "credit", "closing_debit", "closing_credit"];
    let total_obj: serde_json::Map<String, Value> = names.iter().zip(totals).map(|(n, t)| (n.to_string(), money(t))).collect();
    Ok(json!({
        "columns": [col("account", "Account", "data"), col("root_type", "Type", "data"), col("opening_debit", "Opening (Dr)", "currency"),
            col("opening_credit", "Opening (Cr)", "currency"), col("debit", "Debit", "currency"), col("credit", "Credit", "currency"),
            col("closing_debit", "Closing (Dr)", "currency"), col("closing_credit", "Closing (Cr)", "currency")],
        "rows": rows,
        "totals": total_obj,
        "meta": {"from_date": from, "to_date": to, "balanced": totals[0] == totals[1] && totals[2] == totals[3] && totals[4] == totals[5]},
    }))
}

async fn general_ledger(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let (account, customer, supplier) = (p.uuid("account")?, p.uuid("customer")?, p.uuid("supplier")?);
    let opening: Decimal = sqlx::query_scalar(
        "select coalesce(sum(debit - credit), 0) from journal_line where tenant_id = $1 and company_id = $2 and posting_date < $3 \
         and ($4::uuid is null or account_id = $4) and ($5::uuid is null or customer_id = $5) and ($6::uuid is null or supplier_id = $6)",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(account)
    .bind(customer)
    .bind(supplier)
    .fetch_one(&mut **tx)
    .await?;
    #[allow(clippy::type_complexity)]
    let lines: Vec<(NaiveDate, String, String, Option<String>, Uuid, Option<String>, Decimal, Decimal, Option<String>)> = sqlx::query_as(
        "select l.posting_date, coalesce(a.account_number || ' - ', '') || a.account_name, e.source_entity, e.source_number, e.source_id, \
         coalesce(c.customer_name, s.supplier_name), l.debit, l.credit, l.remarks \
         from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
         join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
         left join e_customer c on c.tenant_id = l.tenant_id and c.id = l.customer_id \
         left join e_supplier s on s.tenant_id = l.tenant_id and s.id = l.supplier_id \
         where l.tenant_id = $1 and l.company_id = $2 and l.posting_date between $3 and $4 \
         and ($5::uuid is null or l.account_id = $5) and ($6::uuid is null or l.customer_id = $6) and ($7::uuid is null or l.supplier_id = $7) \
         order by l.posting_date, e.created_at, l.entry_id, l.line_no limit $8",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(account)
    .bind(customer)
    .bind(supplier)
    .bind(MAX_ROWS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let truncated = lines.len() as i64 > MAX_ROWS;
    let mut balance = opening;
    let mut rows = vec![
        json!({"posting_date": from, "account": "Opening", "debit": money(opening.max(Decimal::ZERO)), "credit": money((-opening).max(Decimal::ZERO)), "balance": money(opening), "_bold": true}),
    ];
    let (mut tdr, mut tcr) = (Decimal::ZERO, Decimal::ZERO);
    for (day, acc, entity, number, sid, party, dr, cr, remarks) in lines.into_iter().take(MAX_ROWS as usize) {
        balance += dr - cr;
        tdr += dr;
        tcr += cr;
        rows.push(json!({"posting_date": day, "account": acc, "voucher_type": entity, "voucher_no": number, "voucher_id": sid, "party": party,
            "debit": money(dr), "credit": money(cr), "balance": money(balance), "remarks": remarks}));
    }
    rows.push(json!({"account": "Closing", "debit": money(tdr), "credit": money(tcr), "balance": money(balance), "_bold": true}));
    Ok(json!({
        "columns": [col("posting_date", "Date", "date"), col("account", "Account", "data"), col("voucher_type", "Voucher Type", "data"),
            col("voucher_no", "Voucher No", "data"), col("party", "Party", "data"), col("debit", "Debit", "currency"), col("credit", "Credit", "currency"),
            col("balance", "Balance (Dr - Cr)", "currency"), col("remarks", "Remarks", "data")],
        "rows": rows,
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

async fn profit_and_loss(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let accs = accounts(tx, ctx, co).await?;
    let bal = balances_where(tx, ctx, co, Some(from), to, CLOSING_SOURCES).await?;
    let (inc_rows, income) = tree(&accs, &bal, "Income", co.precision);
    let (exp_rows, expense) = tree(&accs, &bal, "Expense", co.precision);
    let mut rows = vec![json!({"account": "Income", "_bold": true})];
    rows.extend(inc_rows.into_iter().map(|mut r| {
        indent(&mut r);
        r
    }));
    rows.push(json!({"account": "Total Income", "amount": money(income), "_bold": true}));
    rows.push(json!({"account": "Expenses", "_bold": true}));
    rows.extend(exp_rows.into_iter().map(|mut r| {
        indent(&mut r);
        r
    }));
    rows.push(json!({"account": "Total Expenses", "amount": money(expense), "_bold": true}));
    let net = income - expense;
    rows.push(json!({"account": if net >= Decimal::ZERO { "Net Profit" } else { "Net Loss" }, "amount": money(net), "_bold": true}));
    Ok(json!({
        "columns": [col("account", "Account", "data"), col("amount", "Amount", "currency")],
        "rows": rows,
        "totals": {"income": money(income), "expense": money(expense), "net_profit": money(net)},
        "meta": {"from_date": from, "to_date": to},
    }))
}

fn indent(r: &mut Value) {
    if let Some(i) = r.get("_indent").and_then(Value::as_u64) {
        r["_indent"] = json!(i + 1);
    }
}

async fn balance_sheet(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let as_of = p.date("as_of")?.unwrap_or_else(today);
    let accs = accounts(tx, ctx, co).await?;
    let bal = balances(tx, ctx, co, None, as_of).await?;
    let (asset_rows, assets) = tree(&accs, &bal, "Asset", co.precision);
    let (liab_rows, liabilities) = tree(&accs, &bal, "Liability", co.precision);
    let (eq_rows, equity) = tree(&accs, &bal, "Equity", co.precision);
    let (_, income) = tree(&accs, &bal, "Income", co.precision);
    let (_, expense) = tree(&accs, &bal, "Expense", co.precision);
    let unclosed = income - expense;
    let mut rows = vec![json!({"account": "Assets", "_bold": true})];
    rows.extend(asset_rows.into_iter().map(|mut r| {
        indent(&mut r);
        r
    }));
    rows.push(json!({"account": "Total Assets", "amount": money(assets), "_bold": true}));
    rows.push(json!({"account": "Liabilities", "_bold": true}));
    rows.extend(liab_rows.into_iter().map(|mut r| {
        indent(&mut r);
        r
    }));
    rows.push(json!({"account": "Total Liabilities", "amount": money(liabilities), "_bold": true}));
    rows.push(json!({"account": "Equity", "_bold": true}));
    rows.extend(eq_rows.into_iter().map(|mut r| {
        indent(&mut r);
        r
    }));
    rows.push(json!({"account": "Profit/loss not yet closed to equity", "amount": money(unclosed), "_indent": 1}));
    rows.push(json!({"account": "Total Equity", "amount": money(equity + unclosed), "_bold": true}));
    let total = liabilities + equity + unclosed;
    rows.push(json!({"account": "Total Liabilities and Equity", "amount": money(total), "_bold": true}));
    Ok(json!({
        "columns": [col("account", "Account", "data"), col("amount", "Amount", "currency")],
        "rows": rows,
        "totals": {"assets": money(assets), "liabilities": money(liabilities), "equity": money(equity + unclosed)},
        "meta": {"as_of": as_of, "balanced": assets == total, "difference": money(assets - total)},
    }))
}

// ─────────────────────────── receivables / payables ───────────────────────────

async fn ageing(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>, kind: &str) -> Result<Value> {
    let as_of = p.date("as_of")?.unwrap_or_else(today);
    let party = p.uuid(kind)?;
    #[allow(clippy::type_complexity)]
    let items: Vec<(Uuid, String, Option<String>, NaiveDate, Option<NaiveDate>, Option<String>, String, Decimal, Decimal, Decimal)> = sqlx::query_as(
        "select o.id, o.entity, o.number, o.posting_date, o.due_date, coalesce(c.customer_name, s.supplier_name), o.currency, o.amount, \
         o.amount - coalesce((select sum(x.amount) from allocation x where x.tenant_id = o.tenant_id and x.open_item_id = o.id and x.posting_date <= $3), 0), \
         o.exchange_rate \
         from open_item o left join e_customer c on c.tenant_id = o.tenant_id and c.id = o.customer_id \
         left join e_supplier s on s.tenant_id = o.tenant_id and s.id = o.supplier_id \
         where o.tenant_id = $1 and o.company_id = $2 and o.party_kind = $4 and o.posting_date <= $3 and o.status <> 'cancelled' \
         and ($5::uuid is null or o.customer_id = $5 or o.supplier_id = $5) \
         order by coalesce(o.due_date, o.posting_date), o.posting_date limit $6",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(as_of)
    .bind(kind)
    .bind(party)
    .bind(MAX_ROWS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let truncated = items.len() as i64 > MAX_ROWS;
    let buckets = ["not_due", "d1_30", "d31_60", "d61_90", "d90_plus"];
    let mut totals: BTreeMap<&str, Decimal> = buckets.iter().map(|b| (*b, Decimal::ZERO)).collect();
    let mut total_outstanding = Decimal::ZERO;
    let mut rows = Vec::new();
    for (oid, entity, number, posted, due, name, currency, amount, outstanding, rate) in items.into_iter().take(MAX_ROWS as usize) {
        if outstanding.is_zero() {
            continue;
        }
        let base = crate::util::round(outstanding * rate, co.precision);
        let days = (as_of - due.unwrap_or(posted)).num_days();
        let bucket = match days {
            d if d <= 0 => "not_due",
            1..=30 => "d1_30",
            31..=60 => "d31_60",
            61..=90 => "d61_90",
            _ => "d90_plus",
        };
        *totals.entry(bucket).or_default() += base;
        total_outstanding += base;
        let mut row = json!({"party": name, "voucher_type": entity, "voucher_no": number, "voucher_id": oid, "posting_date": posted, "due_date": due,
            "currency": currency, "invoice_amount": money(amount), "outstanding": money(outstanding), "outstanding_base": money(base), "days_overdue": days.max(0)});
        for b in buckets {
            row[b] = money(if b == bucket { base } else { Decimal::ZERO });
        }
        rows.push(row);
    }
    let mut total_obj = serde_json::Map::new();
    total_obj.insert("outstanding_base".into(), money(total_outstanding));
    for (b, v) in totals {
        total_obj.insert(b.into(), money(v));
    }
    Ok(json!({
        "columns": [col("party", if kind == "customer" { "Customer" } else { "Supplier" }, "data"), col("voucher_no", "Voucher", "data"),
            col("posting_date", "Date", "date"), col("due_date", "Due Date", "date"), col("currency", "Currency", "data"),
            col("invoice_amount", "Invoice Amount", "currency"), col("outstanding", "Outstanding", "currency"),
            col("outstanding_base", "Outstanding (Company Currency)", "currency"), col("days_overdue", "Days Overdue", "int"),
            col("not_due", "Not Due", "currency"), col("d1_30", "1-30", "currency"), col("d31_60", "31-60", "currency"),
            col("d61_90", "61-90", "currency"), col("d90_plus", "90+", "currency")],
        "rows": rows,
        "totals": total_obj,
        "meta": {"as_of": as_of, "truncated": truncated},
    }))
}

// ───────────────────────────── stock ─────────────────────────────

async fn stock_balance(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (wh, item) = (p.uuid("warehouse")?, p.uuid("item")?);
    let as_of = p.date("as_of")?;
    let sql = if as_of.is_some() {
        // Historical: sum the append-only moves up to the date.
        "select i.item_code, i.item_name, w.warehouse_name, sum(m.qty), sum(m.value) from stock_move m \
         join e_item i on i.tenant_id = m.tenant_id and i.id = m.item_id join e_warehouse w on w.tenant_id = m.tenant_id and w.id = m.warehouse_id \
         where m.tenant_id = $1 and m.company_id = $2 and ($3::uuid is null or m.warehouse_id = $3) and ($4::uuid is null or m.item_id = $4) \
         and m.posting_date <= $5::date group by i.item_code, i.item_name, w.warehouse_name having sum(m.qty) <> 0 or sum(m.value) <> 0 \
         order by i.item_code, w.warehouse_name limit $6"
    } else {
        "select i.item_code, i.item_name, w.warehouse_name, b.qty, b.value from stock_balance b \
         join e_item i on i.tenant_id = b.tenant_id and i.id = b.item_id join e_warehouse w on w.tenant_id = b.tenant_id and w.id = b.warehouse_id \
         where b.tenant_id = $1 and b.company_id = $2 and ($3::uuid is null or b.warehouse_id = $3) and ($4::uuid is null or b.item_id = $4) \
         and $5::date is null and (b.qty <> 0 or b.value <> 0) order by i.item_code, w.warehouse_name limit $6"
    };
    let data: Vec<(String, String, String, Decimal, Decimal)> =
        sqlx::query_as(sql).bind(ctx.tenant).bind(co.id).bind(wh).bind(item).bind(as_of).bind(MAX_ROWS + 1).fetch_all(&mut **tx).await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let (mut tq, mut tv) = (Decimal::ZERO, Decimal::ZERO);
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|(code, name, whn, qty, value)| {
            tq += qty;
            tv += value;
            let rate = if qty.is_zero() { Decimal::ZERO } else { (value / qty).round_dp(co.precision + 2) };
            json!({"item_code": code, "item_name": name, "warehouse": whn, "qty": money(qty), "valuation_rate": money(rate), "value": money(value)})
        })
        .collect();
    Ok(json!({
        "columns": [col("item_code", "Item", "data"), col("item_name", "Item Name", "data"), col("warehouse", "Warehouse", "data"),
            col("qty", "Quantity", "decimal"), col("valuation_rate", "Valuation Rate", "currency"), col("value", "Value", "currency")],
        "rows": rows,
        "totals": {"qty": money(tq), "value": money(tv)},
        "meta": {"as_of": as_of, "truncated": truncated},
    }))
}

async fn stock_ledger(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let (wh, item) = (p.uuid("warehouse")?, p.uuid("item")?);
    #[allow(clippy::type_complexity)]
    let data: Vec<(NaiveDate, String, String, Decimal, Decimal, Decimal, Decimal, Decimal, String, Uuid, Option<String>, Option<String>, bool)> = sqlx::query_as(
        "select m.posting_date, i.item_code, w.warehouse_name, m.qty, m.valuation_rate, m.value, m.qty_after, m.value_after, m.source_entity, m.source_id, \
         m.batch_no, m.serial_no, m.reversal_of is not null \
         from stock_move m join e_item i on i.tenant_id = m.tenant_id and i.id = m.item_id join e_warehouse w on w.tenant_id = m.tenant_id and w.id = m.warehouse_id \
         where m.tenant_id = $1 and m.company_id = $2 and m.posting_date between $3 and $4 and ($5::uuid is null or m.warehouse_id = $5) \
         and ($6::uuid is null or m.item_id = $6) order by m.posting_ts, m.seq limit $7",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(wh)
    .bind(item)
    .bind(MAX_ROWS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|(day, code, whn, qty, rate, value, qa, va, entity, sid, batch, serial, reversal)| {
            json!({"posting_date": day, "item_code": code, "warehouse": whn, "qty": money(qty), "valuation_rate": money(rate.round_dp(9)), "value": money(value),
                "qty_after": money(qa), "value_after": money(va), "voucher_type": entity, "voucher_id": sid, "batch_no": batch, "serial_no": serial, "is_reversal": reversal})
        })
        .collect();
    Ok(json!({
        "columns": [col("posting_date", "Date", "date"), col("item_code", "Item", "data"), col("warehouse", "Warehouse", "data"), col("qty", "Qty", "decimal"),
            col("valuation_rate", "Rate", "currency"), col("value", "Value", "currency"), col("qty_after", "Balance Qty", "decimal"),
            col("value_after", "Balance Value", "currency"), col("voucher_type", "Voucher Type", "data"), col("batch_no", "Batch", "data"),
            col("serial_no", "Serial No", "data"), col("is_reversal", "Reversal", "check")],
        "rows": rows,
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

// ───────────────────────────── registers ─────────────────────────────

async fn register(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>, kind: &str) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let party = p.uuid(kind)?;
    let (table, party_name, extra) =
        if kind == "customer" { ("e_sales_invoice", "customer_name", "po_no") } else { ("e_purchase_invoice", "supplier_name", "bill_no") };
    #[allow(clippy::type_complexity)]
    let data: Vec<(Uuid, Option<String>, NaiveDate, Option<String>, Option<String>, Option<Decimal>, Option<Decimal>, Option<Decimal>, Option<Decimal>, Option<Decimal>, Option<String>, Option<bool>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select i.id, i.number, i.posting_date, i.{party_name}, i.{extra}, i.net_total, i.total_taxes, i.grand_total, i.base_grand_total, i.outstanding_amount, \
             i.status, i.is_return from {table} i where i.tenant_id = $1 and i.company = $2 and i.docstatus = 1 and i.posting_date between $3 and $4 \
             and ($5::uuid is null or i.{kind} = $5) order by i.posting_date, i.number limit $6"
        )))
        .bind(ctx.tenant)
        .bind(co.id)
        .bind(from)
        .bind(to)
        .bind(party)
        .bind(MAX_ROWS + 1)
        .fetch_all(&mut **tx)
        .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let mut total_base = Decimal::ZERO;
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|(iid, number, day, name, reference, net, taxes, grand, base, outstanding, status, ret)| {
            total_base += base.unwrap_or_default();
            json!({"voucher_id": iid, "voucher_no": number, "posting_date": day, "party": name, "reference": reference,
                "net_total": money(net.unwrap_or_default()), "total_taxes": money(taxes.unwrap_or_default()), "grand_total": money(grand.unwrap_or_default()),
                "base_grand_total": money(base.unwrap_or_default()), "outstanding_amount": money(outstanding.unwrap_or_default()), "status": status,
                "is_return": ret.unwrap_or(false)})
        })
        .collect();
    let count = rows.len();
    Ok(json!({
        "columns": [col("voucher_no", "Invoice", "data"), col("posting_date", "Date", "date"), col("party", if kind == "customer" { "Customer" } else { "Supplier" }, "data"),
            col("reference", if kind == "customer" { "Customer PO" } else { "Supplier Invoice No" }, "data"), col("net_total", "Net Total", "currency"),
            col("total_taxes", "Taxes", "currency"), col("grand_total", "Grand Total", "currency"), col("base_grand_total", "Grand Total (Company Currency)", "currency"),
            col("outstanding_amount", "Outstanding", "currency"), col("status", "Status", "data")],
        "rows": rows,
        "totals": {"base_grand_total": money(total_base), "count": count},
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
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

    #[test]
    fn catalog_lists_every_report_with_params() {
        let c = catalog();
        assert_eq!(c.as_array().unwrap().len(), DEFS.len());
        assert!(c[0]["params"].as_array().unwrap().iter().any(|p| p["name"] == "company"));
    }
}
