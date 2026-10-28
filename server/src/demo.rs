//! `seed_demo tenant:<slug>` — fills a tenant with a small, realistic dataset: a GST-registered
//! company with its chart of accounts, masters, opening stock, a purchase and a sales cycle, a
//! payment and a bill of materials. Safe to re-run: it stops if the demo items already exist.

use ferrum_kernel::{Ctx, Doc, Kernel, ListQuery, Result};
use serde_json::{Value, json};
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap_or_default()
}

fn id(d: &Doc) -> String {
    d.get("id").and_then(Value::as_str).unwrap_or_default().to_string()
}

async fn find(k: &Kernel, ctx: &Ctx, entity: &str, filters: Value) -> Result<Option<Doc>> {
    let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters)?, limit: Some(1), ..Default::default() };
    Ok(k.list(ctx, entity, &q).await?.rows.into_iter().next())
}

async fn submit(k: &Kernel, ctx: &Ctx, entity: &str, d: &Doc) -> Result<Doc> {
    k.submit(ctx, entity, id(d).parse().unwrap_or_default(), None).await
}

async fn follow(k: &Kernel, ctx: &Ctx, entity: &str, d: &Doc, action: &str) -> Result<Doc> {
    let out = k.run_action(ctx, entity, id(d).parse().unwrap_or_default(), action, json!({})).await?;
    let target = out["entity"].as_str().unwrap_or_default().to_string();
    let target_id: Uuid = out["id"].as_str().unwrap_or_default().parse().unwrap_or_default();
    k.get(ctx, &target, target_id, false).await
}

pub async fn seed(k: &Kernel, tenant: Uuid) -> Result<String> {
    let ctx = Ctx::system(tenant, k.registry.schema(tenant).await?);
    let ctx = &ctx;
    if find(k, ctx, "Item", json!({"item_code": "DEMO-WIDGET"})).await?.is_some() {
        return Ok("demo data already present".into());
    }
    let inr = find(k, ctx, "Currency", json!({"code": "INR"})).await?.ok_or_else(|| ferrum_kernel::Error::invalid("INR currency missing"))?;
    let india = ctx.schema.packages.iter().any(|p| p == "india");
    let company = match find(k, ctx, "Company", json!({})).await? {
        Some(c) => c,
        None => {
            let mut c = json!({"company_name": "Demo Industries Pvt Ltd", "abbr": "DI", "default_currency": inr["id"]});
            if india {
                c["in_gstin"] = json!("27AAPFU0939F1ZV");
            }
            k.insert(ctx, "Company", obj(c)).await?
        }
    };
    let cid = id(&company);
    let cuuid: Uuid = cid.parse().unwrap_or_default();
    if india && company.get("in_gstin").is_none_or(Value::is_null) {
        k.update(ctx, "Company", cuuid, obj(json!({"in_gstin": "27AAPFU0939F1ZV"})), None).await?;
    }
    k.run_action(ctx, "Company", cuuid, "setup_chart_of_accounts", json!({})).await?;
    let wh = |name: &'static str| find(k, ctx, "Warehouse", json!({"warehouse_name": name, "company": cid}));
    let stores = id(&wh("Stores").await?.unwrap_or_default());
    let group = |name: &'static str| find(k, ctx, "ItemGroup", json!({"group_name": name}));
    let products = id(&group("Products").await?.unwrap_or_default());
    let raw_group = id(&group("Raw Material").await?.unwrap_or_default());
    let nos = id(&find(k, ctx, "Uom", json!({"uom_name": "Nos"})).await?.unwrap_or_default());

    let gstin = |first14: &str| match india {
        true => json!(format!("{first14}{}", ferrum_domain::india::gstin_check_char(first14).unwrap_or('0'))),
        false => Value::Null,
    };
    let supplier = k.insert(ctx, "Supplier", obj(json!({"supplier_name": "Shakti Steels", "in_gstin": gstin("27AAACS1111C1Z")}))).await?;
    let customer = k.insert(ctx, "Customer", obj(json!({"customer_name": "Bharat Retail", "in_gstin": gstin("29AABCB2222D1Z")}))).await?;
    let raw = k
        .insert(
            ctx,
            "Item",
            obj(json!({"item_code": "DEMO-STEEL", "item_name": "Steel Sheet 2mm", "item_group": raw_group, "stock_uom": nos, "hsn_code": "7208",
                       "safety_stock": "50", "suppliers": [{"supplier": id(&supplier), "priority": 1}]})),
        )
        .await?;
    let widget = k
        .insert(
            ctx,
            "Item",
            obj(json!({"item_code": "DEMO-WIDGET", "item_name": "Industrial Widget", "item_group": products, "stock_uom": nos, "hsn_code": "8479",
                       "standard_rate": "1500", "is_manufactured": true, "is_purchased": false})),
        )
        .await?;
    k.insert(
        ctx,
        "Item",
        obj(json!({"item_code": "DEMO-SERVICE", "item_name": "Installation Service", "item_group": products, "stock_uom": nos,
        "hsn_code": "998719", "is_stock_item": false, "standard_rate": "2500"})),
    )
    .await?;

    let opening = k
        .insert(
            ctx,
            "StockEntry",
            obj(json!({"company": cid, "purpose": "Opening Stock", "items": [
                {"item": id(&raw), "qty": "500", "basic_rate": "120", "target_warehouse": stores},
                {"item": id(&widget), "qty": "40", "basic_rate": "900", "target_warehouse": stores}]})),
        )
        .await?;
    submit(k, ctx, "StockEntry", &opening).await?;

    let po = k
        .insert(
            ctx,
            "PurchaseOrder",
            obj(json!({"company": cid, "supplier": id(&supplier), "items": [{"item": id(&raw), "qty": "200", "price_list_rate": "125", "warehouse": stores}]})),
        )
        .await?;
    submit(k, ctx, "PurchaseOrder", &po).await?;
    let pr = follow(k, ctx, "PurchaseOrder", &po, "make_purchase_receipt").await?;
    submit(k, ctx, "PurchaseReceipt", &pr).await?;
    let pi = follow(k, ctx, "PurchaseReceipt", &pr, "make_purchase_invoice").await?;
    k.update(ctx, "PurchaseInvoice", id(&pi).parse().unwrap_or_default(), obj(json!({"bill_no": "SS/2026/0412"})), None).await?;
    submit(k, ctx, "PurchaseInvoice", &pi).await?;

    let so = k
        .insert(
            ctx,
            "SalesOrder",
            obj(json!({"company": cid, "customer": id(&customer), "set_warehouse": stores, "items": [{"item": id(&widget), "qty": "12"}]})),
        )
        .await?;
    submit(k, ctx, "SalesOrder", &so).await?;
    let dn = follow(k, ctx, "SalesOrder", &so, "make_delivery_note").await?;
    submit(k, ctx, "DeliveryNote", &dn).await?;
    let si = follow(k, ctx, "DeliveryNote", &dn, "make_sales_invoice").await?;
    let si = submit(k, ctx, "SalesInvoice", &si).await?;
    let pe = k.run_action(ctx, "SalesInvoice", id(&si).parse().unwrap_or_default(), "make_payment", json!({})).await?;
    let pe_id: Uuid = pe["id"].as_str().unwrap_or_default().parse().unwrap_or_default();
    k.update(
        ctx,
        "PaymentEntry",
        pe_id,
        obj(json!({"paid_amount": "10000", "references": [{"invoice_type": "Sales Invoice", "invoice": id(&si), "allocated_amount": "10000"}]})),
        None,
    )
    .await?;
    k.submit(ctx, "PaymentEntry", pe_id, None).await?;

    let bom = k
        .insert(
            ctx,
            "Bom",
            obj(json!({"company": cid, "item": id(&widget), "quantity": "1", "operating_cost": "150", "items": [{"item": id(&raw), "qty": "4"}]})),
        )
        .await?;
    submit(k, ctx, "Bom", &bom).await?;
    let so2 = k
        .insert(
            ctx,
            "SalesOrder",
            obj(json!({"company": cid, "customer": id(&customer), "set_warehouse": stores, "items": [{"item": id(&widget), "qty": "60"}]})),
        )
        .await?;
    submit(k, ctx, "SalesOrder", &so2).await?;
    Ok(format!("demo data created for company {cid}"))
}
