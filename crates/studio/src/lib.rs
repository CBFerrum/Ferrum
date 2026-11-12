//! Studio: Analytics engine, report builder, charts, number cards, dashboards and spaces, all customizable without code.
//!
//! * [`spec`] — the query spec (self-describing JSON), relative dates, buckets, comparison periods;
//! * [`query`] — the compiler: spec + metadata + the caller's permissions → parameterized SQL;
//! * [`run`] — execution in a read-only transaction and result shaping (pivots, time series, titles);
//! * [`builder`] — saved reports, charts, number cards, dashboards and spaces;
//! * [`site`] — the `/api/app/studio/...` endpoints.

pub mod builder;
pub mod logic;
pub use ferrum_calc::formula;
pub mod query;
pub mod run;
pub mod site;
pub mod spec;

use async_trait::async_trait;
use ferrum_domain::reports::ReportSource;
use ferrum_kernel::{Ctx, Error, HookRegistry, Kernel, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// This app's manifest (`app.yaml`).
pub const MANIFEST: &str = include_str!("../app.yaml");

/// This app's metadata packages, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers this app's document hooks (and web endpoints).
pub fn register(hooks: &mut HookRegistry) {
    for entity in builder::ENTITIES {
        hooks.register(entity, Arc::new(builder::StudioHook(entity)));
    }
    hooks.add_site("studio", Arc::new(site::StudioSite));
    hooks.set_query_rows(Arc::new(StudioRows));
}

/// Saved reports' rows for documents about a query (docs/builder/decisions.md BD-21), through the report path: the
/// report seen and its rows read as the caller may, as `/api/reports` runs a saved report.
struct StudioRows;

#[async_trait]
impl ferrum_kernel::documents::QueryRows for StudioRows {
    async fn rows(&self, k: &Kernel, ctx: &Ctx, report: &str, params: &Value) -> Result<Value> {
        let sys = Ctx::system(ctx.tenant, ctx.schema.clone());
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let found = k.find_tx(&sys, &mut tx, builder::REPORT, &[ferrum_kernel::query::Filter::eq("label", report)], None, 2).await?;
        tx.commit().await?;
        let id = match found.as_slice() {
            [one] => ferrum_kernel::store::doc_id(one)?,
            [] => return Err(Error::field("query", format!("there is no report {report}"))),
            _ => return Err(Error::field("query", format!("more than one report is called {report}: name one by a label of its own"))),
        };
        Ok(builder::run_report(k, ctx, id, &builder::Runtime::parse(params)?).await?["data"].take())
    }
}

/// This app's reports: ad-hoc queries and saved reports through the platform's report API, so
/// `/api/reports` and AI agents (MCP `run_report`) reach the engine with the same permissions.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![Arc::new(StudioReports)]
}

struct StudioReports;

#[async_trait]
impl ReportSource for StudioReports {
    fn catalog(&self) -> Vec<Value> {
        vec![
            // The query engine applies every queried entity's row scope itself.
            json!({"name": "studio_query", "label": "Analytics Query", "module": "studio", "entity": builder::REPORT, "scoped": true,
                "params": [{"name": "spec", "label": "Query spec (JSON, see /api/app/studio/fields/{entity})", "type": "data", "required": true}]}),
            json!({"name": "studio_report", "label": "Saved Report", "module": "studio", "entity": builder::REPORT, "scoped": true,
                "params": [{"name": "report", "label": "Report", "type": "link", "to": builder::REPORT, "required": true},
                           {"name": "filters", "label": "Extra filters (JSON)", "type": "data"},
                           {"name": "from", "label": "From", "type": "data"}, {"name": "to", "label": "To", "type": "data"}]}),
        ]
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, params: &HashMap<String, String>) -> Result<Value> {
        let json_param = |key: &str| -> Result<Value> {
            match params.get(key).map(|v| v.trim()).filter(|v| !v.is_empty()) {
                None => Ok(Value::Null),
                Some(text) => serde_json::from_str(text).map_err(|e| Error::field(key, format!("must be JSON: {e}"))),
            }
        };
        match name {
            "studio_query" => run::run(k, ctx, &spec::Spec::parse(&json_param("spec")?)?).await,
            "studio_report" => {
                let id = params.get("report").and_then(|v| v.trim().parse().ok()).ok_or_else(|| Error::field("report", "is required"))?;
                let mut rt = json!({"filters": json_param("filters")?});
                for key in ["from", "to"] {
                    if let Some(v) = params.get(key).filter(|v| !v.trim().is_empty()) {
                        rt[key] = json!(v);
                    }
                }
                let out = builder::run_report(k, ctx, id, &builder::Runtime::parse(&rt)?).await?;
                let mut data = out["data"].clone();
                data["label"] = out["report"]["label"].clone();
                Ok(data)
            }
            other => Err(Error::NotFound(format!("report {other}"))),
        }
    }
}

/// Periodic upkeep: every minute, on one replica at a time (advisory lock); must be idempotent and cheap.
pub async fn maintenance(_k: &Kernel) -> Result<()> {
    // Nothing is cached or scheduled: every query runs live with the viewer's permissions.
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn packages_load_with_the_platform() {
        let mut pkgs = ferrum_domain::packages().expect("platform packages");
        let ours = super::packages().expect("studio packages parse");
        assert_eq!(ours.iter().map(|p| p.name()).collect::<Vec<_>>(), ["studio"]);
        pkgs.extend(ours);
        let catalog = ferrum_meta::Catalog::from_packages(pkgs).expect("they validate together");
        let schema = ferrum_meta::effective::build(&catalog, &["studio".into()], &[], 1).expect("schema");
        for e in super::builder::ENTITIES {
            let ent = schema.entity(e).unwrap_or_else(|| panic!("{e}"));
            assert!(ent.children.contains_key("roles"), "{e} shares with roles");
            for f in ["label", "sharing", "is_standard", "module"] {
                assert!(ent.field(f).is_some(), "{e}.{f}");
            }
        }
        assert!(super::MANIFEST.contains("id: studio"));
    }
}
