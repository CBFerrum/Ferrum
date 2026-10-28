//! Posting engine for the universal journal plus the AR/AP open-item subledger.
//!
//! Every financial effect goes through `post`: lines are merged, signs normalized, rounded,
//! checked to balance exactly, hashed and appended. The database re-checks balance at commit and
//! refuses edits, deletes and postings into closed periods. Cancellation appends reversals.

use crate::util::round;
use chrono::NaiveDate;
use ferrum_kernel::value::canonical_json;
use ferrum_kernel::{Ctx, Error, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct Line {
    pub account: Uuid,
    /// Positive = debit, negative = credit (company currency).
    pub amount: Decimal,
    /// Same sign convention, in the transaction currency.
    pub amount_txn: Decimal,
    pub currency: String,
    pub exchange_rate: Decimal,
    pub customer: Option<Uuid>,
    pub supplier: Option<Uuid>,
    pub cost_center: Option<Uuid>,
    pub project: Option<Uuid>,
    pub against: Option<(String, Uuid)>,
    pub remarks: Option<String>,
}

impl Line {
    pub fn new(account: Uuid, amount: Decimal, currency: &str) -> Line {
        Line { account, amount, amount_txn: amount, currency: currency.to_string(), exchange_rate: Decimal::ONE, ..Default::default() }
    }
    pub fn txn(mut self, amount_txn: Decimal, currency: &str, rate: Decimal) -> Line {
        self.amount_txn = amount_txn;
        self.currency = currency.to_string();
        self.exchange_rate = rate;
        self
    }
    pub fn customer(mut self, c: Option<Uuid>) -> Line {
        self.customer = c;
        self
    }
    pub fn supplier(mut self, s: Option<Uuid>) -> Line {
        self.supplier = s;
        self
    }
    pub fn cost_center(mut self, c: Option<Uuid>) -> Line {
        self.cost_center = c;
        self
    }
    pub fn project(mut self, p: Option<Uuid>) -> Line {
        self.project = p;
        self
    }
    pub fn against(mut self, entity: &str, id: Uuid) -> Line {
        self.against = Some((entity.to_string(), id));
        self
    }
    pub fn remarks(mut self, r: impl Into<String>) -> Line {
        self.remarks = Some(r.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub company: Uuid,
    pub posting_date: NaiveDate,
    pub source_entity: String,
    pub source_id: Uuid,
    pub source_number: Option<String>,
    pub memo: Option<String>,
    pub lines: Vec<Line>,
    /// Set when this entry reverses another one.
    pub reversal_of: Option<Uuid>,
}

impl Entry {
    pub fn new(company: Uuid, posting_date: NaiveDate, source_entity: &str, source_id: Uuid, source_number: Option<String>) -> Entry {
        Entry { company, posting_date, source_entity: source_entity.to_string(), source_id, source_number, memo: None, lines: vec![], reversal_of: None }
    }
}

type MergeKey = (Uuid, Option<Uuid>, Option<Uuid>, Option<Uuid>, Option<Uuid>, Option<(String, Uuid)>, String);

/// Appends one balanced journal entry. Returns `None` when every line nets to zero.
pub async fn post(tx: &mut Tx, ctx: &Ctx, entry: Entry, precision: u32) -> Result<Option<Uuid>> {
    let mut merged: BTreeMap<MergeKey, Line> = BTreeMap::new();
    for l in entry.lines {
        let key = (l.account, l.customer, l.supplier, l.cost_center, l.project, l.against.clone(), l.currency.clone());
        match merged.get_mut(&key) {
            Some(m) => {
                m.amount += l.amount;
                m.amount_txn += l.amount_txn;
                if m.remarks.is_none() {
                    m.remarks = l.remarks;
                }
            }
            None => {
                merged.insert(key, l);
            }
        }
    }
    let lines: Vec<Line> = merged
        .into_values()
        .map(|mut l| {
            l.amount = round(l.amount, precision);
            l.amount_txn = round(l.amount_txn, 9);
            l
        })
        .filter(|l| !l.amount.is_zero())
        .collect();
    if lines.is_empty() {
        return Ok(None);
    }
    let debit: Decimal = lines.iter().filter(|l| l.amount > Decimal::ZERO).map(|l| l.amount).sum();
    let credit: Decimal = lines.iter().filter(|l| l.amount < Decimal::ZERO).map(|l| -l.amount).sum();
    if debit != credit {
        return Err(Error::Internal(format!("unbalanced posting for {} {}: debit {debit} credit {credit}", entry.source_entity, entry.source_id)));
    }
    let entry_id = Uuid::now_v7();
    let line_rows: Vec<Value> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let (dr, cr) = if l.amount > Decimal::ZERO { (l.amount, Decimal::ZERO) } else { (Decimal::ZERO, -l.amount) };
            let (drt, crt) = if l.amount > Decimal::ZERO { (l.amount_txn.abs(), Decimal::ZERO) } else { (Decimal::ZERO, l.amount_txn.abs()) };
            json!({
                "tenant_id": ctx.tenant, "entry_id": entry_id, "line_no": i, "company_id": entry.company,
                "posting_date": entry.posting_date, "account_id": l.account, "customer_id": l.customer, "supplier_id": l.supplier,
                "debit": dr.to_string(), "credit": cr.to_string(), "currency": l.currency,
                "debit_txn": drt.to_string(), "credit_txn": crt.to_string(), "exchange_rate": l.exchange_rate.to_string(),
                "cost_center_id": l.cost_center, "project_id": l.project, "dims": {},
                "against_entity": l.against.as_ref().map(|a| a.0.clone()), "against_id": l.against.as_ref().map(|a| a.1),
                "remarks": l.remarks,
            })
        })
        .collect();
    let mut h = Sha256::new();
    h.update(canonical_json(&json!({
        "company": entry.company, "posting_date": entry.posting_date, "source": [entry.source_entity, entry.source_id],
        "lines": line_rows.iter().map(|r| {
            let mut r = r.clone();
            if let Value::Object(m) = &mut r { m.remove("tenant_id"); m.remove("entry_id"); }
            r
        }).collect::<Vec<_>>(),
    })));
    sqlx::query(
        "insert into journal_entry (tenant_id, id, company_id, posting_date, source_entity, source_id, source_number, memo, \
         total_debit, total_credit, entry_hash, created_by, reversal_of) values ($1, $2, $3, $4, $5, $6, $7, $8, $9::numeric, $9::numeric, $10, $11, $12)",
    )
    .bind(ctx.tenant)
    .bind(entry_id)
    .bind(entry.company)
    .bind(entry.posting_date)
    .bind(&entry.source_entity)
    .bind(entry.source_id)
    .bind(&entry.source_number)
    .bind(&entry.memo)
    .bind(debit.to_string())
    .bind(h.finalize().to_vec())
    .bind((!ctx.user.is_nil()).then_some(ctx.user))
    .bind(entry.reversal_of)
    .execute(&mut **tx)
    .await?;
    sqlx::query("insert into journal_line select * from jsonb_populate_recordset(null::journal_line, $1::jsonb)")
        .bind(Value::Array(line_rows))
        .execute(&mut **tx)
        .await?;
    Ok(Some(entry_id))
}

pub async fn period_open(tx: &mut Tx, tenant: Uuid, company: Uuid, on: NaiveDate) -> Result<bool> {
    let closed: Option<bool> = sqlx::query_scalar(
        "select true from accounting_period where tenant_id = $1 and company_id = $2 and $3 between period_start and period_end and status = 'closed'",
    )
    .bind(tenant)
    .bind(company)
    .bind(on)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(closed.is_none())
}

/// Where to date the reversal of a document's stock and ledger postings: on the document's own
/// date and time while its period is open (so reports never show a phantom gap), else now.
pub async fn reversal_point(tx: &mut Tx, tenant: Uuid, company: Uuid, doc: &ferrum_kernel::Doc) -> Result<(NaiveDate, chrono::DateTime<chrono::Utc>)> {
    let on = crate::util::date(doc, "posting_date")?;
    if period_open(tx, tenant, company, on).await? {
        Ok((on, crate::util::posting_ts(doc)?))
    } else {
        let now = chrono::Utc::now();
        Ok((now.date_naive(), now))
    }
}

#[derive(sqlx::FromRow)]
struct LineRow {
    account_id: Uuid,
    debit: rust_decimal::Decimal,
    credit: rust_decimal::Decimal,
    currency: String,
    debit_txn: rust_decimal::Decimal,
    credit_txn: rust_decimal::Decimal,
    exchange_rate: rust_decimal::Decimal,
    customer_id: Option<Uuid>,
    supplier_id: Option<Uuid>,
    cost_center_id: Option<Uuid>,
    project_id: Option<Uuid>,
    against_entity: Option<String>,
    against_id: Option<Uuid>,
}

/// Appends reversing entries for every not-yet-reversed entry of a source document. The
/// reversal is dated like the original unless that period is closed (then it is dated `fallback`).
pub async fn reverse(tx: &mut Tx, ctx: &Ctx, source_entity: &str, source_id: Uuid, fallback: NaiveDate, precision: u32) -> Result<usize> {
    let entries: Vec<(Uuid, Uuid, NaiveDate, Option<String>)> = sqlx::query_as(
        "select e.id, e.company_id, e.posting_date, e.source_number from journal_entry e \
         where e.tenant_id = $1 and e.source_entity = $2 and e.source_id = $3 and e.reversal_of is null \
         and not exists (select 1 from journal_entry r where r.tenant_id = e.tenant_id and r.reversal_of = e.id) order by e.created_at",
    )
    .bind(ctx.tenant)
    .bind(source_entity)
    .bind(source_id)
    .fetch_all(&mut **tx)
    .await?;
    for (entry_id, company, date, number) in &entries {
        let lines: Vec<LineRow> = sqlx::query_as(
            "select account_id, debit, credit, currency, debit_txn, credit_txn, exchange_rate, customer_id, supplier_id, cost_center_id, \
             project_id, against_entity, against_id from journal_line where tenant_id = $1 and entry_id = $2 and posting_date = $3 order by line_no",
        )
        .bind(ctx.tenant)
        .bind(entry_id)
        .bind(date)
        .fetch_all(&mut **tx)
        .await?;
        let on = if period_open(tx, ctx.tenant, *company, *date).await? { *date } else { fallback };
        let reversed: Vec<Line> = lines
            .into_iter()
            .map(|l| Line {
                account: l.account_id,
                amount: l.credit - l.debit,
                amount_txn: l.credit_txn - l.debit_txn,
                currency: l.currency.trim().to_string(),
                exchange_rate: l.exchange_rate,
                customer: l.customer_id,
                supplier: l.supplier_id,
                cost_center: l.cost_center_id,
                project: l.project_id,
                against: l.against_entity.zip(l.against_id),
                remarks: Some("Reversal".into()),
            })
            .collect();
        post(
            tx,
            ctx,
            Entry {
                company: *company,
                posting_date: on,
                source_entity: source_entity.to_string(),
                source_id,
                source_number: number.clone(),
                memo: Some(format!("Reversal of {entry_id}")),
                lines: reversed,
                reversal_of: Some(*entry_id),
            },
            precision,
        )
        .await?;
    }
    Ok(entries.len())
}

// ───────────── open items (receivables / payables) ─────────────

#[derive(Debug, Clone)]
pub struct OpenItem {
    pub id: Uuid,
    pub company: Uuid,
    pub entity: String,
    pub number: Option<String>,
    pub customer: Option<Uuid>,
    pub supplier: Option<Uuid>,
    pub account: Uuid,
    pub posting_date: NaiveDate,
    pub due_date: Option<NaiveDate>,
    pub currency: String,
    pub exchange_rate: Decimal,
    pub amount: Decimal,
    pub base_amount: Decimal,
}

pub async fn open_item(tx: &mut Tx, ctx: &Ctx, o: &OpenItem) -> Result<()> {
    if o.amount.is_zero() {
        return Ok(());
    }
    sqlx::query(
        "insert into open_item (tenant_id, id, company_id, entity, number, party_kind, customer_id, supplier_id, account_id, posting_date, \
         due_date, currency, exchange_rate, amount, base_amount, outstanding, base_outstanding) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13::numeric, $14::numeric, $15::numeric, $14::numeric, $15::numeric)",
    )
    .bind(ctx.tenant)
    .bind(o.id)
    .bind(o.company)
    .bind(&o.entity)
    .bind(&o.number)
    .bind(if o.customer.is_some() { "customer" } else { "supplier" })
    .bind(o.customer)
    .bind(o.supplier)
    .bind(o.account)
    .bind(o.posting_date)
    .bind(o.due_date)
    .bind(&o.currency)
    .bind(o.exchange_rate.to_string())
    .bind(o.amount.to_string())
    .bind(o.base_amount.to_string())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OpenItemRow {
    pub id: Uuid,
    pub entity: String,
    pub number: Option<String>,
    pub customer_id: Option<Uuid>,
    pub supplier_id: Option<Uuid>,
    pub account_id: Uuid,
    pub currency: String,
    pub exchange_rate: Decimal,
    pub amount: Decimal,
    pub outstanding: Decimal,
    pub base_outstanding: Decimal,
    pub due_date: Option<NaiveDate>,
    pub status: String,
}

pub async fn load_open_item(tx: &mut Tx, tenant: Uuid, id: Uuid, lock: bool) -> Result<Option<OpenItemRow>> {
    let sql = if lock {
        "select id, entity, number, customer_id, supplier_id, account_id, trim(currency) as currency, exchange_rate, amount, outstanding, \
         base_outstanding, due_date, status from open_item where tenant_id = $1 and id = $2 for update"
    } else {
        "select id, entity, number, customer_id, supplier_id, account_id, trim(currency) as currency, exchange_rate, amount, outstanding, \
         base_outstanding, due_date, status from open_item where tenant_id = $1 and id = $2"
    };
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(tenant).bind(id).fetch_optional(&mut **tx).await?)
}

/// Settles `amount` (same sign as the item) of an open item; the database refuses over-allocation.
#[allow(clippy::too_many_arguments)]
pub async fn allocate(
    tx: &mut Tx,
    ctx: &Ctx,
    company: Uuid,
    item: Uuid,
    payment_entity: &str,
    payment_id: Uuid,
    on: NaiveDate,
    amount: Decimal,
    base_amount: Decimal,
) -> Result<()> {
    let n = sqlx::query(
        "update open_item set outstanding = outstanding - $3::numeric, base_outstanding = base_outstanding - $4::numeric, \
         status = case when outstanding - $3::numeric = 0 then 'paid' else 'open' end, updated_at = now() \
         where tenant_id = $1 and id = $2 and status <> 'cancelled' \
         and abs(outstanding) >= abs($3::numeric) and sign(outstanding) * sign($3::numeric) >= 0",
    )
    .bind(ctx.tenant)
    .bind(item)
    .bind(amount.to_string())
    .bind(base_amount.to_string())
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(Error::invalid("the allocated amount exceeds what is outstanding on the invoice (or it was cancelled)"));
    }
    sqlx::query(
        "insert into allocation (tenant_id, id, company_id, open_item_id, payment_entity, payment_id, posting_date, amount, base_amount) \
         values ($1, $2, $3, $4, $5, $6, $7, $8::numeric, $9::numeric)",
    )
    .bind(ctx.tenant)
    .bind(Uuid::now_v7())
    .bind(company)
    .bind(item)
    .bind(payment_entity)
    .bind(payment_id)
    .bind(on)
    .bind(amount.to_string())
    .bind(base_amount.to_string())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Reverses every allocation a payment made (append-only), restoring the items' outstanding.
/// Returns the affected open-item ids.
pub async fn unallocate(tx: &mut Tx, ctx: &Ctx, payment_entity: &str, payment_id: Uuid, on: NaiveDate) -> Result<Vec<Uuid>> {
    let rows: Vec<(Uuid, Uuid, Uuid, Decimal, Decimal)> = sqlx::query_as(
        "select a.id, a.company_id, a.open_item_id, a.amount, a.base_amount from allocation a where a.tenant_id = $1 \
         and a.payment_entity = $2 and a.payment_id = $3 and a.reversal_of is null \
         and not exists (select 1 from allocation r where r.tenant_id = a.tenant_id and r.reversal_of = a.id)",
    )
    .bind(ctx.tenant)
    .bind(payment_entity)
    .bind(payment_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut items = Vec::new();
    for (alloc, company, item, amount, base) in rows {
        sqlx::query(
            "update open_item set outstanding = outstanding + $3::numeric, base_outstanding = base_outstanding + $4::numeric, \
             status = case when status = 'cancelled' then status else 'open' end, updated_at = now() where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(item)
        .bind(amount.to_string())
        .bind(base.to_string())
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "insert into allocation (tenant_id, id, company_id, open_item_id, payment_entity, payment_id, posting_date, amount, base_amount, reversal_of) \
             values ($1, $2, $3, $4, $5, $6, $7, $8::numeric, $9::numeric, $10)",
        )
        .bind(ctx.tenant)
        .bind(Uuid::now_v7())
        .bind(company)
        .bind(item)
        .bind(payment_entity)
        .bind(payment_id)
        .bind(on)
        .bind((-amount).to_string())
        .bind((-base).to_string())
        .bind(alloc)
        .execute(&mut **tx)
        .await?;
        items.push(item);
    }
    Ok(items)
}

/// Cancels an open item; refused when payments are still allocated to it.
pub async fn cancel_open_item(tx: &mut Tx, ctx: &Ctx, id: Uuid) -> Result<()> {
    let live: i64 = sqlx::query_scalar(
        "select count(*) from allocation a where a.tenant_id = $1 and a.open_item_id = $2 and a.reversal_of is null \
         and not exists (select 1 from allocation r where r.tenant_id = a.tenant_id and r.reversal_of = a.id) \
         and not (a.payment_entity in ('SalesInvoice', 'PurchaseInvoice') and a.payment_id = $2)",
    )
    .bind(ctx.tenant)
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    if live > 0 {
        return Err(Error::invalid("payments are allocated to this document; cancel those payments first"));
    }
    sqlx::query("update open_item set status = 'cancelled', outstanding = 0, base_outstanding = 0, updated_at = now() where tenant_id = $1 and id = $2")
        .bind(ctx.tenant)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
