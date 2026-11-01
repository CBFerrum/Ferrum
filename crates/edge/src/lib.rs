//! IoT Edge (docs/plan/edge.md): equipment reached by definitions, through gateways that keep
//! working on their own.
//!
//! * [`config`]: what each gateway should be, worked out from the records;
//! * [`facts`]: what gateways report, arriving when the link allows;
//! * [`site`]: the HTTP endpoints.
//!
//! Commands, identity, telemetry and authority are Operations'. This adds how devices are described
//! and looked after. Rules raise alarms and move nothing: anything physical goes through the ledger.

pub mod config;
pub mod facts;
pub mod site;

use async_trait::async_trait;
use ferrum_domain::reports::ReportSource;
use ferrum_domain::util::{id, s, sys};
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, EventSink, HookCtx, HookRegistry, Kernel, Result};
use serde_json::{Value, json};
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));
include!(concat!(env!("OUT_DIR"), "/ui.rs"));

/// This app's manifest (`app.yaml`).
pub const MANIFEST: &str = include_str!("../app.yaml");

/// This app's metadata packages, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers this app's document hooks and endpoints.
pub fn register(h: &mut HookRegistry) {
    h.register("DeviceDefinition", Arc::new(DefinitionHook));
    h.register("EdgeRule", Arc::new(RuleHook));
    h.add_event_sink(Arc::new(Facts));
    h.add_site("edge", Arc::new(site::EdgeSite));
}

/// This app's reports.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![]
}

/// A definition is checked when it is saved, with its reason, so a mistake is a person's to fix at a
/// desk and not a gateway's to find. Publishing is what reaches gateways, so a draft breaks nothing.
struct DefinitionHook;

#[async_trait]
impl DocHook for DefinitionHook {
    fn name(&self) -> &'static str {
        "edge.definition"
    }

    async fn validate_change(&self, _hc: &mut HookCtx<'_>, _old: Option<&Doc>, doc: &Doc) -> Result<()> {
        ferrum_ops::contract::driver::parse(s(doc, "body").unwrap_or_default()).map(drop).map_err(|why| Error::field("body", why))
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "publish" {
            return Ok(None);
        }
        let record = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let body = s(doc, "body").unwrap_or_default();
        let revision = doc.get("revision").and_then(Value::as_i64).unwrap_or(0) + 1;
        ferrum_ops::contract::driver::parse(body).map_err(|why| Error::field("body", why))?;
        let mut values = Doc::new();
        values.insert("published_body".into(), json!(body));
        values.insert("revision".into(), json!(revision));
        values.insert("status".into(), json!("Published"));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "DeviceDefinition", record, values).await?;
        ferrum_kernel::audit::record(hc.tx, hc.ctx, "DeviceDefinition", record, "edge.published", None, json!({"revision": revision})).await?;
        Ok(Some(json!({"revision": revision})))
    }
}

/// A rule's limits, which only make sense together.
struct RuleHook;

#[async_trait]
impl DocHook for RuleHook {
    fn name(&self) -> &'static str {
        "edge.rule"
    }

    async fn validate_change(&self, _hc: &mut HookCtx<'_>, _old: Option<&Doc>, doc: &Doc) -> Result<()> {
        let number = |k: &str| doc.get(k).and_then(|v| v.as_str().and_then(|t| t.parse::<f64>().ok()).or(v.as_f64()));
        let comparison = s(doc, "comparison").unwrap_or("Above");
        if comparison == "Silent" {
            if doc.get("hold_secs").and_then(Value::as_i64).unwrap_or(0) <= 0 {
                return Err(Error::field("hold_secs", "say how long silence must last"));
            }
            return Ok(());
        }
        let Some(limit) = number("limit_value") else { return Err(Error::field("limit_value", "say what the limit is")) };
        if let Some(clear) = number("clear_value") {
            let wrong = if comparison == "Above" { clear > limit } else { clear < limit };
            if wrong {
                return Err(Error::field("clear_value", "it clears on the safe side of the limit"));
            }
        }
        Ok(())
    }
}

/// What gateways report, in any order and at least once, which [`facts`] is written for.
struct Facts;

#[async_trait]
impl EventSink for Facts {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        let Some(kind) = ev.topic.strip_prefix("ops.fact.edge.") else { return Ok(()) };
        let ctx = Ctx::system(ev.tenant_id, k.registry.schema(ev.tenant_id).await?);
        let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
        facts::apply(k, &ctx, &mut tx, kind, &ev.payload).await?;
        tx.commit().await?;
        Ok(())
    }
}

/// Periodic upkeep: events older than this go, and with them whatever only they recorded.
pub async fn maintenance(k: &Kernel) -> Result<()> {
    let mut tx = k.db.system_tx().await?;
    sqlx::query("delete from edge_event where at < now() - interval '90 days'").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
