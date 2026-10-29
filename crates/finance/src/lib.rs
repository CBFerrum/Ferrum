//! Finance: budgets with commitment control, bank accounts with statement import and
//! reconciliation, period closing, payment terms and schedules, dunning, subscriptions and the
//! allocation of advance payments.
//!
//! Every posting goes through `ferrum_domain::ledger` (balanced entries, period locks,
//! append-only reversals, open items and allocations). Hooks on other packages' documents
//! (invoices, orders, payments, journals, any submitted document for budgets) do nothing for
//! tenants that did not install this package.

mod advances;
mod bank;
mod budget;
mod closing;
mod common;
mod dunning;
mod reports;
mod rules;
mod subscription;
mod terms;

use ferrum_domain::reports::ReportSource;
use ferrum_kernel::{HookRegistry, Kernel, Result};
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// This app's manifest (`app.yaml`).
pub const MANIFEST: &str = include_str!("../app.yaml");

/// This app's metadata packages, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers this app's document hooks (and web endpoints). Hooks on platform documents run
/// after the platform's own (registered first), so they see totals, postings and open items.
pub fn register(h: &mut HookRegistry) {
    let schedule = Arc::new(terms::ScheduleHook);
    h.register("MonthlyDistribution", Arc::new(budget::DistributionHook))
        .register("Budget", Arc::new(budget::BudgetHook))
        .register("PurchaseOrder", Arc::new(budget::PurchaseOrderHook))
        .register("PurchaseInvoice", Arc::new(budget::PurchaseInvoiceDims))
        .register("MaterialRequest", Arc::new(budget::MaterialRequestHook))
        .register("*", Arc::new(budget::ActualExpenseCheck))
        .register("BankAccount", Arc::new(bank::BankAccountHook))
        .register("BankTransaction", Arc::new(bank::BankTransactionHook))
        .register("PaymentEntry", Arc::new(bank::VoucherGuard))
        .register("JournalEntry", Arc::new(bank::VoucherGuard))
        .register("PeriodClosingVoucher", Arc::new(closing::ClosingHook))
        .register("PaymentTermsTemplate", Arc::new(terms::TemplateHook))
        .register("SalesInvoice", schedule.clone())
        .register("PurchaseInvoice", schedule.clone())
        .register("SalesOrder", schedule.clone())
        .register("PurchaseOrder", schedule)
        .register("SalesInvoice", Arc::new(advances::AdvanceHook))
        .register("PurchaseInvoice", Arc::new(advances::AdvanceHook))
        .register("PaymentEntry", Arc::new(advances::AdvancePaymentHook))
        .register("DunningType", Arc::new(dunning::DunningTypeHook))
        .register("Dunning", Arc::new(dunning::DunningHook))
        .register("SubscriptionPlan", Arc::new(subscription::PlanHook))
        .register("Subscription", Arc::new(subscription::SubscriptionHook));
}

/// This app's reports.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![Arc::new(reports::FinanceReports)]
}

/// Periodic upkeep: every minute, on one replica at a time (advisory lock); must be idempotent and cheap.
/// Subscriptions bill what is due (each period exactly once); once a day per workspace,
/// subscription statuses are refreshed and dunnings resolved and drafted. One workspace's failure
/// does not stop the others.
pub async fn maintenance(k: &Kernel) -> Result<()> {
    for ctx in common::tenants(k).await? {
        if let Err(e) = subscription::upkeep(k, &ctx).await {
            tracing::warn!(tenant = %ctx.tenant, "subscription upkeep failed: {e}");
        }
        if let Err(e) = dunning::upkeep(k, &ctx).await {
            tracing::warn!(tenant = %ctx.tenant, "dunning upkeep failed: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn packages_validate_with_the_platform() {
        let mut all = ferrum_domain::packages().expect("platform packages");
        let ours = super::packages().expect("finance packages parse");
        assert_eq!(ours.iter().map(|p| p.name()).collect::<Vec<_>>(), ["finance"]);
        all.extend(ours);
        ferrum_meta::Catalog::from_packages(all).expect("they validate together");
        assert!(super::MANIFEST.contains("id: finance"));
    }
}
