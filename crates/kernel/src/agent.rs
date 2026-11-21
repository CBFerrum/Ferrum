//! Agents as identities (gapclose/design/A2-A3-agents.md, AG-1). An agent is a record one member owns; on every request
//! its context is its owner's as they stand now, narrowed by the agent's grants ([`as_agent`]): never more than its owner,
//! never an administrator, a machine wherever a step-up is asked. Its row, its owner and the workspace's `agents` setting
//! are read on every request ([`Kernel::agent_ctx`]), so a pause, a revocation or the switch refuses the next request on
//! every replica. Making, changing, pausing and revoking agents and their keys is a person's, at a screen
//! ([`Ctx::person_only`]): no key (the owner's own included), agent, job or app does it.
//!
//! Until every check an agent's call meets has landed (AG-9), a build switch (the feature `agents-preview`, named only by
//! the test kit) refuses every agent credential.

use crate::Kernel;
use crate::controls::Proof;
use crate::ctx::{Ctx, Egress, Made};
use crate::db::Tx;
use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use ferrum_meta::agents::{Class, Decision, Table, default_table};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Whether this build lets agents in at all: until AG-9 only a build with the feature `agents-preview` does.
pub const OPEN: bool = cfg!(feature = "agents-preview");

/// The build switch's decision: refused (403 `agents_closed`) unless `open`.
pub fn admitted(open: bool) -> Result<()> {
    match open {
        true => Ok(()),
        false => Err(refused("agents_closed", 403, "connecting AI assistants is not available yet")),
    }
}

/// Live agents one person may own.
const MAX_AGENTS: i64 = 20;
/// Live keys one agent may hold.
const MAX_KEYS: i64 = 5;
/// Entities, and operations, an agent's grants may name.
const MAX_GRANTS: usize = 200;
/// Currencies an agent may have money ceilings in.
const MAX_CURRENCIES: usize = 20;

fn refused(code: &'static str, status: u16, message: impl Into<String>) -> Error {
    Error::Refused { code, status, message: message.into(), pointer: None }
}

/// The workspace's `agents` setting (saved by an administrator at a screen, crate::tenant): whether agents work here at
/// all, whether members make their own, what each class of operation needs, who approves, and the caps every agent's
/// limits keep within. Absent: these defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    pub enabled: bool,
    pub members_may_create: bool,
    pub classes: Table,
    pub approver_role: Option<String>,
    pub caps: Caps,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { enabled: true, members_may_create: true, classes: default_table(), approver_role: None, caps: Caps::default() }
    }
}

/// What no agent's limits may pass in this workspace.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Caps {
    pub calls_per_minute: u32,
    /// Every agent of one person together.
    pub owner_calls_per_minute: u32,
    pub rows_per_minute: u32,
    pub asks_per_day: u32,
    pub upload_mb_per_day: u32,
}

impl Default for Caps {
    fn default() -> Self {
        Caps { calls_per_minute: 300, owner_calls_per_minute: 600, rows_per_minute: 5000, asks_per_day: 100, upload_mb_per_day: 200 }
    }
}

impl Settings {
    /// As the workspace's settings hold it, every class given (the default where the setting names none) and none below
    /// its floor. A setting that cannot be read is the default switched off: nothing is let in on a guess.
    pub fn of(v: Option<&Value>) -> Settings {
        let mut s = match v {
            None | Some(Value::Null) => Settings::default(),
            Some(v) => serde_json::from_value(v.clone()).unwrap_or_else(|e| {
                tracing::warn!("a workspace's agents setting could not be read: {e}");
                Settings { enabled: false, ..Settings::default() }
            }),
        };
        let defaults = default_table();
        for c in Class::ALL {
            let d = s.classes.get(&c).copied().unwrap_or(defaults[&c]);
            s.classes.insert(c, d.max(c.floor()));
        }
        s
    }
}

/// The setting as an administrator saves it, checked whole: known keys only, `decides` and `unclassified` never
/// `allowed`, caps from 1, an approver role the workspace has. What is kept.
pub async fn check_settings(k: &Kernel, ctx: &Ctx, v: &Value) -> Result<Value> {
    let s: Settings = serde_json::from_value(v.clone()).map_err(|e| Error::field("agents", e.to_string()))?;
    for (c, d) in &s.classes {
        if *d < c.floor() {
            return Err(Error::field("agents", format!("{} waits for a person or is forbidden: it cannot be allowed", c.as_str())));
        }
    }
    let caps = [s.caps.calls_per_minute, s.caps.owner_calls_per_minute, s.caps.rows_per_minute, s.caps.asks_per_day, s.caps.upload_mb_per_day];
    if caps.contains(&0) {
        return Err(Error::field("agents", "each cap is 1 or more"));
    }
    if let Some(role) = &s.approver_role {
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        role_exists(&mut tx, ctx.tenant, role, "agents").await?;
        tx.commit().await?;
    }
    serde_json::to_value(&s).map_err(Error::internal)
}

async fn role_exists(tx: &mut Tx, tenant: Uuid, role: &str, field: &str) -> Result<()> {
    let known: bool =
        sqlx::query_scalar("select exists (select 1 from role where tenant_id = $1 and name = $2)").bind(tenant).bind(role).fetch_one(&mut **tx).await?;
    if known { Ok(()) } else { Err(Error::field(field, format!("{role} is not a role of this workspace"))) }
}

/// What an agent may reach: per entity (or `*`), the verbs a records key names (`read`, `write (own records)`, `read
/// field group <g>`...), and the capabilities it may call (ids, or `prefix.*`; none: every one the verbs allow but the
/// platform's own, which are named).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grants {
    #[serde(default)]
    pub entities: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    pub operations: Vec<String>,
}

impl Default for Grants {
    /// Reading, nothing else.
    fn default() -> Self {
        Grants { entities: BTreeMap::from([("*".into(), BTreeSet::from(["read".into()]))]), operations: vec![] }
    }
}

/// An agent's limits as set (none: the default, within the workspace's caps).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls_per_minute: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_per_minute: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub records_per_call: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asks_per_day: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_mb_per_day: Option<u32>,
    /// Per currency (the document's own), what one call and one day may move without a person: an administrator's.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub money: BTreeMap<String, Ceiling>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ceiling {
    pub per_call: Decimal,
    pub per_day: Decimal,
}

/// An agent's limits in force: each as set, else its default, never above the workspace's cap.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Effective {
    pub calls_per_minute: u32,
    pub rows_per_minute: u32,
    pub records_per_call: u32,
    pub asks_per_day: u32,
    pub upload_mb_per_day: u32,
    pub money: BTreeMap<String, Ceiling>,
}

impl Limits {
    pub fn effective(&self, caps: &Caps) -> Effective {
        Effective {
            calls_per_minute: self.calls_per_minute.unwrap_or(120).min(caps.calls_per_minute),
            rows_per_minute: self.rows_per_minute.unwrap_or(2000).min(caps.rows_per_minute),
            records_per_call: self.records_per_call.unwrap_or(100),
            asks_per_day: self.asks_per_day.unwrap_or(50).min(caps.asks_per_day),
            upload_mb_per_day: self.upload_mb_per_day.unwrap_or(50).min(caps.upload_mb_per_day),
            money: self.money.clone(),
        }
    }
}

/// What one request of an agent carries ([`Ctx::agent`]): who it is, under which revision, and what it may do without a
/// person. The gate (AG-7) decides by it; nothing else reads it to decide.
#[derive(Debug, Clone, Serialize)]
pub struct AgentCtx {
    pub id: Uuid,
    pub name: String,
    pub revision: i32,
    pub owner: Uuid,
    /// Per class, what an agent's call of it needs.
    pub table: Table,
    pub operations: Vec<String>,
    pub limits: Effective,
    /// Who approves its requests: its own, else the workspace's; none: its owner.
    pub approver_role: Option<String>,
}

/// An agent's row.
#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: Uuid,
    owner_id: Uuid,
    name: String,
    state: String,
    revision: i32,
    grants: Value,
    classes: Value,
    admin_classes: Vec<String>,
    limits: Value,
    data_policy: Value,
    approver_role: Option<String>,
    networks: Option<Vec<String>>,
    expires_at: Option<DateTime<Utc>>,
    oauth_client: Option<String>,
    oauth_redirect: Option<String>,
    created_by: Uuid,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
    paused_by: Option<Uuid>,
    revoked_at: Option<DateTime<Utc>>,
    revoked_by: Option<Uuid>,
}

const COLUMNS: &str = "a.id, a.owner_id, a.name, a.state, a.revision, a.grants, a.classes, a.admin_classes, a.limits, a.data_policy, \
     a.approver_role, a.networks, a.expires_at, a.oauth_client, a.oauth_redirect, a.created_by, a.created_at, a.updated_at, a.last_used_at, \
     a.paused_by, a.revoked_at, a.revoked_by";

/// A row's JSON as it was checked when saved; one that no longer reads is the server's fault, never a reason to let more in.
fn stored<T: for<'de> Deserialize<'de>>(v: &Value, what: &str) -> Result<T> {
    serde_json::from_value(v.clone()).map_err(|e| Error::Internal(format!("an agent's {what} could not be read: {e}")))
}

impl Row {
    fn grants(&self) -> Result<Grants> {
        stored(&self.grants, "grants")
    }
    fn classes(&self) -> Result<BTreeMap<Class, Decision>> {
        stored(&self.classes, "classes")
    }
    fn admin_classes(&self) -> Result<BTreeSet<Class>> {
        stored(&json!(self.admin_classes), "classes")
    }
    fn limits(&self) -> Result<Limits> {
        stored(&self.limits, "limits")
    }
    fn data_policy(&self) -> Result<Egress> {
        stored(&self.data_policy, "data policy")
    }
    fn table(&self, ws: &Settings) -> Result<Table> {
        Ok(table(&ws.classes, &self.classes()?, &self.admin_classes()?))
    }
}

/// An agent's table in force: a class an administrator set stands as set; a class its owner set counts at least as the
/// workspace's does now; any other is the workspace's; none below its floor.
fn table(ws: &Table, set: &BTreeMap<Class, Decision>, admin: &BTreeSet<Class>) -> Table {
    Class::ALL
        .into_iter()
        .map(|c| {
            let w = ws.get(&c).copied().unwrap_or(Decision::NeedsPerson);
            let d = match set.get(&c) {
                Some(d) if admin.contains(&c) => *d,
                Some(d) => (*d).max(w),
                None => w,
            };
            (c, d.max(c.floor()))
        })
        .collect()
}

/// The classes `ctx` may give an agent, over what it holds (`old`, and the classes an administrator set): an
/// administrator any, but none below its floor; its owner only stricter than the workspace's, leaving an administrator's
/// as they are. Which classes an administrator has then set.
fn check_classes(
    ctx: &Ctx,
    ws: &Table,
    given: &BTreeMap<Class, Decision>,
    old: &BTreeMap<Class, Decision>,
    old_admin: &BTreeSet<Class>,
) -> Result<BTreeSet<Class>> {
    for (c, d) in given {
        if *d < c.floor() {
            return Err(Error::field("classes", format!("{} waits for a person or is forbidden: it cannot be allowed", c.as_str())));
        }
    }
    if ctx.is_admin {
        return Ok(given.keys().copied().collect());
    }
    for c in old_admin {
        if given.get(c) != old.get(c) {
            return Err(Error::Forbidden(format!("what an agent may do for {} was set by an administrator: ask one to change it", c.as_str())));
        }
    }
    for (c, d) in given.iter().filter(|(c, _)| !old_admin.contains(c)) {
        let w = ws.get(c).copied().unwrap_or(Decision::NeedsPerson);
        if *d < w {
            return Err(Error::Forbidden(format!(
                "you can only make {} stricter than your workspace's ({})",
                c.as_str(),
                json!(w).as_str().unwrap_or_default()
            )));
        }
    }
    Ok(old_admin.clone())
}

/// Grants checked against the workspace: known entities, verbs a records key knows, field groups the entity has,
/// capability ids or prefixes; at most 200 of each.
fn check_grants(ctx: &Ctx, g: &Grants) -> Result<()> {
    let bad = |m: String| Err(Error::field("grants", m));
    if g.entities.len() > MAX_GRANTS || g.operations.len() > MAX_GRANTS {
        return bad(format!("at most {MAX_GRANTS} entities and {MAX_GRANTS} operations"));
    }
    let verbs: BTreeSet<&str> = crate::ctx::action_verbs().collect();
    for (entity, said) in &g.entities {
        let ent = match entity.as_str() {
            "*" => None,
            e => match ctx.schema.entity(e) {
                Some(ent) => Some(ent),
                None => return bad(format!("{e} is not an entity of this workspace")),
            },
        };
        if said.is_empty() {
            return bad(format!("{entity}: name what it may do"));
        }
        let group = |g: &str| match ent {
            Some(ent) => ent.def.fields.iter().any(|f| f.perm_group.as_deref() == Some(g)),
            None => ctx.schema.entities.values().any(|e| e.def.fields.iter().any(|f| f.perm_group.as_deref() == Some(g))),
        };
        for v in said {
            let known = verbs.contains(v.as_str())
                || v.strip_suffix(" (own records)").is_some_and(|b| verbs.contains(b))
                || ["read field group ", "write field group "].iter().any(|p| v.strip_prefix(p).is_some_and(group));
            if !known {
                return bad(format!("{entity}: {v} is not something an agent may be given (read, write (own records), read field group <g>, ...)"));
            }
        }
    }
    for op in &g.operations {
        let id = op.strip_suffix(".*").unwrap_or(op);
        let fits = (3..=200).contains(&op.len())
            && id.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b':'));
        if !fits {
            return bad(format!("{op} is not a capability's id or a prefix of ids (selling.quotation.*)"));
        }
    }
    Ok(())
}

/// Limits checked against the workspace's caps; money ceilings are an administrator's.
fn check_limits(ctx: &Ctx, l: &Limits, caps: &Caps) -> Result<()> {
    let within = |name: &str, v: Option<u32>, cap: u32| match v {
        Some(v) if v == 0 || v > cap => Err(Error::field("limits", format!("{name}: 1 to {cap} (the workspace's cap)"))),
        _ => Ok(()),
    };
    within("calls_per_minute", l.calls_per_minute, caps.calls_per_minute)?;
    within("rows_per_minute", l.rows_per_minute, caps.rows_per_minute)?;
    within("asks_per_day", l.asks_per_day, caps.asks_per_day)?;
    within("upload_mb_per_day", l.upload_mb_per_day, caps.upload_mb_per_day)?;
    within("records_per_call", l.records_per_call, 200)?;
    if l.money.is_empty() {
        return Ok(());
    }
    if !ctx.is_admin {
        return Err(Error::Forbidden("money ceilings are set by an administrator".into()));
    }
    if l.money.len() > MAX_CURRENCIES {
        return Err(Error::field("limits", format!("money ceilings in at most {MAX_CURRENCIES} currencies")));
    }
    for (currency, c) in &l.money {
        if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(Error::field("limits", format!("{currency} is not a currency's code (INR, EUR)")));
        }
        if c.per_call <= Decimal::ZERO || c.per_day < c.per_call {
            return Err(Error::field("limits", format!("{currency}: per_call above 0, per_day at least per_call")));
        }
    }
    Ok(())
}

/// `null` given for a field that may be cleared: `Some(None)`; the field left out: `None`.
fn given<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

/// A new agent, as the console sends it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct New {
    name: String,
    owner: Option<Uuid>,
    grants: Option<Grants>,
    classes: Option<BTreeMap<Class, Decision>>,
    limits: Option<Limits>,
    data_policy: Option<Egress>,
    networks: Option<Vec<String>>,
    expires_at: Option<DateTime<Utc>>,
    approver_role: Option<String>,
    oauth_client: Option<String>,
}

/// A change to an agent, against the revision it was read at.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    revision: i32,
    name: Option<String>,
    grants: Option<Grants>,
    classes: Option<BTreeMap<Class, Decision>>,
    limits: Option<Limits>,
    #[serde(default, deserialize_with = "given")]
    data_policy: Option<Option<Egress>>,
    #[serde(default, deserialize_with = "given")]
    networks: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "given")]
    expires_at: Option<Option<DateTime<Utc>>>,
    #[serde(default, deserialize_with = "given")]
    approver_role: Option<Option<String>>,
    #[serde(default, deserialize_with = "given")]
    oauth_client: Option<Option<String>>,
}

fn clean_name(name: &str) -> Result<String> {
    let name = name.trim();
    match name.chars().count() {
        1..=100 => Ok(name.to_string()),
        _ => Err(Error::field("name", "1 to 100 characters")),
    }
}

fn future(at: Option<DateTime<Utc>>) -> Result<Option<DateTime<Utc>>> {
    match at {
        Some(at) if at <= Utc::now() => Err(Error::field("expires_at", "a time still to come")),
        at => Ok(at),
    }
}

/// An administrator's own field, which an owner who is not one may not set.
fn admins_only(ctx: &Ctx, set: bool, what: &str) -> Result<()> {
    match set && !ctx.is_admin {
        true => Err(Error::Forbidden(format!("{what} is set by an administrator"))),
        false => Ok(()),
    }
}

/// The client an agent is bound to, which the installation keeps (the system reads it).
async fn client_known(k: &Kernel, client: &str) -> Result<()> {
    match k.oauth_client(client).await {
        Err(Error::NotFound(_)) => Err(Error::field("oauth_client", "no such OAuth client")),
        other => other.map(|_| ()),
    }
}

/// The workspace's `agents` setting, read in `tx` now.
async fn settings(tx: &mut Tx, tenant: Uuid) -> Result<Settings> {
    let v: Option<Value> = sqlx::query_scalar("select settings->'agents' from tenant where id = $1").bind(tenant).fetch_one(&mut **tx).await?;
    Ok(Settings::of(v.as_ref()))
}

/// A unique index's refusal, in words.
fn taken(e: Error, name: &str) -> Error {
    match e {
        Error::Conflict(m) if m.contains("agent_name") => Error::Conflict(format!("there is already an agent named {name} for this person")),
        Error::Conflict(m) if m.contains("agent_oauth") => Error::Conflict("this person already has an agent for that OAuth client".into()),
        e => e,
    }
}

impl Kernel {
    /// Whether this kernel lets agents in at all: refused (403 `agents_closed`) in a build without `agents-preview`
    /// (until AG-9), or once [`Kernel::close_agents`] shut them out. Nobody makes an agent or a key, and no agent's
    /// credential opens anything, while it refuses (AG-1-2-fix 5).
    pub fn agents_admitted(&self) -> Result<()> {
        admitted(OPEN && !self.agents_closed.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Shuts agents out of this kernel as a build without them does: what a test asks of such a build. Never opens.
    pub fn close_agents(&self) {
        self.agents_closed.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Makes an agent (§2.5, F1), at a screen: a member for themselves where the workspace lets members; an
    /// administrator for any active member, and the fields that are an administrator's. Refused while agents are off,
    /// and at the owner's 21st live agent (counted under a lock of the owner's, so two at once cannot pass it).
    pub async fn create_agent(&self, ctx: &Ctx, input: &Value) -> Result<Value> {
        self.agents_admitted()?;
        ctx.person_only("making an agent")?;
        let n: New = serde_json::from_value(input.clone()).map_err(|e| Error::invalid(e.to_string()))?;
        let owner = n.owner.unwrap_or(ctx.user);
        if owner != ctx.user && !ctx.is_admin {
            return Err(Error::Forbidden("you make agents for yourself only".into()));
        }
        let name = clean_name(&n.name)?;
        let grants = n.grants.unwrap_or_default();
        check_grants(ctx, &grants)?;
        let limits = n.limits.unwrap_or_default();
        admins_only(ctx, n.data_policy.is_some(), "an agent's data policy")?;
        admins_only(ctx, n.approver_role.is_some(), "an agent's approver")?;
        admins_only(ctx, n.oauth_client.is_some(), "binding an OAuth client")?;
        let networks = crate::auth::key_networks(&n.networks.unwrap_or_default())?;
        let expires_at = future(n.expires_at)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let ws = settings(&mut tx, ctx.tenant).await?;
        if !ws.enabled {
            return Err(refused("agents_off", 403, "agents are turned off in this workspace"));
        }
        if !ctx.is_admin && !ws.members_may_create {
            return Err(Error::Forbidden("your workspace's administrators connect agents".into()));
        }
        check_limits(ctx, &limits, &ws.caps)?;
        let classes = n.classes.unwrap_or_default();
        let admin_classes = check_classes(ctx, &ws.classes, &classes, &BTreeMap::new(), &BTreeSet::new())?;
        if let Some(role) = &n.approver_role {
            role_exists(&mut tx, ctx.tenant, role, "approver_role").await?;
        }
        if let Some(client) = &n.oauth_client {
            client_known(self, client).await?;
        }
        let member: Option<(String, bool)> = sqlx::query_as("select kind, is_active from app_user where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await?;
        if member != Some(("member".into(), true)) {
            return Err(Error::field("owner", "an agent's owner is an active member of this workspace"));
        }
        // One owner's agents are counted one at a time: the count and the insert hold this lock to the commit.
        sqlx::query("select pg_advisory_xact_lock(hashtext('ferrum.agent'), hashtext($1::text))").bind(owner).execute(&mut *tx).await?;
        let live: i64 = sqlx::query_scalar("select count(*) from agent where tenant_id = $1 and owner_id = $2 and state <> 'revoked'")
            .bind(ctx.tenant)
            .bind(owner)
            .fetch_one(&mut *tx)
            .await?;
        if live >= MAX_AGENTS {
            return Err(Error::Conflict(format!("a person has at most {MAX_AGENTS} agents: revoke one first")));
        }
        let id = Uuid::now_v7();
        let admin_classes: Vec<&str> = admin_classes.iter().map(|c| c.as_str()).collect();
        sqlx::query(
            "insert into agent (tenant_id, id, owner_id, name, grants, classes, admin_classes, limits, data_policy, approver_role, networks, \
             expires_at, oauth_client, created_by) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(owner)
        .bind(&name)
        .bind(json!(grants))
        .bind(json!(classes))
        .bind(&admin_classes)
        .bind(json!(limits))
        .bind(json!(n.data_policy.unwrap_or_default()))
        .bind(&n.approver_role)
        .bind(&networks)
        .bind(expires_at)
        .bind(&n.oauth_client)
        .bind(ctx.user)
        .execute(&mut *tx)
        .await
        .map_err(|e| taken(e.into(), &name))?;
        let made = json!({"name": name, "owner": owner, "grants": grants, "classes": classes, "limits": limits, "networks": networks,
                          "expires_at": expires_at, "approver_role": n.approver_role, "oauth_client": n.oauth_client});
        crate::audit::record_admin(&mut tx, ctx, "Agent", id, "create", Some(1), made).await?;
        let out = detail(self, &mut tx, ctx, &ws, id).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Changes an agent (§2.5), at a screen, by its owner or an administrator, each only what §2.2's table lets them,
    /// against the revision it was read at (409 otherwise); every change moves the revision on.
    pub async fn update_agent(&self, ctx: &Ctx, id: Uuid, change: &Value) -> Result<Value> {
        ctx.person_only("changing an agent")?;
        let c: Change = serde_json::from_value(change.clone()).map_err(|e| Error::invalid(e.to_string()))?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row = locked(&mut tx, ctx, id).await?;
        if row.state == "revoked" {
            return Err(Error::Conflict("this agent is revoked: it changes no more".into()));
        }
        if c.revision != row.revision {
            return Err(Error::Conflict("this agent changed since it was read: read it again".into()));
        }
        let ws = settings(&mut tx, ctx.tenant).await?;
        // Where members do not make their own, an agent an administrator made for one is the administrator's choice: its
        // owner may narrow it, never widen it (AG-1-2-fix 1).
        let narrow_only = !ctx.is_admin && row.created_by != row.owner_id && !ws.members_may_create;
        admins_only(ctx, c.data_policy.is_some(), "an agent's data policy")?;
        admins_only(ctx, c.approver_role.is_some(), "an agent's approver")?;
        admins_only(ctx, c.oauth_client.is_some(), "binding an OAuth client")?;
        let name = c.name.as_deref().map(clean_name).transpose()?.unwrap_or(row.name.clone());
        let grants = match c.grants {
            Some(g) => {
                check_grants(ctx, &g)?;
                if narrow_only && !narrower(&g, &row.grants()?) {
                    return Err(Error::Forbidden(ADMINS_CHOICE.into()));
                }
                g
            }
            None => row.grants()?,
        };
        let old_limits = row.limits()?;
        let limits = match c.limits {
            Some(mut l) => {
                // An owner's change leaves an administrator's money ceilings as they are.
                if !ctx.is_admin {
                    if !l.money.is_empty() && l.money != old_limits.money {
                        return Err(Error::Forbidden("money ceilings are set by an administrator".into()));
                    }
                    l.money = BTreeMap::new();
                }
                check_limits(ctx, &l, &ws.caps)?;
                if !ctx.is_admin {
                    l.money = old_limits.money.clone();
                }
                l
            }
            None => old_limits.clone(),
        };
        let (old_classes, old_admin) = (row.classes()?, row.admin_classes()?);
        let (classes, admin_classes) = match c.classes {
            Some(given) => {
                let admin = check_classes(ctx, &ws.classes, &given, &old_classes, &old_admin)?;
                (given, admin)
            }
            None => (old_classes, old_admin),
        };
        let data_policy = match c.data_policy {
            Some(p) => json!(p.unwrap_or_default()),
            None => row.data_policy.clone(),
        };
        let approver_role = match c.approver_role {
            Some(Some(role)) => {
                role_exists(&mut tx, ctx.tenant, &role, "approver_role").await?;
                Some(role)
            }
            Some(None) => None,
            None => row.approver_role.clone(),
        };
        let oauth_client = match c.oauth_client {
            Some(Some(client)) => {
                client_known(self, &client).await?;
                Some(client)
            }
            Some(None) => None,
            None => row.oauth_client.clone(),
        };
        let networks = match c.networks {
            Some(n) => crate::auth::key_networks(&n.unwrap_or_default())?,
            None => row.networks.clone(),
        };
        let expires_at = match c.expires_at {
            Some(at) => future(at)?,
            None => row.expires_at,
        };
        if narrow_only {
            let old_nets: BTreeSet<&String> = row.networks.iter().flatten().collect();
            let nets_widened = !old_nets.is_empty() && networks.as_ref().is_none_or(|n| n.iter().any(|x| !old_nets.contains(x)));
            let later = row.expires_at.is_some_and(|old| expires_at.is_none_or(|new| new > old));
            let limits_raised = limits.effective(&ws.caps) != limits_lowered(&limits.effective(&ws.caps), &old_limits.effective(&ws.caps));
            if nets_widened || later || limits_raised {
                return Err(Error::Forbidden(ADMINS_CHOICE.into()));
            }
        }
        let admin_classes: Vec<&str> = admin_classes.iter().map(|c| c.as_str()).collect();
        sqlx::query(
            "update agent set name = $3, grants = $4, classes = $5, admin_classes = $6, limits = $7, data_policy = $8, approver_role = $9, \
             oauth_client = $10, networks = $11, expires_at = $12, revision = revision + 1, updated_at = now() where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(&name)
        .bind(json!(grants))
        .bind(json!(classes))
        .bind(&admin_classes)
        .bind(json!(limits))
        .bind(&data_policy)
        .bind(&approver_role)
        .bind(&oauth_client)
        .bind(&networks)
        .bind(expires_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| taken(e.into(), &name))?;
        let changed: serde_json::Map<String, Value> = change.as_object().cloned().unwrap_or_default().into_iter().filter(|(k, _)| k != "revision").collect();
        crate::audit::record_admin(&mut tx, ctx, "Agent", id, "update", Some(row.revision + 1), Value::Object(changed)).await?;
        let out = detail(self, &mut tx, ctx, &ws, id).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Pauses (`pause`), resumes (`resume`) or revokes (`revoke`) an agent (§2.5, F7), at a screen, by its owner or an
    /// administrator: one row's update with its audit line, waiting on no lock an agent's call holds. Asking for the state
    /// it is in does nothing; a revoked agent stays revoked. Revoking revokes its keys in the same transaction.
    pub async fn set_agent_state(&self, ctx: &Ctx, id: Uuid, to: &str) -> Result<Value> {
        ctx.person_only("pausing, resuming or revoking an agent")?;
        let state = match to {
            "pause" => "paused",
            "resume" => "active",
            "revoke" => "revoked",
            other => return Err(Error::invalid(format!("{other}: pause, resume or revoke"))),
        };
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let now: Option<String> = sqlx::query_scalar("select state from agent where tenant_id = $1 and id = $2 and (owner_id = $3 or $4)")
            .bind(ctx.tenant)
            .bind(id)
            .bind(ctx.user)
            .bind(ctx.is_admin)
            .fetch_optional(&mut *tx)
            .await?;
        match now.as_deref() {
            None => return Err(Error::NotFound("agent".into())),
            Some("revoked") if state != "revoked" => return Err(Error::Conflict("this agent is revoked for good".into())),
            _ => {}
        }
        // An agent an administrator paused is resumed only by an administrator (AG-1-2-fix 1).
        if state == "active" && !ctx.is_admin {
            let paused_by: Option<Uuid> = sqlx::query_scalar("select paused_by from agent where tenant_id = $1 and id = $2 and state = 'paused'")
                .bind(ctx.tenant)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .flatten();
            if paused_by.is_some_and(|p| p != ctx.user) {
                return Err(Error::Forbidden("an administrator paused this agent: ask one to resume it".into()));
            }
        }
        let changed = sqlx::query(
            "update agent set state = $3, paused_by = case when $3 = 'paused' then $4 end, \
             revoked_at = case when $3 = 'revoked' then now() end, revoked_by = case when $3 = 'revoked' then $4 end, updated_at = now() \
             where tenant_id = $1 and id = $2 and state <> $3 and state <> 'revoked'",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(state)
        .bind(ctx.user)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed > 0 {
            if state == "revoked" {
                sqlx::query("update api_key set revoked_at = now() where tenant_id = $1 and agent_id = $2 and revoked_at is null")
                    .bind(ctx.tenant)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            crate::audit::record_admin(&mut tx, ctx, "Agent", id, to, None, json!({"state": state})).await?;
        }
        let ws = settings(&mut tx, ctx.tenant).await?;
        let out = detail(self, &mut tx, ctx, &ws, id).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Makes a key for agent `id` (§2.5), at a screen, by its owner or an administrator: an `api_key` row naming the agent,
    /// its person the agent's owner, no scope. Never for a revoked agent, nor past five live keys (counted under the
    /// agent's row lock). The key, shown this once.
    pub async fn create_agent_key(&self, ctx: &Ctx, id: Uuid, name: &str, expires_at: Option<DateTime<Utc>>) -> Result<(Uuid, String)> {
        self.agents_admitted()?;
        ctx.person_only("making an agent's key")?;
        let name = clean_name(name)?;
        let expires_at = future(expires_at)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row = locked(&mut tx, ctx, id).await?;
        if row.state == "revoked" {
            return Err(Error::Conflict("this agent is revoked: it takes no new key".into()));
        }
        let live: i64 = sqlx::query_scalar(
            "select count(*) from api_key where tenant_id = $1 and agent_id = $2 and revoked_at is null and (expires_at is null or expires_at > now())",
        )
        .bind(ctx.tenant)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if live >= MAX_KEYS {
            return Err(Error::Conflict(format!("an agent holds at most {MAX_KEYS} live keys: revoke one first")));
        }
        let (prefix, secret) = crate::auth::new_key();
        let key = Uuid::now_v7();
        sqlx::query("insert into api_key (tenant_id, id, user_id, agent_id, name, prefix, secret_hash, expires_at) values ($1, $2, $3, $4, $5, $6, $7, $8)")
            .bind(ctx.tenant)
            .bind(key)
            .bind(row.owner_id)
            .bind(id)
            .bind(&name)
            .bind(&prefix)
            .bind(crate::auth::sha256_hex(&secret))
            .bind(expires_at)
            .execute(&mut *tx)
            .await?;
        let line = json!({"key": key, "name": name, "expires_at": expires_at, "owner": row.owner_id});
        crate::audit::record_admin(&mut tx, ctx, "Agent", id, "key", None, line).await?;
        tx.commit().await?;
        Ok((key, format!("fk_{prefix}_{secret}")))
    }

    /// Revokes key `key` of agent `id`, at a screen, by its owner or an administrator.
    pub async fn revoke_agent_key(&self, ctx: &Ctx, id: Uuid, key: Uuid) -> Result<()> {
        ctx.person_only("revoking an agent's key")?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row = locked(&mut tx, ctx, id).await?;
        let n = sqlx::query("update api_key set revoked_at = now() where tenant_id = $1 and id = $2 and agent_id = $3 and revoked_at is null")
            .bind(ctx.tenant)
            .bind(key)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("key".into()));
        }
        crate::audit::record_admin(&mut tx, ctx, "Agent", id, "revoke_key", None, json!({"key": key, "owner": row.owner_id})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Agent `id`'s keys, newest first: never a secret.
    pub async fn agent_keys(&self, ctx: &Ctx, id: Uuid) -> Result<Vec<Value>> {
        no_agent(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row = seen(&mut tx, ctx, id).await?;
        let out = keys(&mut tx, ctx.tenant, row.id).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// The agents `ctx` may see (§2.11): an administrator's, everyone's (of `owner` and in `state` when given); anyone
    /// else's, their own.
    pub async fn agents(&self, ctx: &Ctx, owner: Option<Uuid>, state: Option<&str>) -> Result<Vec<Value>> {
        no_agent(ctx)?;
        if owner.is_some_and(|o| o != ctx.user) && !ctx.is_admin {
            return Err(Error::Forbidden("you see your own agents only".into()));
        }
        let owner = if ctx.is_admin { owner } else { Some(ctx.user) };
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', a.id, 'name', a.name, 'state', a.state, 'revision', a.revision, \
               'owner', jsonb_build_object('id', u.id, 'name', u.full_name, 'email', u.email), 'created_at', a.created_at, \
               'expires_at', a.expires_at, 'last_used_at', a.last_used_at, 'oauth_client', a.oauth_client) \
             from agent a join app_user u on u.tenant_id = a.tenant_id and u.id = a.owner_id \
             where a.tenant_id = $1 and ($2::uuid is null or a.owner_id = $2) and ($3::text is null or a.state = $3) order by a.created_at desc limit 1000",
        )
        .bind(ctx.tenant)
        .bind(owner)
        .bind(state)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Agent `id` as the console shows it (§2.11): to its owner and to administrators.
    pub async fn agent(&self, ctx: &Ctx, id: Uuid) -> Result<Value> {
        no_agent(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        seen(&mut tx, ctx, id).await?;
        let ws = settings(&mut tx, ctx.tenant).await?;
        let out = detail(self, &mut tx, ctx, &ws, id).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Agent `id`'s days, the last `days` (1 to 400) of the workspace's, newest first.
    pub async fn agent_usage(&self, ctx: &Ctx, id: Uuid, days: u32) -> Result<Value> {
        no_agent(ctx)?;
        if !(1..=400).contains(&days) {
            return Err(Error::field("days", "1 to 400"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        seen(&mut tx, ctx, id).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('day', day, 'calls', calls, 'refused', refused, 'asked', asked, 'upload_bytes', upload_bytes) \
             from agent_day where tenant_id = $1 and agent_id = $2 and day > $3::date - $4 order by day desc",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(ctx.today())
        .bind(days as i32)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(json!({"days": rows}))
    }

    /// What was done with agent `id` (its audit lines), newest first, `limit` (1 to 200) at a time from before `before`;
    /// `next_before` while more are left.
    pub async fn agent_activity(&self, ctx: &Ctx, id: Uuid, before: Option<i64>, limit: u32) -> Result<Value> {
        no_agent(ctx)?;
        if !(1..=200).contains(&limit) {
            return Err(Error::field("limit", "1 to 200"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        seen(&mut tx, ctx, id).await?;
        let rows: Vec<(i64, Value)> = sqlx::query_as(
            "select id, jsonb_build_object('kind', 'audit', 'at', at, 'action', action, 'by', actor, 'details', diff) from audit_event \
             where tenant_id = $1 and entity = 'Agent' and record_id = $2 and ($3::int8 is null or id < $3) order by id desc limit $4",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(before)
        .bind(limit as i64)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let next = (rows.len() == limit as usize).then(|| rows.last().map(|r| r.0)).flatten();
        Ok(json!({"items": rows.into_iter().map(|r| r.1).collect::<Vec<_>>(), "next_before": next}))
    }

    /// The context of agent `agent` for a request its credential made, `ctx` being its owner's for that request (A2-A3
    /// 2.7.1): refused while the build is closed; its row, its owner and the workspace's switch read now; its networks, or
    /// the staff networks, asked; its calls and its owner's counted across replicas; its owner's rights narrowed by its
    /// grants ([`as_agent`]).
    pub async fn agent_ctx(&self, ctx: &Ctx, agent: Uuid) -> Result<Ctx> {
        self.agents_admitted()?;
        let invalid = || Error::Unauthorized("this agent's credential is not valid".into());
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let found: Option<Row> =
            sqlx::query_as(AssertSqlSafe(format!("select {COLUMNS} from agent a where a.tenant_id = $1 and a.id = $2 and a.owner_id = $3")))
                .bind(ctx.tenant)
                .bind(agent)
                .bind(ctx.user)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(a) = found else { return Err(invalid()) };
        let (kind, active, ws, nets, frozen, open): (String, bool, Option<Value>, Option<Value>, bool, bool) = sqlx::query_as(
            "select u.kind, u.is_active, t.settings->'agents', t.settings->'staff_networks', ferrum_workspace_frozen(t.id::text), t.status = 'active' \
             from app_user u join tenant t on t.id = u.tenant_id where u.tenant_id = $1 and u.id = $2",
        )
        .bind(ctx.tenant)
        .bind(a.owner_id)
        .fetch_one(&mut *tx)
        .await?;
        // A workspace that is not active opens no agent, as it opens no key (AG-1-2-fix 8).
        if a.state == "revoked" || a.expires_at.is_some_and(|at| at <= Utc::now()) || !open {
            return Err(invalid());
        }
        let ws = Settings::of(ws.as_ref());
        if !ws.enabled {
            return Err(refused("agents_off", 403, "agents are turned off in this workspace"));
        }
        if a.state == "paused" {
            return Err(refused("agent_paused", 403, "this agent is paused"));
        }
        if kind != "member" || !active || ctx.outside {
            return Err(Error::Forbidden("an agent works only for an active member of the workspace".into()));
        }
        let address = ctx.zone.as_ref().map(|e| e.address.as_str()).unwrap_or_default();
        match a.networks.as_ref().filter(|n| !n.is_empty()) {
            Some(own) => {
                let inside =
                    address.parse::<std::net::IpAddr>().is_ok_and(|ip| own.iter().any(|n| crate::zones::Net::parse(n).is_some_and(|n| n.contains(ip))));
                if !inside {
                    return Err(crate::gate::refusal());
                }
            }
            None => {
                let staff = crate::gate::StaffNetworks::parse(nets.as_ref()).map_err(|_| crate::gate::refusal())?;
                if !staff.admits(address) {
                    crate::gate::count_refusal(staff.mode, crate::zones::Credential::Agent, ctx.tenant, ctx.user, address);
                    if staff.mode == crate::zones::Mode::Enforce {
                        return Err(crate::gate::refusal());
                    }
                }
            }
        }
        let limits = a.limits()?.effective(&ws.caps);
        let minute = Duration::from_secs(60);
        let calls = self.db.count_hit(&format!("agent|{}|{}", ctx.tenant, a.id), minute).await?;
        // Its owner's share is counted only for a call its own limit lets through: one agent looping is refused alone
        // (AG-1-2-fix 7).
        let over = calls > i64::from(limits.calls_per_minute)
            || self.db.count_hit(&format!("agent-owner|{}|{}", ctx.tenant, a.owner_id), minute).await? > i64::from(ws.caps.owner_calls_per_minute);
        if !frozen {
            let counted = if over { "refused" } else { "calls" };
            sqlx::query(AssertSqlSafe(format!(
                "insert into agent_day (tenant_id, agent_id, day, {counted}) values ($1, $2, $3, 1) \
                 on conflict (tenant_id, agent_id, day) do update set {counted} = agent_day.{counted} + 1"
            )))
            .bind(ctx.tenant)
            .bind(a.id)
            .bind(ctx.today())
            .execute(&mut *tx)
            .await?;
            if !over {
                sqlx::query("update agent set last_used_at = now() where tenant_id = $1 and id = $2 and (last_used_at is null or last_used_at < now() - interval '1 minute')")
                    .bind(ctx.tenant)
                    .bind(a.id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        if over {
            return Err(Error::TooManyRequests("this agent, or its owner's agents together, made too many calls this minute".into()));
        }
        as_agent(ctx, &a, &ws)
    }

    /// Agents' days past 400, in maintenance; a workspace held or moving keeps them.
    pub(crate) async fn purge_agent_days(&self) -> Result<u64> {
        self.db
            .purge(&[(
                "agent_day",
                "day < current_date - 400 and not ferrum_workspace_held(tenant_id) and not ferrum_workspace_frozen(tenant_id::text)".into(),
            )])
            .await
    }
}

/// What an owner is told when they try to widen what an administrator chose for their agent.
const ADMINS_CHOICE: &str = "an administrator chose this for your agent: you may narrow it, pause it or revoke it, never widen it";

/// The console is a person's: an agent reads none of it (AG-1-2-fix 8).
fn no_agent(ctx: &Ctx) -> Result<()> {
    match ctx.agent {
        Some(_) => Err(Error::Forbidden("an agent's console is its owner's and administrators', at a screen".into())),
        None => Ok(()),
    }
}

/// Whether `new` grants nothing `old` does not: each verb on each entity given by `old` there or on `*` (an own-records
/// verb by the verb itself too), and capabilities only within those named (none named: every one).
fn narrower(new: &Grants, old: &Grants) -> bool {
    let gives = |entity: &str, verb: &str| [entity, "*"].iter().any(|e| old.entities.get(*e).is_some_and(|v| v.contains(verb)));
    let entities =
        new.entities.iter().all(|(e, verbs)| verbs.iter().all(|v| gives(e, v) || v.strip_suffix(" (own records)").is_some_and(|base| gives(e, base))));
    let covered = |op: &str| old.operations.iter().any(|o| o == op || o.strip_suffix('*').is_some_and(|p| op.starts_with(p)));
    let operations = old.operations.is_empty() || (!new.operations.is_empty() && new.operations.iter().all(|op| covered(op)));
    entities && operations
}

/// `new`'s limits, none above `old`'s: what an owner may set on an administrator's agent.
fn limits_lowered(new: &Effective, old: &Effective) -> Effective {
    Effective {
        calls_per_minute: new.calls_per_minute.min(old.calls_per_minute),
        rows_per_minute: new.rows_per_minute.min(old.rows_per_minute),
        records_per_call: new.records_per_call.min(old.records_per_call),
        asks_per_day: new.asks_per_day.min(old.asks_per_day),
        upload_mb_per_day: new.upload_mb_per_day.min(old.upload_mb_per_day),
        money: old.money.clone(),
    }
}

/// The only place an agent's reach is made: its owner's context narrowed by its grants (named actions kept as narrow as
/// the owner's rules), never an administrator, its data policy the egress of what it reads, a machine where a step-up
/// is asked, made [`Made::Agent`], carrying [`AgentCtx`].
fn as_agent(ctx: &Ctx, a: &Row, ws: &Settings) -> Result<Ctx> {
    let grants = a.grants()?;
    let agent = AgentCtx {
        id: a.id,
        name: a.name.clone(),
        revision: a.revision,
        owner: a.owner_id,
        table: a.table(ws)?,
        operations: grants.operations.clone(),
        limits: a.limits()?.effective(&ws.caps),
        approver_role: a.approver_role.clone().or_else(|| ws.approver_role.clone()),
    };
    Ok(Ctx {
        perms: Arc::new(ctx.perms.limited_to(&grants.entities, &BTreeSet::new(), &ctx.schema)),
        is_admin: false,
        admin_since: None,
        // Never the system, an app or a key's write, whatever context it was built from (AG-1-2-fix 8).
        is_system: false,
        app: None,
        effect_key: None,
        egress: Some(Arc::new(a.data_policy()?)),
        proof: Proof::Machine,
        made: Made::Agent,
        agent: Some(Arc::new(agent)),
        ..ctx.clone()
    })
}

/// Agent `id`, its row locked for this change, if `ctx` may manage it (its owner, an administrator); else not found.
async fn locked(tx: &mut Tx, ctx: &Ctx, id: Uuid) -> Result<Row> {
    let row: Option<Row> = sqlx::query_as(AssertSqlSafe(format!("select {COLUMNS} from agent a where a.tenant_id = $1 and a.id = $2 for update")))
        .bind(ctx.tenant)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    row.filter(|r| r.owner_id == ctx.user || ctx.is_admin).ok_or_else(|| Error::NotFound("agent".into()))
}

/// Agent `id`, if `ctx` may see it (its owner, an administrator); else not found.
async fn seen(tx: &mut Tx, ctx: &Ctx, id: Uuid) -> Result<Row> {
    let row: Option<Row> = sqlx::query_as(AssertSqlSafe(format!("select {COLUMNS} from agent a where a.tenant_id = $1 and a.id = $2")))
        .bind(ctx.tenant)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    row.filter(|r| r.owner_id == ctx.user || ctx.is_admin).ok_or_else(|| Error::NotFound("agent".into()))
}

async fn keys(tx: &mut Tx, tenant: Uuid, agent: Uuid) -> Result<Vec<Value>> {
    Ok(sqlx::query_scalar(
        "select jsonb_build_object('id', id, 'name', name, 'created_at', created_at, 'expires_at', expires_at, 'last_used_at', last_used_at, \
           'revoked_at', revoked_at) from api_key where tenant_id = $1 and agent_id = $2 order by created_at desc",
    )
    .bind(tenant)
    .bind(agent)
    .fetch_all(&mut **tx)
    .await?)
}

/// Agent `id` as the console shows it (§2.11).
async fn detail(k: &Kernel, tx: &mut Tx, ctx: &Ctx, ws: &Settings, id: Uuid) -> Result<Value> {
    let row = seen(tx, ctx, id).await?;
    let owner: Value =
        sqlx::query_scalar("select jsonb_build_object('id', id, 'name', full_name, 'email', email) from app_user where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(row.owner_id)
            .fetch_one(&mut **tx)
            .await?;
    let client = match &row.oauth_client {
        Some(c) => k.oauth_client(c).await.ok().map(|c| c.name),
        None => None,
    };
    let usage: Value = sqlx::query_scalar(
        "select jsonb_build_object('calls', coalesce(sum(calls), 0), 'refused', coalesce(sum(refused), 0), 'asked', coalesce(sum(asked), 0), \
           'upload_bytes', coalesce(sum(upload_bytes), 0)) from agent_day where tenant_id = $1 and agent_id = $2 and day > $3::date - 30",
    )
    .bind(ctx.tenant)
    .bind(id)
    .bind(ctx.today())
    .fetch_one(&mut **tx)
    .await?;
    let admin: Vec<&str> = row.admin_classes.iter().map(String::as_str).collect();
    Ok(json!({
        "id": row.id, "name": row.name, "state": row.state, "revision": row.revision, "owner": owner,
        "created_by": row.created_by, "created_at": row.created_at, "updated_at": row.updated_at, "expires_at": row.expires_at,
        "last_used_at": row.last_used_at, "paused_by": row.paused_by, "revoked_at": row.revoked_at, "revoked_by": row.revoked_by,
        "connected": row.oauth_client.as_ref().map(|c| json!({"oauth_client": c, "client_name": client, "redirect": row.oauth_redirect})),
        "keys": keys(tx, ctx.tenant, id).await?,
        "grants": row.grants,
        "classes": {"set": row.classes, "set_by_administrator": admin, "effective": row.table(ws)?},
        "limits": row.limits()?.effective(&ws.caps),
        "data_policy": row.data_policy, "approver_role": row.approver_role, "networks": row.networks,
        "usage_30d": usage,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owners_class_counts_at_least_as_the_workspaces_now_and_an_administrators_as_set() {
        let mut ws = default_table();
        let set = BTreeMap::from([(Class::External, Decision::NeedsPerson), (Class::Money, Decision::Allowed)]);
        let admin = BTreeSet::from([Class::Money]);
        // The workspace made external forbidden after the owner chose "needs a person": the workspace's holds.
        ws.insert(Class::External, Decision::Forbidden);
        let t = table(&ws, &set, &admin);
        assert_eq!(t[&Class::External], Decision::Forbidden);
        assert_eq!(t[&Class::Money], Decision::Allowed, "an administrator's exception stands");
        // Floors hold whatever was set.
        let t = table(&ws, &BTreeMap::from([(Class::Decides, Decision::Allowed)]), &BTreeSet::from([Class::Decides]));
        assert_eq!(t[&Class::Decides], Decision::NeedsPerson);
    }

    #[test]
    fn limits_never_pass_the_caps() {
        let caps = Caps { calls_per_minute: 60, ..Caps::default() };
        let e = Limits::default().effective(&caps);
        assert_eq!((e.calls_per_minute, e.rows_per_minute, e.records_per_call), (60, 2000, 100));
    }

    #[test]
    fn an_unreadable_setting_lets_nothing_in() {
        assert!(!Settings::of(Some(&json!({"enabled": true, "surprise": 1}))).enabled);
        assert!(Settings::of(None).enabled);
    }

    #[test]
    fn a_setting_keeps_every_floor() {
        let s = Settings::of(Some(&json!({"classes": {"decides": "allowed"}})));
        assert_eq!(s.classes[&Class::Decides], Decision::NeedsPerson);
        assert_eq!(s.classes[&Class::Transactional], Decision::Allowed);
    }
}
