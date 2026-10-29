//! Journal entries and payment entries.

use crate::flow::sync_invoice_outstanding;
use crate::ledger::{self, Entry, Line, OpenItem};
use crate::util::{company, d, date, exchange_rate, get, id, money, round, rows, s, set, sys};
use async_trait::async_trait;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

pub struct JournalEntryHook;

#[async_trait]
impl DocHook for JournalEntryHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let lines = rows(doc, "accounts");
        let dr: Decimal = lines.iter().map(|l| round(d(l, "debit"), co.precision)).sum();
        let cr: Decimal = lines.iter().map(|l| round(d(l, "credit"), co.precision)).sum();
        set(doc, "total_debit", money(dr));
        set(doc, "total_credit", money(cr));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let mut errors = Vec::new();
        for (i, l) in rows(doc, "accounts").iter().enumerate() {
            let Some(acc) = id(l, "account") else { continue };
            let a = get(hc.kernel, hc.ctx, hc.tx, "Account", acc).await?;
            if id(&a, "company") != Some(cid) {
                errors.push(FieldError::new("account", "belongs to another company").in_row("accounts", i));
            }
            let typ = s(&a, "account_type").unwrap_or_default();
            match (s(l, "party_type"), typ) {
                (Some("Customer"), t) if t != "Receivable" => {
                    errors.push(FieldError::new("account", "a customer line must use a Receivable account").in_row("accounts", i))
                }
                (Some("Supplier"), t) if t != "Payable" => {
                    errors.push(FieldError::new("account", "a supplier line must use a Payable account").in_row("accounts", i))
                }
                (None, "Receivable" | "Payable") => errors.push(FieldError::new("party_type", "receivable/payable lines need a party").in_row("accounts", i)),
                _ => {}
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let jid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let on = date(doc, "posting_date")?;
        let mut entry = Entry::new(co.id, on, "JournalEntry", jid, s(doc, "number").map(str::to_string));
        entry.memo = s(doc, "remark").map(str::to_string);
        for l in rows(doc, "accounts") {
            let amount = d(&l, "debit") - d(&l, "credit");
            let line_id = id(&l, "id").ok_or_else(|| Error::internal("line without id"))?;
            let mut line = Line::new(id(&l, "account").unwrap_or_default(), amount, &co.currency_code)
                .customer(id(&l, "customer").filter(|_| s(&l, "party_type") == Some("Customer")))
                .supplier(id(&l, "supplier").filter(|_| s(&l, "party_type") == Some("Supplier")))
                .cost_center(id(&l, "cost_center"))
                .project(id(&l, "project"));
            if let Some(r) = s(&l, "remarks") {
                line = line.remarks(r);
            }
            if line.customer.is_some() || line.supplier.is_some() {
                line = line.against("JournalEntry", line_id);
                // Party lines become open items so they show in ageing and can be settled.
                let open = if line.customer.is_some() { amount } else { -amount };
                ledger::open_item(
                    hc.tx,
                    hc.ctx,
                    &OpenItem {
                        id: line_id,
                        company: co.id,
                        entity: "JournalEntry".into(),
                        number: s(doc, "number").map(str::to_string),
                        customer: line.customer,
                        supplier: line.supplier,
                        account: line.account,
                        posting_date: on,
                        due_date: Some(on),
                        currency: co.currency_code.clone(),
                        exchange_rate: Decimal::ONE,
                        amount: open,
                        base_amount: open,
                    },
                )
                .await?;
            }
            entry.lines.push(line);
        }
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let jid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        for l in rows(doc, "accounts") {
            if s(&l, "party_type").is_some()
                && let Some(lid) = id(&l, "id")
                && ledger::load_open_item(hc.tx, hc.ctx.tenant, lid, true).await?.is_some()
            {
                ledger::cancel_open_item(hc.tx, hc.ctx, lid).await?;
            }
        }
        ledger::reverse(hc.tx, hc.ctx, "JournalEntry", jid, hc.ctx.today(), co.precision).await?;
        Ok(())
    }
}

pub struct PaymentEntryHook;

/// What an open item is, for people: its entity's label ("Sales Invoice", "Journal Entry").
fn item_kind(ctx: &ferrum_kernel::Ctx, entity: &str) -> String {
    ctx.schema.entity(entity).map(|e| e.def.label.clone()).unwrap_or_else(|| entity.to_string())
}

fn party(doc: &Doc) -> (Option<Uuid>, Option<Uuid>, bool) {
    let receive = s(doc, "payment_type") != Some("Pay");
    if receive { (id(doc, "customer"), None, true) } else { (None, id(doc, "supplier"), false) }
}

/// What a payment settles with its parties: the bank amount, plus deductions (debits) on a receipt
/// or minus them on a payment, so that 98 received with 2 of bank charges settles 100.
fn settled(doc: &Doc, receive: bool) -> Decimal {
    let deductions = d(doc, "total_deductions");
    d(doc, "paid_amount") + if receive { deductions } else { -deductions }
}

#[async_trait]
impl DocHook for PaymentEntryHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let (customer, supplier, receive) = party(doc);
        if receive {
            set(doc, "supplier", Value::Null);
        } else {
            set(doc, "customer", Value::Null);
        }
        let party_doc = match (customer, supplier) {
            (Some(c), _) => Some(get(hc.kernel, hc.ctx, hc.tx, "Customer", c).await?),
            (_, Some(s_)) => Some(get(hc.kernel, hc.ctx, hc.tx, "Supplier", s_).await?),
            _ => None,
        };
        if let Some(p) = &party_doc {
            let name = s(p, if receive { "customer_name" } else { "supplier_name" }).unwrap_or_default().to_string();
            set(doc, "party_name", name);
            let acc = if receive {
                id(p, "receivable_account").or(co.account("default_receivable_account"))
            } else {
                id(p, "payable_account").or(co.account("default_payable_account"))
            };
            set(doc, "party_account", acc.map(|a| json!(a.to_string())).unwrap_or(Value::Null));
        }
        if id(doc, "currency").is_none() {
            set(doc, "currency", co.currency_id.to_string());
        }
        let cur = id(doc, "currency").unwrap_or(co.currency_id);
        let rate = if cur == co.currency_id {
            Decimal::ONE
        } else {
            let given = d(doc, "exchange_rate");
            if given > Decimal::ZERO && given != Decimal::ONE {
                given
            } else {
                exchange_rate(hc.kernel, hc.ctx, hc.tx, cur, co.currency_id, date(doc, "posting_date")?).await?
            }
        };
        set(doc, "exchange_rate", money(rate));
        set(doc, "base_paid_amount", money(round(d(doc, "paid_amount") * rate, co.precision)));
        let (code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, cur).await?;
        let mut refs = rows(doc, "references");
        let mut allocated = Decimal::ZERO;
        for r in refs.iter_mut() {
            let Some(inv) = id(r, "invoice") else { continue };
            if let Some(item) = ledger::load_open_item(hc.tx, hc.ctx.tenant, inv, false).await? {
                set(r, "invoice_type", item_kind(hc.ctx, &item.entity));
                set(r, "invoice_number", item.number.clone().map(Value::String).unwrap_or(Value::Null));
                set(r, "outstanding", money(item.outstanding));
                set(r, "due_date", item.due_date.map(|x| json!(x.to_string())).unwrap_or(Value::Null));
                if item.currency != code {
                    return Err(Error::Validation(vec![FieldError::new("invoice", format!("this invoice is in {}, the payment in {code}", item.currency))]));
                }
            }
            allocated += d(r, "allocated_amount");
        }
        set(doc, "references", Value::Array(refs.into_iter().map(Value::Object).collect()));
        let deductions: Decimal = rows(doc, "deductions").iter().map(|r| d(r, "amount")).sum();
        set(doc, "total_deductions", money(deductions));
        set(doc, "unallocated_amount", money(settled(doc, receive) - allocated));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        if d(doc, "unallocated_amount") < Decimal::ZERO {
            let msg =
                if d(doc, "total_deductions").is_zero() { "allocations exceed the paid amount" } else { "allocations exceed the paid amount and deductions" };
            return Err(Error::field("references", msg));
        }
        if let Some(bank) = id(doc, "bank_account") {
            let a = get(hc.kernel, hc.ctx, hc.tx, "Account", bank).await?;
            if id(&a, "company") != Some(cid) || !matches!(s(&a, "account_type"), Some("Bank" | "Cash")) {
                return Err(Error::field("bank_account", "choose a Bank or Cash account of this company"));
            }
        }
        if id(doc, "party_account").is_none() {
            return Err(Error::field("party_account", "no receivable/payable account found for this party or company"));
        }
        for (i, r) in rows(doc, "deductions").iter().enumerate() {
            let bad = |field: &str, msg: &str| Err(Error::Validation(vec![FieldError::new(field, msg).in_row("deductions", i)]));
            if d(r, "amount").is_zero() {
                return bad("amount", "cannot be zero");
            }
            let Some(acc) = id(r, "account") else { return bad("account", "is required") };
            if Some(acc) == id(doc, "bank_account") || Some(acc) == id(doc, "party_account") {
                return bad("account", "use an account other than the bank and party accounts");
            }
            let a = get(hc.kernel, hc.ctx, hc.tx, "Account", acc).await?;
            if id(&a, "company") != Some(cid) {
                return bad("account", "belongs to another company");
            }
            if matches!(s(&a, "account_type"), Some("Receivable" | "Payable")) {
                return bad("account", "receivable and payable accounts need a party; allocate to an invoice instead");
            }
            if let Some(cc) = id(r, "cost_center")
                && id(&get(hc.kernel, hc.ctx, hc.tx, "CostCenter", cc).await?, "company") != Some(cid)
            {
                return bad("cost_center", "belongs to another company");
            }
        }
        let (customer, supplier, _) = party(doc);
        for (i, r) in rows(doc, "references").iter().enumerate() {
            let Some(inv) = id(r, "invoice") else {
                return Err(Error::Validation(vec![FieldError::new("invoice", "not a valid invoice id").in_row("references", i)]));
            };
            let Some(item) = ledger::load_open_item(hc.tx, hc.ctx.tenant, inv, false).await? else {
                return Err(Error::Validation(vec![FieldError::new("invoice", "invoice is not open for payment").in_row("references", i)]));
            };
            if item.customer_id != customer || item.supplier_id != supplier {
                return Err(Error::Validation(vec![FieldError::new("invoice", "invoice belongs to another party").in_row("references", i)]));
            }
            if d(r, "allocated_amount") > item.outstanding {
                return Err(Error::Validation(vec![FieldError::new("allocated_amount", "more than the outstanding amount").in_row("references", i)]));
            }
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let on = date(doc, "posting_date")?;
        let (customer, supplier, receive) = party(doc);
        let rate = d(doc, "exchange_rate");
        let (code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, id(doc, "currency").unwrap_or(co.currency_id)).await?;
        let paid = d(doc, "paid_amount");
        let base_paid = d(doc, "base_paid_amount");
        let party_account = id(doc, "party_account").ok_or_else(|| Error::field("party_account", "is required"))?;
        let mut lines = Vec::new();
        let mut touched = Vec::new();
        for r in rows(doc, "references") {
            let amount = d(&r, "allocated_amount");
            if amount.is_zero() {
                continue;
            }
            let inv = id(&r, "invoice").ok_or_else(|| Error::internal("allocation without invoice"))?;
            let item = ledger::load_open_item(hc.tx, hc.ctx.tenant, inv, true).await?.ok_or_else(|| Error::invalid("invoice is no longer open"))?;
            let base_at_invoice = round(amount * item.exchange_rate, co.precision);
            ledger::allocate(hc.tx, hc.ctx, co.id, inv, "PaymentEntry", pid, on, amount, base_at_invoice).await?;
            let sign = if receive { -Decimal::ONE } else { Decimal::ONE };
            lines.push(
                Line::new(party_account, sign * base_at_invoice, &co.currency_code)
                    .txn(sign * amount, &code, rate)
                    .customer(customer)
                    .supplier(supplier)
                    .against(&item.entity, inv),
            );
            touched.push((item.entity.clone(), inv));
        }
        let unallocated = d(doc, "unallocated_amount");
        if unallocated > Decimal::ZERO {
            let base_un = round(unallocated * rate, co.precision);
            let sign = if receive { -Decimal::ONE } else { Decimal::ONE };
            lines.push(
                Line::new(party_account, sign * base_un, &co.currency_code)
                    .txn(sign * unallocated, &code, rate)
                    .customer(customer)
                    .supplier(supplier)
                    .against("PaymentEntry", pid),
            );
            ledger::open_item(
                hc.tx,
                hc.ctx,
                &OpenItem {
                    id: pid,
                    company: co.id,
                    entity: "PaymentEntry".into(),
                    number: s(doc, "number").map(str::to_string),
                    customer,
                    supplier,
                    account: party_account,
                    posting_date: on,
                    due_date: Some(on),
                    currency: code.clone(),
                    exchange_rate: rate,
                    amount: -unallocated,
                    base_amount: -base_un,
                },
            )
            .await?;
        }
        let bank = id(doc, "bank_account").ok_or_else(|| Error::field("bank_account", "is required"))?;
        let bank_sign = if receive { Decimal::ONE } else { -Decimal::ONE };
        lines.push(Line::new(bank, bank_sign * base_paid, &co.currency_code).txn(bank_sign * paid, &code, rate));
        for r in rows(doc, "deductions") {
            let (amount, Some(account)) = (d(&r, "amount"), id(&r, "account")) else { continue };
            lines.push(
                Line::new(account, round(amount * rate, co.precision), &co.currency_code)
                    .txn(amount, &code, rate)
                    .cost_center(id(&r, "cost_center"))
                    .remarks(s(&r, "remarks").unwrap_or("Payment deduction")),
            );
        }
        // What is left is the exchange difference between the invoices' booked rates and today's
        // rate: positive is a gain, credited; negative a loss, debited.
        let diff: Decimal = lines.iter().map(|l| l.amount).sum();
        if !diff.is_zero() {
            let gl = co.require("exchange_gain_loss_account", "exchange gain/loss account")?;
            lines.push(Line::new(gl, -diff, &co.currency_code).remarks("Exchange difference"));
            let mut v = Doc::new();
            v.insert("exchange_difference".into(), money(diff));
            hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "PaymentEntry", pid, v).await?;
        }
        let mut entry = Entry::new(co.id, on, "PaymentEntry", pid, s(doc, "number").map(str::to_string));
        entry.lines = lines;
        entry.memo = s(doc, "remarks").map(str::to_string);
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
        for (entity, inv) in touched {
            sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, &entity, inv).await?;
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        // Allocations, ledger and receivables all reverse on the same date, so as-of reports agree.
        let (on, _) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        let items = ledger::unallocate(hc.tx, hc.ctx, "PaymentEntry", pid, on).await?;
        if ledger::load_open_item(hc.tx, hc.ctx.tenant, pid, true).await?.is_some() {
            ledger::cancel_open_item(hc.tx, hc.ctx, pid).await?;
        }
        ledger::reverse(hc.tx, hc.ctx, "PaymentEntry", pid, on, co.precision).await?;
        for inv in items {
            if let Some(item) = ledger::load_open_item(hc.tx, hc.ctx.tenant, inv, false).await? {
                sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, &item.entity, inv).await?;
            }
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "get_outstanding_invoices" {
            return Ok(None);
        }
        let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let (customer, supplier, receive) = party(doc);
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let (code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, id(doc, "currency").unwrap_or(co.currency_id)).await?;
        let items: Vec<(Uuid, String, Option<String>, Decimal, Option<chrono::NaiveDate>)> = sqlx::query_as(
            "select id, entity, number, outstanding, due_date from open_item where tenant_id = $1 and company_id = $2 and status = 'open' \
             and outstanding > 0 and customer_id is not distinct from $3 and supplier_id is not distinct from $4 and trim(currency) = $5 \
             order by coalesce(due_date, posting_date), posting_date limit 500",
        )
        .bind(hc.ctx.tenant)
        .bind(cid)
        .bind(customer)
        .bind(supplier)
        .bind(&code)
        .fetch_all(&mut **hc.tx)
        .await?;
        // Deductions settle invoices too, so they count towards what can be allocated.
        let mut budget = settled(doc, receive);
        let fill_all = d(doc, "paid_amount").is_zero();
        let mut refs = Vec::new();
        let mut total = Decimal::ZERO;
        for (inv, entity, number, outstanding, due) in items {
            let take = if fill_all { outstanding } else { outstanding.min(budget) };
            if take <= Decimal::ZERO {
                break;
            }
            budget -= take;
            total += take;
            let kind = item_kind(hc.ctx, &entity);
            refs.push(json!({"invoice_type": kind, "invoice": inv.to_string(), "invoice_number": number, "due_date": due, "outstanding": outstanding.to_string(), "allocated_amount": take.to_string()}));
        }
        let mut patch = Doc::new();
        patch.insert("references".into(), Value::Array(refs));
        if fill_all {
            let bank_amount = total - (settled(doc, receive) - d(doc, "paid_amount"));
            patch.insert("paid_amount".into(), json!(bank_amount.to_string()));
        }
        let updated = hc.kernel.update_tx(hc.ctx, hc.tx, "PaymentEntry", pid, patch, None).await?;
        Ok(Some(Value::Object(updated)))
    }
}
