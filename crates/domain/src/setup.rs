//! Company setup (standard chart of accounts, cost center, warehouses, GST templates) and
//! fiscal years with monthly accounting periods that can be closed and reopened.

use crate::util::{d, date, id, rows, s, sys};
use async_trait::async_trait;
use chrono::{Datelike, NaiveDate};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, Filter, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// (number, name, parent name, is_group, root_type, account_type)
const COA: &[(&str, &str, &str, bool, &str, &str)] = &[
    ("1000", "Assets", "", true, "Asset", ""),
    ("1100", "Current Assets", "Assets", true, "Asset", ""),
    ("1110", "Accounts Receivable", "Current Assets", true, "Asset", ""),
    ("1111", "Debtors", "Accounts Receivable", false, "Asset", "Receivable"),
    ("1120", "Bank Accounts", "Current Assets", true, "Asset", ""),
    ("1121", "Primary Bank", "Bank Accounts", false, "Asset", "Bank"),
    ("1130", "Cash In Hand", "Current Assets", true, "Asset", ""),
    ("1131", "Cash", "Cash In Hand", false, "Asset", "Cash"),
    ("1140", "Stock Assets", "Current Assets", true, "Asset", ""),
    ("1141", "Stock In Hand", "Stock Assets", false, "Asset", "Stock"),
    ("1142", "Work In Progress", "Stock Assets", false, "Asset", "Stock"),
    ("1150", "Tax Assets", "Current Assets", true, "Asset", ""),
    ("1151", "Input Tax", "Tax Assets", false, "Asset", "Tax"),
    ("1152", "TDS Receivable", "Tax Assets", false, "Asset", "Tax"),
    ("1160", "Advances to Suppliers", "Current Assets", false, "Asset", ""),
    ("1200", "Fixed Assets", "Assets", true, "Asset", ""),
    ("1210", "Plant and Machinery", "Fixed Assets", false, "Asset", "Fixed Asset"),
    ("1220", "Furniture and Fixtures", "Fixed Assets", false, "Asset", "Fixed Asset"),
    ("1230", "Computers", "Fixed Assets", false, "Asset", "Fixed Asset"),
    ("2000", "Liabilities", "", true, "Liability", ""),
    ("2100", "Current Liabilities", "Liabilities", true, "Liability", ""),
    ("2110", "Accounts Payable", "Current Liabilities", true, "Liability", ""),
    ("2111", "Creditors", "Accounts Payable", false, "Liability", "Payable"),
    ("2120", "Stock Liabilities", "Current Liabilities", true, "Liability", ""),
    ("2121", "Stock Received But Not Billed", "Stock Liabilities", false, "Liability", "Stock Received But Not Billed"),
    ("2130", "Duties and Taxes", "Current Liabilities", true, "Liability", ""),
    ("2131", "Output Tax", "Duties and Taxes", false, "Liability", "Tax"),
    ("2132", "TDS Payable", "Duties and Taxes", false, "Liability", "Tax"),
    ("2140", "Customer Advances", "Current Liabilities", false, "Liability", ""),
    ("2200", "Loans", "Liabilities", true, "Liability", ""),
    ("2210", "Secured Loans", "Loans", false, "Liability", ""),
    ("3000", "Equity", "", true, "Equity", ""),
    ("3100", "Capital Stock", "Equity", false, "Equity", "Equity"),
    ("3200", "Retained Earnings", "Equity", false, "Equity", "Equity"),
    ("3300", "Opening Balance Equity", "Equity", false, "Equity", "Temporary"),
    ("4000", "Income", "", true, "Income", ""),
    ("4100", "Direct Income", "Income", true, "Income", ""),
    ("4110", "Sales", "Direct Income", false, "Income", "Income"),
    ("4120", "Service Income", "Direct Income", false, "Income", "Income"),
    ("4200", "Indirect Income", "Income", true, "Income", ""),
    ("4210", "Other Income", "Indirect Income", false, "Income", "Income"),
    ("5000", "Expenses", "", true, "Expense", ""),
    ("5100", "Direct Expenses", "Expenses", true, "Expense", ""),
    ("5110", "Cost of Goods Sold", "Direct Expenses", false, "Expense", "Cost of Goods Sold"),
    ("5120", "Stock Adjustment", "Direct Expenses", false, "Expense", "Stock Adjustment"),
    ("5130", "Manufacturing Overheads Absorbed", "Direct Expenses", false, "Expense", "Expense"),
    ("5200", "Indirect Expenses", "Expenses", true, "Expense", ""),
    ("5210", "Administrative Expenses", "Indirect Expenses", false, "Expense", "Expense"),
    ("5220", "Salaries", "Indirect Expenses", false, "Expense", "Expense"),
    ("5230", "Rent", "Indirect Expenses", false, "Expense", "Expense"),
    ("5240", "Utilities", "Indirect Expenses", false, "Expense", "Expense"),
    ("5250", "Freight and Forwarding", "Indirect Expenses", false, "Expense", "Expense"),
    ("5260", "Exchange Gain or Loss", "Indirect Expenses", false, "Expense", "Exchange Gain Loss"),
    ("5270", "Round Off", "Indirect Expenses", false, "Expense", "Round Off"),
    ("5280", "Bank Charges", "Indirect Expenses", false, "Expense", "Expense"),
];

const GST_ACCOUNTS: &[(&str, &str, &str, &str)] = &[
    ("1153", "CGST Input", "Tax Assets", "Asset"),
    ("1154", "SGST Input", "Tax Assets", "Asset"),
    ("1155", "IGST Input", "Tax Assets", "Asset"),
    ("2133", "CGST Output", "Duties and Taxes", "Liability"),
    ("2134", "SGST Output", "Duties and Taxes", "Liability"),
    ("2135", "IGST Output", "Duties and Taxes", "Liability"),
];

/// Creates the standard chart, cost center, warehouses and default links. Idempotent.
pub async fn setup_company(hc: &mut HookCtx<'_>, company: Uuid) -> Result<bool> {
    let k = hc.kernel;
    let sctx = sys(hc.ctx);
    let existing = k.find_tx(&sctx, hc.tx, "Account", &[Filter::eq("company", company.to_string())], None, 1).await?;
    if !existing.is_empty() {
        return Ok(false);
    }
    let co = k.get_tx(&sctx, hc.tx, "Company", company, false).await?;
    let currency = co.get("default_currency").cloned().unwrap_or(Value::Null);
    let mut ids: HashMap<&str, String> = HashMap::new();
    let india = hc.ctx.schema.packages.iter().any(|p| p == "india");
    let mut accounts: Vec<(&str, &str, &str, bool, &str, &str)> = COA.to_vec();
    if india {
        accounts.extend(GST_ACCOUNTS.iter().map(|(n, name, parent, root)| (*n, *name, *parent, false, *root, "Tax")));
    }
    for (number, name, parent, is_group, root, typ) in accounts {
        let mut d = Doc::new();
        d.insert("account_name".into(), json!(name));
        d.insert("account_number".into(), json!(number));
        d.insert("company".into(), json!(company.to_string()));
        d.insert("is_group".into(), json!(is_group));
        d.insert("root_type".into(), json!(root));
        d.insert("account_currency".into(), currency.clone());
        if !typ.is_empty() {
            d.insert("account_type".into(), json!(typ));
        }
        if !parent.is_empty() {
            d.insert("parent_account".into(), json!(ids.get(parent).cloned()));
        }
        let created = k.insert_tx(&sctx, hc.tx, "Account", d).await?;
        ids.insert(name, s(&created, "id").unwrap_or_default().to_string());
    }
    let mut cc = Doc::new();
    cc.insert("cost_center_name".into(), json!("Main"));
    cc.insert("company".into(), json!(company.to_string()));
    let cc = k.insert_tx(&sctx, hc.tx, "CostCenter", cc).await?;
    let mut group = Doc::new();
    group.insert("warehouse_name".into(), json!("All Warehouses"));
    group.insert("company".into(), json!(company.to_string()));
    group.insert("is_group".into(), json!(true));
    let group = k.insert_tx(&sctx, hc.tx, "Warehouse", group).await?;
    for (name, account) in [("Stores", "Stock In Hand"), ("Finished Goods", "Stock In Hand"), ("Work In Progress", "Work In Progress")] {
        let mut w = Doc::new();
        w.insert("warehouse_name".into(), json!(name));
        w.insert("company".into(), json!(company.to_string()));
        w.insert("parent_warehouse".into(), group.get("id").cloned().unwrap_or(Value::Null));
        w.insert("account".into(), json!(ids.get(account).cloned()));
        k.insert_tx(&sctx, hc.tx, "Warehouse", w).await?;
    }
    let mut v = Doc::new();
    for (field, account) in [
        ("default_receivable_account", "Debtors"),
        ("default_payable_account", "Creditors"),
        ("default_income_account", "Sales"),
        ("default_expense_account", "Administrative Expenses"),
        ("default_cash_account", "Cash"),
        ("default_bank_account", "Primary Bank"),
        ("default_inventory_account", "Stock In Hand"),
        ("cogs_account", "Cost of Goods Sold"),
        ("srbnb_account", "Stock Received But Not Billed"),
        ("stock_adjustment_account", "Stock Adjustment"),
        ("round_off_account", "Round Off"),
        ("exchange_gain_loss_account", "Exchange Gain or Loss"),
        ("retained_earnings_account", "Retained Earnings"),
        ("opening_balance_account", "Opening Balance Equity"),
        ("overhead_absorbed_account", "Manufacturing Overheads Absorbed"),
    ] {
        v.insert(field.into(), json!(ids.get(account).cloned()));
    }
    v.insert("default_cost_center".into(), cc.get("id").cloned().unwrap_or(Value::Null));
    k.set_values_tx(&sctx, hc.tx, "Company", company, v).await?;
    if india {
        gst_templates(hc, company, &ids).await?;
    }
    Ok(true)
}

async fn gst_templates(hc: &mut HookCtx<'_>, company: Uuid, ids: &HashMap<&str, String>) -> Result<()> {
    let sctx = sys(hc.ctx);
    let row = |acc: &str, desc: &str, rate: &str| json!({"charge_type": "On Net Total", "account": ids.get(acc), "description": desc, "rate": rate});
    let templates = [
        ("GST 18% Intra-State (Sales)", "Sales", "Intra-State", true, vec![row("CGST Output", "CGST 9%", "9"), row("SGST Output", "SGST 9%", "9")]),
        ("GST 18% Inter-State (Sales)", "Sales", "Inter-State", false, vec![row("IGST Output", "IGST 18%", "18")]),
        ("GST Export (Sales)", "Sales", "Export", false, vec![row("IGST Output", "IGST 0% (LUT)", "0")]),
        ("GST 18% Intra-State (Purchase)", "Purchase", "Intra-State", true, vec![row("CGST Input", "CGST 9%", "9"), row("SGST Input", "SGST 9%", "9")]),
        ("GST 18% Inter-State (Purchase)", "Purchase", "Inter-State", false, vec![row("IGST Input", "IGST 18%", "18")]),
    ];
    for (title, kind, supply, default, rows_) in templates {
        let mut t = Doc::new();
        t.insert("title".into(), json!(title));
        t.insert("company".into(), json!(company.to_string()));
        t.insert("kind".into(), json!(kind));
        t.insert("is_default".into(), json!(default));
        t.insert("in_supply_type".into(), json!(supply));
        t.insert("rows".into(), Value::Array(rows_));
        hc.kernel.insert_tx(&sctx, hc.tx, "TaxTemplate", t).await?;
    }
    Ok(())
}

/// An item's other units: each once, and the stock unit (if listed) at 1.
pub struct ItemHook;

#[async_trait]
impl DocHook for ItemHook {
    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut seen = HashSet::new();
        let mut errors = Vec::new();
        for (i, u) in rows(doc, "uoms").iter().enumerate() {
            let unit = id(u, "uom");
            if unit.is_some() && unit == id(doc, "stock_uom") && d(u, "conversion_factor") != Decimal::ONE {
                errors.push(FieldError::new("conversion_factor", "the stock unit converts at 1").in_row("uoms", i));
            }
            if !seen.insert(unit) {
                errors.push(FieldError::new("uom", "is listed twice").in_row("uoms", i));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }
}

pub struct CompanyHook;

#[async_trait]
impl DocHook for CompanyHook {
    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        if old.is_none() {
            let company = id(doc, "id").ok_or_else(|| Error::internal("company without id"))?;
            setup_company(hc, company).await?;
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "setup_chart_of_accounts" {
            return Ok(None);
        }
        let company = id(doc, "id").ok_or_else(|| Error::internal("company without id"))?;
        let created = setup_company(hc, company).await?;
        Ok(Some(json!({"created": created, "message": if created { "Standard chart of accounts created" } else { "This company already has accounts" }})))
    }
}

fn month_starts(start: NaiveDate, end: NaiveDate) -> Vec<(NaiveDate, NaiveDate)> {
    let mut out = Vec::new();
    let mut cur = start;
    while cur <= end {
        let next = if cur.month() == 12 { NaiveDate::from_ymd_opt(cur.year() + 1, 1, 1) } else { NaiveDate::from_ymd_opt(cur.year(), cur.month() + 1, 1) }
            .unwrap_or(end);
        let period_end = (next - chrono::Duration::days(1)).min(end);
        out.push((cur, period_end));
        if next <= cur {
            break;
        }
        cur = next;
    }
    out
}

pub struct FiscalYearHook;

#[async_trait]
impl DocHook for FiscalYearHook {
    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let (start, end) = (date(doc, "start_date")?, date(doc, "end_date")?);
        if (end - start).num_days() > 400 {
            return Err(Error::field("end_date", "a fiscal year cannot be longer than 400 days"));
        }
        Ok(())
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, _old: Option<&Doc>) -> Result<()> {
        let company = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        for (a, b) in month_starts(date(doc, "start_date")?, date(doc, "end_date")?) {
            sqlx::query("insert into accounting_period (tenant_id, company_id, period_start, period_end) values ($1, $2, $3, $4) on conflict do nothing")
                .bind(hc.ctx.tenant)
                .bind(company)
                .bind(a)
                .bind(b)
                .execute(&mut **hc.tx)
                .await
                .map_err(|e| match Error::from(e) {
                    Error::Conflict(_) => Error::invalid("these dates overlap accounting periods of another fiscal year"),
                    other => other,
                })?;
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let status = match name {
            "close_period" => "closed",
            "reopen_period" => "open",
            _ => return Ok(None),
        };
        if !hc.ctx.has_role("Accounts Manager") {
            return Err(Error::Forbidden("only an Accounts Manager can close or reopen periods".into()));
        }
        let company = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let month =
            params.get("period_start").and_then(Value::as_str).ok_or_else(|| Error::field("period_start", "give the first day of the month, YYYY-MM-DD"))?;
        let start = ferrum_kernel::value::parse_date(month).map_err(|m| Error::field("period_start", m))?;
        let n = sqlx::query(
            "update accounting_period set status = $4, closed_by = case when $4 = 'closed' then $5 else null end, \
             closed_at = case when $4 = 'closed' then now() else null end where tenant_id = $1 and company_id = $2 and period_start = $3",
        )
        .bind(hc.ctx.tenant)
        .bind(company)
        .bind(start)
        .bind(status)
        .bind((!hc.ctx.user.is_nil()).then_some(hc.ctx.user))
        .execute(&mut **hc.tx)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("accounting period starting on that date".into()));
        }
        let _ = rows(doc, "");
        Ok(Some(json!({"period_start": start, "status": status})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_cover_the_year() {
        let m = month_starts(NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(), NaiveDate::from_ymd_opt(2027, 3, 31).unwrap());
        assert_eq!(m.len(), 12);
        assert_eq!(m[0].1, NaiveDate::from_ymd_opt(2026, 4, 30).unwrap());
        assert_eq!(m[11].1, NaiveDate::from_ymd_opt(2027, 3, 31).unwrap());
        let parents: Vec<&str> = COA.iter().map(|a| a.1).collect();
        for a in COA {
            assert!(a.2.is_empty() || parents.contains(&a.2), "{} has unknown parent {}", a.1, a.2);
        }
    }
}
