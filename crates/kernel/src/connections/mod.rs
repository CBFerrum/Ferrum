//! Connections to outside services (docs/builder/decisions.md BD-29 to BD-33): one described HTTP service a workspace
//! calls, for apps, configuration and packs alike. No provider is named here: a provider is a description a workspace,
//! a package or a pack supplies.
//!
//! * **A connection** is an address, a way to authenticate (none, a header holding a secret, basic, bearer, a request
//!   signed with a stated scheme, or tokens a person's sign-in gave), default headers, the hosts it may reach, time and
//!   size limits, and who may use it (roles, apps). Its secrets are sealed with the server's key and never read back.
//! * **Operations** are named calls on it: method, path, query, headers, a body or form, each a template over
//!   `inputs` and `secret`; where the results and the error are in the answer; which statuses mean "later"; the header
//!   that carries an idempotency key.
//! * **Three ways in**: [`call`] for apps, the `call` effect for buttons, transitions, approvals and automations (inline
//!   with its result for later effects, or queued and retried), and the `ferrum.call` capability through the gate.
//! * **The guard** ([`Kernel::fetch`]): `https` only (plain `http` for a host the operator names, or in development),
//!   only the connection's hosts, every resolved address public unless the operator names the host, pinned so a DNS
//!   change cannot move the call, redirects followed only to those hosts and checked again, a time limit, an answer
//!   size limit. Secrets never reach a log, an error, the audit trail, the call log or an event.
//! * **The log**: every call, its outcome and duration and the record it was about; no bodies unless an administrator
//!   turns on a short capture, redacted.

use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::jobs::{Job, JobHandler, NewJob};
use crate::{Kernel, audit};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub mod inbound;
pub mod oauth;
pub mod pull;

pub const CALL_KIND: &str = "platform.connection.call";
/// Attempts at a queued call while the service says "later".
const CALL_ATTEMPTS: i32 = 8;
const DEFAULT_TIMEOUT_MS: u64 = 15_000;
const MAX_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_MAX_BYTES: u64 = 1 << 20;
const MAX_BYTES: u64 = 10 << 20;
const MAX_REDIRECTS: usize = 3;
/// The statuses that mean "try again later", when a description says none.
pub(crate) const TEMPORARY: [u16; 7] = [408, 425, 429, 500, 502, 503, 504];
/// The longest a debug capture stays on, and how much of a body it keeps.
const DEBUG_MAX_MINUTES: i64 = 60;
const DEBUG_BODY: usize = 4096;
/// How long the call log and processed inbound calls are kept. A call with a key that went through is kept for good, so
/// its key never calls twice.
const LOG_DAYS: i64 = 90;
/// What a redacted value is shown as.
const HIDDEN: &str = "••••••";

fn admin_only(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("connections are configured by administrators".into())) }
}

/// A name of a connection, an operation, an input or a pull: lower case, digits and `_`, starting with a letter.
pub(crate) fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

// ── descriptions ──

/// How a connection is called (stored as `conn.description`). See the module documentation and docs/api.md.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Description {
    /// Answers each operation from its `fixture` and calls nobody.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub test: bool,
    /// `https://api.example.test/v2`: what every operation's path is added to.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base: String,
    #[serde(default)]
    pub auth: Auth,
    /// Sent with every operation; templates over `secret`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Hosts besides the base's that calls (and redirects) may reach.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// Who may call it besides administrators and configuration: people holding one of these roles ...
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// ... and these apps (packages), through [`call`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub operations: BTreeMap<String, Operation>,
    /// What it takes from outside, on its inbound address (BD-30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbound: Option<inbound::Inbound>,
    /// Schedules that pull from it (BD-31).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pulls: Vec<pull::Pull>,
}

/// How a call proves who makes it. Each `secret` names one of the connection's sealed values.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Auth {
    #[default]
    None,
    /// `header: <prefix><secret>`.
    Header {
        header: String,
        secret: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        prefix: String,
    },
    Basic {
        username: String,
        secret: String,
    },
    Bearer {
        secret: String,
    },
    /// HMAC-SHA256 under the secret of `<timestamp>.<body>` (with a `timestamp_header`, which carries the Unix time) or of
    /// the body, in `header` as `<prefix><hex or base64>`.
    Signed {
        header: String,
        secret: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp_header: Option<String>,
        #[serde(default)]
        encoding: Encoding,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        prefix: String,
    },
    /// Tokens a person's sign-in at the provider gave (BD-32): the workspace's account, or each person's own.
    Oauth {
        #[serde(default)]
        owner: oauth::Owner,
        provider: oauth::Provider,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    #[default]
    Hex,
    Base64,
}

/// One named call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// GET, POST, PUT, PATCH or DELETE; POST with a body or form, else GET.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Added to the base: starts with `/`; a template whose values are percent-encoded.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub query: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// A template giving JSON (`| json` writes a value as JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub form: BTreeMap<String, String>,
    /// Result name → JSON pointer into the answer; none: the whole answer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub result: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporary: Option<Vec<u16>>,
    /// Sends the call's key in this header, the same on every attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_header: Option<String>,
    /// Whether it changes something at the service (then it needs a key): unless said, every method but GET.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<bool>,
    /// The inputs it takes; others are refused. Empty: any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// A test connection's answer: `{status, body}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixture: Option<Value>,
}

impl Operation {
    fn method(&self) -> &str {
        match &self.method {
            Some(m) => m,
            None if self.body.is_some() || !self.form.is_empty() => "POST",
            None => "GET",
        }
    }

    pub(crate) fn changes(&self) -> bool {
        self.changes.unwrap_or(self.method() != "GET")
    }
}

/// What is wrong with a template, if anything.
fn template_problem(t: &str) -> Option<String> {
    crate::print::environment().template_from_str(t).err().map(|e| format!("'{t}': {e}"))
}

pub(crate) fn pointer_problem(p: &str) -> Option<String> {
    (!p.is_empty() && !p.starts_with('/')).then(|| format!("{p} is not a JSON pointer (/a/0/b)"))
}

pub(crate) fn header_problem(h: &str) -> Option<String> {
    reqwest::header::HeaderName::from_bytes(h.as_bytes()).is_err().then(|| format!("{h} is not a header name"))
}

impl Description {
    /// What is wrong with it, said plainly; `None` when it may be saved.
    pub fn problem(&self) -> Option<String> {
        if !self.test {
            let Ok(base) = reqwest::Url::parse(&self.base) else { return Some("base must be an https address".into()) };
            if !matches!(base.scheme(), "https" | "http") || base.host_str().is_none() {
                return Some("base must be an https address".into());
            }
            if !base.username().is_empty() || base.password().is_some() || base.query().is_some() || base.fragment().is_some() {
                return Some("base is a scheme, a host and a path: no user, query or fragment".into());
            }
        }
        if let Some(h) = self.allowed_hosts.iter().find(|h| h.is_empty() || h.contains(['/', ':', '@', ' '])) {
            return Some(format!("allowed_hosts: {h} is not a host name"));
        }
        if self.timeout_ms.is_some_and(|t| t == 0 || t > MAX_TIMEOUT_MS) {
            return Some(format!("timeout_ms is 1 to {MAX_TIMEOUT_MS}"));
        }
        if self.max_bytes.is_some_and(|b| b == 0 || b > MAX_BYTES) {
            return Some(format!("max_bytes is 1 to {MAX_BYTES}"));
        }
        if self.roles.iter().chain(&self.apps).any(|r| r.trim().is_empty()) {
            return Some("roles and apps are names".into());
        }
        match &self.auth {
            Auth::None => {}
            Auth::Header { header, secret, .. } | Auth::Signed { header, secret, .. } => {
                if let Some(p) = header_problem(header) {
                    return Some(p);
                }
                if secret.is_empty() {
                    return Some("auth.secret names one of the connection's secrets".into());
                }
            }
            Auth::Basic { secret, .. } | Auth::Bearer { secret } if secret.is_empty() => {
                return Some("auth.secret names one of the connection's secrets".into());
            }
            Auth::Basic { .. } | Auth::Bearer { .. } => {}
            Auth::Oauth { provider, .. } => {
                if let Some(p) = provider.problem() {
                    return Some(p);
                }
            }
        }
        if let Auth::Signed { timestamp_header: Some(h), .. } = &self.auth
            && let Some(p) = header_problem(h)
        {
            return Some(p);
        }
        for (h, t) in &self.headers {
            if let Some(p) = header_problem(h).or_else(|| template_problem(t)) {
                return Some(p);
            }
        }
        for (name, op) in &self.operations {
            if let Some(p) = op.problem() {
                return Some(format!("operation {name}: {p}"));
            }
            if !is_name(name) {
                return Some(format!("operation {name}: a name is lower case letters, digits and _"));
            }
        }
        if let Some(p) = self.inbound.as_ref().and_then(|i| i.problem(&self.apps)) {
            return Some(p);
        }
        let mut names = std::collections::BTreeSet::new();
        for p in &self.pulls {
            if !names.insert(&p.name) {
                return Some(format!("two pulls are named {}", p.name));
            }
            if let Some(m) = p.problem(self) {
                return Some(m);
            }
        }
        None
    }
}

impl Operation {
    fn problem(&self) -> Option<String> {
        if !["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&self.method()) {
            return Some("method is GET, POST, PUT, PATCH or DELETE".into());
        }
        if !self.path.is_empty() && !self.path.starts_with('/') {
            return Some("path starts with /".into());
        }
        if self.body.is_some() && !self.form.is_empty() {
            return Some("a request has a body or a form, not both".into());
        }
        let templates = std::iter::once(&self.path).chain(self.query.values()).chain(self.headers.values()).chain(self.body.iter()).chain(self.form.values());
        if let Some(p) = templates.filter_map(|t| template_problem(t)).next() {
            return Some(p);
        }
        if let Some(p) = self.headers.keys().chain(self.idempotency_header.iter()).find_map(|h| header_problem(h)) {
            return Some(p);
        }
        if let Some(p) = self.result.values().chain(self.error_at.iter()).find_map(|p| pointer_problem(p)) {
            return Some(p);
        }
        if let Some(i) = self.inputs.iter().find(|i| !is_name(i)) {
            return Some(format!("input {i}: a name is lower case letters, digits and _"));
        }
        None
    }
}

// ── the guard ──

/// An address no connection reaches unless the operator names its host: private, loopback, link-local (the cloud
/// metadata addresses among them), shared, multicast, reserved, benchmarking, and v6 forms carrying one of them.
pub(crate) fn blocked(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    if crate::outbox::is_private(ip) {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_multicast() || o[0] >= 240 || (o[0] == 192 && o[1] == 0 && o[2] == 0) || (o[0] == 198 && (o[1] & 0xfe) == 18)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            let nat64 = s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0];
            let embedded = std::net::Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
            v6.is_multicast() || (nat64 && blocked(IpAddr::V4(embedded))) || (s[..6] == [0, 0, 0, 0, 0, 0] && blocked(IpAddr::V4(embedded)))
        }
    }
}

/// A request, made: where, how, what.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub method: reqwest::Method,
    pub url: reqwest::Url,
    pub headers: Vec<(String, String)>,
    pub body: Option<(Vec<u8>, &'static str)>,
}

/// How far a request may go: hosts (empty: the request's own), time, answer size.
#[derive(Debug, Clone)]
pub(crate) struct Reach {
    pub hosts: Vec<String>,
    pub timeout: Duration,
    pub max_bytes: u64,
}

/// What came back.
#[derive(Debug, Clone)]
pub(crate) struct Answer {
    pub status: u16,
    /// JSON when it was, else the text.
    pub body: Value,
}

impl Kernel {
    /// Whether the operator named this host (`FERRUM_CONNECTION_HOSTS`): then a private address and plain http are
    /// allowed for it, and for it alone.
    fn operator_allows(&self, host: &str) -> bool {
        self.settings.connection_hosts.iter().any(|h| h.eq_ignore_ascii_case(host))
    }

    /// A client for one hop to `url`, its addresses checked and pinned; no redirects of its own.
    async fn vet(&self, url: &reqwest::Url, reach: &Reach) -> Result<reqwest::Client> {
        let host = url.host_str().ok_or_else(|| Error::invalid("the address has no host"))?.trim_matches(['[', ']']).to_string();
        if !reach.hosts.is_empty() && !reach.hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
            return Err(Error::invalid(format!("{host} is not one of the hosts this connection may reach")));
        }
        let open = self.settings.webhooks_allow_private || self.operator_allows(&host);
        match url.scheme() {
            "https" => {}
            "http" if open => {}
            other => return Err(Error::invalid(format!("{other} addresses are not called: https only"))),
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::invalid("an address with a user name in it is not called"));
        }
        let port = url.port_or_known_default().unwrap_or(443);
        let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => tokio::net::lookup_host((host.as_str(), port)).await.map_err(|_| Error::Unavailable(format!("{host} does not resolve")))?.collect(),
        };
        if addrs.is_empty() {
            return Err(Error::Unavailable(format!("{host} does not resolve")));
        }
        if !open && addrs.iter().any(|a| blocked(a.ip())) {
            return Err(Error::invalid(format!("{host} is a private, loopback, link-local or reserved address")));
        }
        reqwest::Client::builder()
            .resolve_to_addrs(&host, &addrs)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(reach.timeout)
            .user_agent("Ferrum-Connections/1")
            .build()
            .map_err(|e| Error::Unavailable(format!("no client: {}", e.without_url())))
    }

    /// Makes `req` within `reach`: every hop, the first and each redirect's, checked as [`Kernel::vet`] says; at most
    /// [`MAX_REDIRECTS`]; the answer read up to its size limit. A service that does not answer in time, or at all, is
    /// `Unavailable` (worth trying again); a refused address or an answer too large is `Invalid`.
    pub(crate) async fn fetch(&self, mut req: Request, reach: &Reach) -> Result<Answer> {
        let mut reach = reach.clone();
        if reach.hosts.is_empty() {
            reach.hosts.push(req.url.host_str().unwrap_or_default().trim_matches(['[', ']']).to_string());
        }
        for _ in 0..=MAX_REDIRECTS {
            let client = self.vet(&req.url, &reach).await?;
            let mut rb = client.request(req.method.clone(), req.url.clone());
            for (h, v) in &req.headers {
                rb = rb.header(h.as_str(), v.as_str());
            }
            if let Some((body, kind)) = &req.body {
                rb = rb.header(reqwest::header::CONTENT_TYPE, *kind).body(body.clone());
            }
            let mut res = rb.send().await.map_err(|e| Error::Unavailable(format!("the service did not answer: {}", e.without_url())))?;
            let status = res.status().as_u16();
            if res.status().is_redirection()
                && let Some(to) = res.headers().get(reqwest::header::LOCATION).and_then(|l| l.to_str().ok())
            {
                req.url = req.url.join(to).map_err(|_| Error::invalid("the service redirected to something that is not an address"))?;
                if status == 303 || (matches!(status, 301 | 302) && req.method != reqwest::Method::GET) {
                    req.method = reqwest::Method::GET;
                    req.body = None;
                }
                continue;
            }
            if res.content_length().is_some_and(|n| n > reach.max_bytes) {
                return Err(Error::invalid(format!("the answer is larger than {} bytes", reach.max_bytes)));
            }
            let mut buf = Vec::new();
            while let Some(chunk) = res.chunk().await.map_err(|e| Error::Unavailable(format!("the answer broke off: {}", e.without_url())))? {
                buf.extend_from_slice(&chunk);
                if buf.len() as u64 > reach.max_bytes {
                    return Err(Error::invalid(format!("the answer is larger than {} bytes", reach.max_bytes)));
                }
            }
            let body = serde_json::from_slice(&buf).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&buf).into_owned()));
            return Ok(Answer { status, body });
        }
        Err(Error::invalid(format!("more than {MAX_REDIRECTS} redirects")))
    }
}

// ── templates and answers ──

/// Percent-encodes all but unreserved characters (RFC 3986), for values put into a path.
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `src` over `vars`, as text (no HTML escaping; the print formats' engine, fuel-limited).
pub(crate) fn render(src: &str, vars: &Value) -> Result<String> {
    let mut env = crate::print::environment();
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
    env.add_filter("json", |v: minijinja::Value| serde_json::to_string(&v).unwrap_or_default());
    env.render_str(src, minijinja::Value::from_serialize(vars)).map_err(|e| Error::invalid(format!("a template failed: {e}")))
}

/// A path template: each value it prints percent-encoded, so an input cannot add a segment, a query or a host.
fn render_path(src: &str, vars: &Value) -> Result<String> {
    let mut env = crate::print::environment();
    env.set_formatter(|out, _state, value| {
        let text = if value.is_none() || value.is_undefined() { String::new() } else { value.to_string() };
        out.write_str(&pct(&text)).map_err(|_| minijinja::Error::new(minijinja::ErrorKind::WriteFailure, "path"))
    });
    env.render_str(src, minijinja::Value::from_serialize(vars)).map_err(|e| Error::invalid(format!("the path failed: {e}")))
}

/// Where a JSON pointer leads in `v`, as text.
pub(crate) fn at(v: &Value, pointer: Option<&str>) -> Option<String> {
    match v.pointer(pointer?)? {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// `v` with each of `mask` (secret values, tokens) written as dots, wherever it appears.
pub(crate) fn masked(v: &Value, mask: &[String]) -> Value {
    match v {
        Value::String(s) => Value::String(mask.iter().filter(|m| m.len() >= 4).fold(s.clone(), |s, m| s.replace(m.as_str(), HIDDEN))),
        Value::Array(a) => Value::Array(a.iter().map(|x| masked(x, mask)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), masked(x, mask))).collect()),
        _ => v.clone(),
    }
}

pub(crate) fn masked_text(s: &str, mask: &[String]) -> String {
    masked(&Value::String(s.to_string()), mask).as_str().unwrap_or_default().chars().take(1000).collect()
}

/// A digest of a call's inputs, to tell the same request from another under one key.
fn digest(v: &Value) -> String {
    hex::encode(Sha256::digest(v.to_string().as_bytes()))
}

// ── connections as stored ──

pub(crate) fn sealed_label(tenant: Uuid, id: Uuid) -> String {
    format!("conn:{tenant}:{id}")
}

/// A connection, loaded for the length of a call: its secrets open.
#[derive(Clone)]
pub(crate) struct Loaded {
    pub tenant: Uuid,
    pub id: Uuid,
    pub name: String,
    pub desc: Description,
    pub secrets: BTreeMap<String, String>,
    pub enabled: bool,
    pub debug: bool,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded").field("id", &self.id).field("name", &self.name).finish_non_exhaustive()
    }
}

impl Loaded {
    /// Every value that must never be shown: the secrets.
    pub(crate) fn mask(&self) -> Vec<String> {
        self.secrets.values().cloned().collect()
    }

    pub(crate) fn secret(&self, name: &str) -> Result<&str> {
        self.secrets
            .get(name)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::invalid(format!("the connection {} has no secret {name}: an administrator gives it one", self.name)))
    }

    fn reach(&self) -> Reach {
        let mut hosts = self.desc.allowed_hosts.clone();
        if let Some(h) = reqwest::Url::parse(&self.desc.base).ok().and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_string())) {
            hosts.push(h);
        }
        Reach {
            hosts,
            timeout: Duration::from_millis(self.desc.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
            max_bytes: self.desc.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
        }
    }
}

const ROW: &str = "select id, name, description, sealed, enabled, coalesce(debug_until > now(), false) from conn where tenant_id = $1";

type Row = (Uuid, String, Value, Option<String>, bool, bool);

impl Kernel {
    fn open_row(&self, tenant: Uuid, (id, name, desc, sealed, enabled, debug): Row) -> Result<Loaded> {
        let desc: Description = serde_json::from_value(desc).map_err(Error::internal)?;
        let secrets = match sealed {
            Some(s) => serde_json::from_str(&self.open_sealed(&sealed_label(tenant, id), &s)?).map_err(Error::internal)?,
            None => BTreeMap::new(),
        };
        Ok(Loaded { tenant, id, name, desc, secrets, enabled, debug })
    }

    /// A connection by name or id, its secrets open.
    pub(crate) async fn load_connection(&self, tenant: Uuid, which: &str) -> Result<Loaded> {
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let row: Option<Row> = match which.parse::<Uuid>() {
            Ok(id) => sqlx::query_as(sqlx::AssertSqlSafe(format!("{ROW} and id = $2"))).bind(tenant).bind(id).fetch_optional(&mut *tx).await?,
            Err(_) => sqlx::query_as(sqlx::AssertSqlSafe(format!("{ROW} and name = $2"))).bind(tenant).bind(which).fetch_optional(&mut *tx).await?,
        };
        tx.commit().await?;
        self.open_row(tenant, row.ok_or_else(|| Error::NotFound(format!("connection {which}")))?)
    }
}

// ── calling ──

/// One call, as apps make it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Call {
    /// The connection's name.
    pub connection: String,
    pub operation: String,
    #[serde(default)]
    pub inputs: Value,
    /// The same for every attempt at one change: a second call with it gets the first's result and calls nobody, and
    /// the service gets it in the operation's idempotency header. An operation that changes something needs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The record the call is about, for the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<Uuid>,
}

/// How a call ended.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The mapped result, and the whole answer.
    Ok(Value, Value),
    /// The service said "later", or did not answer.
    Later(String),
    /// The service said no, or the call could not be made.
    No(String),
}

/// Calls an operation of a connection and gives its mapped result: for apps (docs/api.md, "Connections"). `app` is the
/// calling package, which the connection must list; `None` is a person's own call (an administrator, or one holding a
/// role the connection lists) or the system's. A temporary failure is `Unavailable`; any other, `Invalid`.
pub async fn call(k: &Kernel, ctx: &Ctx, app: Option<&str>, c: &Call) -> Result<Value> {
    k.call_connection(ctx, app, c).await
}

/// Whether `ctx` (as `app`, when one calls) may use the connection.
pub(crate) fn may_use(ctx: &Ctx, app: Option<&str>, desc: &Description) -> Result<()> {
    let ok = match app {
        Some(app) => desc.apps.iter().any(|a| a == app),
        None => ctx.is_system || ctx.is_admin || desc.roles.iter().any(|r| ctx.has_role(r)),
    };
    if ok { Ok(()) } else { Err(Error::Forbidden(format!("{} may not use this connection", app.unwrap_or("you")))) }
}

impl Kernel {
    pub async fn call_connection(&self, ctx: &Ctx, app: Option<&str>, c: &Call) -> Result<Value> {
        let conn = self.load_connection(ctx.tenant, &c.connection).await?;
        if !conn.enabled {
            return Err(Error::invalid(format!("the connection {} is turned off", conn.name)));
        }
        may_use(ctx, app, &conn.desc)?;
        let op = conn.desc.operations.get(&c.operation).ok_or_else(|| Error::invalid(format!("{} has no operation {}", conn.name, c.operation)))?;
        let inputs = match &c.inputs {
            Value::Null => Value::Object(Map::new()),
            Value::Object(o) => {
                if let Some(n) = o.keys().find(|n| !op.inputs.is_empty() && !op.inputs.contains(n)) {
                    return Err(Error::invalid(format!("{} takes no input {n}", c.operation)));
                }
                Value::Object(o.clone())
            }
            _ => return Err(Error::invalid("inputs are an object")),
        };
        let key = c.key.as_deref().filter(|k| !k.trim().is_empty());
        if op.changes() && key.is_none() {
            return Err(Error::invalid(format!("{} changes something at the service, so it needs a key", c.operation)));
        }
        let given = digest(&inputs);
        if let Some(key) = key.filter(|_| op.changes())
            && let Some((was, result)) = self.done_before(&conn, &c.operation, key).await?
        {
            return if was == given {
                Ok(result)
            } else {
                Err(Error::Conflict(format!("the key {key} was used for {} with other inputs: a new request needs a new key", c.operation)))
            };
        }
        let about = c.entity.clone().zip(c.record);
        let outcome = self
            .perform(ctx, &conn, &c.operation, &inputs, key, None, about.as_ref().map(|(e, r)| (e.as_str(), *r)), op.changes().then_some(given.as_str()))
            .await?;
        match outcome {
            Outcome::Ok(result, _) => Ok(result),
            Outcome::Later(why) => Err(Error::Unavailable(why)),
            Outcome::No(why) => Err(Error::invalid(why)),
        }
    }

    /// The result an earlier call with this key had, and its inputs' digest.
    async fn done_before(&self, conn: &Loaded, op: &str, key: &str) -> Result<Option<(String, Value)>> {
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        let row: Option<(Option<String>, Option<Value>)> = sqlx::query_as(
            "select input_digest, result from conn_call where tenant_id = $1 and conn_id = $2 and operation = $3 and key = $4 and outcome = 'ok'",
        )
        .bind(conn.tenant)
        .bind(conn.id)
        .bind(op)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row.map(|(d, r)| (d.unwrap_or_default(), r.unwrap_or(Value::Null))))
    }

    /// One operation, made and logged. `extra` are more template variables (a pull's `cursor`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn perform(
        &self,
        ctx: &Ctx,
        conn: &Loaded,
        name: &str,
        inputs: &Value,
        key: Option<&str>,
        extra: Option<(&str, &Value)>,
        about: Option<(&str, Uuid)>,
        keep: Option<&str>,
    ) -> Result<Outcome> {
        let op = conn.desc.operations.get(name).ok_or_else(|| Error::invalid(format!("{} has no operation {name}", conn.name)))?;
        let started = Instant::now();
        let mut mask = conn.mask();
        let mut debug = None;
        let answered = if conn.desc.test {
            let f = op.fixture.clone().unwrap_or_else(|| json!({"status": 200, "body": {}}));
            Ok(Answer { status: f["status"].as_u64().unwrap_or(200) as u16, body: f.get("body").cloned().unwrap_or(Value::Null) })
        } else {
            let mut tries = 0;
            let got = loop {
                tries += 1;
                let got = match self.build_request(ctx, conn, op, inputs, key, extra).await {
                    Ok((req, more)) => {
                        mask.extend(more);
                        if conn.debug {
                            debug = Some(json!({"request": captured(&req, &mask)}));
                        }
                        let got = self.fetch(req, &conn.reach()).await;
                        if let (Some(d), Ok(a)) = (debug.as_mut(), &got) {
                            d["answer"] = json!({"status": a.status, "body": clipped(&masked(&a.body, &mask))});
                        }
                        got
                    }
                    Err(e) => Err(e),
                };
                // A token the service no longer takes: refreshed once, and the call made again.
                if tries == 1
                    && matches!(&got, Ok(a) if a.status == 401)
                    && matches!(conn.desc.auth, Auth::Oauth { .. })
                    && self.access_token(conn, ctx, true).await.is_ok()
                {
                    continue;
                }
                break got;
            };
            got.map_err(|e| match e {
                Error::Unavailable(m) => Error::Unavailable(masked_text(&m, &mask)),
                other => Error::invalid(masked_text(&other.to_string(), &mask)),
            })
        };
        let temporary = op.temporary.clone().unwrap_or_else(|| TEMPORARY.to_vec());
        let error_of = |a: &Answer| {
            let said = at(&a.body, op.error_at.as_deref()).unwrap_or_else(|| a.body.to_string().chars().take(300).collect());
            masked_text(&format!("{}: {said}", a.status), &mask)
        };
        let (outcome, status, row_outcome) = match answered {
            Ok(a) if (200..300).contains(&a.status) => {
                let result = if op.result.is_empty() {
                    a.body.clone()
                } else {
                    Value::Object(op.result.iter().map(|(n, p)| (n.clone(), a.body.pointer(p).cloned().unwrap_or(Value::Null))).collect())
                };
                let status = a.status;
                (Outcome::Ok(masked(&result, &mask), a.body), Some(status), "ok")
            }
            Ok(a) if temporary.contains(&a.status) => (Outcome::Later(error_of(&a)), Some(a.status), "temporary"),
            Ok(a) => (Outcome::No(error_of(&a)), Some(a.status), "failed"),
            Err(Error::Unavailable(m)) => (Outcome::Later(m), None, "temporary"),
            Err(e) => (Outcome::No(e.to_string()), None, "refused"),
        };
        let error = match &outcome {
            Outcome::Later(m) | Outcome::No(m) => Some(m.clone()),
            Outcome::Ok(..) => None,
        };
        let kept = match &outcome {
            Outcome::Ok(r, _) if keep.is_some() => Some(r.clone()),
            _ => None,
        };
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        sqlx::query(
            "insert into conn_call (tenant_id, id, conn_id, operation, outcome, status, duration_ms, entity, record_id, error, key, input_digest, result, debug) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) on conflict do nothing",
        )
        .bind(conn.tenant)
        .bind(Uuid::now_v7())
        .bind(conn.id)
        .bind(name)
        .bind(row_outcome)
        .bind(status.map(i32::from))
        .bind(started.elapsed().as_millis().min(i32::MAX as u128) as i32)
        .bind(about.map(|a| a.0))
        .bind(about.map(|a| a.1))
        .bind(error)
        .bind(key.filter(|_| keep.is_some()))
        .bind(keep)
        .bind(kept)
        .bind(debug)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(outcome)
    }

    /// The request an operation makes, and what more it must never show (a token).
    async fn build_request(
        &self,
        ctx: &Ctx,
        conn: &Loaded,
        op: &Operation,
        inputs: &Value,
        key: Option<&str>,
        extra: Option<(&str, &Value)>,
    ) -> Result<(Request, Vec<String>)> {
        let mut vars = json!({"inputs": inputs, "secret": conn.secrets, "key": key});
        if let Some((n, v)) = extra {
            vars[n] = v.clone();
        }
        let r = |s: &str| render(s, &vars);
        let mut url = reqwest::Url::parse(&format!("{}{}", conn.desc.base.trim_end_matches('/'), render_path(&op.path, &vars)?))
            .map_err(|_| Error::invalid("the operation's address is not an address"))?;
        if !op.query.is_empty() {
            let pairs = op.query.iter().map(|(k, v)| Ok((k.clone(), r(v)?))).collect::<Result<Vec<_>>>()?;
            url.query_pairs_mut().extend_pairs(pairs);
        }
        let mut headers = Vec::new();
        for (h, v) in conn.desc.headers.iter().chain(&op.headers) {
            headers.push((h.clone(), r(v)?));
        }
        if let (Some(h), Some(k)) = (&op.idempotency_header, key) {
            headers.push((h.clone(), k.to_string()));
        }
        let body = match &op.body {
            Some(b) => {
                let text = r(b)?;
                serde_json::from_str::<Value>(&text).map_err(|e| Error::invalid(format!("the operation's body is not JSON: {e}")))?;
                Some((text.into_bytes(), "application/json"))
            }
            None if !op.form.is_empty() => {
                let mut f = reqwest::Url::parse("http://form/").map_err(Error::internal)?;
                for (k, v) in &op.form {
                    f.query_pairs_mut().append_pair(k, &r(v)?);
                }
                Some((f.query().unwrap_or_default().as_bytes().to_vec(), "application/x-www-form-urlencoded"))
            }
            None => None,
        };
        let more = self.authenticate(ctx, conn, &mut headers, body.as_ref().map(|b| b.0.as_slice())).await?;
        let method = reqwest::Method::from_bytes(op.method().as_bytes()).map_err(Error::internal)?;
        Ok((Request { method, url, headers, body }, more))
    }

    /// Adds what proves who calls; what more it must never show.
    async fn authenticate(&self, ctx: &Ctx, conn: &Loaded, headers: &mut Vec<(String, String)>, body: Option<&[u8]>) -> Result<Vec<String>> {
        use base64::Engine as _;
        match &conn.desc.auth {
            Auth::None => {}
            Auth::Oauth { .. } => {
                let token = self.access_token(conn, ctx, false).await?;
                headers.push(("authorization".into(), format!("Bearer {token}")));
                return Ok(vec![token]);
            }
            Auth::Header { header, secret, prefix } => headers.push((header.clone(), format!("{prefix}{}", conn.secret(secret)?))),
            Auth::Bearer { secret } => headers.push(("authorization".into(), format!("Bearer {}", conn.secret(secret)?))),
            Auth::Basic { username, secret } => {
                let pair = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{}", conn.secret(secret)?));
                headers.push(("authorization".into(), format!("Basic {pair}")));
                return Ok(vec![pair]);
            }
            Auth::Signed { header, secret, timestamp_header, encoding, prefix } => {
                use hmac::Mac;
                let mut mac = hmac::Hmac::<Sha256>::new_from_slice(conn.secret(secret)?.as_bytes()).map_err(Error::internal)?;
                if let Some(th) = timestamp_header {
                    let ts = Utc::now().timestamp().to_string();
                    mac.update(format!("{ts}.").as_bytes());
                    headers.push((th.clone(), ts));
                }
                mac.update(body.unwrap_or_default());
                let sig = mac.finalize().into_bytes();
                let sig = match encoding {
                    Encoding::Hex => hex::encode(sig),
                    Encoding::Base64 => base64::engine::general_purpose::STANDARD.encode(sig),
                };
                headers.push((header.clone(), format!("{prefix}{sig}")));
            }
        }
        Ok(vec![])
    }
}

/// A request as the debug capture keeps it: every header that may prove who calls hidden, every secret masked.
fn captured(req: &Request, mask: &[String]) -> Value {
    let hidden = |h: &str| {
        let h = h.to_ascii_lowercase();
        ["authorization", "cookie", "proxy-authorization"].contains(&h.as_str())
            || h.contains("key")
            || h.contains("token")
            || h.contains("secret")
            || h.contains("signature")
    };
    let headers: Map<String, Value> = req.headers.iter().map(|(h, v)| (h.clone(), if hidden(h) { json!(HIDDEN) } else { masked(&json!(v), mask) })).collect();
    let body = req.body.as_ref().map(|(b, _)| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
    json!({"method": req.method.as_str(), "url": masked_text(req.url.as_str(), mask), "headers": headers, "body": clipped(&masked(&json!(body), mask))})
}

/// At most [`DEBUG_BODY`] characters of a body, as text.
fn clipped(v: &Value) -> Value {
    let text = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    Value::String(text.chars().take(DEBUG_BODY).collect())
}

// ── administration ──

/// What `POST /api/admin/connections` takes. `secrets` are sealed and never read back; left out, the ones there are
/// stay.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionInput {
    pub name: String,
    pub label: String,
    pub description: Description,
    #[serde(default)]
    pub secrets: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub enabled: bool,
}

impl Kernel {
    /// Adds a connection, or changes one. Its secrets are sealed; a description that is wrong is refused.
    pub async fn save_connection_def(&self, ctx: &Ctx, id: Option<Uuid>, input: &ConnectionInput) -> Result<Uuid> {
        admin_only(ctx)?;
        if !is_name(&input.name) {
            return Err(Error::field("name", "lower case letters, digits and _, starting with a letter"));
        }
        let label = input.label.trim();
        if label.is_empty() || label.chars().count() > 100 {
            return Err(Error::field("label", "give it a label of at most 100 characters"));
        }
        if let Some(p) = self.description_problem(&ctx.schema, &input.description) {
            return Err(Error::field("description", p));
        }
        if let Some(s) = &input.secrets
            && (s.len() > 20 || s.values().any(|v| v.len() > crate::secret::MAX_SECRET) || s.keys().any(|n| !is_name(n)))
        {
            return Err(Error::field("secrets", "at most 20, each named as an input is, of at most 16 KB"));
        }
        let id = id.unwrap_or_else(Uuid::now_v7);
        let source = self.packaged_connections(ctx).into_iter().find(|(_, c)| c.name == input.name).map(|(p, _)| format!("package:{p}"));
        let sealed = match &input.secrets {
            Some(s) => {
                let key = self
                    .settings
                    .secret_key
                    .as_ref()
                    .ok_or_else(|| Error::Unavailable("connection secrets need the server's secret key (FERRUM_SECRET_KEY)".into()))?;
                Some(key.seal(&sealed_label(ctx.tenant, id), &serde_json::to_string(s).map_err(Error::internal)?)?)
            }
            None => None,
        };
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query(
            "insert into conn (tenant_id, id, name, label, description, sealed, enabled, source) values ($1, $2, $3, $4, $5, $6, $7, $8) \
             on conflict (tenant_id, id) do update set name = excluded.name, label = excluded.label, description = excluded.description, \
             sealed = coalesce(excluded.sealed, conn.sealed), enabled = excluded.enabled, updated_at = now()",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(&input.name)
        .bind(label)
        .bind(serde_json::to_value(&input.description).map_err(Error::internal)?)
        .bind(sealed)
        .bind(input.enabled)
        .bind(source)
        .execute(&mut *tx)
        .await;
        match n {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => return Err(Error::field("name", "another connection has this name")),
            other => other?,
        };
        self.after_save(&mut tx, ctx.tenant, id, &input.description).await?;
        audit::record(&mut tx, ctx, "Connection", id, "save", None, json!({"name": input.name, "enabled": input.enabled, "secrets": input.secrets.is_some()}))
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// What is wrong with a description, its mappings read against the workspace's entities.
    pub(crate) fn description_problem(&self, schema: &ferrum_meta::Schema, d: &Description) -> Option<String> {
        d.problem()
            .or_else(|| d.inbound.as_ref().and_then(|i| i.map.as_ref()).and_then(|m| m.problem(schema)))
            .or_else(|| d.pulls.iter().filter_map(|p| p.map.as_ref()).find_map(|m| m.problem(schema)))
    }

    /// What a saved description sets going (pulls' schedules).
    pub(crate) async fn after_save(&self, tx: &mut Tx, tenant: Uuid, id: Uuid, d: &Description) -> Result<()> {
        pull::keep(tx, tenant, id, d).await
    }

    /// The connections, never a secret: `{id, name, label, description, enabled, secrets (names), account (the workspace's
    /// signed-in account: `{status, account, expires_at}`), source, refused, debug_until, inbound}`.
    pub async fn connection_defs(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(Uuid, String, String, Value, Option<String>, bool, Option<String>, i32, Option<DateTime<Utc>>)> =
            sqlx::query_as("select id, name, label, description, sealed, enabled, source, refused, debug_until from conn where tenant_id = $1 order by name")
                .bind(ctx.tenant)
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        let mut out = Vec::new();
        for (id, name, label, description, sealed, enabled, source, refused, debug_until) in rows {
            let names: Vec<String> = match sealed {
                Some(s) => serde_json::from_str::<BTreeMap<String, String>>(&self.open_sealed(&sealed_label(ctx.tenant, id), &s)?)
                    .map_err(Error::internal)?
                    .into_keys()
                    .collect(),
                None => vec![],
            };
            let account = self.workspace_account(ctx.tenant, id).await?;
            out.push(json!({"id": id, "name": name, "label": label, "description": description, "enabled": enabled, "secrets": names, "account": account,
                            "source": source, "refused": refused, "debug_until": debug_until, "inbound": format!("/api/connections/inbound/{id}")}));
        }
        // What packages offer and the workspace has not taken up: turned off, without an id until it is saved.
        for (package, c) in self.packaged_connections(ctx) {
            if !out.iter().any(|o| o["name"] == c.name) {
                out.push(json!({"id": null, "name": c.name, "label": c.label, "description": c.description, "enabled": false, "secrets": [],
                                "source": format!("package:{package}"), "offered": true}));
            }
        }
        Ok(out)
    }

    pub async fn delete_connection_def(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("delete from conn where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound("connection".into()));
        }
        audit::record(&mut tx, ctx, "Connection", id, "delete", None, json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// An administrator runs one operation deliberately ("try this"), with its inputs: the mapped result and the whole
    /// answer, both masked, or what went wrong. A test connection answers from its fixture. An operation that changes
    /// something needs a key here too.
    pub async fn try_connection(&self, ctx: &Ctx, id: Uuid, operation: &str, inputs: &Value, key: Option<&str>) -> Result<Value> {
        admin_only(ctx)?;
        let conn = self.load_connection(ctx.tenant, &id.to_string()).await?;
        let op = conn.desc.operations.get(operation).ok_or_else(|| Error::invalid(format!("{} has no operation {operation}", conn.name)))?;
        if op.changes() && key.is_none_or(|k| k.trim().is_empty()) {
            return Err(Error::invalid(format!("{operation} changes something at the service, so trying it needs a key")));
        }
        let inputs = if inputs.is_null() { json!({}) } else { inputs.clone() };
        Ok(match self.perform(ctx, &conn, operation, &inputs, key, None, None, None).await? {
            Outcome::Ok(result, answer) => json!({"ok": true, "result": result, "answer": masked(&answer, &conn.mask())}),
            Outcome::Later(why) => json!({"ok": false, "temporary": true, "error": why}),
            Outcome::No(why) => json!({"ok": false, "temporary": false, "error": why}),
        })
    }

    /// Turns on the debug capture for `minutes` (at most an hour; 0 turns it off).
    pub async fn debug_connection(&self, ctx: &Ctx, id: Uuid, minutes: i64) -> Result<()> {
        admin_only(ctx)?;
        if !(0..=DEBUG_MAX_MINUTES).contains(&minutes) {
            return Err(Error::field("minutes", format!("0 to {DEBUG_MAX_MINUTES}")));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query(
            "update conn set debug_until = case when $3 = 0 then null else now() + make_interval(mins => $3::int) end where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(minutes)
        .execute(&mut *tx)
        .await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound("connection".into()));
        }
        audit::record(&mut tx, ctx, "Connection", id, "debug", None, json!({"minutes": minutes})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The call log, newest first: `{at, operation, outcome, status, duration_ms, entity, record_id, error, debug}`.
    pub async fn connection_log(&self, ctx: &Ctx, id: Uuid, limit: i64) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows = sqlx::query_scalar(
            "select jsonb_build_object('at', at, 'operation', operation, 'outcome', outcome, 'status', status, 'duration_ms', duration_ms, \
               'entity', entity, 'record_id', record_id, 'error', error, 'debug', debug) \
             from conn_call where tenant_id = $1 and conn_id = $2 order by at desc limit $3",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(limit.clamp(1, 500))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Upkeep: debug captures past the longest a capture stays on, calls logged more than [`LOG_DAYS`] ago, inbound
    /// calls processed that long ago, sign-ins never finished.
    pub(crate) async fn purge_connections(&self) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let mut n = sqlx::query("update conn_call set debug = null where debug is not null and at < now() - make_interval(mins => $1::int)")
            .bind(DEBUG_MAX_MINUTES)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        n += sqlx::query("delete from conn_call where at < now() - make_interval(days => $1::int) and (key is null or outcome <> 'ok')")
            .bind(LOG_DAYS)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        n += sqlx::query("delete from conn_inbound where state = 'done' and received_at < now() - make_interval(days => $1::int)")
            .bind(LOG_DAYS)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        n += sqlx::query("delete from conn_signin where expires_at < now()").execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(n)
    }
}

// ── packs and packages ──

/// A connection as a pack or a package carries it: its description, never a secret or a token.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackConnection {
    pub name: String,
    pub label: String,
    pub description: Description,
}

/// The workspace's connections, for a pack.
pub(crate) async fn for_pack(tx: &mut Tx, tenant: Uuid) -> Result<Vec<PackConnection>> {
    let rows: Vec<(String, String, Value)> =
        sqlx::query_as("select name, label, description from conn where tenant_id = $1 order by name").bind(tenant).fetch_all(&mut **tx).await?;
    Ok(rows.into_iter().filter_map(|(name, label, d)| Some(PackConnection { name, label, description: serde_json::from_value(d).ok()? })).collect())
}

/// Installs a pack's connection, by name, turned off: one of the name here is left as it is.
pub(crate) async fn install(k: &Kernel, tx: &mut Tx, ctx: &Ctx, c: &PackConnection, source: &str) -> Result<&'static str> {
    admin_only(ctx)?;
    let here: bool =
        sqlx::query_scalar("select exists (select 1 from conn where tenant_id = $1 and name = $2)").bind(ctx.tenant).bind(&c.name).fetch_one(&mut **tx).await?;
    if here {
        return Ok("unchanged");
    }
    if !is_name(&c.name) || c.label.trim().is_empty() || c.label.chars().count() > 100 {
        return Err(Error::field("name", "a name of lower case letters, digits and _, and a label of at most 100 characters"));
    }
    if let Some(p) = k.description_problem(&ctx.schema, &c.description) {
        return Err(Error::field("description", p));
    }
    let id = Uuid::now_v7();
    sqlx::query("insert into conn (tenant_id, id, name, label, description, enabled, source) values ($1, $2, $3, $4, $5, false, $6)")
        .bind(ctx.tenant)
        .bind(id)
        .bind(&c.name)
        .bind(c.label.trim())
        .bind(serde_json::to_value(&c.description).map_err(Error::internal)?)
        .bind(source)
        .execute(&mut **tx)
        .await?;
    k.after_save(tx, ctx.tenant, id, &c.description).await?;
    Ok("created")
}

impl Kernel {
    /// The connections the workspace's packages describe: `(package, connection)`. One that does not read as a
    /// connection is left out (and said in the log).
    fn packaged_connections(&self, ctx: &Ctx) -> Vec<(String, PackConnection)> {
        let mut out = Vec::new();
        for name in &ctx.schema.packages {
            let Some(p) = self.registry.catalog().package(name).cloned() else { continue };
            for v in &p.manifest.connections {
                match serde_json::from_value::<PackConnection>(v.clone()) {
                    Ok(c) => out.push((name.clone(), c)),
                    Err(e) => tracing::warn!(package = %name, "a connection it describes does not read: {e}"),
                }
            }
        }
        out
    }
}

// ── the `call` effect, queued ──

/// Queues a call (the `call` effect with `queue`): made after the transaction commits, retried while the service
/// says "later", up to [`CALL_ATTEMPTS`].
pub(crate) async fn queue(tx: &mut Tx, ctx: &Ctx, c: &Call) -> Result<Uuid> {
    let mut job = NewJob::new(CALL_KIND, json!({"call": c, "user": ctx.user}));
    job.max_attempts = CALL_ATTEMPTS;
    crate::jobs::enqueue(tx, ctx.tenant, (!ctx.user.is_nil()).then_some(ctx.user), job).await
}

struct Queued;

#[async_trait]
impl JobHandler for Queued {
    fn kind(&self) -> &'static str {
        CALL_KIND
    }

    fn title(&self) -> &'static str {
        "A call to a connection"
    }

    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        let c: Call = serde_json::from_value(job.payload["call"].clone()).map_err(|e| Error::invalid(e.to_string()))?;
        let mut ctx = Ctx::system(job.tenant_id, k.registry.schema(job.tenant_id).await?);
        if let Some(u) = job.payload["user"].as_str().and_then(|u| u.parse().ok()) {
            ctx.user = u;
        }
        let result = k.call_connection(&ctx, None, &c).await?;
        Ok(json!({"result": result}))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(120)
    }
}

pub(crate) fn handler(kind: &str) -> Option<Arc<dyn JobHandler>> {
    (kind == CALL_KIND)
        .then(|| Arc::new(Queued) as Arc<dyn JobHandler>)
        .or_else(|| inbound::handler(kind))
        .or_else(|| pull::handler(kind))
        .or_else(|| oauth::handler(kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_blocks_what_is_not_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.100.100.200",
            "0.0.0.0",
            "224.0.0.1",
            "240.0.0.1",
            "198.18.0.1",
            "192.0.0.170",
            "::1",
            "fe80::1",
            "fd00:ec2::254",
            "::ffff:10.0.0.1",
            "::ffff:198.18.0.1",
            "64:ff9b::a9fe:a9fe",
            "::127.0.0.1",
            "ff02::1",
        ] {
            assert!(blocked(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["93.184.216.34", "1.1.1.1", "2606:4700:4700::1111", "64:ff9b::101:101"] {
            assert!(!blocked(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn paths_keep_their_shape() {
        let vars = json!({"inputs": {"id": "../admin?x=1#y", "n": 5}});
        assert_eq!(render_path("/orders/{{ inputs.id }}/{{ inputs.n }}", &vars).unwrap(), "/orders/..%2Fadmin%3Fx%3D1%23y/5");
        assert_eq!(pct("a b/é"), "a%20b%2F%C3%A9");
    }

    #[test]
    fn descriptions_are_checked() {
        let ok: Description = serde_json::from_value(json!({"base": "https://api.example.test/v1", "auth": {"type": "bearer", "secret": "token"},
            "operations": {"get_order": {"path": "/orders/{{ inputs.id }}", "result": {"total": "/total"}}}}))
        .unwrap();
        assert_eq!(ok.problem(), None);
        let bad = |v: Value| serde_json::from_value::<Description>(v).map(|d| d.problem()).unwrap_or_else(|e| Some(e.to_string()));
        assert!(bad(json!({"base": "ftp://x.test"})).is_some());
        assert!(bad(json!({"base": "https://u:p@x.test"})).is_some());
        assert!(bad(json!({"base": "https://x.test", "operations": {"a": {"path": "x"}}})).unwrap().contains("starts with /"));
        assert!(bad(json!({"base": "https://x.test", "operations": {"a": {"method": "TRACE"}}})).is_some());
        assert!(bad(json!({"base": "https://x.test", "operations": {"a": {"result": {"x": "id"}}}})).is_some());
        assert!(bad(json!({"base": "https://x.test", "operations": {"A": {}}})).is_some());
        assert!(bad(json!({"base": "https://x.test", "allowed_hosts": ["http://y.test"]})).is_some());
        assert!(bad(json!({"base": "https://x.test", "timeout_ms": 600000})).is_some());
        assert!(bad(json!({"base": "https://x.test", "auth": {"type": "header", "header": "bad header", "secret": "k"}})).is_some());
        assert!(bad(json!({"base": "https://x.test", "auth": {"type": "magic"}})).is_some());
        assert!(bad(json!({"base": "https://x.test", "operations": {"a": {"body": "{{ x", "method": "POST"}}})).is_some());
    }

    #[test]
    fn secrets_are_masked() {
        let v = masked(&json!({"e": "token s3cr3t-value refused", "n": [1, "s3cr3t-value"]}), &["s3cr3t-value".into(), "ab".into()]);
        assert_eq!(v, json!({"e": format!("token {HIDDEN} refused"), "n": [1, HIDDEN]}));
    }
}
