//! Leads: one lead per email address, qualification, conversion into a customer with its contact
//! and address, and opportunities created from a lead.

use crate::{check_assignee, lookup, person_name, today};
use async_trait::async_trait;
use ferrum_domain::flow::created;
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{b, find, id, s, set, sys};
use ferrum_kernel::{Doc, DocHook, Error, Filter, HookCtx, Op, Result};
use serde_json::{Map, Value, json};
use uuid::Uuid;

pub struct LeadHook;

#[async_trait]
impl DocHook for LeadHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if s(doc, "organization_name").is_none()
            && let Some(p) = lookup(hc, "Prospect", id(doc, "prospect")).await?
        {
            set(doc, "organization_name", json!(s(&p, "prospect_name")));
        }
        let person = person_name(s(doc, "first_name"), s(doc, "last_name"));
        let name = if person.is_empty() { s(doc, "organization_name").unwrap_or_default().to_string() } else { person };
        set(doc, "lead_name", name);
        let status = s(doc, "status").unwrap_or("New").to_string();
        if matches!(status.as_str(), "Qualified" | "Converted") && s(doc, "qualified_on").is_none() {
            set(doc, "qualified_on", today().to_string());
            set(doc, "qualified_by", hc.ctx.email.clone());
        }
        if status != "Lost" {
            set(doc, "lost_reason", Value::Null);
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if s(doc, "lead_name").is_none() {
            return Err(Error::field("first_name", "enter the person's name or the organization"));
        }
        let Some(email) = s(doc, "email") else { return Ok(()) };
        // Two saves of the same address queue here, so the second one sees the first.
        sqlx::query("select pg_advisory_xact_lock(hashtext($1))")
            .bind(format!("ferrum:crm:lead-email:{}:{email}", hc.ctx.tenant))
            .execute(&mut **hc.tx)
            .await?;
        let same = find(hc.kernel, hc.ctx, hc.tx, "Lead", vec![Filter::eq("email", email)], None, 2).await?;
        if let Some(other) = same.iter().find(|d| id(d, "id") != id(doc, "id")) {
            return Err(Error::field(
                "email",
                format!("a lead with this email already exists: {} ({})", s(other, "number").unwrap_or_default(), s(other, "lead_name").unwrap_or_default()),
            ));
        }
        Ok(())
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        check_assignee(hc, doc, old).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        match name {
            "convert_to_customer" => convert(hc, doc, params).await.map(Some),
            "make_opportunity" => make_opportunity(hc, doc).await.map(Some),
            _ => Ok(None),
        }
    }
}

fn copy(from: &Doc, to: &mut Doc, fields: &[&str]) {
    for f in fields {
        if let Some(v) = from.get(*f).filter(|v| !v.is_null() && v.as_str() != Some("")) {
            to.insert((*f).to_string(), v.clone());
        }
    }
}

/// Marks the lead qualified (an opportunity or a conversion proves it), keeping the first date.
fn qualify(lead: &Doc, hc: &HookCtx<'_>, values: &mut Doc) {
    if s(lead, "qualified_on").is_none() {
        values.insert("qualified_on".into(), json!(today().to_string()));
        values.insert("qualified_by".into(), json!(hc.ctx.email));
    }
}

/// Creates the customer (or attaches to an existing one: the `customer` parameter, or the one the
/// lead's prospect already became) and, unless the lead already has them, a contact for the
/// person and an address when one was captured; then moves everything kept for the lead
/// (opportunities, quotations, activities, contacts, addresses) over to the customer. A lead
/// converts once.
async fn convert(hc: &mut HookCtx<'_>, lead: &Doc, params: &Value) -> Result<Value> {
    let lead_id = id(lead, "id").ok_or_else(|| Error::internal("lead without id"))?;
    if id(lead, "customer").is_some() {
        return Err(Error::invalid(format!("lead {} was already converted to a customer", s(lead, "number").unwrap_or_default())));
    }
    let prospect = lookup(hc, "Prospect", id(lead, "prospect")).await?;
    let requested = params.get("customer").and_then(Value::as_str).filter(|v| !v.is_empty());
    let existing = match requested {
        Some(c) => {
            let cid: Uuid = c.parse().map_err(|_| Error::field("customer", "is not a record id"))?;
            Some(hc.kernel.get_tx(hc.ctx, hc.tx, "Customer", cid, false).await?)
        }
        None => lookup(hc, "Customer", prospect.as_ref().and_then(|p| id(p, "customer"))).await?,
    };
    let is_new_customer = existing.is_none();
    let customer = match existing {
        Some(c) => c,
        None => {
            let org = s(lead, "organization_name");
            let mut c = Doc::new();
            c.insert("customer_name".into(), json!(org.or(s(lead, "lead_name"))));
            c.insert("customer_type".into(), json!(if org.is_some() { "Company" } else { "Individual" }));
            c.insert("email".into(), json!(s(lead, "email")));
            c.insert("phone".into(), json!(s(lead, "phone").or(s(lead, "mobile"))));
            if let Some(g) = params.get("customer_group").and_then(Value::as_str).filter(|v| !v.is_empty()) {
                c.insert("customer_group".into(), json!(g));
            }
            create_for_user(hc, "Customer", c).await?
        }
    };
    let cid = id(&customer, "id").ok_or_else(|| Error::internal("customer without id"))?;

    let of_lead = || vec![Filter::eq("lead", lead_id.to_string())];
    let kept_contact = find(hc.kernel, hc.ctx, hc.tx, "Contact", of_lead(), Some("created_at asc"), 1).await?.into_iter().next();
    let kept_address = find(hc.kernel, hc.ctx, hc.tx, "Address", of_lead(), Some("created_at asc"), 1).await?.into_iter().next();

    let contact = match (kept_contact, s(lead, "first_name"), s(lead, "last_name")) {
        (Some(kept), _, _) => Some(kept),
        (None, None, None) => None,
        (None, first, last) => {
            let mut ct = Doc::new();
            ct.insert("first_name".into(), json!(first.or(last)));
            ct.insert("last_name".into(), json!(first.and(last)));
            ct.insert("designation".into(), json!(s(lead, "job_title")));
            if let Some(e) = s(lead, "email") {
                ct.insert("emails".into(), json!([{"email": e, "is_primary": true}]));
            }
            let mut phones = Vec::new();
            if let Some(p) = s(lead, "phone") {
                phones.push(json!({"phone": p, "phone_type": "Work"}));
            }
            if let Some(m) = s(lead, "mobile") {
                phones.push(json!({"phone": m, "phone_type": "Mobile"}));
            }
            ct.insert("phones".into(), Value::Array(phones));
            ct.insert("customer".into(), json!(cid.to_string()));
            ct.insert("lead".into(), json!(lead_id.to_string()));
            ct.insert("unsubscribed".into(), json!(b(lead, "unsubscribed")));
            copy(lead, &mut ct, &["prospect"]);
            Some(create_for_user(hc, "Contact", ct).await?)
        }
    };

    let address = if kept_address.is_some() {
        kept_address
    } else if s(lead, "address_line1").is_some() {
        let mut a = Doc::new();
        copy(lead, &mut a, &["address_line1", "address_line2", "city", "state", "postal_code", "country", "email", "phone"]);
        a.insert("address_title".into(), json!(s(&customer, "customer_name")));
        a.insert("address_type".into(), json!("Billing"));
        // An existing customer keeps its preferred address.
        a.insert("is_primary".into(), json!(is_new_customer));
        a.insert("customer".into(), json!(cid.to_string()));
        a.insert("lead".into(), json!(lead_id.to_string()));
        Some(create_for_user(hc, "Address", a).await?)
    } else {
        None
    };

    let sctx = sys(hc.ctx);
    let mut v = Doc::new();
    v.insert("status".into(), json!("Converted"));
    v.insert("customer".into(), json!(cid.to_string()));
    v.insert("contact".into(), json!(contact.as_ref().and_then(|c| s(c, "id"))));
    v.insert("converted_on".into(), json!(today().to_string()));
    qualify(lead, hc, &mut v);
    hc.kernel.set_values_tx(&sctx, hc.tx, "Lead", lead_id, v).await?;
    if let Some(p) = prospect.as_ref().filter(|p| id(p, "customer").is_none()).and_then(|p| id(p, "id")) {
        let mut v = Doc::new();
        v.insert("customer".into(), json!(cid.to_string()));
        hc.kernel.set_values_tx(&sctx, hc.tx, "Prospect", p, v).await?;
    }
    for entity in ["Opportunity", "Quotation", "Activity", "Contact", "Address"] {
        let open = find(
            hc.kernel,
            hc.ctx,
            hc.tx,
            entity,
            vec![Filter::eq("lead", lead_id.to_string()), Filter { field: "customer".into(), op: Op::Is, value: json!("not set") }],
            None,
            10_000,
        )
        .await?;
        for d in open {
            let Some(rid) = id(&d, "id") else { continue };
            let mut v = Doc::new();
            v.insert("customer".into(), json!(cid.to_string()));
            hc.kernel.set_values_tx(&sctx, hc.tx, entity, rid, v).await?;
        }
    }

    let mut out = created("Customer", &customer);
    out["contact"] = json!(contact.as_ref().and_then(|c| s(c, "id")));
    out["address"] = json!(address.as_ref().and_then(|a| s(a, "id")));
    Ok(out)
}

/// A new opportunity for the lead, which counts as qualified from then on.
async fn make_opportunity(hc: &mut HookCtx<'_>, lead: &Doc) -> Result<Value> {
    let lead_id = id(lead, "id").ok_or_else(|| Error::internal("lead without id"))?;
    let mut o = Map::new();
    o.insert("opportunity_from".into(), json!("Lead"));
    o.insert("lead".into(), json!(lead_id.to_string()));
    copy(lead, &mut o, &["company", "source", "campaign", "territory", "prospect", "assigned_to", "contact"]);
    let opp = create_for_user(hc, "Opportunity", o).await?;
    if matches!(s(lead, "status"), Some("New" | "Contacted")) {
        let mut v = Doc::new();
        v.insert("status".into(), json!("Qualified"));
        qualify(lead, hc, &mut v);
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "Lead", lead_id, v).await?;
    }
    Ok(created("Opportunity", &opp))
}
