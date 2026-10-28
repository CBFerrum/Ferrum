//! Opportunities: party and amounts, the sales stage ↔ status ↔ probability rules, weighted
//! amounts in the company currency, quotations made from them, and winning (or reopening) them
//! as their quotations are ordered (or those orders cancelled).

use crate::{check_assignee, lookup, lost_reason, stored, today};
use async_trait::async_trait;
use ferrum_domain::flow::created;
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{company, currency, d, elevated, exchange_rate, find, id, money, round, rows, s, set, sys};
use ferrum_kernel::{Doc, DocHook, Error, Filter, HookCtx, Op, Result};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use uuid::Uuid;

pub struct OpportunityHook;

#[async_trait]
impl DocHook for OpportunityHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let old = stored(hc, doc).await?;
        party(hc, doc, old.as_ref()).await?;
        if s(doc, "title").is_none() {
            set(doc, "title", json!(s(doc, "party_name")));
        }
        stage(hc, doc, old.as_ref()).await?;
        amounts(hc, doc, old.as_ref()).await?;
        let status = s(doc, "status").unwrap_or("Open").to_string();
        if status == "Open" {
            set(doc, "closed_on", Value::Null);
        } else if s(doc, "closed_on").is_none() || old.as_ref().is_some_and(|o| s(o, "status") != Some(&status)) {
            set(doc, "closed_on", today().to_string());
        }
        if status != "Lost" {
            set(doc, "lost_reason", Value::Null);
            set(doc, "lost_detail", Value::Null);
        }
        Ok(())
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        check_assignee(hc, doc, old).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let oid = id(doc, "id").ok_or_else(|| Error::internal("opportunity without id"))?;
        let mut patch = Doc::new();
        match name {
            "make_quotation" => return make_quotation(hc, doc).await.map(Some),
            "mark_won" => {
                patch.insert("status".into(), json!("Won"));
            }
            "mark_lost" => {
                patch.insert("status".into(), json!("Lost"));
                patch.insert("lost_reason".into(), json!(lost_reason(hc, params).await?.map(|u| u.to_string())));
                patch.insert("lost_detail".into(), params.get("lost_detail").cloned().unwrap_or(Value::Null));
            }
            _ => return Ok(None),
        }
        let saved = hc.kernel.update_tx(hc.ctx, hc.tx, "Opportunity", oid, patch, None).await?;
        Ok(Some(json!({"status": saved.get("status"), "sales_stage": saved.get("sales_stage")})))
    }
}

/// Party name, and for a lead its customer once converted (plus its prospect and source when the
/// opportunity is created). A lost or junk lead must be reopened before new business is opened.
async fn party(hc: &mut HookCtx<'_>, doc: &mut Doc, old: Option<&Doc>) -> Result<()> {
    if s(doc, "opportunity_from") == Some("Customer") {
        let c = lookup(hc, "Customer", id(doc, "customer")).await?;
        set(doc, "party_name", json!(c.as_ref().and_then(|c| s(c, "customer_name"))));
        return Ok(());
    }
    let lead_id = id(doc, "lead");
    let Some(lead) = lookup(hc, "Lead", lead_id).await? else { return Ok(()) };
    let changed = old.is_none_or(|o| id(o, "lead") != lead_id);
    if changed && let Some(st @ ("Lost" | "Junk")) = s(&lead, "status") {
        return Err(Error::field("lead", format!("lead {} is {st}; reopen it first", s(&lead, "number").unwrap_or_default())));
    }
    set(doc, "customer", json!(s(&lead, "customer")));
    set(doc, "party_name", json!(s(&lead, "organization_name").or(s(&lead, "lead_name"))));
    if changed {
        for f in ["prospect", "source", "campaign", "territory"] {
            if s(doc, f).is_none()
                && let Some(v) = s(&lead, f)
            {
                set(doc, f, v.to_string());
            }
        }
    }
    Ok(())
}

/// The first (or last) enabled sales stage with this outcome.
pub(crate) async fn stage_with(hc: &mut HookCtx<'_>, outcome: &str, last: bool) -> Result<Option<Doc>> {
    let sort = if last { "stage_order desc" } else { "stage_order asc" };
    let found = find(hc.kernel, hc.ctx, hc.tx, "SalesStage", vec![Filter::eq("outcome", outcome), Filter::eq("disabled", false)], Some(sort), 1).await?;
    Ok(found.into_iter().next())
}

fn outcome(stage: &Option<Doc>) -> String {
    stage.as_ref().and_then(|x| s(x, "outcome")).unwrap_or("Open").to_string()
}

/// Keeps stage, status and probability consistent: choosing a Won/Lost stage closes the
/// opportunity, closing it moves it to the matching stage, an open opportunity needs an open
/// stage, and a new stage brings its default probability unless the user set one in the same edit.
async fn stage(hc: &mut HookCtx<'_>, doc: &mut Doc, old: Option<&Doc>) -> Result<()> {
    let mut status = s(doc, "status").unwrap_or("Open").to_string();
    let stage_id = id(doc, "sales_stage");
    let mut stage = lookup(hc, "SalesStage", stage_id).await?;
    let stage_changed = old.is_none_or(|o| id(o, "sales_stage") != stage_id);
    let status_changed = old.is_none_or(|o| s(o, "status") != Some(&status));
    let closing = status == "Won" || status == "Lost";
    if stage.is_none() {
        stage = stage_with(hc, if closing { &status } else { "Open" }, false).await?;
    } else if stage_changed && outcome(&stage) != "Open" {
        status = outcome(&stage);
    } else if status_changed
        && closing
        && outcome(&stage) != status
        && let Some(st) = stage_with(hc, &status, false).await?
    {
        stage = Some(st);
    }
    let out = outcome(&stage);
    let stage_name = stage.as_ref().and_then(|x| s(x, "stage_name")).unwrap_or_default().to_string();
    if status == "Open" && out != "Open" {
        return Err(Error::field("sales_stage", format!("{stage_name} closes the opportunity; choose an open stage to keep it open")));
    }
    if out != "Open" && out != status {
        return Err(Error::field("sales_stage", format!("{stage_name} is a {out} stage but the opportunity is {status}")));
    }
    let new_stage = stage.as_ref().and_then(|x| id(x, "id"));
    let probability = match status.as_str() {
        "Won" => Some(Decimal::ONE_HUNDRED),
        "Lost" => Some(Decimal::ZERO),
        _ => {
            let moved = old.is_none_or(|o| id(o, "sales_stage") != new_stage);
            let untouched = match old {
                Some(o) => o.get("probability") == doc.get("probability"),
                None => doc.get("probability").is_none_or(Value::is_null),
            };
            (moved && untouched).then(|| stage.as_ref().map(|x| d(x, "probability"))).flatten()
        }
    };
    set(doc, "sales_stage", json!(new_stage.map(|u| u.to_string())));
    set(doc, "status", json!(status));
    if let Some(p) = probability {
        set(doc, "probability", money(p));
    }
    Ok(())
}

/// Amount of `probability` percent, at the currency's precision.
pub(crate) fn weighted(amount: Decimal, probability: Decimal, precision: u32) -> Decimal {
    round(amount * probability / Decimal::ONE_HUNDRED, precision)
}

/// Currency and rate, item amounts and total, weighted amount, and both in company currency so
/// the pipeline can add up opportunities in different currencies.
async fn amounts(hc: &mut HookCtx<'_>, doc: &mut Doc, old: Option<&Doc>) -> Result<()> {
    let Some(company_id) = id(doc, "company") else { return Ok(()) };
    let co = company(hc.kernel, hc.ctx, hc.tx, company_id).await?;
    if id(doc, "currency").is_none() {
        set(doc, "currency", co.currency_id.to_string());
    }
    let currency_id = id(doc, "currency").unwrap_or(co.currency_id);
    let (_, precision) = currency(hc.kernel, hc.ctx, hc.tx, currency_id).await?;
    let rate = if currency_id == co.currency_id {
        Decimal::ONE
    } else {
        // A rate the user gave stands; a new currency without one gets today's rate.
        let given = d(doc, "conversion_rate");
        let currency_changed = old.is_some_and(|o| id(o, "currency") != Some(currency_id));
        let rate_changed = old.is_none_or(|o| o.get("conversion_rate") != doc.get("conversion_rate"));
        if given > Decimal::ZERO && given != Decimal::ONE && (!currency_changed || rate_changed) {
            given
        } else {
            exchange_rate(hc.kernel, hc.ctx, hc.tx, currency_id, co.currency_id, today()).await?
        }
    };
    set(doc, "conversion_rate", money(rate));
    let mut items = rows(doc, "items");
    if !items.is_empty() {
        let mut total = Decimal::ZERO;
        for r in items.iter_mut() {
            let a = round(d(r, "qty") * d(r, "rate"), precision);
            set(r, "amount", money(a));
            total += a;
        }
        set(doc, "items", Value::Array(items.into_iter().map(Value::Object).collect()));
        set(doc, "amount", money(total));
    }
    let amount = d(doc, "amount");
    let w = weighted(amount, d(doc, "probability"), precision);
    set(doc, "weighted_amount", money(w));
    set(doc, "base_amount", money(round(amount * rate, co.precision)));
    set(doc, "base_weighted_amount", money(round(w * rate, co.precision)));
    Ok(())
}

/// A draft quotation for the opportunity's customer (or its lead until converted), carrying the
/// items and any rates already discussed.
async fn make_quotation(hc: &mut HookCtx<'_>, opp: &Doc) -> Result<Value> {
    let mut q = Map::new();
    match id(opp, "customer") {
        Some(c) => {
            q.insert("party_type".into(), json!("Customer"));
            q.insert("customer".into(), json!(c.to_string()));
        }
        None => {
            q.insert("party_type".into(), json!("Lead"));
            q.insert("lead".into(), opp.get("lead").cloned().unwrap_or(Value::Null));
        }
    }
    for f in ["company", "currency", "conversion_rate", "contact", "campaign", "assigned_to"] {
        if let Some(v) = opp.get(f).filter(|v| !v.is_null()) {
            q.insert(f.into(), v.clone());
        }
    }
    q.insert("opportunity".into(), opp.get("id").cloned().unwrap_or(Value::Null));
    let items: Vec<Value> = rows(opp, "items")
        .iter()
        .map(|r| {
            let mut line = json!({"item": r.get("item"), "qty": r.get("qty"), "description": r.get("description")});
            if d(r, "rate") > Decimal::ZERO {
                line["price_list_rate"] = json!(r.get("rate"));
            }
            line
        })
        .collect();
    q.insert("items".into(), Value::Array(items));
    let quotation = create_for_user(hc, "Quotation", q).await?;
    Ok(created("Quotation", &quotation))
}

/// An opportunity is won once one of its quotations is ordered, and reopens in its last open
/// stage when all of those orders are cancelled again.
pub(crate) async fn follow_orders(hc: &mut HookCtx<'_>, opp: Uuid) -> Result<()> {
    let doc = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "Opportunity", opp, false).await?;
    let ordered = find(
        hc.kernel,
        hc.ctx,
        hc.tx,
        "Quotation",
        vec![
            Filter::eq("opportunity", opp.to_string()),
            Filter::eq("docstatus", 1),
            Filter { field: "status".into(), op: Op::In, value: json!(["Ordered", "Partially Ordered"]) },
        ],
        None,
        1,
    )
    .await?;
    let mut patch = Doc::new();
    match (s(&doc, "status"), ordered.is_empty()) {
        (Some("Open"), false) => {
            patch.insert("status".into(), json!("Won"));
        }
        (Some("Won"), true) => {
            let Some(stage) = stage_with(hc, "Open", true).await? else { return Ok(()) };
            patch.insert("status".into(), json!("Open"));
            patch.insert("sales_stage".into(), stage.get("id").cloned().unwrap_or(Value::Null));
        }
        _ => return Ok(()),
    }
    hc.kernel.update_tx(&elevated(hc.ctx), hc.tx, "Opportunity", opp, patch, None).await?;
    Ok(())
}

/// Closes the opportunity as lost when its last live quotation is lost.
pub(crate) async fn lose_with_quotation(hc: &mut HookCtx<'_>, opp: Uuid, quotation: Uuid, reason: Uuid, detail: Value) -> Result<()> {
    let doc = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "Opportunity", opp, false).await?;
    if s(&doc, "status") != Some("Open") {
        return Ok(());
    }
    let live = find(
        hc.kernel,
        hc.ctx,
        hc.tx,
        "Quotation",
        vec![
            Filter::eq("opportunity", opp.to_string()),
            Filter::eq("docstatus", 1),
            Filter { field: "id".into(), op: Op::Ne, value: json!(quotation.to_string()) },
            Filter { field: "status".into(), op: Op::In, value: json!(["Open", "Partially Ordered", "Ordered"]) },
        ],
        None,
        1,
    )
    .await?;
    if !live.is_empty() {
        return Ok(());
    }
    let mut patch = Doc::new();
    patch.insert("status".into(), json!("Lost"));
    patch.insert("lost_reason".into(), json!(reason.to_string()));
    patch.insert("lost_detail".into(), detail);
    hc.kernel.update_tx(&elevated(hc.ctx), hc.tx, "Opportunity", opp, patch, None).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn weighted_amounts_round_to_the_currency() {
        let n = |s: &str| Decimal::from_str(s).unwrap();
        assert_eq!(weighted(n("1500"), n("60"), 2), n("900.00"));
        assert_eq!(weighted(n("333.33"), n("33.333"), 2), n("111.11"));
        assert_eq!(weighted(n("1000"), n("12.5"), 0), n("125"));
        assert_eq!(weighted(n("10"), Decimal::ZERO, 3), n("0.000"));
    }
}
