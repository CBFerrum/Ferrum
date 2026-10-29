//! Product bundles: a non-stock item sold as a kit of components.
//!
//! The selling engine posts stock only for stock items, so a bundle line on a delivery note (or a
//! sales invoice that updates stock) moves nothing by itself. This module lists the line's
//! components as packed items (where the user picks warehouse, batch and serials) and, after the
//! engine's own postings, moves them under the same document: issues for a delivery, receipts at the
//! cost they left at for a return. The moves and their cost-of-goods-sold entry carry the
//! document as their source, so the engine's cancellation reverses them with everything else.

use crate::common::{child_table, finish_rows, installed, items, rows_value, serials, stock_lines, validation};
use async_trait::async_trait;
use ferrum_domain::ledger::{self, Entry, Line};
use ferrum_domain::stock::{self, MoveIn};
use ferrum_domain::util::{b, company, d, date, id, money, posting_ts, rows, s, set, stock_qty, sys};
use ferrum_kernel::ddl::q;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub const PACKED: &str = "stk_packed_items";

/// Components of the enabled bundles among `items`: bundle item → `[(component, qty per bundle)]`.
pub async fn bundles(tx: &mut Tx, ctx: &ferrum_kernel::Ctx, items: &[Uuid]) -> Result<HashMap<Uuid, Vec<(Uuid, Decimal)>>> {
    if items.is_empty() || ctx.schema.entity("ProductBundle").is_none() {
        return Ok(HashMap::new());
    }
    let table = child_table(ctx, "ProductBundle", "items")?;
    let rows: Vec<(Uuid, Uuid, Decimal)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select b.item, c.item, c.qty from e_product_bundle b join {} c on c.tenant_id = b.tenant_id and c.parent_id = b.id \
         where b.tenant_id = $1 and b.item = any($2) and not coalesce(b.disabled, false) order by b.item, c.idx",
        q(&table)
    )))
    .bind(ctx.tenant)
    .bind(items)
    .fetch_all(&mut **tx)
    .await?;
    let mut out: HashMap<Uuid, Vec<(Uuid, Decimal)>> = HashMap::new();
    for (bundle, comp, qty) in rows {
        out.entry(bundle).or_default().push((comp, qty));
    }
    Ok(out)
}

/// Validates bundle definitions.
pub struct BundleHook;

#[async_trait]
impl DocHook for BundleHook {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        let bundle = id(doc, "item");
        let comps = rows(doc, "items");
        let mut ids: Vec<Uuid> = comps.iter().filter_map(|r| id(r, "item")).collect();
        ids.extend(bundle);
        let infos = items(hc.tx, hc.ctx.tenant, &ids).await?;
        if let Some(info) = bundle.and_then(|b| infos.get(&b)) {
            if info.is_stock {
                errors.push(FieldError::new("item", format!("{} is a stock item; a bundle item must not keep stock of its own", info.code)));
            }
            if info.template {
                errors.push(FieldError::new("item", format!("{} is a variant template", info.code)));
            }
        }
        let nested = bundles(hc.tx, hc.ctx, &comps.iter().filter_map(|r| id(r, "item")).collect::<Vec<_>>()).await?;
        let mut seen = HashSet::new();
        for (i, r) in comps.iter().enumerate() {
            let Some(c) = id(r, "item") else { continue };
            let code = infos.get(&c).map(|x| x.code.clone()).unwrap_or_default();
            let mut err = |m: String| errors.push(FieldError::new("item", m).in_row("items", i));
            if Some(c) == bundle {
                err("a bundle cannot contain itself".into());
            } else if nested.contains_key(&c) {
                err(format!("{code} is itself a product bundle; list its components instead"));
            }
            if infos.get(&c).is_some_and(|x| x.template) {
                err(format!("{code} is a variant template; choose one of its variants"));
            }
            if !seen.insert(c) {
                err(format!("{code} is listed twice"));
            }
        }
        validation(errors)
    }
}

/// Unit cost at which a component left on the original document of a return.
async fn returned_cost(hc: &mut HookCtx<'_>, entity: &str, original: Uuid, item: Uuid) -> Result<Option<Decimal>> {
    let r: Option<Decimal> = sqlx::query_scalar(
        "select sum(value) / nullif(sum(qty), 0) from stock_move where tenant_id = $1 and source_entity = $2 and source_id = $3 \
         and item_id = $4 and qty < 0 and reversal_of is null",
    )
    .bind(hc.ctx.tenant)
    .bind(entity)
    .bind(original)
    .bind(item)
    .fetch_one(&mut **hc.tx)
    .await?;
    Ok(r.map(|x| x.abs().round_dp(12)))
}

/// Packed components of delivery notes and stock-updating sales invoices.
pub struct PackedStock;

impl PackedStock {
    fn applies(hc: &HookCtx<'_>, doc: &Doc) -> bool {
        installed(hc.ctx) && (hc.entity.def.name == "DeliveryNote" || b(doc, "update_stock"))
    }
}

#[async_trait]
impl DocHook for PackedStock {
    /// Lists every bundle line's components, keeping what the user chose for each (warehouse,
    /// batch, serials, or a split of one component over several rows).
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        if !Self::applies(hc, doc) {
            if !rows(doc, PACKED).is_empty() {
                set(doc, PACKED, Value::Array(vec![]));
            }
            return Ok(());
        }
        let lines = rows(doc, "items");
        let kits = bundles(hc.tx, hc.ctx, &lines.iter().filter_map(|r| id(r, "item")).collect::<Vec<_>>()).await?;
        let existing = rows(doc, PACKED);
        if kits.is_empty() {
            if !existing.is_empty() {
                set(doc, PACKED, Value::Array(vec![]));
            }
            return Ok(());
        }
        let comp_ids: Vec<Uuid> = kits.values().flatten().map(|(c, _)| *c).collect();
        let infos = items(hc.tx, hc.ctx.tenant, &comp_ids).await?;
        let mut out: Vec<Doc> = Vec::new();
        for line in &lines {
            let (Some(bundle), Some(line_id)) = (id(line, "item"), s(line, "id")) else { continue };
            let Some(comps) = kits.get(&bundle) else { continue };
            for (comp, per) in comps {
                let required = (stock_qty(line) * per).round_dp(9);
                let mut group: Vec<Doc> = existing.iter().filter(|r| s(r, "parent_line") == Some(line_id) && id(r, "item") == Some(*comp)).cloned().collect();
                if group.is_empty() {
                    group.push(Doc::new());
                }
                let single = group.len() == 1;
                for mut r in group {
                    if single {
                        r.insert("qty".into(), money(required));
                    }
                    r.insert("parent_item".into(), json!(bundle.to_string()));
                    r.insert("parent_line".into(), json!(line_id));
                    r.insert("item".into(), json!(comp.to_string()));
                    if let Some(info) = infos.get(comp) {
                        r.insert("item_name".into(), json!(info.name));
                        r.insert("uom".into(), info.stock_uom.map(|u| json!(u.to_string())).unwrap_or(Value::Null));
                    }
                    if id(&r, "warehouse").is_none() {
                        r.insert("warehouse".into(), line.get("warehouse").cloned().unwrap_or(Value::Null));
                    }
                    out.push(r);
                }
            }
        }
        finish_rows(&mut out);
        set(doc, PACKED, rows_value(out));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !Self::applies(hc, doc) {
            return Ok(());
        }
        let packed = rows(doc, PACKED);
        if packed.is_empty() {
            return Ok(());
        }
        let lines = rows(doc, "items");
        let kits = bundles(hc.tx, hc.ctx, &lines.iter().filter_map(|r| id(r, "item")).collect::<Vec<_>>()).await?;
        let infos = items(hc.tx, hc.ctx.tenant, &packed.iter().filter_map(|r| id(r, "item")).collect::<Vec<_>>()).await?;
        let mut errors = Vec::new();
        for line in &lines {
            let (Some(bundle), Some(line_id)) = (id(line, "item"), s(line, "id")) else { continue };
            for (comp, per) in kits.get(&bundle).into_iter().flatten() {
                let required = (stock_qty(line) * per).round_dp(9);
                let given: Decimal = packed.iter().filter(|r| s(r, "parent_line") == Some(line_id) && id(r, "item") == Some(*comp)).map(|r| d(r, "qty")).sum();
                if given != required {
                    let code = infos.get(comp).map(|i| i.code.clone()).unwrap_or_default();
                    errors.push(FieldError::new(
                        PACKED,
                        format!("the packed rows of {code} add up to {} but the bundle needs {}", given.normalize(), required.normalize()),
                    ));
                }
            }
        }
        for (i, r) in packed.iter().enumerate() {
            let stock = id(r, "item").and_then(|c| infos.get(&c)).is_some_and(|x| x.is_stock);
            if stock && id(r, "warehouse").is_none() {
                errors.push(FieldError::new("warehouse", "is required for stock components").in_row(PACKED, i));
            }
        }
        validation(errors)
    }

    /// Locks every balance the document will touch, its own lines' and the components', in one
    /// ordered step before any posting, so concurrent deliveries cannot deadlock.
    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !Self::applies(hc, doc) || rows(doc, PACKED).is_empty() {
            return Ok(());
        }
        let mut keys: Vec<(Uuid, Uuid)> = rows(doc, "items").iter().filter_map(|r| Some((id(r, "item")?, id(r, "warehouse")?))).collect();
        keys.extend(rows(doc, PACKED).iter().filter_map(|r| Some((id(r, "item")?, id(r, "warehouse")?))));
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        stock::lock_balances(hc.tx, hc.ctx.tenant, cid, &keys).await?;
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !Self::applies(hc, doc) {
            return Ok(());
        }
        let packed = rows(doc, PACKED);
        if packed.is_empty() {
            return Ok(());
        }
        let entity = hc.entity.def.name.clone();
        let did = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let ret = b(doc, "is_return");
        // A delivery note carries positive quantities (direction from is_return); an invoice
        // carries negative ones on credit notes and always issues.
        let direction = if entity == "DeliveryNote" && ret { Decimal::ONE } else { -Decimal::ONE };
        let infos = items(hc.tx, hc.ctx.tenant, &packed.iter().filter_map(|r| id(r, "item")).collect::<Vec<_>>()).await?;
        let mut moves = Vec::new();
        for r in &packed {
            let Some(info) = id(r, "item").and_then(|c| infos.get(&c)) else { continue };
            if !info.is_stock {
                continue;
            }
            let qty = d(r, "qty") * direction;
            if qty.is_zero() {
                continue;
            }
            let rate = match (qty > Decimal::ZERO, id(doc, "return_against")) {
                (true, Some(orig)) => returned_cost(hc, &entity, orig, info.id).await?,
                _ => None,
            };
            moves.push(MoveIn {
                item: info.id,
                warehouse: id(r, "warehouse").ok_or_else(|| Error::field(PACKED, "a stock component has no warehouse"))?,
                qty,
                rate,
                batch_no: s(r, "batch_no").map(str::to_string),
                serials: serials(s(r, "serial_no")),
                source_line: id(r, "id"),
                ..Default::default()
            });
        }
        let on = date(doc, "posting_date")?;
        let posted = stock::post_moves(hc.tx, hc.ctx, &co, &entity, did, on, posting_ts(doc)?, moves, "delivered").await?;
        let cogs = co.require("cogs_account", "cost of goods sold account")?;
        let mut entry = Entry::new(co.id, on, &entity, did, s(doc, "number").map(str::to_string));
        entry.memo = Some("Product bundle components".into());
        for (line, p) in stock_lines(hc.tx, hc.ctx.tenant, &co, &posted).await?.into_iter().zip(&posted) {
            entry.lines.push(line);
            entry.lines.push(Line::new(cogs, -p.value, &co.currency_code));
        }
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;

        // Costs on the packed rows, and the kit's unit cost on its line.
        let sctx = sys(hc.ctx);
        let mut per_line: HashMap<String, Decimal> = HashMap::new();
        for p in &posted {
            let Some(row) = p.source_line else { continue };
            let mut v = Doc::new();
            v.insert("valuation_rate".into(), money(p.rate.round_dp(9)));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, &entity, PACKED, row, v).await?;
            if let Some(parent) = packed.iter().find(|r| id(r, "id") == Some(row)).and_then(|r| s(r, "parent_line")) {
                *per_line.entry(parent.to_string()).or_default() += p.value.abs();
            }
        }
        for line in rows(doc, "items") {
            let (Some(lid), Some(total)) = (id(&line, "id"), s(&line, "id").and_then(|k| per_line.get(k))) else { continue };
            let q = stock_qty(&line).abs();
            if q.is_zero() {
                continue;
            }
            let mut v = Doc::new();
            v.insert("valuation_rate".into(), money((*total / q).round_dp(9)));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, &entity, "items", lid, v).await?;
        }
        Ok(())
    }
}
