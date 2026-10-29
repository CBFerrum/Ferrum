//! Budgets: annual amounts per income or expense account for a cost center (with its children)
//! or a project, phased over the months by a monthly distribution, and the checks guarding them.
//!
//! What a budget has consumed is what the ledger booked on its account (never the year-end
//! closing entry) plus, when the budget tracks them, the commitments of open purchase orders
//! (their unbilled part) and of material requests (their unordered part). A document is checked
//! once its own effect is in place inside its submit transaction: purchase orders and material
//! requests once they count as open commitments, and any document posting expense (journal
//! entries, purchase invoices, stock entries, ...) once the ledger holds its lines - so the check
//! sees exactly what was posted, a billed order is never counted twice, and a refused document
//! leaves nothing behind. A transaction lock per budget makes concurrent submits check in turn.

use crate::common::{FiscalYear, cost_center_ancestors, cost_center_subtree, fiscal_year, fiscal_year_of, fmt, installed, is_new, notify_user};
use crate::rules::{self, MONTHS};
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_domain::util::{b, company, d, get, id, money, round, rows, s, set};
use ferrum_kernel::store::field_expr;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, HookCtx, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

// ───────────────────────────── monthly distribution ─────────────────────────────

pub struct DistributionHook;

#[async_trait]
impl DocHook for DistributionHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let mut pcts = rows(doc, "percentages");
        if pcts.is_empty() {
            pcts = MONTHS
                .iter()
                .zip(rules::even_distribution())
                .map(|(m, p)| {
                    let mut r = Doc::new();
                    r.insert("month".into(), json!(m));
                    r.insert("percentage_allocation".into(), money(p));
                    r
                })
                .collect();
        }
        let total: Decimal = pcts.iter().map(|r| d(r, "percentage_allocation")).sum();
        set(doc, "percentages", crate::common::child_rows(pcts));
        set(doc, "total_percentage", money(total));
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let pcts = rows(doc, "percentages");
        let mut seen = HashSet::new();
        let mut errors = Vec::new();
        for (i, r) in pcts.iter().enumerate() {
            if !seen.insert(s(r, "month").unwrap_or_default().to_string()) {
                errors.push(FieldError::new("month", "appears twice").in_row("percentages", i));
            }
        }
        if pcts.len() != 12 || seen.len() != 12 {
            errors.push(FieldError::new("percentages", "give one percentage for each of the twelve months"));
        }
        let total = d(doc, "total_percentage");
        if total != Decimal::ONE_HUNDRED {
            errors.push(FieldError::new("percentages", format!("the percentages add up to {}%, not 100%", total.normalize())));
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }
}

/// The twelve percentages of a distribution, indexed by calendar month (January = 0).
async fn distribution(tx: &mut Tx, tenant: Uuid, id: Uuid) -> Result<Option<[Decimal; 12]>> {
    let rows: Vec<(String, Decimal)> =
        sqlx::query_as("select p.month, p.percentage_allocation from e_monthly_distribution__percentages p where p.tenant_id = $1 and p.parent_id = $2")
            .bind(tenant)
            .bind(id)
            .fetch_all(&mut **tx)
            .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut out = [Decimal::ZERO; 12];
    for (m, p) in rows {
        if let Some(i) = MONTHS.iter().position(|x| *x == m) {
            out[i] = p;
        }
    }
    Ok(Some(out))
}

// ───────────────────────────── budget ─────────────────────────────

pub struct BudgetHook;

#[async_trait]
impl DocHook for BudgetHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if s(doc, "budget_against") == Some("Project") {
            set(doc, "cost_center", Value::Null);
        } else {
            set(doc, "project", Value::Null);
        }
        if !["applicable_on_material_request", "applicable_on_purchase_order", "applicable_on_booking_actual_expenses"].iter().any(|f| b(doc, f)) {
            set(doc, "applicable_on_booking_actual_expenses", true);
        }
        if let Some(cid) = id(doc, "company") {
            let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
            let total: Decimal = rows(doc, "accounts").iter().map(|r| round(d(r, "budget_amount"), co.precision)).sum();
            set(doc, "total_budget", money(total));
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let mut errors = Vec::new();
        if let Some(fy) = id(doc, "fiscal_year")
            && fiscal_year(hc.tx, hc.ctx.tenant, fy).await?.is_some_and(|(c, _)| c != cid)
        {
            errors.push(FieldError::new("fiscal_year", "belongs to another company"));
        }
        for (field, entity) in [("cost_center", "CostCenter"), ("project", "Project")] {
            if let Some(x) = id(doc, field) {
                let target = get(hc.kernel, hc.ctx, hc.tx, entity, x).await?;
                if id(&target, "company") != Some(cid) {
                    errors.push(FieldError::new(field, "belongs to another company"));
                }
            }
        }
        let mut accounts = HashSet::new();
        for (i, r) in rows(doc, "accounts").iter().enumerate() {
            let Some(acc) = id(r, "account") else { continue };
            if !accounts.insert(acc) {
                errors.push(FieldError::new("account", "is entered twice").in_row("accounts", i));
                continue;
            }
            let a = get(hc.kernel, hc.ctx, hc.tx, "Account", acc).await?;
            if id(&a, "company") != Some(cid) {
                errors.push(FieldError::new("account", "belongs to another company").in_row("accounts", i));
            } else if b(&a, "is_group") {
                errors.push(FieldError::new("account", "budgets are set on ledger accounts, not groups").in_row("accounts", i));
            } else if !matches!(s(&a, "root_type"), Some("Income" | "Expense")) {
                errors.push(FieldError::new("account", "budgets are set on income or expense accounts").in_row("accounts", i));
            }
        }
        let (mr, po, actual) =
            (b(doc, "applicable_on_material_request"), b(doc, "applicable_on_purchase_order"), b(doc, "applicable_on_booking_actual_expenses"));
        if mr && !(po && actual) {
            errors.push(FieldError::new(
                "applicable_on_material_request",
                "material requests turn into purchase orders and then expenses: make the budget applicable on purchase orders and actual expenses too",
            ));
        } else if po && !actual {
            errors
                .push(FieldError::new("applicable_on_purchase_order", "purchase orders turn into expenses: make the budget applicable on actual expenses too"));
        }
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        let ids: Vec<Uuid> = accounts.into_iter().collect();
        let clash: Option<(Option<String>, String)> = sqlx::query_as(
            "select b.number, coalesce(a2.account_number || ' - ', '') || a2.account_name from e_budget b \
             join e_budget__accounts a on a.tenant_id = b.tenant_id and a.parent_id = b.id \
             join e_account a2 on a2.tenant_id = a.tenant_id and a2.id = a.account \
             where b.tenant_id = $1 and b.company = $2 and b.fiscal_year = $3 and b.docstatus < 2 and b.id <> $4 and b.budget_against = $5 \
             and b.cost_center is not distinct from $6 and b.project is not distinct from $7 and a.account = any($8) limit 1",
        )
        .bind(hc.ctx.tenant)
        .bind(cid)
        .bind(id(doc, "fiscal_year"))
        .bind(id(doc, "id"))
        .bind(s(doc, "budget_against").unwrap_or("Cost Center"))
        .bind(id(doc, "cost_center"))
        .bind(id(doc, "project"))
        .bind(&ids)
        .fetch_optional(&mut **hc.tx)
        .await?;
        if let Some((number, account)) = clash {
            return Err(Error::field(
                "accounts",
                format!("budget {} already covers {account} for this fiscal year and {}", number.unwrap_or_else(|| "(draft)".into()), against_word(doc)),
            ));
        }
        Ok(())
    }
}

fn against_word(doc: &Doc) -> &'static str {
    if s(doc, "budget_against") == Some("Project") { "project" } else { "cost center" }
}

// ───────────────────────────── checks ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Actual,
    PurchaseOrder,
    MaterialRequest,
}

impl Kind {
    fn applies(self, b: &BudgetRow) -> bool {
        match self {
            Kind::Actual => b.applicable_on_booking_actual_expenses,
            Kind::PurchaseOrder => b.applicable_on_purchase_order,
            Kind::MaterialRequest => b.applicable_on_material_request,
        }
    }

    /// (annual action, accumulated monthly action)
    fn actions(self, b: &BudgetRow) -> (Option<String>, Option<String>) {
        match self {
            Kind::Actual => (b.action_if_annual_budget_exceeded.clone(), b.action_if_accumulated_monthly_budget_exceeded.clone()),
            Kind::PurchaseOrder => (b.action_if_annual_budget_exceeded_on_po.clone(), b.action_if_accumulated_monthly_budget_exceeded_on_po.clone()),
            Kind::MaterialRequest => (b.action_if_annual_budget_exceeded_on_mr.clone(), b.action_if_accumulated_monthly_budget_exceeded_on_mr.clone()),
        }
    }
}

/// More spending the document causes on `account` for these dimensions.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Effect {
    pub account: Uuid,
    pub cost_center: Option<Uuid>,
    pub project: Option<Uuid>,
    pub day: NaiveDate,
    pub amount: Decimal,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct BudgetRow {
    id: Uuid,
    number: Option<String>,
    budget_against: String,
    cost_center: Option<Uuid>,
    project: Option<Uuid>,
    monthly_distribution: Option<Uuid>,
    budget_owner: Option<String>,
    created_by: Option<Uuid>,
    applicable_on_material_request: bool,
    applicable_on_purchase_order: bool,
    applicable_on_booking_actual_expenses: bool,
    action_if_annual_budget_exceeded: Option<String>,
    action_if_accumulated_monthly_budget_exceeded: Option<String>,
    action_if_annual_budget_exceeded_on_po: Option<String>,
    action_if_accumulated_monthly_budget_exceeded_on_po: Option<String>,
    action_if_annual_budget_exceeded_on_mr: Option<String>,
    action_if_accumulated_monthly_budget_exceeded_on_mr: Option<String>,
    budget_amount: Decimal,
}

/// Whose spending a budget limits: a cost center with its children, or a project.
#[derive(Debug, Clone)]
pub struct Scope {
    pub cost_centers: Option<Vec<Uuid>>,
    pub project: Option<Uuid>,
}

/// Booked, ordered and requested amounts against a budget line over `[from, to]`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Consumed {
    pub actual: Decimal,
    pub ordered: Decimal,
    pub requested: Decimal,
}

impl Consumed {
    pub fn total(&self) -> Decimal {
        self.actual + self.ordered + self.requested
    }
}

/// Expense booked on `account` within `scope` over `[from, to]`, the year-end closing excluded.
pub async fn actual(tx: &mut Tx, tenant: Uuid, company: Uuid, account: Uuid, scope: &Scope, from: NaiveDate, to: NaiveDate) -> Result<Decimal> {
    Ok(sqlx::query_scalar(
        "select coalesce(sum(l.debit - l.credit), 0) from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
         where l.tenant_id = $1 and l.company_id = $2 and l.account_id = $3 and l.posting_date between $4 and $5 \
         and e.source_entity <> 'PeriodClosingVoucher' \
         and (($6::uuid[] is not null and l.cost_center_id = any($6)) or ($7::uuid is not null and l.project_id = $7))",
    )
    .bind(tenant)
    .bind(company)
    .bind(account)
    .bind(from)
    .bind(to)
    .bind(&scope.cost_centers)
    .bind(scope.project)
    .fetch_one(&mut **tx)
    .await?)
}

/// The base-currency value of what an open purchase order line `i` of order `o` has not billed
/// yet, rounded to the precision held by SQL parameter `p`.
fn po_open(p: &str) -> String {
    format!("round(i.net_amount * coalesce(o.conversion_rate, 1) * (i.qty - coalesce(i.billed_qty, 0)) / i.qty, {p})")
}

/// Unbilled purchase orders dated within `[from, to]` committing `account` within `scope`.
pub async fn ordered(
    tx: &mut Tx,
    tenant: Uuid,
    company: Uuid,
    account: Uuid,
    scope: &Scope,
    from: NaiveDate,
    to: NaiveDate,
    precision: u32,
) -> Result<Decimal> {
    let sql = format!(
        "select coalesce(sum({}), 0) from e_purchase_order o join e_purchase_order__items i on i.tenant_id = o.tenant_id and i.parent_id = o.id \
         where o.tenant_id = $1 and o.company = $2 and o.docstatus = 1 and o.status <> 'Closed' and o.posting_date between $4 and $5 \
         and i.qty > coalesce(i.billed_qty, 0) and nullif(i.ext->>'fin_expense_account', '')::uuid = $3 \
         and (($6::uuid[] is not null and nullif(i.ext->>'fin_cost_center', '')::uuid = any($6)) or ($7::uuid is not null and o.project = $7))",
        po_open("$8")
    );
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(tenant)
        .bind(company)
        .bind(account)
        .bind(from)
        .bind(to)
        .bind(&scope.cost_centers)
        .bind(scope.project)
        .bind(precision as i32)
        .fetch_one(&mut **tx)
        .await?)
}

/// Material requests (the procurement package, when installed) commit budget too: the unordered
/// part of submitted purchase requests. Their fields are read from the tenant's schema, so any
/// request with a company, a date and lines with an item, a quantity and a rate or amount works;
/// dimensions come from the line, then the request, then the company defaults.
struct MrShape {
    from: String,
    company: String,
    date: String,
    account: String,
    cost_center: String,
    project: String,
    amount: String,
    filter: String,
}

fn mr_shape(ctx: &Ctx) -> Option<MrShape> {
    let h = ctx.schema.entity("MaterialRequest")?;
    let info = h.children.get("items")?;
    let c = ctx.schema.entity(&info.entity)?;
    let hf = |f: &str| field_expr(h, f, "m");
    let cf = |f: &str| field_expr(c, f, "i");
    let qty = cf("stock_qty").or_else(|| cf("qty"))?;
    let ordered = cf("ordered_qty").map(|o| format!("coalesce({o}, 0)")).unwrap_or_else(|| "0".into());
    let open = format!("greatest({qty} - {ordered}, 0)");
    let amount = match (cf("rate"), cf("amount")) {
        (Some(rate), _) => format!("{open} * coalesce({rate}, 0)"),
        (None, Some(amt)) => format!("case when {qty} = 0 then 0 else coalesce({amt}, 0) * {open} / {qty} end"),
        _ => return None,
    };
    let company = hf("company")?;
    let date = ["posting_date", "transaction_date", "schedule_date"].iter().find_map(|f| hf(f))?;
    let item = cf("item").or_else(|| cf("item_code"));
    let line_account = cf("expense_account");
    let account = format!("coalesce({}, it.expense_account, co.default_expense_account)", line_account.clone().unwrap_or_else(|| "null::uuid".into()));
    let cost_center = format!(
        "coalesce({}, {}, co.default_cost_center)",
        cf("cost_center").unwrap_or_else(|| "null::uuid".into()),
        hf("cost_center").unwrap_or_else(|| "null::uuid".into())
    );
    let project = format!("coalesce({}, {})", cf("project").unwrap_or_else(|| "null::uuid".into()), hf("project").unwrap_or_else(|| "null::uuid".into()));
    let mut filter = format!("m.docstatus = 1 and {open} > 0");
    if let Some(t) = hf("material_request_type") {
        filter.push_str(&format!(" and coalesce({t}, 'Purchase') = 'Purchase'"));
    }
    if let Some(st) = hf("status") {
        filter.push_str(&format!(" and coalesce({st}, '') not in ('Stopped', 'Closed', 'Cancelled')"));
    }
    // Stock items are inventory, not expense, unless the line names an expense account itself.
    filter.push_str(&format!(" and (coalesce(it.is_stock_item, false) = false or {} is not null)", line_account.unwrap_or_else(|| "null::uuid".into())));
    let from = format!(
        "{} m join {} i on i.tenant_id = m.tenant_id and i.parent_id = m.id left join e_item it on it.tenant_id = i.tenant_id and it.id = {} \
         join e_company co on co.tenant_id = m.tenant_id and co.id = {company}",
        ferrum_kernel::ddl::q(&h.table),
        ferrum_kernel::ddl::q(&info.table),
        item.unwrap_or_else(|| "null::uuid".into())
    );
    Some(MrShape { from, company, date, account, cost_center, project, amount, filter })
}

/// Unordered purchase requests dated within `[from, to]` for `account` within `scope`.
pub async fn requested(tx: &mut Tx, ctx: &Ctx, company: Uuid, account: Uuid, scope: &Scope, from: NaiveDate, to: NaiveDate, precision: u32) -> Result<Decimal> {
    let Some(m) = mr_shape(ctx) else { return Ok(Decimal::ZERO) };
    let sql = format!(
        "select coalesce(sum(round({amount}, $8)), 0) from {from} where m.tenant_id = $1 and {company} = $2 and {account} = $3 \
         and {date} between $4 and $5 and {filter} and (($6::uuid[] is not null and {cc} = any($6)) or ($7::uuid is not null and {project} = $7))",
        amount = m.amount,
        from = m.from,
        company = m.company,
        account = m.account,
        date = m.date,
        filter = m.filter,
        cc = m.cost_center,
        project = m.project
    );
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(ctx.tenant)
        .bind(company)
        .bind(account)
        .bind(from)
        .bind(to)
        .bind(&scope.cost_centers)
        .bind(scope.project)
        .bind(precision as i32)
        .fetch_one(&mut **tx)
        .await?)
}

pub async fn consumed(
    tx: &mut Tx,
    ctx: &Ctx,
    company: Uuid,
    account: Uuid,
    scope: &Scope,
    (from, to): (NaiveDate, NaiveDate),
    (orders, requests): (bool, bool),
    precision: u32,
) -> Result<Consumed> {
    Ok(Consumed {
        actual: actual(tx, ctx.tenant, company, account, scope, from, to).await?,
        ordered: if orders { ordered(tx, ctx.tenant, company, account, scope, from, to, precision).await? } else { Decimal::ZERO },
        requested: if requests { requested(tx, ctx, company, account, scope, from, to, precision).await? } else { Decimal::ZERO },
    })
}

/// The scope of a budget row (a cost center budget covers the cost centers below it).
pub async fn scope_of(tx: &mut Tx, tenant: Uuid, against: &str, cost_center: Option<Uuid>, project: Option<Uuid>) -> Result<Scope> {
    Ok(match (against, cost_center) {
        ("Cost Center", Some(cc)) => Scope { cost_centers: Some(cost_center_subtree(tx, tenant, cc).await?), project: None },
        _ => Scope { cost_centers: None, project },
    })
}

/// Accumulated share of the annual budget through the month of `on`.
pub async fn monthly_share(tx: &mut Tx, tenant: Uuid, fy: &FiscalYear, distribution_id: Option<Uuid>, on: NaiveDate) -> Result<Decimal> {
    let dist = match distribution_id {
        Some(dd) => distribution(tx, tenant, dd).await?,
        None => None,
    };
    Ok(rules::budget_share(fy.start, fy.end, on, dist.as_ref()))
}

async fn label(tx: &mut Tx, tenant: Uuid, table: &str, title: &str, id: Option<Uuid>) -> Result<String> {
    let Some(id) = id else { return Ok(String::new()) };
    let sql = format!("select {title} from {table} where tenant_id = $1 and id = $2");
    let v: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).bind(tenant).bind(id).fetch_optional(&mut **tx).await?;
    Ok(v.unwrap_or_default())
}

/// Checks the document's effects against every budget they touch. Stop refuses the submit with
/// all reasons at once; Warn lets it through and tells the budget's owner.
pub async fn check(hc: &mut HookCtx<'_>, company_id: Uuid, kind: Kind, effects: Vec<Effect>, record: Option<Uuid>) -> Result<()> {
    let tenant = hc.ctx.tenant;
    let mut groups: BTreeMap<(Uuid, Uuid), (BudgetRow, FiscalYear, Decimal, Vec<NaiveDate>)> = BTreeMap::new();
    let mut years: HashMap<NaiveDate, Option<FiscalYear>> = HashMap::new();
    for e in effects.into_iter().filter(|e| e.amount > Decimal::ZERO) {
        if let std::collections::hash_map::Entry::Vacant(slot) = years.entry(e.day) {
            slot.insert(fiscal_year_of(hc.tx, tenant, company_id, e.day).await?);
        }
        let Some(fy) = years.get(&e.day).cloned().flatten() else { continue };
        let ancestors = match e.cost_center {
            Some(cc) => cost_center_ancestors(hc.tx, tenant, cc).await?,
            None => vec![],
        };
        let budgets: Vec<BudgetRow> = sqlx::query_as(
            "select b.id, b.number, b.budget_against, b.cost_center, b.project, b.monthly_distribution, b.budget_owner, b.created_by, \
             b.applicable_on_material_request, b.applicable_on_purchase_order, b.applicable_on_booking_actual_expenses, \
             b.action_if_annual_budget_exceeded, b.action_if_accumulated_monthly_budget_exceeded, \
             b.action_if_annual_budget_exceeded_on_po, b.action_if_accumulated_monthly_budget_exceeded_on_po, \
             b.action_if_annual_budget_exceeded_on_mr, b.action_if_accumulated_monthly_budget_exceeded_on_mr, a.budget_amount \
             from e_budget b join e_budget__accounts a on a.tenant_id = b.tenant_id and a.parent_id = b.id \
             where b.tenant_id = $1 and b.company = $2 and b.fiscal_year = $3 and b.docstatus = 1 and a.account = $4 \
             and ((b.budget_against = 'Cost Center' and b.cost_center = any($5)) or (b.budget_against = 'Project' and b.project = $6))",
        )
        .bind(tenant)
        .bind(company_id)
        .bind(fy.id)
        .bind(e.account)
        .bind(&ancestors)
        .bind(e.project)
        .fetch_all(&mut **hc.tx)
        .await?;
        for bud in budgets.into_iter().filter(|x| kind.applies(x)) {
            let entry = groups.entry((bud.id, e.account)).or_insert_with(|| (bud.clone(), fy.clone(), Decimal::ZERO, vec![]));
            entry.2 += e.amount;
            entry.3.push(e.day);
        }
    }
    if groups.is_empty() {
        return Ok(());
    }
    let co = company(hc.kernel, hc.ctx, hc.tx, company_id).await?;
    let p = co.precision;
    let mut stops = Vec::new();
    let mut warnings: Vec<(BudgetRow, String)> = Vec::new();
    for ((budget_id, account), (bud, fy, doc_amount, mut dates)) in groups {
        // Serialize checks of one budget across concurrent submits (released at commit).
        sqlx::query("select pg_advisory_xact_lock(hashtextextended('ferrum:budget:' || $1::text, 0))").bind(budget_id).execute(&mut **hc.tx).await?;
        let scope = scope_of(hc.tx, tenant, &bud.budget_against, bud.cost_center, bud.project).await?;
        let tracks = (bud.applicable_on_purchase_order, bud.applicable_on_material_request && mr_shape(hc.ctx).is_some());
        let (annual_action, monthly_action) = kind.actions(&bud);
        let account_label = label(hc.tx, tenant, "e_account", "coalesce(account_number || ' - ', '') || account_name", Some(account)).await?;
        let against = match bud.budget_against.as_str() {
            "Project" => format!("project {}", label(hc.tx, tenant, "e_project", "project_name", bud.project).await?),
            _ => format!("cost center {}", label(hc.tx, tenant, "e_cost_center", "cost_center_name", bud.cost_center).await?),
        };
        let mut evaluate = |what: String, limit: Decimal, used: Consumed, action: Option<String>| {
            let total = used.total();
            if total <= limit {
                return;
            }
            let already = total - doc_amount > limit;
            let msg = format!(
                "{what} budget of {} for {account_label} against {against} ({}) {} exceeded by {} (booked {}, ordered {}, requested {}).",
                fmt(limit, p),
                bud.number.clone().unwrap_or_default(),
                if already { "is already" } else { "would be" },
                fmt(total - limit, p),
                fmt(used.actual, p),
                fmt(used.ordered, p),
                fmt(used.requested, p),
            );
            match action.as_deref() {
                Some("Stop") => stops.push(msg),
                Some("Warn") => warnings.push((bud.clone(), msg)),
                _ => {}
            }
        };
        if matches!(annual_action.as_deref(), Some("Stop" | "Warn")) {
            let used = consumed(hc.tx, hc.ctx, company_id, account, &scope, (fy.start, fy.end), tracks, p).await?;
            evaluate("Annual".into(), bud.budget_amount, used, annual_action.clone());
        }
        if matches!(monthly_action.as_deref(), Some("Stop" | "Warn")) {
            dates.sort();
            dates.dedup_by_key(|x| rules::month_end(*x));
            for on in dates {
                let to = rules::month_end(on).min(fy.end);
                let share = monthly_share(hc.tx, tenant, &fy, bud.monthly_distribution, on).await?;
                let limit = round(bud.budget_amount * share, p);
                let used = consumed(hc.tx, hc.ctx, company_id, account, &scope, (fy.start, to), tracks, p).await?;
                evaluate(format!("Accumulated monthly (through {to})"), limit, used, monthly_action.clone());
            }
        }
    }
    if !stops.is_empty() {
        return Err(Error::invalid(stops.join(" ")));
    }
    let entity = hc.entity.def.name.clone();
    for (bud, msg) in warnings {
        let title = format!("Budget {} exceeded", bud.number.clone().unwrap_or_default());
        notify_user(hc.tx, tenant, bud.budget_owner.as_deref(), bud.created_by, &title, &msg, &entity, record).await?;
    }
    Ok(())
}

/// Whether the company has any budget in force (cheap guard before the detailed checks).
async fn has_budgets(tx: &mut Tx, tenant: Uuid, company: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar("select exists(select 1 from e_budget where tenant_id = $1 and company = $2 and docstatus = 1)")
        .bind(tenant)
        .bind(company)
        .fetch_one(&mut **tx)
        .await?)
}

/// Actual expense of any submitted document: what its journal entries put on expense accounts.
/// Registered for every entity (`*`), so journals, purchase invoices, stock entries and other
/// packages' postings are all covered.
pub struct ActualExpenseCheck;

#[async_trait]
impl DocHook for ActualExpenseCheck {
    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) || matches!(hc.entity.def.name.as_str(), "PurchaseOrder" | "MaterialRequest" | "PeriodClosingVoucher" | "Budget") {
            return Ok(());
        }
        let (Some(cid), Some(did)) = (id(doc, "company"), id(doc, "id")) else { return Ok(()) };
        if !has_budgets(hc.tx, hc.ctx.tenant, cid).await? {
            return Ok(());
        }
        let effects: Vec<Effect> = sqlx::query_as(
            "select l.account_id as account, l.cost_center_id as cost_center, l.project_id as project, l.posting_date as day, \
             sum(l.debit - l.credit) as amount from journal_entry e \
             join journal_line l on l.tenant_id = e.tenant_id and l.entry_id = e.id and l.posting_date = e.posting_date \
             join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
             where e.tenant_id = $1 and e.source_entity = $2 and e.source_id = $3 and e.reversal_of is null and a.root_type = 'Expense' \
             group by 1, 2, 3, 4 having sum(l.debit - l.credit) > 0",
        )
        .bind(hc.ctx.tenant)
        .bind(&hc.entity.def.name)
        .bind(did)
        .fetch_all(&mut **hc.tx)
        .await?;
        check(hc, cid, Kind::Actual, effects, Some(did)).await
    }
}

/// Purchase orders: budget dimensions of each line (expense account, cost center), defaulted the
/// way the invoice will book them, and the commitment check at submit.
pub struct PurchaseOrderHook;

#[async_trait]
impl DocHook for PurchaseOrderHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let header_cc = id(doc, "fin_cost_center").or(co.account("default_cost_center"));
        let mut items = rows(doc, "items");
        let mut cache: HashMap<Uuid, Doc> = HashMap::new();
        for r in items.iter_mut() {
            if id(r, "fin_cost_center").is_none()
                && let Some(cc) = header_cc
            {
                set(r, "fin_cost_center", cc.to_string());
            }
            if id(r, "fin_expense_account").is_none()
                && let Some(item) = id(r, "item")
            {
                if let std::collections::hash_map::Entry::Vacant(slot) = cache.entry(item) {
                    slot.insert(get(hc.kernel, hc.ctx, hc.tx, "Item", item).await?);
                }
                let it = &cache[&item];
                // Stock items are inventory: they reach expense only when consumed.
                if !b(it, "is_stock_item")
                    && let Some(acc) = id(it, "expense_account").or(co.account("default_expense_account"))
                {
                    set(r, "fin_expense_account", acc.to_string());
                }
            }
        }
        set(doc, "items", Value::Array(items.into_iter().map(Value::Object).collect()));
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let (Some(cid), Some(po)) = (id(doc, "company"), id(doc, "id")) else { return Ok(()) };
        if !has_budgets(hc.tx, hc.ctx.tenant, cid).await? {
            return Ok(());
        }
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let sql = format!(
            "select nullif(i.ext->>'fin_expense_account', '')::uuid as account, nullif(i.ext->>'fin_cost_center', '')::uuid as cost_center, \
             o.project as project, o.posting_date as day, sum({}) as amount \
             from e_purchase_order o join e_purchase_order__items i on i.tenant_id = o.tenant_id and i.parent_id = o.id \
             join e_account a on a.tenant_id = o.tenant_id and a.id = nullif(i.ext->>'fin_expense_account', '')::uuid \
             where o.tenant_id = $1 and o.id = $2 and a.root_type = 'Expense' and i.qty > coalesce(i.billed_qty, 0) \
             group by 1, 2, 3, 4",
            po_open("$3")
        );
        let effects: Vec<Effect> =
            sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(hc.ctx.tenant).bind(po).bind(co.precision as i32).fetch_all(&mut **hc.tx).await?;
        check(hc, cid, Kind::PurchaseOrder, effects, Some(po)).await
    }
}

/// Material requests (procurement package): the commitment check at submit.
pub struct MaterialRequestHook;

#[async_trait]
impl DocHook for MaterialRequestHook {
    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let (Some(cid), Some(mr)) = (id(doc, "company"), id(doc, "id")) else { return Ok(()) };
        let Some(m) = mr_shape(hc.ctx) else { return Ok(()) };
        if !has_budgets(hc.tx, hc.ctx.tenant, cid).await? {
            return Ok(());
        }
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let sql = format!(
            "select {account} as account, {cc} as cost_center, {project} as project, {date} as day, sum(round({amount}, $3)) as amount \
             from {from} join e_account acc on acc.tenant_id = m.tenant_id and acc.id = {account} \
             where m.tenant_id = $1 and m.id = $2 and acc.root_type = 'Expense' and {filter} group by 1, 2, 3, 4",
            account = m.account,
            cc = m.cost_center,
            project = m.project,
            date = m.date,
            amount = m.amount,
            from = m.from,
            filter = m.filter
        );
        let effects: Vec<Effect> =
            sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(hc.ctx.tenant).bind(mr).bind(co.precision as i32).fetch_all(&mut **hc.tx).await?;
        check(hc, cid, Kind::MaterialRequest, effects, Some(mr)).await
    }
}

/// A purchase invoice made from an order books on the order lines' cost center and expense
/// account (the order committed the budget there), set on its first save; edits stay the user's.
pub struct PurchaseInvoiceDims;

#[async_trait]
impl DocHook for PurchaseInvoiceDims {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let mut items = rows(doc, "items");
        let lines: Vec<Uuid> = items.iter().filter_map(|r| id(r, "po_line")).collect();
        if lines.is_empty() || !is_new(hc, doc).await? {
            return Ok(());
        }
        let dims: Vec<(Uuid, Option<String>, Option<String>)> = sqlx::query_as(
            "select id, nullif(ext->>'fin_expense_account', ''), nullif(ext->>'fin_cost_center', '') from e_purchase_order__items \
             where tenant_id = $1 and id = any($2)",
        )
        .bind(hc.ctx.tenant)
        .bind(&lines)
        .fetch_all(&mut **hc.tx)
        .await?;
        let dims: HashMap<Uuid, (Option<String>, Option<String>)> = dims.into_iter().map(|(i, a, c)| (i, (a, c))).collect();
        for r in items.iter_mut() {
            let Some((acc, cc)) = id(r, "po_line").and_then(|l| dims.get(&l)) else { continue };
            if let Some(cc) = cc {
                set(r, "cost_center", cc.clone());
            }
            if let (Some(acc), Some(item)) = (acc, id(r, "item")) {
                let it = get(hc.kernel, hc.ctx, hc.tx, "Item", item).await?;
                if !b(&it, "is_stock_item") {
                    set(r, "expense_account", acc.clone());
                }
            }
        }
        set(doc, "items", Value::Array(items.into_iter().map(Value::Object).collect()));
        Ok(())
    }
}
