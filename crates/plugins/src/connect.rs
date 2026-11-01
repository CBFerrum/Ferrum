//! A plugin that is a connector: the kernel's contract (`ferrum_kernel::connect::Connector`) carried
//! over the plugin boundary, so a provider's adapter is a signed, sandboxed module instead of code in
//! the server. Each operation is one call with `{"kind": "connector", "op", "args", "connection"}`.
//! The connection's credentials are held by the host, as a plugin's own are: the guest names them
//! (`fetch_with`, `verify_hmac_sha256`) and never reads them, so one connector serves many accounts.

use crate::host::{Host, Scope};
use crate::{Loaded, PluginHost, reply_error};
use async_trait::async_trait;
use ferrum_kernel::connect::{Connection, Connector, ConnectorInfo, ConnectorSource, Hook, Incoming, Outgoing, Pulled, StreamSchema};
use ferrum_kernel::{Error, Kernel, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

pub struct PluginConnectors(pub Arc<PluginHost>);

#[async_trait]
impl ConnectorSource for PluginConnectors {
    async fn find(&self, k: &Kernel, tenant: Uuid, provider: &str) -> Result<Option<Arc<dyn Connector>>> {
        let plugins = self.0.plugins(&k.db, tenant).await?;
        let plugin = plugins.iter().find(|p| p.name == provider && p.manifest.connector.is_some());
        Ok(plugin.map(|p| Arc::new(PluginConnector { host: self.0.clone(), plugin: p.clone() }) as Arc<dyn Connector>))
    }

    async fn list(&self, k: &Kernel, tenant: Uuid) -> Result<Vec<ConnectorInfo>> {
        Ok(self.0.plugins(&k.db, tenant).await?.iter().filter_map(info_of).collect())
    }
}

fn info_of(p: &Loaded) -> Option<ConnectorInfo> {
    p.manifest.connector.clone().map(|c| ConnectorInfo { provider: p.name.clone(), ..c })
}

struct PluginConnector {
    host: Arc<PluginHost>,
    plugin: Loaded,
}

/// One record as a guest hands it over.
fn incoming(v: &Value) -> Incoming {
    Incoming {
        external_id: v.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
        version: v.get("version").and_then(Value::as_str).map(str::to_string),
        deleted: v.get("deleted") == Some(&json!(true)),
        fields: v.get("fields").and_then(Value::as_object).cloned().unwrap_or_default(),
    }
}

/// What a guest said other than an answer: to wait, that the provider is down, or that it refuses.
fn refusal(reply: &Value) -> Option<Error> {
    if let Some(seconds) = reply.get("wait").and_then(Value::as_u64) {
        return Some(Error::Wait(reply.get("why").and_then(Value::as_str).unwrap_or("the provider asked to wait").to_string(), seconds));
    }
    if let Some(why) = reply.get("unavailable").and_then(Value::as_str) {
        return Some(Error::Unavailable(why.to_string()));
    }
    reply_error(reply)
}

impl PluginConnector {
    async fn ask(&self, k: &Kernel, c: &Connection, op: &str, args: Value) -> Result<Value> {
        // For this account: the credentials the guest may name are the connection's.
        let mut plugin = self.plugin.clone();
        plugin.credentials = Arc::new(c.credentials.as_object().cloned().unwrap_or_default());
        let ctx = self.host.service_ctx(k, c.tenant, &plugin).await?;
        let input = json!({"kind": "connector", "op": op, "args": args, "connection": {"id": c.id, "config": c.config}});
        let mut api = Host { host: &self.host, plugin: &plugin, scope: Scope::Own { k, ctx }, limit_to_manifest: false };
        let reply = self.host.invoke(&k.db, c.tenant, &plugin, input, &mut api).await?;
        match refusal(&reply) {
            Some(e) => Err(e),
            None => Ok(reply),
        }
    }
}

#[async_trait]
impl Connector for PluginConnector {
    fn info(&self) -> ConnectorInfo {
        info_of(&self.plugin).unwrap_or_default()
    }

    async fn check(&self, k: &Kernel, c: &Connection) -> Result<String> {
        let reply = self.ask(k, c, "check", json!({})).await?;
        Ok(reply.get("account").and_then(Value::as_str).unwrap_or("connected").to_string())
    }

    async fn discover(&self, k: &Kernel, c: &Connection) -> Result<Vec<StreamSchema>> {
        let reply = self.ask(k, c, "discover", json!({})).await?;
        serde_json::from_value(reply.get("streams").cloned().unwrap_or_else(|| json!([])))
            .map_err(|e| Error::invalid(format!("plugin '{}' described its streams wrongly: {e}", self.plugin.name)))
    }

    async fn pull(&self, k: &Kernel, c: &Connection, stream: &str, cursor: Option<&str>) -> Result<Pulled> {
        let reply = self.ask(k, c, "pull", json!({"stream": stream, "cursor": cursor})).await?;
        let records = reply.get("records").and_then(Value::as_array).map(|r| r.iter().map(incoming).collect()).unwrap_or_default();
        Ok(Pulled { records, next: reply.get("next").and_then(Value::as_str).map(str::to_string) })
    }

    async fn push(&self, k: &Kernel, c: &Connection, change: &Outgoing) -> Result<String> {
        let args = json!({"key": change.key, "entity": change.entity, "record": change.record, "external_id": change.external_id, "fields": change.fields});
        let reply = self.ask(k, c, "push", args).await?;
        reply
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::invalid(format!("plugin '{}' did not say what the provider called it", self.plugin.name)))
    }

    async fn webhook(&self, k: &Kernel, c: &Connection, headers: &BTreeMap<String, String>, body: &[u8]) -> Result<Vec<Hook>> {
        let reply = self.ask(k, c, "webhook", json!({"headers": headers, "body": String::from_utf8_lossy(body)})).await?;
        Ok(reply
            .get("hooks")
            .and_then(Value::as_array)
            .map(|hooks| {
                hooks
                    .iter()
                    .map(|h| Hook {
                        event_id: h.get("event_id").and_then(Value::as_str).unwrap_or_default().to_string(),
                        stream: h.get("stream").and_then(Value::as_str).unwrap_or_default().to_string(),
                        record: incoming(h.get("record").unwrap_or(&Value::Null)),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guest_can_say_wait_down_or_no() {
        assert!(matches!(refusal(&json!({"wait": 30, "why": "rate limit"})), Some(Error::Wait(why, 30)) if why == "rate limit"));
        assert!(matches!(refusal(&json!({"unavailable": "502 from the provider"})), Some(Error::Unavailable(_))));
        assert!(matches!(refusal(&json!({"error": "that account is closed"})), Some(Error::Invalid(_))));
        assert!(refusal(&json!({"records": []})).is_none());
    }

    #[test]
    fn a_record_comes_over_as_the_kernel_knows_it() {
        let r = incoming(&json!({"id": "p1", "version": "7", "deleted": true, "fields": {"name": "Asha"}}));
        assert_eq!((r.external_id.as_str(), r.version.as_deref(), r.deleted), ("p1", Some("7"), true));
        assert_eq!(r.fields["name"], json!("Asha"));
        assert_eq!(incoming(&Value::Null).external_id, "");
    }
}
