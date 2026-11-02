//! The host API plugins call through `host_call`: records (through the kernel, within the
//! plugin's declared permissions), private key-value storage, outbound HTTP to declared hosts,
//! events, sockets, logging and the clock.

use crate::runtime::{CALL_DEADLINE, HOOK_DEADLINE, HostApi};
use crate::sockets::{Outgoing, Overflow, Sent};
use crate::{Credential, Loaded, PluginHost};
use async_trait::async_trait;
use ferrum_kernel::query::parse_filters;
use ferrum_kernel::{Action, Ctx, Doc, Error, HookCtx, Kernel, ListQuery, Result, Tx};
use hmac::{Hmac, Mac};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How deep plugin-triggered work may nest (a hook creating a document whose hook creates...).
pub const MAX_DEPTH: u8 = 4;
const MAX_QUERY_ROWS: u32 = 500;
const MAX_HTTP_RESPONSE: usize = 1 << 20;
const MAX_LOG: usize = 2000;

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

/// What a plugin's log line keeps: its start, cut between characters. A plugin chooses this text, so
/// a cut through a multi-byte character would let it panic the host.
fn clip_log(message: &str) -> &str {
    &message[..message.floor_char_boundary(MAX_LOG)]
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

    fn deadline(&self) -> Duration {
        if matches!(self.scope, Scope::Hook(_)) { HOOK_DEADLINE } else { CALL_DEADLINE }
    }

    fn nested(&self) -> bool {
        matches!(self.scope, Scope::Hook(_)) && self.ctx().depth > 0
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
            // What leaves the system cannot be taken back if the document's transaction rolls
            // back, and a hook must not hold that transaction open waiting for a provider.
            "http.fetch" | "sockets.send" | "sockets.close" | "resource.query" | "resource.execute" if matches!(self.scope, Scope::Hook(_)) => {
                Err(Error::invalid(format!(
                    "{op} cannot run in a hook: the document may still roll back. Emit an event and do it from the event, after the commit"
                )))
            }
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
            "crypto.verify_hmac_sha256" => {
                let secret = self.plugin.credential(s(&a, "credential")?)?;
                Ok(json!({"valid": signed_with(secret, s(&a, "message")?, s(&a, "signature")?)}))
            }
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
                let overflow = match a.get("overflow").and_then(Value::as_str) {
                    None | Some("fail") => Overflow::Fail,
                    Some("drop") => Overflow::Drop,
                    Some(_) => return Err(Error::field("overflow", "use fail or drop")),
                };
                let sent = self.host.sockets.send(&self.kernel().db, self.ctx().tenant, connection, msg, overflow).await?;
                Ok(json!({"sent": sent != Sent::Dropped, "queued": sent == Sent::Queued}))
            }
            "log" => {
                let message = a.get("message").and_then(Value::as_str).unwrap_or_default();
                let message = clip_log(message);
                match a.get("level").and_then(Value::as_str).unwrap_or("info") {
                    "error" => tracing::error!(plugin = %self.plugin.name, tenant = %self.ctx().tenant, "{message}"),
                    "warn" => tracing::warn!(plugin = %self.plugin.name, tenant = %self.ctx().tenant, "{message}"),
                    _ => tracing::info!(plugin = %self.plugin.name, tenant = %self.ctx().tenant, "{message}"),
                }
                Ok(Value::Null)
            }
            "time.now" => Ok(json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true))),
            "resource.query" | "resource.execute" => {
                // Who is asking is the host's to say. A plugin names a handle, and nothing about itself.
                if let Some(field) = a.as_object().into_iter().flatten().map(|(k, _)| k).find(|k| !matches!(k.as_str(), "handle" | "sql" | "params")) {
                    tracing::warn!(plugin = %self.plugin.name, tenant = %self.ctx().tenant, field, "a plugin tried to say more than a resource call takes");
                    return Err(Error::field(field, "is not something a plugin can say"));
                }
                let (handle, sql) = (s(&a, "handle")?, s(&a, "sql")?);
                let params = a.get("params").and_then(Value::as_array).cloned().unwrap_or_default();
                let k = self.kernel();
                let (tenant, app) = (self.ctx().tenant, &self.plugin.name);
                let resources = k.hooks.resources().ok_or_else(|| Error::Unavailable("nothing provides resources on this server".into()))?;
                if op == "resource.query" {
                    resources.query(k, tenant, app, handle, sql, &params).await
                } else {
                    Ok(json!({"changed": resources.execute(k, tenant, app, handle, sql, &params).await?}))
                }
            }
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
                // With `if_value`, only while it still holds that value: an old connection closing
                // late must not remove the mapping of the one that replaced it.
                let deleted = sqlx::query("delete from plugin_kv where tenant_id = $1 and plugin_id = $2 and key = $3 and ($4::jsonb is null or value = $4)")
                    .bind(tenant)
                    .bind(plugin)
                    .bind(s(a, "key")?)
                    .bind(a.get("if_value"))
                    .execute(&mut **tx)
                    .await?;
                json!({"deleted": deleted.rows_affected() > 0})
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

/// Whether `signature` (hex) is `message` signed with `secret`, compared in constant time.
fn signed_with(secret: &str, message: &str, signature: &str) -> bool {
    let (Ok(mut mac), Ok(given)) = (Hmac::<Sha256>::new_from_slice(secret.as_bytes()), hex::decode(signature)) else { return false };
    mac.update(message.as_bytes());
    mac.verify_slice(&given).is_ok()
}

fn host_allowed(allowed: &[String], host: &str) -> bool {
    allowed.iter().any(|h| host == h || h.strip_prefix("*.").is_some_and(|d| host.ends_with(&format!(".{d}"))))
}

impl Loaded {
    /// A credential's value, for the host's own use. It is never handed to the plugin.
    fn credential(&self, name: &str) -> Result<&str> {
        if !self.manifest.credentials.iter().any(|c| c.name == name) {
            return Err(Error::Forbidden(format!("plugin '{}' did not declare the credential {name}", self.name)));
        }
        self.credentials
            .get(name)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::invalid(format!("set {name} in the {} plugin's settings", self.name)))
    }

    /// Credential `name`, with its value, if it may go to `host`.
    fn declared_for(&self, name: &str, host: &str) -> Result<(&Credential, &str)> {
        let value = self.credential(name)?;
        let c = self.manifest.credentials.iter().find(|c| c.name == name).ok_or_else(|| Error::internal("credential checked above"))?;
        if !host_allowed(&c.hosts, host) {
            return Err(Error::Forbidden(format!("the credential {name} may not be sent to {host}")));
        }
        Ok((c, value))
    }

    /// The user name paired with a credential.
    fn user_of(&self, c: &Credential) -> &str {
        c.user.as_ref().and_then(|u| self.credentials.get(u).or_else(|| self.config.get(u))).and_then(Value::as_str).unwrap_or_default()
    }
}

/// Reads a provider's answer, up to what a plugin may be handed.
async fn body_of(mut resp: reqwest::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| Error::Unavailable(format!("reading the response: {e}")))? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_HTTP_RESPONSE {
            return Err(Error::invalid("response larger than 1 MB"));
        }
    }
    Ok(body)
}

impl PluginHost {
    /// A token for an `oauth2` credential: the one held, until a minute before it expires, else a
    /// new one. Held under a digest of the secret, so a changed secret is a new sign-in. `key` says
    /// which token this was, so that a provider refusing it can have it dropped.
    async fn access_token(&self, plugin: &Loaded, client: &reqwest::Client, c: &Credential, secret: &str, url: &reqwest::Url) -> Result<(String, String)> {
        let host = url.host_str().unwrap_or_default();
        let user = plugin.user_of(c);
        let digest = hex::encode(Sha256::digest(format!("{user}:{secret}")));
        let key = format!("{}/{}/{host}/{digest}", plugin.id, c.name);
        if let Some(held) = self.tokens.get(&key).filter(|t| t.1 > Instant::now()) {
            return Ok((held.0.clone(), key));
        }
        let mut at = url.clone();
        at.set_path(c.token_path.as_deref().unwrap_or_default());
        at.set_query(None);
        let resp = client
            .post(at)
            .basic_auth(user, Some(secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body("grant_type=client_credentials")
            .send()
            .await
            .map_err(|e| Error::Unavailable(format!("signing in to {host}: {e}")))?;
        let status = resp.status();
        let answer: Value = serde_json::from_slice(&body_of(resp).await?).unwrap_or(Value::Null);
        let Some(token) = answer["access_token"].as_str().filter(|t| status.is_success() && !t.is_empty()) else {
            let why = answer["error_description"].as_str().or(answer["error"].as_str()).unwrap_or("no reason given");
            return Err(Error::invalid(format!("{host} refused the credential {}: {why}", c.name)));
        };
        let life = answer["expires_in"].as_u64().unwrap_or(300).saturating_sub(60);
        self.tokens.retain(|_, t| t.1 > Instant::now());
        self.tokens.insert(key.clone(), (token.to_string(), Instant::now() + Duration::from_secs(life)));
        Ok((token.to_string(), key))
    }

    /// Outbound HTTP for plugins: only hosts the plugin declared, never private addresses (unless
    /// allowed for development), DNS pinned between the check and the connection, no redirects.
    /// With `auth`, the host signs the request in with a credential the plugin never sees.
    pub(crate) async fn fetch(&self, plugin: &Loaded, a: &Value) -> Result<Value> {
        let url = reqwest::Url::parse(s(a, "url")?).map_err(|e| Error::field("url", e.to_string()))?;
        let host = url.host_str().ok_or_else(|| Error::field("url", "has no host"))?.to_ascii_lowercase();
        if url.scheme() != "https" && !(url.scheme() == "http" && self.allow_private_http) {
            return Err(Error::field("url", "plugins may only call https URLs"));
        }
        if !host_allowed(&plugin.manifest.http_hosts, &host) {
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
        let mut req = client.request(method, url.clone());
        let auth = a.get("auth").and_then(Value::as_str);
        if let Some(Value::Object(h)) = a.get("headers") {
            for (k, v) in h {
                if auth.is_some() && k.eq_ignore_ascii_case("authorization") {
                    return Err(Error::field("headers", "auth sets the Authorization header"));
                }
                if let Some(v) = v.as_str() {
                    req = req.header(k.as_str(), v);
                }
            }
        }
        let mut token = None;
        if let Some(name) = auth {
            let (c, secret) = plugin.declared_for(name, &host)?;
            req = match c.scheme.as_str() {
                "oauth2" => {
                    let (value, key) = self.access_token(plugin, &client, c, secret, &url).await?;
                    token = Some(key);
                    req.bearer_auth(value)
                }
                "basic" => req.basic_auth(plugin.user_of(c), Some(secret)),
                _ => req.bearer_auth(secret),
            };
        }
        if let Some(body) = a.get("body") {
            req = match body {
                Value::String(t) => req.body(t.clone()),
                Value::Null => req,
                other => req.header("content-type", "application/json").body(other.to_string()),
            };
        }
        let resp = req.send().await.map_err(|e| Error::Unavailable(format!("request failed: {e}")))?;
        let status = resp.status().as_u16();
        // A token the provider no longer takes (revoked, or the clock disagrees) is not tried again.
        if status == 401
            && let Some(key) = token
        {
            self.tokens.remove(&key);
        }
        let headers: Map<String, Value> = resp.headers().iter().filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), json!(v)))).collect();
        let body = body_of(resp).await?;
        Ok(json!({"status": status, "headers": headers, "body": String::from_utf8_lossy(&body)}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Manifest;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn a_long_log_line_is_cut_between_characters() {
        let start = "x".repeat(MAX_LOG - 1);
        assert_eq!(clip_log(&format!("{start}é and the rest")), start, "é would span the limit");
        assert_eq!(clip_log(&format!("{start}e and the rest")).len(), MAX_LOG);
        assert_eq!(clip_log("short"), "short");
        assert_eq!(clip_log(""), "");
    }

    /// A provider that keeps what it is sent, and answers with its own idea of the secret.
    async fn provider() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let kept = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                kept.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).to_lowercase());
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}").await;
            }
        });
        (url, seen)
    }

    fn plugin(credentials: Value, config: Value) -> Loaded {
        let manifest: Manifest = serde_json::from_value(json!({"http_hosts": ["127.0.0.1", "other.example.com"], "credentials": credentials})).unwrap();
        let (config, held) = crate::split_credentials(&manifest, config);
        Loaded {
            id: Uuid::nil(),
            name: "pay".into(),
            sha: String::new(),
            manifest: Arc::new(manifest),
            config: Arc::new(config),
            credentials: Arc::new(held),
            user_id: None,
            fuel: 1,
            memory_mb: 1,
        }
    }

    #[test]
    fn a_plugin_does_not_read_what_it_declared_as_a_credential() {
        let p = plugin(json!([{"name": "secret_key", "hosts": ["127.0.0.1"]}]), json!({"secret_key": "sk_live_1", "success_url": "https://x"}));
        assert_eq!(*p.config, json!({"success_url": "https://x"}), "what stays is what it may read");
        assert_eq!(p.credential("secret_key").unwrap(), "sk_live_1");
    }

    #[tokio::test]
    async fn a_credential_goes_only_where_it_was_declared_for() {
        let host = PluginHost::new(true).unwrap();
        let (url, seen) = provider().await;
        let p = plugin(
            json!([{"name": "secret_key", "hosts": ["127.0.0.1"]}, {"name": "elsewhere", "hosts": ["other.example.com"]}, {"name": "unset", "hosts": ["127.0.0.1"]}]),
            json!({"secret_key": "sk_live_1", "elsewhere": "sk_2"}),
        );

        host.fetch(&p, &json!({"url": url, "auth": "secret_key"})).await.unwrap();
        assert!(seen.lock().unwrap()[0].contains("authorization: bearer sk_live_1"), "the host signed it in: {:?}", seen.lock().unwrap());

        let refused = |e: Error| matches!(e, Error::Forbidden(_));
        assert!(refused(host.fetch(&p, &json!({"url": url, "auth": "elsewhere"})).await.unwrap_err()), "not to another host");
        assert!(refused(host.fetch(&p, &json!({"url": url, "auth": "undeclared"})).await.unwrap_err()), "not one it did not declare");
        assert!(host.fetch(&p, &json!({"url": url, "auth": "unset"})).await.unwrap_err().to_string().contains("set unset in the pay plugin's settings"));
        let forged = host.fetch(&p, &json!({"url": url, "auth": "secret_key", "headers": {"Authorization": "Bearer mine"}})).await;
        assert!(matches!(forged, Err(Error::Validation(_))), "and a plugin does not choose the header: {forged:?}");
        assert_eq!(seen.lock().unwrap().len(), 1, "none of these left");
    }

    #[tokio::test]
    async fn basic_credentials_pair_a_user_with_the_secret() {
        let host = PluginHost::new(true).unwrap();
        let (url, seen) = provider().await;
        let p = plugin(
            json!([{"name": "key_secret", "hosts": ["127.0.0.1"], "scheme": "basic", "user": "key_id"}]),
            json!({"key_secret": "s3", "key_id": "rzp_1"}),
        );
        host.fetch(&p, &json!({"url": url, "auth": "key_secret"})).await.unwrap();
        assert!(seen.lock().unwrap()[0].contains("authorization: basic cnpwxze6czm="), "{:?}", seen.lock().unwrap());
    }

    /// A provider answering each request with the next of `answers`, then 200.
    async fn scripted(answers: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v2/payments?page=1", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let kept = seen.clone();
        tokio::spawn(async move {
            let mut answers = answers.into_iter();
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                kept.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).to_string());
                let (status, body) = answers.next().unwrap_or((200, "{}"));
                let reply = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (url, seen)
    }

    fn oauth2_plugin(hosts: Value, secret: &str) -> Loaded {
        plugin(
            json!([{"name": "client_secret", "hosts": hosts, "scheme": "oauth2", "user": "client_id", "token_path": "/oauth/token"}]),
            json!({"client_secret": secret, "client_id": "client-1"}),
        )
    }

    #[tokio::test]
    async fn the_host_keeps_an_oauth2_token_and_the_plugin_never_sees_it() {
        let host = PluginHost::new(true).unwrap();
        let token = r#"{"access_token": "tok_1", "expires_in": 3600}"#;
        let (url, seen) = scripted(vec![(200, token), (200, "{}"), (200, "{}"), (401, "{}"), (200, r#"{"access_token": "tok_2", "expires_in": 3600}"#)]).await;
        let p = oauth2_plugin(json!(["127.0.0.1"]), "s3");

        let first = host.fetch(&p, &json!({"url": url, "auth": "client_secret"})).await.unwrap();
        assert!(!first.to_string().contains("tok_1"), "the answer holds no token: {first}");
        host.fetch(&p, &json!({"url": url, "auth": "client_secret"})).await.unwrap();
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 3, "one sign-in for two calls: {seen:?}");
            assert!(seen[0].starts_with("POST /oauth/token HTTP"), "at the path declared, without the call's query: {}", seen[0]);
            assert!(seen[0].to_lowercase().contains("authorization: basic y2xpzw50lte6czm="), "as the client: {}", seen[0]);
            assert!(seen[0].ends_with("grant_type=client_credentials"), "{}", seen[0]);
            assert!(seen[1].starts_with("GET /v2/payments?page=1"), "{}", seen[1]);
            assert!(seen[1].to_lowercase().contains("authorization: bearer tok_1"), "{}", seen[1]);
        }

        // Refused: that token is gone, and the next call signs in again.
        assert_eq!(host.fetch(&p, &json!({"url": url, "auth": "client_secret"})).await.unwrap()["status"], 401);
        host.fetch(&p, &json!({"url": url, "auth": "client_secret"})).await.unwrap();
        let seen = seen.lock().unwrap();
        assert!(seen[4].starts_with("POST /oauth/token"), "{}", seen[4]);
        assert!(seen[5].to_lowercase().contains("authorization: bearer tok_2"), "{}", seen[5]);
    }

    #[tokio::test]
    async fn a_refused_sign_in_stops_the_call_and_says_why() {
        let host = PluginHost::new(true).unwrap();
        let (url, seen) = scripted(vec![(401, r#"{"error_description": "Client Authentication failed"}"#), (200, "<html>not json</html>")]).await;
        let p = oauth2_plugin(json!(["127.0.0.1"]), "wrong");
        let refused = host.fetch(&p, &json!({"url": url, "auth": "client_secret"})).await.unwrap_err();
        assert!(refused.to_string().contains("Client Authentication failed"), "{refused}");
        assert!(host.fetch(&p, &json!({"url": url, "auth": "client_secret"})).await.is_err(), "and nothing is held from a bad answer");
        assert_eq!(seen.lock().unwrap().len(), 2, "the call itself never left");
    }

    #[tokio::test]
    async fn a_changed_secret_is_a_new_sign_in() {
        let host = PluginHost::new(true).unwrap();
        let (url, seen) =
            scripted(vec![(200, r#"{"access_token": "old", "expires_in": 3600}"#), (200, "{}"), (200, r#"{"access_token": "new", "expires_in": 3600}"#)]).await;
        host.fetch(&oauth2_plugin(json!(["127.0.0.1"]), "s1"), &json!({"url": url, "auth": "client_secret"})).await.unwrap();
        host.fetch(&oauth2_plugin(json!(["127.0.0.1"]), "s2"), &json!({"url": url, "auth": "client_secret"})).await.unwrap();
        assert!(seen.lock().unwrap()[3].to_lowercase().contains("authorization: bearer new"), "{:?}", seen.lock().unwrap());
    }

    #[tokio::test]
    async fn an_oauth2_secret_goes_nowhere_it_was_not_declared_for() {
        let host = PluginHost::new(true).unwrap();
        let (url, seen) = scripted(vec![]).await;
        let p = oauth2_plugin(json!(["other.example.com"]), "s3");
        assert!(matches!(host.fetch(&p, &json!({"url": url, "auth": "client_secret"})).await, Err(Error::Forbidden(_))));
        assert!(seen.lock().unwrap().is_empty(), "not even to sign in");
    }

    #[test]
    fn a_signature_is_checked_without_handing_over_the_secret() {
        let p = plugin(json!([{"name": "webhook_secret"}]), json!({"webhook_secret": "whsec"}));
        let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec").unwrap();
        mac.update(b"123.body");
        let good = hex::encode(mac.finalize().into_bytes());
        let secret = p.credential("webhook_secret").unwrap();
        assert!(signed_with(secret, "123.body", &good));
        assert!(!signed_with(secret, "123.bodY", &good), "another message");
        assert!(!signed_with(secret, "123.body", &good[2..]), "a shortened signature");
        assert!(!signed_with(secret, "123.body", "not hex"));
    }
}
