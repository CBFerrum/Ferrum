//! Procurement: material requests, requests for quotation, supplier quotations and scorecards.
//!
//! * A `MaterialRequest` says what is needed, where and by when. Purchase requests are covered by
//!   purchase orders (`make_purchase_order`, grouped by each item's preferred supplier) and then
//!   receipts; transfers, issues and customer-provided goods by stock entries. Every linked line
//!   carries the request line it fulfils (`proc_*` overlay fields), and the hooks on those
//!   documents keep the request's ordered/received quantities and status exact on submit and
//!   cancel, refusing over-ordering and orders against stopped requests.
//! * Items get per-warehouse re-order levels; upkeep raises (submitted) requests when the
//!   projected quantity falls below them, never two open requests for one item and warehouse.
//! * A `RequestForQuotation` is emailed to each supplier as an HMAC-signed, expiring link to a
//!   public page where they enter rates; that becomes their `SupplierQuotation`, priced by the
//!   buying engine, which expires after its validity and turns into a purchase order.
//! * A `SupplierScorecard` scores ended months or quarters from on-time delivery, received
//!   quality, price against quotations and RFQ responses; its standing warns about or blocks new
//!   RFQs and purchase orders for that supplier.

mod material_request;
mod quotation;
mod reorder;
mod reports;
pub mod rfq;
mod scorecard;

use ferrum_domain::reports::ReportSource;
use ferrum_kernel::{Ctx, HookRegistry, Kernel, Result, Tx};
use rust_decimal::Decimal;
use std::sync::Arc;
use uuid::Uuid;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// This app's manifest (`app.yaml`).
pub const MANIFEST: &str = include_str!("../app.yaml");

/// The package name, as tenants install it.
pub(crate) const PACKAGE: &str = "procurement";

/// This app's metadata packages, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers this app's document hooks, the supplier mail sender and the supplier portal.
pub fn register(h: &mut HookRegistry) {
    h.register("MaterialRequest", Arc::new(material_request::MaterialRequestHook))
        .register("PurchaseOrder", Arc::new(material_request::PurchaseOrderLinks))
        .register("PurchaseReceipt", Arc::new(material_request::ReceiptLinks))
        .register("PurchaseInvoice", Arc::new(material_request::ReceiptLinks))
        .register("StockEntry", Arc::new(material_request::StockEntryLinks))
        .register("RequestForQuotation", Arc::new(rfq::RfqHook))
        .register("SupplierQuotation", Arc::new(quotation::QuotationHook))
        .register("SupplierScorecard", Arc::new(scorecard::ScorecardHook))
        .register("Item", Arc::new(reorder::ItemReorderHook))
        .add_event_sink(Arc::new(rfq::RfqMailer))
        .add_site(PACKAGE, Arc::new(rfq::SupplierPortal));
}

/// This app's reports.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![Arc::new(reports::ProcurementReports)]
}

/// Periodic upkeep (every minute, one replica at a time): auto-reorder, quotation expiry and
/// scorecard periods, per tenant that installed the package. Every step is idempotent; a failing
/// tenant or step does not stop the others, and the first failure is returned at the end.
pub async fn maintenance(k: &Kernel) -> Result<()> {
    let mut sys = k.db.system_tx().await?;
    let tenants: Vec<Uuid> =
        sqlx::query_scalar("select t.id from tenant t join installed_package p on p.tenant_id = t.id and p.name = $1 where t.status = 'active' order by t.id")
            .bind(PACKAGE)
            .fetch_all(&mut *sys)
            .await?;
    sys.commit().await?;
    let mut first_error = None;
    for tenant in tenants {
        let schema = match k.registry.schema(tenant).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(%tenant, "procurement upkeep: {e}");
                first_error.get_or_insert(e);
                continue;
            }
        };
        if !schema.packages.iter().any(|p| p == PACKAGE) {
            continue;
        }
        let ctx = Ctx::system(tenant, schema);
        let steps = [
            ("auto-reorder", reorder::auto_reorder(k, &ctx).await),
            ("quotation expiry", quotation::expire(k, &ctx).await),
            ("scorecards", scorecard::score_due(k, &ctx).await),
        ];
        for (step, outcome) in steps {
            if let Err(e) = outcome {
                tracing::warn!(%tenant, step, "procurement upkeep failed: {e}");
                first_error.get_or_insert(e);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Whether the tenant behind `ctx` installed this package (hooks on other packages' entities
/// run for every tenant and must stay out of the way elsewhere).
pub(crate) fn installed(ctx: &Ctx) -> bool {
    ctx.schema.packages.iter().any(|p| p == PACKAGE)
}

/// Procurement Settings, with defaults when the tenant never saved them.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub public_url: Option<String>,
    pub response_days: i64,
    pub validity_days: i64,
    pub over_order_allowance: Decimal,
    pub auto_reorder: bool,
}

pub(crate) async fn settings(tx: &mut Tx, tenant: Uuid) -> Result<Settings> {
    let row: Option<(Option<String>, Option<i64>, Option<i64>, Option<Decimal>, Option<bool>)> = sqlx::query_as(
        "select public_url, default_response_days, quotation_validity_days, over_order_allowance, auto_reorder \
         from e_procurement_settings where tenant_id = $1 limit 1",
    )
    .bind(tenant)
    .fetch_optional(&mut **tx)
    .await?;
    let (url, response, validity, allowance, auto) = row.unwrap_or_default();
    Ok(Settings {
        public_url: url.map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty()),
        response_days: response.unwrap_or(7).clamp(1, 365),
        validity_days: validity.unwrap_or(30).clamp(1, 3650),
        over_order_allowance: allowance.unwrap_or_default().clamp(Decimal::ZERO, Decimal::ONE_HUNDRED),
        auto_reorder: auto.unwrap_or(true),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn packages_validate_with_the_platform() {
        let mut pkgs = ferrum_domain::packages().expect("platform packages");
        pkgs.extend(super::packages().expect("procurement packages parse"));
        let catalog = ferrum_meta::Catalog::from_packages(pkgs).expect("they validate together");
        let schema = ferrum_meta::effective::build(&catalog, &["procurement".into()], &[], 1).expect("effective schema");
        for e in ["MaterialRequest", "RequestForQuotation", "SupplierQuotation", "SupplierScorecard", "SupplierScorecardPeriod", "ProcurementSettings"] {
            assert!(schema.entity(e).is_some(), "{e}");
        }
        let po_item = schema.entity("PurchaseOrderItem").expect("overlaid");
        assert_eq!(po_item.storage_of("proc_mr_line"), Some(ferrum_meta::Storage::Ext));
        assert!(schema.entity("Item").expect("item").children.contains_key("proc_reorder_levels"));
        // Without the package nothing of it leaks into other tenants' schemas.
        let plain = ferrum_meta::effective::build(&catalog, &["buying".into(), "selling".into()], &[], 1).expect("plain schema");
        assert!(plain.entity("PurchaseOrderItem").expect("po item").field("proc_mr_line").is_none());
    }
}
