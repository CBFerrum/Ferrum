//! Plugin HTTP routes and WebSocket events. Signed-in callers act as themselves, limited to the
//! plugin's declared permissions; public routes and sockets run as the plugin's service user.

use crate::host::{Host, Scope};
use crate::{Loaded, PluginHost, reply_error, user_json};
use ferrum_kernel::{Ctx, Error, Kernel, Result};
use serde_json::{Map, Value, json};
use uuid::Uuid;

pub struct HttpCall {
    pub method: String,
    pub path: String,
    pub query: Map<String, Value>,
    pub headers: Map<String, Value>,
    pub body: String,
}

pub struct HttpReply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl PluginHost {
    /// The identity a request runs as: the signed-in user (limited to the manifest) or, for
    /// public endpoints, the plugin itself.
    async fn caller(&self, k: &Kernel, tenant: Uuid, p: &Loaded, user: Option<Ctx>, public: bool) -> Result<(Ctx, bool, Value)> {
        match user {
            Some(u) => {
                let who = user_json(&u);
                Ok((u, true, who))
            }
            None if public => Ok((self.service_ctx(k, tenant, p).await?, false, Value::Null)),
            None => Err(Error::Unauthorized("sign in required".into())),
        }
    }

    pub async fn handle_http(&self, k: &Kernel, tenant: Uuid, plugin: &str, call: HttpCall, user: Option<Ctx>) -> Result<HttpReply> {
        let p = self.plugin(&k.db, tenant, plugin).await?;
        let route = p.manifest.route(&call.path).ok_or_else(|| Error::NotFound(format!("route {} of plugin {plugin}", call.path)))?;
        let (ctx, limit, who) = self.caller(k, tenant, &p, user, route.public).await?;
        let input =
            json!({"kind": "http", "method": call.method, "path": call.path, "query": call.query, "headers": call.headers, "body": call.body, "user": who});
        let reply = {
            let mut api = Host { host: self, plugin: &p, scope: Scope::Own { k, ctx }, limit_to_manifest: limit };
            self.invoke(&k.db, tenant, &p, input, &mut api).await?
        };
        if let Some(e) = reply_error(&reply) {
            return Err(e);
        }
        let status = reply.get("status").and_then(Value::as_u64).filter(|s| (100..600).contains(s)).unwrap_or(200) as u16;
        let mut headers: Vec<(String, String)> = reply
            .get("headers")
            .and_then(Value::as_object)
            .map(|h| h.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.to_ascii_lowercase(), v.to_string()))).collect())
            .unwrap_or_default();
        let body = match (reply.get("json"), reply.get("body")) {
            (Some(j), _) => {
                headers.retain(|(k, _)| k != "content-type");
                headers.push(("content-type".into(), "application/json".into()));
                serde_json::to_vec(j).map_err(Error::internal)?
            }
            (None, Some(Value::String(b))) => b.clone().into_bytes(),
            (None, Some(other)) => other.to_string().into_bytes(),
            (None, None) => vec![],
        };
        if !headers.iter().any(|(k, _)| k == "content-type") {
            headers.push(("content-type".into(), "text/plain; charset=utf-8".into()));
        }
        Ok(HttpReply { status, headers, body })
    }

    /// A socket event (`connect`, `message`, `close`) for the plugin serving `path`. Returns the
    /// reply: `{"send": text | [texts], "close": bool}`.
    pub async fn socket_event(&self, k: &Kernel, tenant: Uuid, p: &Loaded, user: Option<Ctx>, mut input: Value) -> Result<Value> {
        let path = input.get("path").and_then(Value::as_str).unwrap_or_default().to_string();
        let public = p.manifest.socket(&path).map(|s| s.public).unwrap_or(false);
        let (ctx, limit, who) = self.caller(k, tenant, p, user, public).await?;
        input["kind"] = json!("socket");
        input["user"] = who;
        let reply = {
            let mut api = Host { host: self, plugin: p, scope: Scope::Own { k, ctx }, limit_to_manifest: limit };
            self.invoke(&k.db, tenant, p, input, &mut api).await?
        };
        if let Some(e) = reply_error(&reply) {
            return Err(e);
        }
        Ok(reply)
    }
}
