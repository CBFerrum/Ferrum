//! Packing slips: the packages of a draft delivery note (package number ranges, contents,
//! weights). Packed quantities never exceed the note's; once slips are submitted, the note goes
//! out only fully packed, and cancelling the note cancels its slips.

use crate::common::{child_table, installed, validation};
use async_trait::async_trait;
use ferrum_domain::flow::created;
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{d, elevated, get, id, money, rows, s, set};
use ferrum_kernel::ddl::q;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

pub const ENTITY: &str = "PackingSlip";

/// A slip of the delivery note: `(id, number, docstatus, from, to)`.
type Slip = (Uuid, String, i16, i64, i64);

async fn slips(hc: &mut HookCtx<'_>, dn: Uuid) -> Result<Vec<Slip>> {
    let table = hc.ctx.schema.entity(ENTITY).map(|e| e.table.clone()).unwrap_or_default();
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select id, coalesce(number, 'draft ' || left(id::text, 8)), docstatus, from_case_no, coalesce(to_case_no, from_case_no) from {} \
         where tenant_id = $1 and delivery_note = $2 and docstatus < 2 order by from_case_no",
        q(&table)
    )))
    .bind(hc.ctx.tenant)
    .bind(dn)
    .fetch_all(&mut **hc.tx)
    .await?)
}

/// Quantity packed per delivery note line on its live slips (except `skip`); `submitted_only`
/// counts submitted slips alone.
async fn packed(hc: &mut HookCtx<'_>, dn: Uuid, skip: Option<Uuid>, submitted_only: bool) -> Result<HashMap<String, Decimal>> {
    let items = child_table(hc.ctx, ENTITY, "items")?;
    let table = hc.ctx.schema.entity(ENTITY).map(|e| e.table.clone()).unwrap_or_default();
    let found: Vec<(Option<String>, Decimal)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select i.dn_line, sum(i.qty) from {} i join {} p on p.tenant_id = i.tenant_id and p.id = i.parent_id \
         where i.tenant_id = $1 and p.delivery_note = $2 and p.docstatus < 2 and ($3::uuid is null or p.id <> $3) and (not $4 or p.docstatus = 1) \
         group by i.dn_line",
        q(&items),
        q(&table)
    )))
    .bind(hc.ctx.tenant)
    .bind(dn)
    .bind(skip)
    .bind(submitted_only)
    .fetch_all(&mut **hc.tx)
    .await?;
    Ok(found.into_iter().map(|(l, q)| (l.unwrap_or_default(), q)).collect())
}

pub struct PackingSlipHook;

#[async_trait]
impl DocHook for PackingSlipHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        if doc.get("to_case_no").is_none_or(Value::is_null) {
            let from = doc.get("from_case_no").cloned().unwrap_or(Value::Null);
            set(doc, "to_case_no", from);
        }
        let dn_lines = match id(doc, "delivery_note") {
            Some(dn) => match get(hc.kernel, hc.ctx, hc.tx, "DeliveryNote", dn).await {
                Ok(x) => rows(&x, "items"),
                Err(Error::NotFound(_)) => vec![],
                Err(e) => return Err(e),
            },
            None => vec![],
        };
        let mut lines = rows(doc, "items");
        let mut net = Decimal::ZERO;
        for r in lines.iter_mut() {
            if s(r, "dn_line").is_none() {
                let same: Vec<&Doc> = dn_lines.iter().filter(|l| id(l, "item") == id(r, "item")).collect();
                if let [only] = same.as_slice() {
                    r.insert("dn_line".into(), only.get("id").cloned().unwrap_or(Value::Null));
                }
            }
            net += d(r, "qty") * d(r, "net_weight");
        }
        if id(doc, "weight_uom").is_none()
            && let Some(u) = lines.iter().find_map(|r| r.get("weight_uom").filter(|v| !v.is_null()).cloned())
        {
            set(doc, "weight_uom", u);
        }
        set(doc, "items", crate::common::rows_value(lines));
        set(doc, "net_weight_pkg", money(net.round_dp(9)));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let dn_id = id(doc, "delivery_note").ok_or_else(|| Error::field("delivery_note", "is required"))?;
        let dn = get(hc.kernel, hc.ctx, hc.tx, "DeliveryNote", dn_id).await?;
        let mut errors = Vec::new();
        if d(&dn, "docstatus") != Decimal::ZERO {
            errors.push(FieldError::new("delivery_note", "packing slips are made for draft delivery notes"));
        }
        if ferrum_domain::util::b(&dn, "is_return") {
            errors.push(FieldError::new("delivery_note", "returns are not packed"));
        }
        let me = id(doc, "id");
        let from = doc.get("from_case_no").and_then(Value::as_i64).unwrap_or(0);
        let to = doc.get("to_case_no").and_then(Value::as_i64).unwrap_or(from);
        for (other, number, _, a, b) in slips(hc, dn_id).await? {
            if Some(other) != me && a <= to && from <= b {
                errors.push(FieldError::new("from_case_no", format!("packages {a}-{b} are already on packing slip {number}")));
            }
        }
        let dn_lines = rows(&dn, "items");
        let mut mine: HashMap<String, Decimal> = HashMap::new();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            let line = s(r, "dn_line").unwrap_or_default();
            match dn_lines.iter().find(|l| s(l, "id") == Some(line)) {
                None => errors
                    .push(FieldError::new("dn_line", "pick the delivery note line this packs (the note has none or several for the item)").in_row("items", i)),
                Some(l) if id(l, "item") != id(r, "item") => errors.push(FieldError::new("item", "differs from the delivery note line").in_row("items", i)),
                Some(_) => *mine.entry(line.to_string()).or_default() += d(r, "qty"),
            }
        }
        let others = packed(hc, dn_id, me, false).await?;
        for (line, qty) in mine {
            let Some(l) = dn_lines.iter().find(|l| s(l, "id") == Some(line.as_str())) else { continue };
            let total = qty + others.get(&line).copied().unwrap_or_default();
            if total > d(l, "qty") {
                errors.push(FieldError::new(
                    "items",
                    format!(
                        "packing more {} than the delivery note delivers: {} packed of {}",
                        s(l, "item_name").unwrap_or("stock"),
                        total.normalize(),
                        d(l, "qty").normalize()
                    ),
                ));
            }
        }
        validation(errors)
    }
}

/// The delivery note's side: quantities never drop below what is packed, it goes out fully packed
/// once packing started, and its cancellation cancels the slips.
pub struct DeliveryPacking;

#[async_trait]
impl DocHook for DeliveryPacking {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) || !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        let Some(dn) = id(doc, "id") else { return Ok(()) };
        let done = packed(hc, dn, None, false).await?;
        if done.is_empty() {
            return Ok(());
        }
        let lines = rows(doc, "items");
        let mut errors = Vec::new();
        for (line, qty) in done {
            match lines.iter().position(|l| s(l, "id") == Some(line.as_str())) {
                Some(i) if d(&lines[i], "qty") < qty => {
                    errors.push(FieldError::new("qty", format!("{} is already packed; change the packing slips first", qty.normalize())).in_row("items", i))
                }
                Some(_) => {}
                None => errors.push(FieldError::new("items", "a removed line is packed on a packing slip; change the packing slips first")),
            }
        }
        validation(errors)
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let Some(dn) = id(doc, "id") else { return Ok(()) };
        let all = slips(hc, dn).await?;
        if let Some((_, number, _, _, _)) = all.iter().find(|x| x.2 == 0) {
            return Err(Error::invalid(format!("packing slip {number} is still a draft; submit or delete it first")));
        }
        if all.is_empty() {
            return Ok(());
        }
        let done = packed(hc, dn, None, true).await?;
        let mut errors = Vec::new();
        for (i, l) in rows(doc, "items").iter().enumerate() {
            let qty = done.get(s(l, "id").unwrap_or_default()).copied().unwrap_or_default();
            if !qty.is_zero() && qty != d(l, "qty") {
                errors.push(FieldError::new("qty", format!("only {} of {} is packed", qty.normalize(), d(l, "qty").normalize())).in_row("items", i));
            }
        }
        validation(errors)
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let Some(dn) = id(doc, "id") else { return Ok(()) };
        let ctx = elevated(hc.ctx);
        for (slip, _, status, _, _) in slips(hc, dn).await? {
            if status == 1 {
                hc.kernel.cancel_tx(&ctx, hc.tx, ENTITY, slip, None).await?;
            }
        }
        Ok(())
    }

    /// A draft packing slip with whatever is not packed yet, in the next package numbers.
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "make_packing_slip" || !installed(hc.ctx) {
            return Ok(None);
        }
        let dn = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let done = packed(hc, dn, None, false).await?;
        let next = slips(hc, dn).await?.iter().map(|x| x.4).max().unwrap_or(0) + 1;
        let mut lines = Vec::new();
        for l in rows(doc, "items") {
            let left = d(&l, "qty") - done.get(s(&l, "id").unwrap_or_default()).copied().unwrap_or_default();
            if left > Decimal::ZERO {
                lines.push(json!({"item": l.get("item"), "qty": left.normalize().to_string(), "dn_line": l.get("id"), "batch_no": l.get("batch_no")}));
            }
        }
        if lines.is_empty() {
            return Err(Error::invalid("everything on this delivery note is packed"));
        }
        let mut input = Doc::new();
        input.insert("delivery_note".into(), json!(dn.to_string()));
        input.insert("from_case_no".into(), json!(next));
        input.insert("to_case_no".into(), json!(next));
        input.insert("items".into(), Value::Array(lines));
        let slip = create_for_user(hc, ENTITY, input).await?;
        Ok(Some(created(ENTITY, &slip)))
    }
}
