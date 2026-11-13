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
pub(crate) mod personal;
pub mod pull;
pub mod steps;

pub const CALL_KIND: &str = "platform.connection.call";
/// Attempts at a queued call while the service says "later".
const CALL_ATTEMPTS: i32 = 8;
const DEFAULT_TIMEOUT_MS: u64 = 15_000;
const MAX_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_MAX_BYTES: u64 = 1 << 20;
const MAX_BYTES: u64 = 10 << 20;
const MAX_REDIRECTS: usize = 3;
/// The longest a service's `Retry-After` makes a queued call or a pull wait.
pub(crate) const MAX_RETRY_AFTER: u64 = 3600;
/// A stored file a call sends or an answer gives: 10 MB unless the operation says, at most 25 MB; what one request
/// sends of files, at most 25 MB in all.
const DEFAULT_FILE_BYTES: u64 = 10 << 20;
pub(crate) const MAX_FILE_BYTES: u64 = 25 << 20;
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
#[serde(deny_unknown_fields, remote = "Self")]
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
    /// What its author keeps with it (an app's record of what the service can do): an object, kept and given back,
    /// never read by the platform; at most [`MAX_META`] bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    /// What each operation says besides [`Operation`]'s own fields, by operation (written on the operation itself).
    #[serde(skip)]
    pub more: BTreeMap<String, More>,
}

/// The keys of an operation that are [`More`]'s.
const MORE: [&str; 5] = ["steps", "url", "success", "credentials", "form_fields"];

/// Read as derived, each operation's [`More`] keys taken apart first.
impl<'de> Deserialize<'de> for Description {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        let mut v = Value::deserialize(d)?;
        let mut more = BTreeMap::new();
        if let Some(ops) = v.get_mut("operations").and_then(Value::as_object_mut) {
            for (name, op) in ops.iter_mut() {
                let Some(op) = op.as_object_mut() else { continue };
                let taken: Map<String, Value> = MORE.iter().filter_map(|k| Some((k.to_string(), op.remove(*k)?))).collect();
                if !taken.is_empty() {
                    let m: More = serde_json::from_value(Value::Object(taken)).map_err(|e| D::Error::custom(format!("operation {name}: {e}")))?;
                    more.insert(name.clone(), m);
                }
            }
        }
        let mut desc = Description::deserialize(v).map_err(D::Error::custom)?;
        desc.more = more;
        Ok(desc)
    }
}

/// Written as derived, each operation's [`More`] written back on it.
impl Serialize for Description {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::Error as _;
        let mut v = Description::serialize(self, serde_json::value::Serializer).map_err(S::Error::custom)?;
        for (name, m) in &self.more {
            if let (Some(op), Value::Object(extra)) =
                (v.pointer_mut(&format!("/operations/{}", name.replace('~', "~0").replace('/', "~1"))), serde_json::to_value(m).map_err(S::Error::custom)?)
                && let Some(op) = op.as_object_mut()
            {
                op.extend(extra);
            }
        }
        v.serialize(s)
    }
}

/// What an operation says besides [`Operation`]'s fields (BD-53 onwards). Kept apart so that an app that builds an
/// `Operation` in code is not changed by them; a description writes them on the operation like any other key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct More {
    /// Several requests in order, each another operation (BD-53); the operation then sends nothing itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<steps::Step>,
    /// A template giving the whole address, in place of the base and `path` (one an earlier answer gave): the guard
    /// holds it to the connection's hosts like any other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Statuses besides 2xx that mean it worked (`308` for an upload still going on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<Vec<u16>>,
    /// `false`: sent without the connection's credentials (an address that is signed already).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<bool>,
    /// A template giving more form fields, as JSON (an object, or a list of `{name, value}`), sent before `form`'s in
    /// their order (a signed upload form an earlier answer gave).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form_fields: Option<String>,
}

static NO_MORE: More = More { steps: Vec::new(), url: None, success: None, credentials: None, form_fields: None };

impl More {
    fn problem(&self, op: &Operation) -> Option<String> {
        if !op.path.is_empty() && self.url.is_some() {
            return Some("a request has a path or a url, not both".into());
        }
        if self.success.as_ref().is_some_and(|s| s.iter().any(|c| !(100..=599).contains(c))) {
            return Some("success lists statuses, 100 to 599".into());
        }
        self.url.iter().chain(self.form_fields.iter()).find_map(|t| template_problem(t))
    }
}

/// The most a description's `meta` holds, as JSON.
const MAX_META: usize = 16 << 10;

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
    /// A session the connection signs in to itself (BD-48): its `login` operation exchanges its secrets for a token
    /// (at `token_at` in the answer; a refresh token at `refresh_at`, the expiry at `expires_at`: seconds from now, a
    /// Unix time or a moment), its `refresh` operation (given `inputs.refresh_token`) renews it, else it logs in again.
    /// The token goes in `header` (`authorization`) after `prefix` (`Bearer `). The workspace's own account, sealed and
    /// refreshed as the broker's tokens are; a login or refresh the service refuses marks it "needs reconnecting".
    Session {
        login: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh: Option<String>,
        token_at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh_at: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        header: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
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
    /// A template giving JSON (`| json` writes a value as JSON), or a [`FilePart`] written as an object: the stored
    /// file is the body, with its type.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "specs::one")]
    pub body: Option<String>,
    /// Templates, sent as a form; with a [`FilePart`] written as an object among them (or a `content-type:
    /// multipart/form-data` header), as `multipart/form-data`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", with = "specs")]
    pub form: BTreeMap<String, String>,
    /// Result name → where it is in the answer: a JSON pointer, or a [`PickSpec`] written as an object (kept as
    /// [`spec`] text); none: the whole answer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", with = "specs")]
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

    /// Whether its form goes as `multipart/form-data`: it sends a file, or its headers say so.
    fn multipart(&self) -> bool {
        self.form.values().any(|v| v.starts_with(SPEC))
            || self.headers.iter().any(|(h, v)| h.eq_ignore_ascii_case("content-type") && v.trim().to_ascii_lowercase().starts_with("multipart/form-data"))
    }

    /// The most its answer may be when the answer itself is a file it stores.
    fn answer_cap(&self) -> Option<u64> {
        self.result
            .values()
            .filter_map(|r| match Pick::of(r) {
                Ok(Pick::Spec(PickSpec { file: Some(f), .. })) if f.from.is_none() => Some(f.max_bytes.unwrap_or(DEFAULT_FILE_BYTES)),
                _ => None,
            })
            .max()
    }
}

/// Marks a value of an operation's `result` (and `form` and `body`) that a description writes as an object: it is kept
/// as this character and the object's JSON, so that those fields stay strings for apps that build operations in code
/// (`connections::spec(&json!({"header": "Location"}))`), and written back as the object.
pub const SPEC: char = '\u{0}';

/// An object for a field that takes a string or an object (see [`SPEC`]).
pub fn spec(v: &Value) -> String {
    format!("{SPEC}{v}")
}

/// The object a value written as one is, as `T`; `None` for a plain string.
pub(crate) fn spec_of<T: serde::de::DeserializeOwned>(s: &str) -> Option<std::result::Result<T, String>> {
    s.strip_prefix(SPEC).map(|j| serde_json::from_str(j).map_err(|e| e.to_string()))
}

/// Strings, or objects kept as [`spec`] text, in a map.
mod specs {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::Value;
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(m: &BTreeMap<String, String>, s: S) -> Result<S::Ok, S::Error> {
        m.iter().map(|(k, v)| (k, out(v))).collect::<BTreeMap<_, _>>().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, String>, D::Error> {
        BTreeMap::<String, Value>::deserialize(d)?.into_iter().map(|(k, v)| Ok((k, inward::<D::Error>(v)?))).collect()
    }

    pub(super) fn out(v: &str) -> Value {
        match v.strip_prefix(super::SPEC).and_then(|j| serde_json::from_str::<Value>(j).ok()) {
            Some(o) => o,
            None => Value::String(v.to_string()),
        }
    }

    pub(super) fn inward<E: serde::de::Error>(v: Value) -> Result<String, E> {
        match v {
            Value::String(s) => Ok(s),
            o @ Value::Object(_) => Ok(super::spec(&o)),
            _ => Err(E::custom("a string or an object")),
        }
    }

    /// The same for one optional value.
    pub mod one {
        use serde::{Deserialize, Deserializer, Serialize, Serializer};
        use serde_json::Value;

        pub fn serialize<S: Serializer>(v: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
            v.as_deref().map(super::out).serialize(s)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
            Option::<Value>::deserialize(d)?.map(super::inward::<D::Error>).transpose()
        }
    }
}

/// Where a value is read: in an answer (an operation's `result`, a pull's `next_at`) or in an item (a mapping's
/// values). A bare string is a JSON pointer (`/a/0/b`), whose key not there reads as null; in a mapping, a string that
/// is not a pointer is an expression, as before.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Pick {
    At(String),
    Spec(PickSpec),
}

/// A [`Pick`] said in full: one of `at`, `header`, `link` or `expr`, and whether it must be there.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PickSpec {
    /// A JSON pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// A response header (answers only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// The address of the answer's `Link` header entry with this `rel` (`next`), made absolute (answers only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// An expression (the platform's): over `answer`, `headers`, `inputs` and `connection` in an answer; over
    /// `payload`, `item` and `connection` in a mapping. `connection` is `{name, id}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
    /// The answer is a file to store, or names the address of one (answers only); the value is the stored file's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileResult>,
    /// Null here fails the call (an answer) or the item (a mapping), saying which.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

/// A stored file a request sends: a form's part, or the whole body. The caller must be able to read it (a queued
/// call reads as the system).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePart {
    /// A template giving the file's id.
    pub file: String,
    /// A template giving the name it is sent under; else its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Else its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// The largest it may be (10 MB, at most 25 MB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

impl FilePart {
    fn problem(&self) -> Option<String> {
        if self.max_bytes.is_some_and(|b| b == 0 || b > MAX_FILE_BYTES) {
            return Some(format!("a file's max_bytes is 1 to {MAX_FILE_BYTES}"));
        }
        if self.content_type.as_deref().is_some_and(|c| c.parse::<reqwest::header::HeaderValue>().is_err() || !c.contains('/')) {
            return Some("a file's content_type is a media type".into());
        }
        std::iter::once(&self.file).chain(self.name.iter()).find_map(|t| template_problem(t))
    }
}

/// A file an answer gives, stored as the platform's other files are (scanned, counted against the workspace's storage)
/// and attached to the record the call is about: the answer itself, or the address `from` points at, fetched through
/// the guard within the connection's hosts, without its credentials.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileResult {
    /// A JSON pointer to the file's address in the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// A template over `inputs` and `answer` giving its name; else the address's last part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The largest it may be (10 MB, at most 25 MB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

impl Pick {
    /// An operation's `result` value: a pointer, or a spec written as an object.
    pub(crate) fn of(s: &str) -> std::result::Result<Pick, String> {
        match spec_of::<PickSpec>(s) {
            Some(p) => p.map(Pick::Spec),
            None => Ok(Pick::At(s.to_string())),
        }
    }

    /// It said in full; a bare string is a pointer, or (`exprs`, a mapping) an expression when it is not one.
    pub(crate) fn spec(&self, exprs: bool) -> PickSpec {
        match self {
            Pick::At(s) if exprs && !s.starts_with('/') => PickSpec { expr: Some(s.clone()), ..Default::default() },
            Pick::At(s) => PickSpec { at: Some(s.clone()), ..Default::default() },
            Pick::Spec(p) => p.clone(),
        }
    }

    /// What is wrong with it; `answer`: it reads an answer (headers and links), else an item.
    pub(crate) fn problem(&self, answer: bool) -> Option<String> {
        let p = self.spec(!answer);
        let said = [p.at.is_some(), p.header.is_some(), p.link.is_some(), p.expr.is_some(), p.file.is_some()].iter().filter(|b| **b).count();
        if said != 1 {
            return Some("a value is read from one of at, header, link, expr or file".into());
        }
        if !answer && (p.header.is_some() || p.link.is_some() || p.file.is_some()) {
            return Some("a mapping reads the item: at or expr".into());
        }
        if let Some(f) = &p.file {
            if f.max_bytes.is_some_and(|b| b == 0 || b > MAX_FILE_BYTES) {
                return Some(format!("a file's max_bytes is 1 to {MAX_FILE_BYTES}"));
            }
            return f.from.as_deref().and_then(pointer_problem).or_else(|| f.name.as_deref().and_then(template_problem));
        }
        if let Some(l) = &p.link
            && (l.is_empty() || !l.chars().all(|c| c.is_ascii_alphanumeric() || "-._".contains(c)))
        {
            return Some(format!("link: {l} is not a relation name"));
        }
        p.at.as_deref()
            .and_then(pointer_problem)
            .or_else(|| p.header.as_deref().and_then(header_problem))
            .or_else(|| p.expr.as_deref().and_then(|e| ferrum_meta::Expr::compile(e).err().map(|m| format!("'{e}': {m}"))))
    }
}

/// The target of the `Link` header entry (RFC 8288) whose `rel` holds `rel`, made absolute against `base`.
pub(crate) fn link_target(header: &str, rel: &str, base: &reqwest::Url) -> Option<String> {
    let mut rest = header;
    while let Some(open) = rest.find('<') {
        let close = open + rest[open..].find('>')?;
        let target = &rest[open + 1..close];
        let after = &rest[close + 1..];
        let params = &after[..after.find('<').unwrap_or(after.len())];
        let rels = params.split(';').filter_map(|p| p.trim().strip_prefix("rel=")).map(|v| v.trim_matches(['"', ',', ' ']));
        if rels.flat_map(|v| v.split_whitespace()).any(|r| r.eq_ignore_ascii_case(rel)) {
            return base.join(target).ok().map(String::from);
        }
        rest = after;
    }
    None
}

/// How long a `Retry-After` value (seconds, or an HTTP date) asks to wait, at least a second and at most
/// [`MAX_RETRY_AFTER`].
pub(crate) fn retry_after(v: &str, now: DateTime<Utc>) -> Option<u64> {
    let v = v.trim();
    let secs = match v.parse::<u64>() {
        Ok(n) => n,
        Err(_) => (DateTime::parse_from_rfc2822(v).ok()?.to_utc() - now).num_seconds().max(0) as u64,
    };
    Some(secs.clamp(1, MAX_RETRY_AFTER))
}

/// What is wrong with a template, if anything.
fn template_problem(t: &str) -> Option<String> {
    environment().template_from_str(t).err().map(|e| format!("'{t}': {e}"))
}

/// The template engine of connections: the print formats' (fuel-limited), printing values as they are.
pub(crate) fn environment() -> minijinja::Environment<'static> {
    let mut env = crate::print::environment();
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
    env.add_filter("json", |v: minijinja::Value| serde_json::to_string(&v).unwrap_or_default());
    env
}

pub(crate) fn pointer_problem(p: &str) -> Option<String> {
    (!p.is_empty() && !p.starts_with('/')).then(|| format!("{p} is not a JSON pointer (/a/0/b)"))
}

pub(crate) fn header_problem(h: &str) -> Option<String> {
    reqwest::header::HeaderName::from_bytes(h.as_bytes()).is_err().then(|| format!("{h} is not a header name"))
}

impl Description {
    /// What operation `name` says besides its [`Operation`] fields.
    pub(crate) fn more(&self, name: &str) -> &More {
        self.more.get(name).unwrap_or(&NO_MORE)
    }

    /// Whether operation `name` changes something at the service (then it needs a key): unless it says, every method
    /// but GET, and every operation of steps.
    pub(crate) fn changes(&self, name: &str) -> bool {
        self.operations.get(name).is_some_and(|op| op.changes.unwrap_or(!self.more(name).steps.is_empty() || op.method() != "GET"))
    }

    /// Whether operation `name` is how the connection signs in (a session's login or refresh): never called as an
    /// operation, by an app, a person, an effect, a pull or "try this", since its answer is a token.
    pub(crate) fn signs_in_with(&self, name: &str) -> bool {
        matches!(&self.auth, Auth::Session { login, refresh, .. } if login == name || refresh.as_deref() == Some(name))
    }

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
        if self.meta.as_ref().is_some_and(|m| !m.is_object() || m.to_string().len() > MAX_META) {
            return Some(format!("meta is an object of at most {MAX_META} bytes"));
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
            Auth::Session { login, refresh, token_at, refresh_at, expires_at, header, .. } => {
                if let Some(op) = std::iter::once(login).chain(refresh.iter()).find(|o| !self.operations.contains_key(*o)) {
                    return Some(format!("auth: there is no operation {op}"));
                }
                if let Some(p) = std::iter::once(token_at)
                    .chain(refresh_at.iter())
                    .chain(expires_at.iter())
                    .find_map(|p| pointer_problem(p).or_else(|| p.is_empty().then(|| "auth: a pointer is not empty".to_string())))
                {
                    return Some(p);
                }
                if let Some(p) = header.as_deref().and_then(header_problem) {
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
            let more = self.more(name);
            if let Some(p) = op.problem().or_else(|| more.problem(op)).or_else(|| (!more.steps.is_empty()).then(|| steps::problem(self, op, more)).flatten()) {
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
        let plain = |v: &&String| !v.starts_with(SPEC);
        let templates = std::iter::once(&self.path)
            .chain(self.query.values())
            .chain(self.headers.values())
            .chain(self.body.iter().filter(plain))
            .chain(self.form.values().filter(plain));
        if let Some(p) = templates.filter_map(|t| template_problem(t)).next() {
            return Some(p);
        }
        for (name, v) in self.body.iter().map(|b| ("body", b)).chain(self.form.iter().map(|(k, v)| (k.as_str(), v))) {
            if let Some(p) = spec_of::<FilePart>(v).and_then(|f| f.map_or_else(Some, |f| f.problem())) {
                return Some(format!("{name}: {p}"));
            }
        }
        if let Some(p) = self.headers.keys().chain(self.idempotency_header.iter()).find_map(|h| header_problem(h)) {
            return Some(p);
        }
        if let Some(p) = self.error_at.as_deref().and_then(pointer_problem) {
            return Some(p);
        }
        if let Some((n, p)) = self.result.iter().find_map(|(n, p)| Some((n, Pick::of(p).map_or_else(Some, |p| p.problem(true))?))) {
            return Some(format!("result {n}: {p}"));
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
    /// The bytes and their content type.
    pub body: Option<(Vec<u8>, String)>,
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
    /// Its headers, names in lower case.
    pub headers: Vec<(String, String)>,
    /// Where it came from, after any redirect.
    pub url: reqwest::Url,
    /// The bytes as they came.
    pub raw: Vec<u8>,
}

impl Answer {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(h, _)| h.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// When the service asked to be called again, for a temporary answer.
    fn wait(&self) -> Option<u64> {
        retry_after(self.header("retry-after")?, Utc::now())
    }

    /// What `p` reads here, `vars` bound for an expression.
    pub(crate) fn read(&self, p: &PickSpec, vars: &[(&str, &Value)]) -> Result<Value> {
        Ok(if let Some(at) = &p.at {
            self.body.pointer(at).cloned().unwrap_or(Value::Null)
        } else if let Some(h) = &p.header {
            self.header(h).map_or(Value::Null, |v| Value::String(v.to_string()))
        } else if let Some(rel) = &p.link {
            self.header("link").and_then(|l| link_target(l, rel, &self.url)).map_or(Value::Null, Value::String)
        } else if let Some(e) = &p.expr {
            crate::automation::eval_with(e, &Value::Null, &Value::Null, vars)?
        } else {
            Value::Null
        })
    }
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
                rb = rb.header(reqwest::header::CONTENT_TYPE, kind.as_str()).body(body.clone());
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
            let headers = res.headers().iter().filter_map(|(h, v)| Some((h.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
            return Ok(Answer { status, body, headers, url: req.url, raw: buf });
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
    environment().render_str(src, minijinja::Value::from_serialize(vars)).map_err(|e| Error::invalid(format!("a template failed: {e}")))
}

/// A path template: each value it prints percent-encoded, so an input cannot add a segment, a query or a host.
fn render_path(src: &str, vars: &Value) -> Result<String> {
    let mut env = environment();
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
pub(crate) fn digest(v: &Value) -> String {
    hex::encode(Sha256::digest(v.to_string().as_bytes()))
}

// ── connections as stored ──

/// A connection's secrets, open, its row locked for the rest of `tx` (to change them).
pub(crate) async fn lock_secrets(tx: &mut Tx, k: &Kernel, tenant: Uuid, id: Uuid) -> Result<BTreeMap<String, String>> {
    let sealed: Option<Option<String>> =
        sqlx::query_scalar("select sealed from conn where tenant_id = $1 and id = $2 for update").bind(tenant).bind(id).fetch_optional(&mut **tx).await?;
    match sealed.ok_or_else(|| Error::NotFound("connection".into()))? {
        Some(s) => serde_json::from_str(&k.open_sealed(&sealed_label(tenant, id), &s)?).map_err(Error::internal),
        None => Ok(BTreeMap::new()),
    }
}

/// Seals `held` with `set` set and `remove` removed as the connection's secrets, in `tx` (after [`lock_secrets`]).
pub(crate) async fn store_secrets(
    tx: &mut Tx,
    k: &Kernel,
    tenant: Uuid,
    id: Uuid,
    mut held: BTreeMap<String, String>,
    set: BTreeMap<String, String>,
    remove: &[String],
) -> Result<BTreeMap<String, String>> {
    held.extend(set);
    held.retain(|n, _| !remove.contains(n));
    let key = k.settings.secret_key.as_ref().ok_or_else(|| Error::Unavailable("connection secrets need the server's secret key (FERRUM_SECRET_KEY)".into()))?;
    let sealed = key.seal(&sealed_label(tenant, id), &serde_json::to_string(&held).map_err(Error::internal)?)?;
    sqlx::query("update conn set sealed = $3, updated_at = now() where tenant_id = $1 and id = $2")
        .bind(tenant)
        .bind(id)
        .bind(sealed)
        .execute(&mut **tx)
        .await?;
    Ok(held)
}

/// Whether a secret is one the platform keeps for itself ([`oauth::REGISTERED_ID`], the DPoP key): never set, listed or
/// dropped by an administrator's save.
fn kept_by_platform(name: &str) -> bool {
    name.starts_with('_')
}

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
    Ok(Value, Box<Answer>),
    /// The service said "later", or did not answer; when it said when, in seconds.
    Later(String, Option<u64>),
    /// The service said no, or the call could not be made.
    No(String),
}

/// A page of a pull: its cursor, as the templates see it, and the address the last page's `Link` named, which is
/// asked in place of the operation's path and query.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Page<'a> {
    pub cursor: &'a Value,
    pub address: Option<&'a str>,
}

/// What a request is made with besides its inputs: a pull's page, the clock its templates read.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Extra<'a> {
    pub page: Option<Page<'a>>,
    /// The moment fixed at the first attempt of the call's key; else now.
    pub now: Option<DateTime<Utc>>,
    /// The part of a stored file its file part sends: the file, the first byte, how many.
    pub range: Option<(Uuid, u64, u64)>,
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
        self.call_waiting(ctx, app, c, false).await
    }

    /// [`Kernel::call_connection`]; `waits`: a "later" with the service's `Retry-After` is `RetryAfter` (a queued call's
    /// job then comes back when it said) rather than `Unavailable`.
    async fn call_waiting(&self, ctx: &Ctx, app: Option<&str>, c: &Call, waits: bool) -> Result<Value> {
        let conn = self.load_connection(ctx.tenant, &c.connection).await?;
        if !conn.enabled {
            return Err(Error::invalid(format!("the connection {} is turned off", conn.name)));
        }
        may_use(ctx, app, &conn.desc)?;
        let op = conn.desc.operations.get(&c.operation).ok_or_else(|| Error::invalid(format!("{} has no operation {}", conn.name, c.operation)))?;
        if conn.desc.signs_in_with(&c.operation) {
            return Err(Error::invalid(format!("{} is how {} signs in: it is not called", c.operation, conn.name)));
        }
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
        let (changes, more) = (conn.desc.changes(&c.operation), conn.desc.more(&c.operation));
        if changes && key.is_none() {
            return Err(Error::invalid(format!("{} changes something at the service, so it needs a key", c.operation)));
        }
        let given = digest(&inputs);
        if let Some(key) = key.filter(|_| changes)
            && let Some((was, result)) = self.done_before(&conn, &c.operation, key).await?
        {
            return if was == given {
                Ok(result)
            } else {
                Err(Error::Conflict(format!("the key {key} was used for {} with other inputs: a new request needs a new key", c.operation)))
            };
        }
        // One worker makes a key at a time; its clock is fixed at its first attempt.
        let (mut held, mut clock) = (None, Utc::now());
        if let Some(key) = key.filter(|_| changes) {
            held = Some(self.hold_key(&conn, &c.operation, key).await?.ok_or_else(|| Error::RetryAfter(format!("the call {key} is being made"), 5))?);
            if let Some((was, result)) = self.done_before(&conn, &c.operation, key).await? {
                return if was == given { Ok(result) } else { Err(Error::Conflict(format!("the key {key} was used for {} with other inputs", c.operation))) };
            }
            let a = self.key_attempt(&conn, &c.operation, key, &given).await?;
            if !more.steps.is_empty() && a.digest != given {
                return Err(Error::Conflict(format!("the key {key} was used for {} with other inputs: a new request needs a new key", c.operation)));
            }
            clock = a.clock;
        }
        let about = c.entity.clone().zip(c.record);
        let about = about.as_ref().map(|(e, r)| (e.as_str(), *r));
        let outcome = if more.steps.is_empty() {
            let extra = Extra { now: Some(clock), ..Default::default() };
            self.perform(ctx, &conn, &c.operation, &inputs, key, extra, about, changes.then_some(given.as_str())).await?
        } else {
            let o = Box::pin(self.perform_steps(ctx, &conn, &c.operation, &inputs, key, clock, about)).await?;
            if let (Outcome::Ok(result, _), Some(key)) = (&o, key) {
                self.remember(ctx, &conn, &c.operation, key, &given, result).await?;
            }
            o
        };
        if let Some(tx) = held {
            tx.commit().await?;
        }
        match outcome {
            Outcome::Ok(result, _) => Ok(result),
            Outcome::Later(why, Some(secs)) if waits => Err(Error::RetryAfter(why, secs)),
            Outcome::Later(why, _) => Err(Error::Unavailable(why)),
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

    /// One operation, made and logged. `extra`: a pull's page (its `cursor` a template variable), the clock.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn perform(
        &self,
        ctx: &Ctx,
        conn: &Loaded,
        name: &str,
        inputs: &Value,
        key: Option<&str>,
        extra: Extra<'_>,
        about: Option<(&str, Uuid)>,
        keep: Option<&str>,
    ) -> Result<Outcome> {
        // Boxed: an app's future that awaits a call stays within the compiler's depth for layouts.
        Ok(Box::pin(self.perform_full(ctx, conn, name, inputs, key, extra, about, keep)).await?.0)
    }

    /// [`Kernel::perform`], and the status the service answered and whether a request went out at all.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn perform_full(
        &self,
        ctx: &Ctx,
        conn: &Loaded,
        name: &str,
        inputs: &Value,
        key: Option<&str>,
        extra: Extra<'_>,
        about: Option<(&str, Uuid)>,
        keep: Option<&str>,
    ) -> Result<(Outcome, Option<u16>, bool)> {
        let op = conn.desc.operations.get(name).ok_or_else(|| Error::invalid(format!("{} has no operation {name}", conn.name)))?;
        let more = conn.desc.more(name);
        if !more.steps.is_empty() {
            return Err(Error::invalid(format!("{name} is made of steps: it is called by itself, not inside another")));
        }
        let mut sent = conn.desc.test;
        // Signing in: made without credentials of its own, never captured (its answer is a token), never refreshed.
        let bare = conn.desc.signs_in_with(name);
        let started = Instant::now();
        let mut mask = conn.mask();
        let mut debug = None;
        let answered = if conn.desc.test {
            let f = op.fixture.clone().unwrap_or_else(|| json!({"status": 200, "body": {}}));
            let headers =
                f["headers"].as_object().map(|h| h.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.as_str().unwrap_or_default().to_string())).collect());
            Ok(Answer {
                status: f["status"].as_u64().unwrap_or(200) as u16,
                body: f.get("body").cloned().unwrap_or(Value::Null),
                headers: headers.unwrap_or_default(),
                url: reqwest::Url::parse("https://fixture.invalid/").map_err(Error::internal)?,
                raw: match f.get("body") {
                    Some(Value::String(s)) => s.clone().into_bytes(),
                    Some(v) => v.to_string().into_bytes(),
                    None => vec![],
                },
            })
        } else {
            let mut tries = 0;
            let got = loop {
                tries += 1;
                let got = match self.build_request(ctx, conn, (op, more), inputs, key, extra, bare).await {
                    Ok((req, more)) => {
                        sent = true;
                        mask.extend(more);
                        if conn.debug && !bare {
                            debug = Some(json!({"request": captured(&req, &mask)}));
                        }
                        let mut reach = conn.reach();
                        if let Some(cap) = op.answer_cap() {
                            reach.max_bytes = cap;
                        }
                        let got = self.fetch(req, &reach).await;
                        if let Ok(a) = &got {
                            oauth::remember_nonce(conn.id, a);
                        }
                        if let (Some(d), Ok(a)) = (debug.as_mut(), &got) {
                            d["answer"] = json!({"status": a.status, "body": clipped(&masked(&a.body, &mask))});
                        }
                        got
                    }
                    Err(e) => Err(e),
                };
                // A server binding tokens that asks for its nonce: the call made again with it (RFC 9449 §9).
                if tries == 1
                    && matches!(&conn.desc.auth, Auth::Oauth { provider, .. } if provider.dpop)
                    && matches!(&got, Ok(a) if a.status == 401 && a.header("www-authenticate").is_some_and(|w| w.contains("use_dpop_nonce")))
                {
                    continue;
                }
                // A token the service no longer takes: refreshed once, and the call made again.
                if tries == 1
                    && !bare
                    && matches!(&got, Ok(a) if a.status == 401)
                    && matches!(conn.desc.auth, Auth::Oauth { .. } | Auth::Session { .. })
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
        let worked = |s: u16| (200..300).contains(&s) || more.success.as_ref().is_some_and(|ok| ok.contains(&s));
        let (outcome, status, row_outcome) = match answered {
            Ok(a) if worked(a.status) => {
                let status = a.status;
                match self.mapped(ctx, conn, op, inputs, &a, about).await {
                    Ok(result) => (Outcome::Ok(masked(&result, &mask), Box::new(a)), Some(status), "ok"),
                    Err(Error::Unavailable(m)) => (Outcome::Later(masked_text(&m, &mask), None), Some(status), "temporary"),
                    Err(e) => (Outcome::No(masked_text(&format!("{status}: {e}"), &mask)), Some(status), "failed"),
                }
            }
            Ok(a) if temporary.contains(&a.status) => (Outcome::Later(error_of(&a), a.wait()), Some(a.status), "temporary"),
            Ok(a) => (Outcome::No(error_of(&a)), Some(a.status), "failed"),
            Err(Error::Unavailable(m)) => (Outcome::Later(m, None), None, "temporary"),
            Err(e) => (Outcome::No(e.to_string()), None, "refused"),
        };
        let error = match &outcome {
            Outcome::Later(m, _) | Outcome::No(m) => Some(m.clone()),
            Outcome::Ok(..) => None,
        };
        let kept = match &outcome {
            Outcome::Ok(r, _) if keep.is_some() => Some(r.clone()),
            _ => None,
        };
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        sqlx::query(
            "insert into conn_call (tenant_id, id, conn_id, operation, outcome, status, duration_ms, entity, record_id, error, key, input_digest, result, debug, user_id) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) on conflict do nothing",
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
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((outcome, status, sent))
    }

    /// The mapped result of an answer that worked: each of the operation's `result` read as its [`Pick`] says (none: the
    /// whole answer); one `required` and not there fails the call, naming it.
    async fn mapped(&self, ctx: &Ctx, conn: &Loaded, op: &Operation, inputs: &Value, a: &Answer, about: Option<(&str, Uuid)>) -> Result<Value> {
        if op.result.is_empty() {
            return Ok(a.body.clone());
        }
        let headers = Value::Object(a.headers.iter().map(|(h, v)| (h.clone(), Value::String(v.clone()))).collect());
        let connection = json!({"name": conn.name, "id": conn.id});
        let vars = [("answer", &a.body), ("headers", &headers), ("inputs", inputs), ("connection", &connection)];
        let mut out = Map::new();
        for (name, pick) in &op.result {
            let p = Pick::of(pick).map_err(Error::invalid)?.spec(false);
            let v = match &p.file {
                Some(f) => self.answer_file(ctx, conn, f, inputs, a, about).await?,
                None => a.read(&p, &vars)?,
            };
            if p.required && v.is_null() {
                return Err(Error::invalid(format!("the answer has no {name}")));
            }
            out.insert(name.clone(), v);
        }
        Ok(Value::Object(out))
    }

    /// A file an answer gives ([`FileResult`]), stored and attached to the record the call is about: its id. A test
    /// connection fetches nothing (null).
    async fn answer_file(&self, ctx: &Ctx, conn: &Loaded, f: &FileResult, inputs: &Value, a: &Answer, about: Option<(&str, Uuid)>) -> Result<Value> {
        let cap = f.max_bytes.unwrap_or(DEFAULT_FILE_BYTES);
        let got;
        let (bytes, kind, url) = match &f.from {
            Some(_) if conn.desc.test => return Ok(Value::Null),
            Some(p) => {
                let address = at(&a.body, Some(p)).ok_or_else(|| Error::invalid(format!("the answer gives no file address at {p}")))?;
                let url = a.url.join(&address).map_err(|_| Error::invalid("the file's address is not an address"))?;
                let reach = Reach { max_bytes: cap, ..conn.reach() };
                got = self.fetch(Request { method: reqwest::Method::GET, url, headers: vec![], body: None }, &reach).await?;
                if !(200..300).contains(&got.status) {
                    let said = format!("the file's address answered {}", got.status);
                    return Err(if TEMPORARY.contains(&got.status) { Error::Unavailable(said) } else { Error::invalid(said) });
                }
                (&got.raw, got.header("content-type"), &got.url)
            }
            None => (&a.raw, a.header("content-type"), &a.url),
        };
        if bytes.len() as u64 > cap {
            return Err(Error::invalid(format!("the file is larger than {cap} bytes")));
        }
        let name = match &f.name {
            Some(t) => render(t, &json!({"inputs": inputs, "answer": a.body}))?,
            None => url.path_segments().and_then(|mut s| s.next_back()).filter(|s| !s.is_empty()).unwrap_or("file").to_string(),
        };
        let kind = kind.unwrap_or("application/octet-stream").to_string();
        let stored = self.store_file(ctx, about, &name, &kind, bytes).await?;
        Ok(stored["id"].clone())
    }

    /// A stored file a request sends, as the caller reads it: its name, type and bytes, within `budget` (what the
    /// request may still send) and the part's own limit.
    async fn sent_file(
        &self,
        ctx: &Ctx,
        part: &FilePart,
        vars: &Value,
        budget: &mut u64,
        range: Option<(Uuid, u64, u64)>,
    ) -> Result<(String, String, Vec<u8>)> {
        let given = render(&part.file, vars)?;
        let id: Uuid = given.trim().parse().map_err(|_| Error::invalid(format!("{given} is not a stored file")))?;
        let info = self.file_info(ctx, id).await.map_err(|_| Error::invalid(format!("the file {id} is not one you may send")))?;
        let cap = part.max_bytes.unwrap_or(DEFAULT_FILE_BYTES).min(*budget);
        // A chunk of it (a step's range): only that range is read, whatever the file's size.
        let range = range.filter(|r| r.0 == id);
        let size = range.map_or(info.size, |r| r.2 as i64);
        if size < 0 || size as u64 > cap {
            return Err(Error::invalid(format!("{} is larger than the {cap} bytes this call may send", info.name)));
        }
        let bytes = match range {
            Some((_, offset, len)) if offset.saturating_add(len) <= info.size as u64 => self.file_store()?.get_range(&info.key, offset..offset + len).await?,
            Some(_) => return Err(Error::invalid(format!("the range is beyond the end of {}", info.name))),
            None => self.file_store()?.get(&info.key).await?,
        };
        if bytes.len() as u64 > cap {
            return Err(Error::invalid(format!("{} is larger than the {cap} bytes this call may send", info.name)));
        }
        *budget -= bytes.len() as u64;
        let name = match &part.name {
            Some(t) => render(t, vars)?,
            None => info.name,
        };
        Ok((name, part.content_type.clone().unwrap_or(info.content_type), bytes))
    }

    /// The request an operation makes, and what more it must never show (a token).
    async fn build_request(
        &self,
        ctx: &Ctx,
        conn: &Loaded,
        (op, more): (&Operation, &More),
        inputs: &Value,
        key: Option<&str>,
        extra: Extra<'_>,
        bare: bool,
    ) -> Result<(Request, Vec<String>)> {
        let page = extra.page;
        let now = extra.now.unwrap_or_else(Utc::now);
        let mut vars = json!({"inputs": inputs, "secret": conn.secrets, "key": key, "now": now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                              "today": now.date_naive().to_string()});
        if let Some(p) = page {
            vars["cursor"] = p.cursor.clone();
        }
        let r = |s: &str| render(s, &vars);
        // A page the last one's `Link` named is asked at that address as it is (the guard still checks its host).
        if let Some(address) = page.and_then(|p| p.address) {
            let url = reqwest::Url::parse(address).map_err(|_| Error::invalid("the next page's address is not an address"))?;
            let mut headers = Vec::new();
            for (h, v) in conn.desc.headers.iter().chain(&op.headers) {
                headers.push((h.clone(), r(v)?));
            }
            let more = self.authenticate(ctx, conn, &mut headers, None, &reqwest::Method::GET, &url).await?;
            return Ok((Request { method: reqwest::Method::GET, url, headers, body: None }, more));
        }
        // A whole address (one an earlier answer gave) is taken as it is: the guard holds it to the connection's hosts.
        let address = match &more.url {
            Some(t) => r(t)?.trim().to_string(),
            None => format!("{}{}", conn.desc.base.trim_end_matches('/'), render_path(&op.path, &vars)?),
        };
        let mut url = reqwest::Url::parse(&address).map_err(|_| Error::invalid("the operation's address is not an address"))?;
        if !op.query.is_empty() {
            let pairs = op.query.iter().map(|(k, v)| Ok((k.clone(), r(v)?))).collect::<Result<Vec<_>>>()?;
            url.query_pairs_mut().extend_pairs(pairs);
        }
        let mut headers = Vec::new();
        let multipart = op.multipart();
        for (h, v) in conn.desc.headers.iter().chain(&op.headers) {
            // A file's body and a multipart form say their own type (and boundary).
            if !(h.eq_ignore_ascii_case("content-type") && (multipart || op.body.as_deref().is_some_and(|b| b.starts_with(SPEC)))) {
                headers.push((h.clone(), r(v)?));
            }
        }
        if let (Some(h), Some(k)) = (&op.idempotency_header, key) {
            headers.push((h.clone(), k.to_string()));
        }
        let mut budget = MAX_FILE_BYTES;
        let body = match &op.body {
            Some(b) => match spec_of::<FilePart>(b) {
                Some(part) => {
                    let (_, kind, bytes) = self.sent_file(ctx, &part.map_err(Error::invalid)?, &vars, &mut budget, extra.range).await?;
                    Some((bytes, kind))
                }
                None => {
                    let text = r(b)?;
                    serde_json::from_str::<Value>(&text).map_err(|e| Error::invalid(format!("the operation's body is not JSON: {e}")))?;
                    Some((text.into_bytes(), "application/json".to_string()))
                }
            },
            None if multipart => {
                let mut parts = Vec::new();
                for (k, v) in form_fields(more, &vars)? {
                    parts.push((k, None, v.into_bytes()));
                }
                for (k, v) in &op.form {
                    parts.push(match spec_of::<FilePart>(v) {
                        Some(part) => {
                            let (name, kind, bytes) = self.sent_file(ctx, &part.map_err(Error::invalid)?, &vars, &mut budget, extra.range).await?;
                            (k.clone(), Some((name, kind)), bytes)
                        }
                        None => (k.clone(), None, r(v)?.into_bytes()),
                    });
                }
                Some(form_data(&parts))
            }
            None if !op.form.is_empty() || more.form_fields.is_some() => {
                let mut f = reqwest::Url::parse("http://form/").map_err(Error::internal)?;
                for (k, v) in form_fields(more, &vars)? {
                    f.query_pairs_mut().append_pair(&k, &v);
                }
                for (k, v) in &op.form {
                    f.query_pairs_mut().append_pair(k, &r(v)?);
                }
                Some((f.query().unwrap_or_default().as_bytes().to_vec(), "application/x-www-form-urlencoded".to_string()))
            }
            None => None,
        };
        let method = reqwest::Method::from_bytes(op.method().as_bytes()).map_err(Error::internal)?;
        let hidden = if bare || more.credentials == Some(false) {
            vec![]
        } else {
            self.authenticate(ctx, conn, &mut headers, body.as_ref().map(|b| b.0.as_slice()), &method, &url).await?
        };
        Ok((Request { method, url, headers, body }, hidden))
    }

    /// Adds what proves who calls; what more it must never show.
    async fn authenticate(
        &self,
        ctx: &Ctx,
        conn: &Loaded,
        headers: &mut Vec<(String, String)>,
        body: Option<&[u8]>,
        method: &reqwest::Method,
        url: &reqwest::Url,
    ) -> Result<Vec<String>> {
        use base64::Engine as _;
        match &conn.desc.auth {
            Auth::None => {}
            Auth::Oauth { provider, .. } => {
                let token = self.access_token(conn, ctx, false).await?;
                if provider.dpop {
                    headers.push(("authorization".into(), format!("DPoP {token}")));
                    headers.push(("dpop".into(), self.dpop_proof(conn, method, url, Some(&token)).await?));
                } else {
                    headers.push(("authorization".into(), format!("Bearer {token}")));
                }
                return Ok(vec![token]);
            }
            Auth::Session { header, prefix, .. } => {
                let token = self.access_token(conn, ctx, false).await?;
                let (header, prefix) = (header.as_deref().unwrap_or("authorization"), prefix.as_deref().unwrap_or("Bearer "));
                headers.push((header.to_string(), format!("{prefix}{token}")));
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

/// An operation's `form_fields`, rendered: an object's entries, or a list's `{name, value}`, in their order.
fn form_fields(more: &More, vars: &Value) -> Result<Vec<(String, String)>> {
    let Some(t) = &more.form_fields else { return Ok(vec![]) };
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let given: Value = serde_json::from_str(&render(t, vars)?).map_err(|e| Error::invalid(format!("form_fields is not JSON: {e}")))?;
    match given {
        Value::Object(o) => Ok(o.iter().map(|(k, v)| (k.clone(), text(v))).collect()),
        Value::Array(a) => a
            .iter()
            .map(|p| match (p.get("name").and_then(Value::as_str), p.get("value")) {
                (Some(n), Some(v)) => Ok((n.to_string(), text(v))),
                _ => Err(Error::invalid("form_fields is a list of {name, value}")),
            })
            .collect(),
        Value::Null => Ok(vec![]),
        _ => Err(Error::invalid("form_fields is an object or a list of {name, value}")),
    }
}

/// A `multipart/form-data` body (RFC 7578) of `parts` (name, file name and type for a file, bytes), and its type. Names
/// are the description's; a file name or type cannot end its line or its quotes.
fn form_data(parts: &[(String, Option<(String, String)>, Vec<u8>)]) -> (Vec<u8>, String) {
    let boundary = format!("ferrum-{}", Uuid::new_v4().simple());
    let quoted = |s: &str| s.replace('"', "%22").replace('\r', "%0D").replace('\n', "%0A");
    let mut out = Vec::new();
    for (name, file, bytes) in parts {
        out.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"", quoted(name)).as_bytes());
        if let Some((file_name, kind)) = file {
            let kind: String = kind.chars().filter(|c| !c.is_control()).collect();
            out.extend_from_slice(format!("; filename=\"{}\"\r\nContent-Type: {kind}", quoted(file_name)).as_bytes());
        }
        out.extend_from_slice(b"\r\n\r\n");
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (out, format!("multipart/form-data; boundary={boundary}"))
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
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let sealed = match &input.secrets {
            Some(s) => {
                let key = self
                    .settings
                    .secret_key
                    .as_ref()
                    .ok_or_else(|| Error::Unavailable("connection secrets need the server's secret key (FERRUM_SECRET_KEY)".into()))?;
                // What the platform keeps among them (a registered client, a DPoP key) stays.
                let held = match sqlx::query_scalar::<_, Option<String>>("select sealed from conn where tenant_id = $1 and id = $2 for update")
                    .bind(ctx.tenant)
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .flatten()
                {
                    Some(old) => {
                        serde_json::from_str::<BTreeMap<String, String>>(&self.open_sealed(&sealed_label(ctx.tenant, id), &old)?).map_err(Error::internal)?
                    }
                    None => BTreeMap::new(),
                };
                let mut all: BTreeMap<String, String> = held.into_iter().filter(|(n, _)| kept_by_platform(n)).collect();
                all.extend(s.clone());
                Some(key.seal(&sealed_label(ctx.tenant, id), &serde_json::to_string(&all).map_err(Error::internal)?)?)
            }
            None => None,
        };
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
        // A session signs in again with secrets given anew.
        if input.secrets.is_some() && matches!(input.description.auth, Auth::Session { .. }) {
            self.forget_session(ctx, id).await?;
        }
        Ok(id)
    }

    /// Sets or replaces the secrets named in `set` and removes those in `remove`, keeping the rest
    /// (`PATCH /api/admin/connections/{id}/secrets`); the audit trail names them, never their values. The names the
    /// connection now has.
    pub async fn merge_connection_secrets(&self, ctx: &Ctx, id: Uuid, set: &BTreeMap<String, String>, remove: &[String]) -> Result<Vec<String>> {
        admin_only(ctx)?;
        if set.keys().chain(remove).any(|n| !is_name(n)) || set.values().any(|v| v.len() > crate::secret::MAX_SECRET) {
            return Err(Error::field("secrets", "each named as an input is, of at most 16 KB"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let held = lock_secrets(&mut tx, self, ctx.tenant, id).await?;
        let now = store_secrets(&mut tx, self, ctx.tenant, id, held, set.clone(), remove).await?;
        let names: Vec<String> = now.into_keys().filter(|n| !kept_by_platform(n)).collect();
        if names.len() > 20 {
            return Err(Error::field("secrets", "at most 20"));
        }
        let description: Value =
            sqlx::query_scalar("select description from conn where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).fetch_one(&mut *tx).await?;
        audit::record(&mut tx, ctx, "Connection", id, "secrets", None, json!({"set": set.keys().collect::<Vec<_>>(), "removed": remove})).await?;
        tx.commit().await?;
        // A session signs in again with its secrets changed.
        if description["auth"]["type"] == "session" {
            self.forget_session(ctx, id).await?;
        }
        Ok(names)
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
                    .filter(|n| !kept_by_platform(n))
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
        if !conn.desc.operations.contains_key(operation) {
            return Err(Error::invalid(format!("{} has no operation {operation}", conn.name)));
        }
        if conn.desc.signs_in_with(operation) {
            return Err(Error::invalid(format!("{operation} is how {} signs in: it is not tried", conn.name)));
        }
        if conn.desc.changes(operation) && key.is_none_or(|k| k.trim().is_empty()) {
            return Err(Error::invalid(format!("{operation} changes something at the service, so trying it needs a key")));
        }
        let inputs = if inputs.is_null() { json!({}) } else { inputs.clone() };
        let outcome = if conn.desc.more(operation).steps.is_empty() {
            self.perform(ctx, &conn, operation, &inputs, key, Extra::default(), None, None).await?
        } else {
            Box::pin(self.perform_steps(ctx, &conn, operation, &inputs, key, Utc::now(), None)).await?
        };
        Ok(match outcome {
            Outcome::Ok(result, answer) => json!({"ok": true, "result": result, "answer": masked(&answer.body, &conn.mask())}),
            Outcome::Later(why, _) => json!({"ok": false, "temporary": true, "error": why}),
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
        // A "later" with the service's own Retry-After comes back when it said, capped.
        let result = k.call_waiting(&ctx, None, &c, true).await?;
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
    fn links_and_retry_after_are_read() {
        let base = reqwest::Url::parse("https://api.example.test/v1/feed?page=1").unwrap();
        let h = r#"<https://api.example.test/v1/feed?page=1>; rel="prev", </v1/feed?page=3&a=1,2>; rel="next last"; title="x""#;
        assert_eq!(link_target(h, "next", &base).as_deref(), Some("https://api.example.test/v1/feed?page=3&a=1,2"));
        assert_eq!(link_target(h, "prev", &base).as_deref(), Some("https://api.example.test/v1/feed?page=1"));
        assert_eq!(link_target("<x>; rel=nextish", "next", &base), None);
        assert_eq!(link_target("no link", "next", &base), None);
        let now = DateTime::parse_from_rfc3339("2026-11-13T10:00:00Z").unwrap().to_utc();
        assert_eq!(retry_after("120", now), Some(120));
        assert_eq!(retry_after("0", now), Some(1), "at least a second");
        assert_eq!(retry_after("99999999", now), Some(MAX_RETRY_AFTER), "capped");
        assert_eq!(retry_after("Fri, 13 Nov 2026 10:05:00 GMT", now), Some(300));
        assert_eq!(retry_after("Fri, 13 Nov 2026 09:00:00 GMT", now), Some(1), "a moment past");
        assert_eq!(retry_after("soon", now), None);
    }

    #[test]
    fn picks_are_checked() {
        let p = |v: Value, answer: bool| serde_json::from_value::<Pick>(v).unwrap().problem(answer);
        assert_eq!(p(json!("/a/0"), true), None);
        assert_eq!(p(json!({"header": "Location", "required": true}), true), None);
        assert_eq!(p(json!({"link": "next"}), true), None);
        assert_eq!(p(json!({"expr": "answer.a + 1"}), true), None);
        assert_eq!(p(json!("item.a + 1"), false), None, "a mapping's string is an expression");
        assert!(p(json!("id"), true).is_some(), "an answer's string is a pointer");
        assert!(p(json!({"at": "/a", "header": "x"}), true).is_some(), "one source");
        assert!(p(json!({"header": "x"}), false).is_some(), "a mapping reads the item");
        assert!(p(json!({"header": "bad header"}), true).is_some());
        assert!(p(json!({"link": "next>"}), true).is_some());
        assert!(p(json!({"expr": "a +"}), true).is_some());
        assert!(serde_json::from_value::<Pick>(json!({"at": "/a", "colour": 1})).is_err());
    }

    #[test]
    fn forms_keep_their_shape() {
        let (body, kind) = form_data(&[
            ("caption".into(), None, b"hi".to_vec()),
            ("media".into(), Some(("a\"b\r\nX-Evil: 1.png".into(), "image/png\r\nX-Evil: 1".into())), b"PNG".to_vec()),
        ]);
        let boundary = kind.strip_prefix("multipart/form-data; boundary=").unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"caption\"\r\n\r\nhi\r\n")), "{text}");
        assert!(text.contains("filename=\"a%22b%0D%0AX-Evil: 1.png\"\r\nContent-Type: image/pngX-Evil: 1\r\n\r\nPNG\r\n"), "{text}");
        assert!(!text.contains("\r\nX-Evil"), "no header of its own: {text}");
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
        // An object in `form` or `result` is kept as text and written back as the object.
        let op: Operation = serde_json::from_value(
            json!({"form": {"a": "{{ inputs.a }}", "f": {"file": "{{ inputs.f }}"}}, "result": {"x": "/x", "y": {"header": "Location"}}}),
        )
        .unwrap();
        assert!(op.multipart() && op.form["f"].starts_with(SPEC));
        assert_eq!(serde_json::to_value(&op).unwrap()["form"]["f"], json!({"file": "{{ inputs.f }}"}));
        assert_eq!(serde_json::to_value(&op).unwrap()["result"]["y"], json!({"header": "Location"}));
        assert!(
            serde_json::from_value::<Description>(json!({"test": true, "operations": {"a": {"form": {"f": {"file": "x", "colour": 1}}}}}))
                .unwrap()
                .problem()
                .is_some()
        );
        assert!(
            serde_json::from_value::<Description>(json!({"test": true, "operations": {"a": {"body": {"file": "x", "max_bytes": 1 << 30}}}}))
                .unwrap()
                .problem()
                .is_some()
        );
    }

    #[test]
    fn keys_beyond_an_operation_are_kept_on_it() {
        let v = json!({"test": true, "operations": {
            "put": {"method": "PUT", "url": "{{ inputs.url }}", "success": [308], "credentials": false},
            "go": {"steps": [{"name": "a", "operation": "put", "inputs": {"url": "inputs.u"}}]}}});
        let d: Description = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(d.more("put").success, Some(vec![308]));
        assert!(d.changes("go") && d.changes("put"));
        assert_eq!(d.problem(), None);
        assert_eq!(serde_json::to_value(&d).unwrap()["operations"], v["operations"], "written back on the operation");
        let typo = json!({"test": true, "operations": {"a": {"method": "GET", "colour": 1}}});
        assert!(serde_json::from_value::<Description>(typo).is_err(), "an unknown key is still refused");
        assert!(serde_json::from_value::<Description>(json!({"operations": {"a": {"url": 5}}})).is_err());
    }

    #[test]
    fn secrets_are_masked() {
        let v = masked(&json!({"e": "token s3cr3t-value refused", "n": [1, "s3cr3t-value"]}), &["s3cr3t-value".into(), "ab".into()]);
        assert_eq!(v, json!({"e": format!("token {HIDDEN} refused"), "n": [1, HIDDEN]}));
    }
}
