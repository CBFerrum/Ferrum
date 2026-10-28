//! The host API plugins call through `host_call`: records (through the kernel, within the
//! plugin's declared permissions), private key-value storage, outbound HTTP to declared hosts,
//! events, sockets, logging and the clock.

use crate::runtime::HostApi;
use crate::sockets::Outgoing;
use crate::{Loaded, PluginHost};
use async_trait::async_trait;
use ferrum_kernel::query::parse_filters;
use ferrum_kernel::{Action, Ctx, Doc, Error, HookCtx, Kernel, ListQuery, Result, Tx};
use serde_json::{Map, Value, json};
use std::net::SocketAddr;
use std::time::Duration;
use uuid::Uuid;

/// How deep plugin-triggered work may nest (a hook creating a document whose hook creates...).
pub const MAX_DEPTH: u8 = 4;
const MAX_QUERY_ROWS: u32 = 500;
const MAX_HTTP_RESPONSE: usize = 1 << 20;

/// Where a call runs.
pub enum Scope<'a, 'b> {
    /// Inside a document hook: the caller's transaction and identity.
    Hook(&'a mut HookCtx<'b>),
    /// Anywhere else: own short transactions as `ctx` (the plugin's service user, or the user
    /// calling a plugin route or socket).
    Own { k: &'a Kernel, ctx: Ctx },
}

pub struct Host<'a, 'b> {
    pub host: &'a PluginHost,
    pub plugin: &'a Loaded,
    pub scope: Scope<'a, 'b>,
    /// Acting as a user (not as the plugin's own service user): also hold the plugin to the
    /// permissions it declared, so a plugin never gains a user's full reach.
    pub limit_to_manifest: bool,
}

fn error_value(e: &Error) -> Value {
    let mut v = json!({"code": e.code(), "message": e.to_string()});
    if let Error::Validation(fields) = e {
        v["fields"] = json!(fields);
    }
    v
}

fn s<'v>(a: &'v Value, k: &str) -> Result<&'v str> {
    a.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).ok_or_else(|| Error::field(k, "is required"))
}

fn uuid(a: &Value, k: &str) -> Result<Uuid> {
    s(a, k)?.parse().map_err(|_| Error::field(k, "must be a record id"))
}

fn obj(a: &Value, k: &str) -> Result<Doc> {
    match a.get(k) {
        Some(Value::Object(m)) => Ok(m.clone()),
        _ => Err(Error::field(k, "must be an object")),
    }
}

#[async_trait]
impl HostApi for Host<'_, '_> {
    async fn call(&mut self, op: &str, args: Value) -> std::result::Result<Value, Value> {
        self.dispatch(op, args).await.map_err(|e| error_value(&e))
    }
}

impl Host<'_, '_> {
    fn ctx(&self) -> &Ctx {
        match &self.scope {
            Scope::Hook(hc) => hc.ctx,
            Scope::Own { ctx, .. } => ctx,
        }
    }

    fn kernel(&self) -> &Kernel {
        match &self.scope {
            Scope::Hook(hc) => hc.kernel,
            Scope::Own { k, .. } => k,
        }
    }

    fn check(&self, entity: &str, action: Action) -> Result<()> {
        if self.ctx().schema.entity(entity).is_none() {
            return Err(Error::NotFound(format!("entity {entity}")));
        }
        if self.limit_to_manifest && !self.plugin.manifest.allows(entity, action) {
            return Err(Error::Forbidden(format!("plugin '{}' did not declare {action:?} access to {entity}", self.plugin.name).to_lowercase()));
        }
        Ok(())
    }

    /// The identity for nested kernel calls, one level deeper.
    fn nested(&self) -> Result<Ctx> {
        let mut c = self.ctx().clone();
        c.depth = c.depth.saturating_add(1);
        if c.depth > MAX_DEPTH {
            return Err(Error::invalid("plugin calls nest too deeply"));
        }
        Ok(c)
    }

    async fn dispatch(&mut self, op: &str, a: Value) -> Result<Value> {
        match op {
            "data.get" => {
                let (e, id) = (s(&a, "entity")?, uuid(&a, "id")?);
                self.check(e, Action::Read)?;
                let c = self.nested()?;
                let doc = match &mut self.scope {
                    Scope::Hook(hc) => hc.kernel.get_tx(&c, hc.tx, e, id, false).await?,
                    Scope::Own { k, .. } => k.get(&c, e, id, true).await?,
                };
                Ok(Value::Object(doc))
            }
            "data.query" => {
                let e = s(&a, "entity")?;
                self.check(e, Action::Read)?;
                let c = self.nested()?;
                let filters = match a.get("filters") {
                    None | Some(Value::Null) => vec![],
                    Some(f) => parse_filters(f)?,
                };
                let limit = a.get("limit").and_then(Value::as_u64).map(|l| l.clamp(1, MAX_QUERY_ROWS as u64) as u32).unwrap_or(100);
                let sort = a.get("sort").and_then(Value::as_str).map(str::to_string);
                match &mut self.scope {
                    Scope::Hook(hc) => {
                        let rows = hc.kernel.find_tx(&c, hc.tx, e, &filters, sort.as_deref(), limit).await?;
                        Ok(json!({"rows": rows}))
                    }
                    Scope::Own { k, .. } => {
                        let q = ListQuery {
                            filters,
                            fields: a.get("fields").and_then(Value::as_array).map(|f| f.iter().filter_map(Value::as_str).map(str::to_string).collect()),
                            sort,
                            limit: Some(limit),
                            cursor: a.get("cursor").and_then(Value::as_str).map(str::to_string),
                            ..Default::default()
                        };
                        Ok(serde_json::to_value(k.list(&c, e, &q).await?).map_err(Error::internal)?)
                    }
                }
            }
            "data.insert" => {
                let (e, data) = (s(&a, "entity")?, obj(&a, "data")?);
                self.check(e, Action::Create)?;
                let c = self.nested()?;
                let doc = match &mut self.scope {
                    Scope::Hook(hc) => hc.kernel.insert_tx(&c, hc.tx, e, data).await?,
                    Scope::Own { k, .. } => k.insert(&c, e, data).await?,
                };
                Ok(Value::Object(doc))
            }
            "data.update" => {
                let (e, id, data) = (s(&a, "entity")?, uuid(&a, "id")?, obj(&a, "data")?);
                self.check(e, Action::Write)?;
                let c = self.nested()?;
                let version = a.get("expected_version").and_then(Value::as_i64);
                let doc = match &mut self.scope {
                    Scope::Hook(hc) => hc.kernel.update_tx(&c, hc.tx, e, id, data, version).await?,
                    Scope::Own { k, .. } => k.update(&c, e, id, data, version).await?,
                };
                Ok(Value::Object(doc))
            }
            "data.delete" => {
                let (e, id) = (s(&a, "entity")?, uuid(&a, "id")?);
                self.check(e, Action::Delete)?;
                let c = self.nested()?;
                match &mut self.scope {
                    Scope::Hook(hc) => hc.kernel.delete_tx(&c, hc.tx, e, id, None).await?,
                    Scope::Own { k, .. } => k.delete(&c, e, id, None).await?,
                }
                Ok(json!({"deleted": true}))
            }
            "data.submit" | "data.cancel" => {
                let (e, id) = (s(&a, "entity")?, uuid(&a, "id")?);
                let submit = op == "data.submit";
                self.check(e, if submit { Action::Submit } else { Action::Cancel })?;
                let c = self.nested()?;
                let doc = match (&mut self.scope, submit) {
                    (Scope::Hook(hc), true) => hc.kernel.submit_tx(&c, hc.tx, e, id, None).await?,
                    (Scope::Hook(hc), false) => hc.kernel.cancel_tx(&c, hc.tx, e, id, None).await?,
                    (Scope::Own { k, .. }, true) => k.submit(&c, e, id, None).await?,
                    (Scope::Own { k, .. }, false) => k.cancel(&c, e, id, None).await?,
                };
                Ok(Value::Object(doc))
            }
            "data.action" => {
                let (e, id, action) = (s(&a, "entity")?, uuid(&a, "id")?, s(&a, "action")?);
                self.check(e, Action::Write)?;
                let c = self.nested()?;
                let params = a.get("params").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
                match &mut self.scope {
                    Scope::Hook(hc) => hc.kernel.run_action_tx(&c, hc.tx, e, id, action, &params).await,
                    Scope::Own { k, .. } => k.run_action(&c, e, id, action, params).await,
                }
            }
            "kv.get" | "kv.set" | "kv.delete" | "kv.list" => self.kv(op, &a).await,
            "http.fetch" => self.host.fetch(self.plugin, &a).await,
            "events.emit" => {
                let topic = s(&a, "topic")?;
                if topic.len() > 100 || !topic.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
                    return Err(Error::field("topic", "use letters, digits, '.', '_' or '-' (at most 100)"));
                }
                let topic = format!("plugin.{}.{topic}", self.plugin.name);
                let mut payload = a.get("payload").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
                payload["depth"] = json!(self.ctx().depth.saturating_add(1));
                payload["plugin"] = json!(self.plugin.name);
                let tenant = self.ctx().tenant;
                match &mut self.scope {
                    Scope::Hook(hc) => ferrum_kernel::outbox::enqueue(hc.tx, tenant, &topic, None, None, payload).await?,
                    Scope::Own { k, .. } => {
                        let mut tx = k.db.tenant_tx(tenant).await?;
                        ferrum_kernel::outbox::enqueue(&mut tx, tenant, &topic, None, None, payload).await?;
                        tx.commit().await?;
                    }
                }
                Ok(json!({"topic": topic}))
            }
            "sockets.send" | "sockets.close" => {
                let connection = uuid(&a, "connection")?;
                let msg = if op == "sockets.send" {
                    Outgoing::Text(match a.get("message") {
                        Some(Value::String(t)) => t.clone(),
                        Some(other) => other.to_string(),
                        None => return Err(Error::field("message", "is required")),
                    })
                } else {
                    Outgoing::Close(a.get("reason").and_then(Value::as_str).unwrap_or_default().to_string())
                };
                self.host.sockets.send(&self.kernel().db, self.ctx().tenant, connection, msg).await?;
                Ok(json!({"sent": true}))
            }
            "log" => {
                let message = a.get("message").and_then(Value::as_str).unwrap_or_default();
                let message = &message[..message.len().min(2000)];
                match a.get("level").and_then(Value::as_str).unwrap_or("info") {
                    "error" => tracing::error!(plugin = %self.plugin.name, tenant = %self.ctx().tenant, "{message}"),
                    "warn" => tracing::warn!(plugin = %self.plugin.name, tenant = %self.ctx().tenant, "{message}"),
                    _ => tracing::info!(plugin = %self.plugin.name, tenant = %self.ctx().tenant, "{message}"),
                }
                Ok(Value::Null)
            }
            "time.now" => Ok(json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true))),
            other => Err(Error::invalid(format!("unknown host call {other}"))),
        }
    }

    async fn kv(&mut self, op: &str, a: &Value) -> Result<Value> {
        let (tenant, plugin) = (self.ctx().tenant, self.plugin.id);
        let mut own: Option<Tx> = None;
        let tx: &mut Tx = match &mut self.scope {
            Scope::Hook(hc) => hc.tx,
            Scope::Own { k, .. } => own.insert(k.db.tenant_tx(tenant).await?),
        };
        let out = match op {
            "kv.get" => {
                let v: Option<Value> = sqlx::query_scalar("select value from plugin_kv where tenant_id = $1 and plugin_id = $2 and key = $3")
                    .bind(tenant)
                    .bind(plugin)
                    .bind(s(a, "key")?)
                    .fetch_optional(&mut **tx)
                    .await?;
                v.unwrap_or(Value::Null)
            }
            "kv.set" => {
                let value = a.get("value").cloned().unwrap_or(Value::Null);
                sqlx::query(
                    "insert into plugin_kv (tenant_id, plugin_id, key, value) values ($1, $2, $3, $4) \
                     on conflict (tenant_id, plugin_id, key) do update set value = excluded.value, updated_at = now()",
                )
                .bind(tenant)
                .bind(plugin)
                .bind(s(a, "key")?)
                .bind(&value)
                .execute(&mut **tx)
                .await?;
                Value::Null
            }
            "kv.delete" => {
                sqlx::query("delete from plugin_kv where tenant_id = $1 and plugin_id = $2 and key = $3")
                    .bind(tenant)
                    .bind(plugin)
                    .bind(s(a, "key")?)
                    .execute(&mut **tx)
                    .await?;
                Value::Null
            }
            _ => {
                let prefix = a.get("prefix").and_then(Value::as_str).unwrap_or_default();
                let rows: Vec<(String, Value)> =
                    sqlx::query_as("select key, value from plugin_kv where tenant_id = $1 and plugin_id = $2 and starts_with(key, $3) order by key limit 500")
                        .bind(tenant)
                        .bind(plugin)
                        .bind(prefix)
                        .fetch_all(&mut **tx)
                        .await?;
                Value::Array(rows.into_iter().map(|(k, v)| json!({"key": k, "value": v})).collect())
            }
        };
        if let Some(tx) = own {
            tx.commit().await?;
        }
        Ok(out)
    }
}

impl PluginHost {
    /// Outbound HTTP for plugins: only hosts the plugin declared, never private addresses (unless
    /// allowed for development), DNS pinned between the check and the connection, no redirects.
    pub(crate) async fn fetch(&self, plugin: &Loaded, a: &Value) -> Result<Value> {
        let url = reqwest::Url::parse(s(a, "url")?).map_err(|e| Error::field("url", e.to_string()))?;
        let host = url.host_str().ok_or_else(|| Error::field("url", "has no host"))?.to_ascii_lowercase();
        if url.scheme() != "https" && !(url.scheme() == "http" && self.allow_private_http) {
            return Err(Error::field("url", "plugins may only call https URLs"));
        }
        if !plugin.manifest.http_hosts.iter().any(|h| host == *h || h.strip_prefix("*.").is_some_and(|d| host.ends_with(&format!(".{d}")))) {
            return Err(Error::Forbidden(format!("plugin '{}' did not declare the host {host}", plugin.name)));
        }
        let port = url.port_or_known_default().unwrap_or(443);
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| Error::Unavailable(format!("dns: {e}")))?.collect();
        if addrs.is_empty() {
            return Err(Error::Unavailable(format!("{host} did not resolve")));
        }
        if !self.allow_private_http && addrs.iter().any(|x| ferrum_kernel::outbox::is_private(x.ip())) {
            return Err(Error::Forbidden(format!("{host} resolves to a private address")));
        }
        let client = reqwest::Client::builder()
            .resolve_to_addrs(&host, &addrs)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .user_agent(format!("Ferrum-Plugin/{}", plugin.name))
            .build()
            .map_err(Error::internal)?;
        let method = a.get("method").and_then(Value::as_str).unwrap_or("GET").to_ascii_uppercase();
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| Error::field("method", "is not an HTTP method"))?;
        let mut req = client.request(method, url);
        if let Some(Value::Object(h)) = a.get("headers") {
            for (k, v) in h {
                if let Some(v) = v.as_str() {
                    req = req.header(k.as_str(), v);
                }
            }
        }
        if let Some(body) = a.get("body") {
            req = match body {
                Value::String(t) => req.body(t.clone()),
                Value::Null => req,
                other => req.header("content-type", "application/json").body(other.to_string()),
            };
        }
        let mut resp = req.send().await.map_err(|e| Error::Unavailable(format!("request failed: {e}")))?;
        let status = resp.status().as_u16();
        let headers: Map<String, Value> = resp.headers().iter().filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), json!(v)))).collect();
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| Error::Unavailable(format!("reading the response: {e}")))? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_HTTP_RESPONSE {
                return Err(Error::invalid("response larger than 1 MB"));
            }
        }
        Ok(json!({"status": status, "headers": headers, "body": String::from_utf8_lossy(&body)}))
    }
}
