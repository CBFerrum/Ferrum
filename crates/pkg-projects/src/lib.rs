//! Projects: projects, tasks with dependencies, timesheets with costing and billing, project profitability.

use ferrum_domain::reports::ReportSource;
use ferrum_kernel::{HookRegistry, Kernel, Result};
use std::sync::Arc;

/// Registers this package's document hooks.
pub fn register(_hooks: &mut HookRegistry) {}

/// This package's reports.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![]
}

/// Periodic upkeep (every few minutes on every replica; must be idempotent and safe to run concurrently).
pub async fn maintenance(_k: &Kernel) -> Result<()> {
    Ok(())
}
