//! Outbox events delivered to the plugins that subscribed to them: at least once, at most once
//! per plugin and event (recorded in `plugin_event_run`), as the plugin's service user.

use crate::host::{Host, MAX_DEPTH, Scope};
use crate::{PluginHost, reply_error};
use async_trait::async_trait;
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{EventSink, Kernel, Result};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct PluginEvents(pub Arc<PluginHost>);

#[async_trait]
impl EventSink for PluginEvents {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        let depth = ev.payload.get("depth").and_then(Value::as_u64).unwrap_or(0);
        if depth >= MAX_DEPTH as u64 {
            return Ok(());
        }
        let plugins = self.0.plugins(&k.db, ev.tenant_id).await?;
        for p in plugins.iter().filter(|p| p.user_id.is_some() && p.manifest.subscribes(&ev.topic)) {
            let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
            let done: Option<i32> = sqlx::query_scalar("select 1 from plugin_event_run where tenant_id = $1 and plugin_id = $2 and outbox_id = $3")
                .bind(ev.tenant_id)
                .bind(p.id)
                .bind(ev.id)
                .fetch_optional(&mut *tx)
                .await?;
            tx.commit().await?;
            if done.is_some() {
                continue;
            }
            let mut ctx = self.0.service_ctx(k, ev.tenant_id, p).await?;
            ctx.depth = depth as u8 + 1;
            let input = json!({"kind": "event", "topic": ev.topic, "event": ev.payload});
            let reply = {
                let mut api = Host { host: &self.0, plugin: p, scope: Scope::Own { k, ctx }, limit_to_manifest: false };
                self.0.invoke(&k.db, ev.tenant_id, p, input, &mut api).await?
            };
            if let Some(e) = reply_error(&reply) {
                // Retried with the event; other subscribers that already ran are not repeated.
                return Err(e);
            }
            let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
            sqlx::query("insert into plugin_event_run (tenant_id, plugin_id, outbox_id) values ($1, $2, $3) on conflict do nothing")
                .bind(ev.tenant_id)
                .bind(p.id)
                .bind(ev.id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        Ok(())
    }
}
