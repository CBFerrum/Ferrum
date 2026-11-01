//! HTTP endpoints.
//!
//! Under `/api/app/ops/`, for signed-in people:
//!
//! | Method & path | Who | What |
//! |---|---|---|
//! | `GET /overview` | anyone with a role here | every site: its gateway, what is waiting, what needs a person |
//! | `GET /commands?site=&state=&needs_person=1&limit=` | | commands, newest first |
//! | `POST /commands` | Ops Dispatcher | a [`NewCommand`] |
//! | `GET /commands/{id}` | | a command and everything said about it |
//! | `POST /commands/{id}/approve`, `/refuse` | Ops Manager | `{reason}` for a refusal |
//! | `POST /commands/{id}/cancel` | Ops Dispatcher | `{reason}`: a request, unless it never left |
//! | `POST /commands/{id}/reconcile` | Ops Recovery | `{to, evidence}` |
//! | `GET /authority` | | who holds each domain |
//! | `POST /authority/transfer` | Device Administrator | `{domain, to, reason}` |
//! | `POST /devices/{id}/enrollment` | Device Administrator | a one-time code |
//! | `POST /devices/{id}/lifecycle` | Device Administrator | `{to, reason}` |
//! | `GET /devices/{id}/latest` | | each metric's newest value and its age |
//! | `GET /series?device=&metric=&from=&to=&bucket=` | | minute rollups |
//! | `POST /operators/pin` | Device Administrator | `{site, user, pin}`: for the site's console, offline |
//!
//! Under `/api/site/{tenant}/ops/`, for a site runtime, which signs what it sends:
//! `POST /enroll` with a code, and `POST /sync`.

use crate::commands::{self, NewCommand};
use crate::identity::{self, Refused};
use crate::{authority, devices, sync, telemetry};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use ferrum_kernel::{Ctx, Error, Kernel, Result, Site, SiteRequest, SiteResponse};
use ferrum_ops_contract::sign::Enroll;
use ferrum_ops_contract::{Lifecycle, MAX_SYNC_BYTES, State, SyncRequest};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

pub struct OpsSite;

fn ok(v: Value) -> SiteResponse {
    SiteResponse::json(200, &v)
}

fn body<T: DeserializeOwned>(req: &SiteRequest) -> Result<T> {
    serde_json::from_slice(&req.body).map_err(|e| Error::invalid(format!("the request body: {e}")))
}

fn uuid(seg: &str) -> Result<Uuid> {
    seg.parse().map_err(|_| Error::NotFound("record".into()))
}

fn param<'a>(req: &'a SiteRequest, k: &str) -> Option<&'a str> {
    req.query.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

fn time(req: &SiteRequest, k: &str) -> Result<Option<DateTime<Utc>>> {
    param(req, k).map(|v| v.parse().map_err(|_| Error::field(k, "must be an RFC 3339 time"))).transpose()
}

/// Roles that each kind of act needs. Read, dispatch, configuration and recovery are separate
/// privileges: who may move a robot need not be who may change what it is allowed to do.
fn may(ctx: &Ctx, roles: &[&str]) -> Result<()> {
    if ctx.is_admin || roles.iter().any(|r| ctx.has_role(r)) {
        return Ok(());
    }
    Err(Error::Forbidden(format!("this takes one of: {}", roles.join(", "))))
}

const READ: &[&str] = &["Ops Viewer", "Ops Dispatcher", "Ops Recovery", "Device Administrator", "Ops Manager"];
const DISPATCH: &[&str] = &["Ops Dispatcher", "Ops Manager"];
const RECOVER: &[&str] = &["Ops Recovery", "Ops Manager"];
const CONFIGURE: &[&str] = &["Device Administrator", "Ops Manager"];
const APPROVE: &[&str] = &["Ops Manager"];

/// A site marked members-only is closed to everyone not listed, whatever their roles.
async fn open_sites(k: &Kernel, ctx: &Ctx) -> Result<Option<Vec<Uuid>>> {
    if ctx.is_admin {
        return Ok(None);
    }
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "select s.id from e_site s where s.tenant_id = $1 and (not coalesce(s.restricted, false) or exists ( \
           select 1 from e_site__members m where m.tenant_id = s.tenant_id and m.parent_id = s.id and lower(m.user_email) = lower($2)))",
    )
    .bind(ctx.tenant)
    .bind(&ctx.email)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(ids))
}

async fn site_open(k: &Kernel, ctx: &Ctx, site: Uuid) -> Result<()> {
    match open_sites(k, ctx).await? {
        Some(ids) if !ids.contains(&site) => Err(Error::NotFound("site".into())),
        _ => Ok(()),
    }
}

async fn command_site(k: &Kernel, ctx: &Ctx, id: Uuid) -> Result<()> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let site: Option<Uuid> = sqlx::query_scalar("select site_id from op_command where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    site_open(k, ctx, site.ok_or_else(|| Error::NotFound("command".into()))?).await
}

async fn device_site(k: &Kernel, ctx: &Ctx, id: Uuid) -> Result<()> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let site: Option<Option<Uuid>> = sqlx::query_scalar("select site from e_device where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    match site.ok_or_else(|| Error::NotFound("device".into()))? {
        Some(site) => site_open(k, ctx, site).await,
        None => Ok(()),
    }
}

/// Every site at a glance. What matters first: is its gateway there, and does anything need a
/// person.
async fn overview(k: &Kernel, ctx: &Ctx) -> Result<Value> {
    let open = open_sites(k, ctx).await?;
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "select jsonb_build_object('id', s.id, 'name', s.site_name, 'code', s.site_code, 'status', s.status, 'max_offline_hours', s.max_offline_hours, \
           'domains', coalesce((select jsonb_agg(jsonb_build_object('domain', a.domain, 'epoch', a.epoch, 'holder', a.holder, 'holder_name', d.device_name, \
              'last_seen_at', g.last_seen_at, 'health', g.health, 'silent_secs', extract(epoch from now() - g.last_seen_at)::int8)) \
             from op_authority a left join e_device d on d.tenant_id = a.tenant_id and d.id = a.holder \
             left join op_gateway g on g.tenant_id = a.tenant_id and g.device_id = a.holder where a.tenant_id = s.tenant_id and a.site_id = s.id), '[]'::jsonb), \
           'waiting', (select count(*) from op_command c where c.tenant_id = s.tenant_id and c.site_id = s.id and c.state = 'Queued'), \
           'running', (select count(*) from op_command c where c.tenant_id = s.tenant_id and c.site_id = s.id and c.state in ('Accepted', 'Executing')), \
           'needs_person', (select count(*) from op_command c where c.tenant_id = s.tenant_id and c.site_id = s.id and (c.state in ('Proposed', 'OutcomeUnknown') or c.conflict)), \
           'devices', (select jsonb_object_agg(lifecycle, n) from (select d.lifecycle, count(*) n from e_device d where d.tenant_id = s.tenant_id and d.site = s.id group by 1) x)) \
         from e_site s where s.tenant_id = $1 and ($2::uuid[] is null or s.id = any($2)) order by s.site_name",
    )
    .bind(ctx.tenant)
    .bind(open.as_deref())
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({"sites": rows, "server_time": Utc::now()}))
}

#[derive(Deserialize)]
struct Reason {
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct Reconcile {
    to: State,
    evidence: String,
}

#[derive(Deserialize)]
struct Transfer {
    domain: String,
    to: Uuid,
    reason: String,
}

#[derive(Deserialize)]
struct SetLifecycle {
    to: Lifecycle,
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct Pin {
    site: Uuid,
    user: Uuid,
    pin: String,
}

async fn signed_in(k: &Kernel, ctx: &Ctx, req: &SiteRequest) -> Result<SiteResponse> {
    let segments: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    match (req.method.as_str(), segments.as_slice()) {
        ("GET", ["overview"]) => {
            may(ctx, READ)?;
            Ok(ok(overview(k, ctx).await?))
        }
        ("GET", ["commands"]) => {
            may(ctx, READ)?;
            let open = open_sites(k, ctx).await?;
            let site = param(req, "site").map(uuid).transpose()?;
            let sites = match (site, open) {
                (Some(s), Some(open)) if !open.contains(&s) => return Err(Error::NotFound("site".into())),
                (Some(s), _) => Some(vec![s]),
                (None, open) => open,
            };
            let limit = param(req, "limit").and_then(|v| v.parse().ok()).unwrap_or(100);
            Ok(ok(json!({"commands": commands::list(k, ctx, sites.as_deref(), param(req, "state"), param(req, "needs_person").is_some(), limit).await?})))
        }
        ("POST", ["commands"]) => {
            may(ctx, DISPATCH)?;
            let cmd: NewCommand = body(req)?;
            device_site(k, ctx, cmd.target).await?;
            // Through these endpoints, whoever signed in is a person or an agent acting as one.
            let cmd = if ctx.request_id.starts_with("mcp") { NewCommand { issuer: commands::Issuer::Ai, ..cmd } } else { NewCommand { issuer: commands::Issuer::Person, ..cmd } };
            let mut tx = k.db.tenant_tx(ctx.tenant).await?;
            let issued = commands::issue(&mut tx, ctx, cmd).await?;
            tx.commit().await?;
            Ok(SiteResponse::json(if issued.repeated { 200 } else { 201 }, &json!({"id": issued.id, "state": issued.state.as_str(), "repeated": issued.repeated})))
        }
        ("GET", ["commands", id]) => {
            may(ctx, READ)?;
            let id = uuid(id)?;
            command_site(k, ctx, id).await?;
            Ok(ok(commands::describe(k, ctx, id).await?))
        }
        ("POST", ["commands", id, act @ ("approve" | "refuse" | "cancel" | "reconcile")]) => {
            let id = uuid(id)?;
            may(ctx, match *act {
                "approve" | "refuse" => APPROVE,
                "cancel" => DISPATCH,
                _ => RECOVER,
            })?;
            command_site(k, ctx, id).await?;
            let mut tx = k.db.tenant_tx(ctx.tenant).await?;
            let state = match *act {
                "approve" => Some(commands::approve(&mut tx, ctx, id).await?),
                "refuse" => {
                    let Reason { reason } = body(req)?;
                    commands::refuse(&mut tx, ctx, id, &reason).await?;
                    Some(State::Refused)
                }
                "cancel" => {
                    let Reason { reason } = body(req)?;
                    Some(commands::cancel(&mut tx, ctx, id, &reason).await?)
                }
                _ => {
                    let r: Reconcile = body(req)?;
                    commands::reconcile(&mut tx, ctx, id, r.to, &r.evidence).await?;
                    Some(r.to)
                }
            };
            tx.commit().await?;
            Ok(ok(json!({"state": state.map(State::as_str)})))
        }
        ("GET", ["authority"]) => {
            may(ctx, READ)?;
            Ok(ok(json!({"domains": authority::list(k, ctx).await?})))
        }
        ("POST", ["authority", "transfer"]) => {
            may(ctx, CONFIGURE)?;
            let t: Transfer = body(req)?;
            device_site(k, ctx, t.to).await?;
            Ok(ok(json!({"epoch": authority::transfer(k, ctx, &t.domain, t.to, &t.reason).await?})))
        }
        ("POST", ["devices", id, "enrollment"]) => {
            may(ctx, CONFIGURE)?;
            let id = uuid(id)?;
            device_site(k, ctx, id).await?;
            let (code, expires) = identity::new_enrollment(k, ctx, id).await?;
            Ok(ok(json!({"code": code, "expires_at": expires})))
        }
        ("POST", ["devices", id, "lifecycle"]) => {
            may(ctx, CONFIGURE)?;
            let id = uuid(id)?;
            device_site(k, ctx, id).await?;
            let s: SetLifecycle = body(req)?;
            let to = devices::set_lifecycle(k, ctx, id, s.to, &s.reason).await?;
            Ok(ok(json!({"lifecycle": to.as_str()})))
        }
        ("GET", ["devices", id, "latest"]) => {
            may(ctx, READ)?;
            let id = uuid(id)?;
            device_site(k, ctx, id).await?;
            Ok(ok(json!({"metrics": telemetry::latest(k, ctx, id).await?})))
        }
        ("GET", ["series"]) => {
            may(ctx, READ)?;
            let device = uuid(param(req, "device").ok_or_else(|| Error::field("device", "is required"))?)?;
            device_site(k, ctx, device).await?;
            let metric = param(req, "metric").ok_or_else(|| Error::field("metric", "is required"))?;
            let to = time(req, "to")?.unwrap_or_else(Utc::now);
            let from = time(req, "from")?.unwrap_or(to - Duration::hours(24));
            let bucket = param(req, "bucket").and_then(|v| v.parse().ok()).unwrap_or(1);
            Ok(ok(json!({"series": telemetry::series(k, ctx, device, metric, from, to, bucket).await?})))
        }
        ("POST", ["operators", "pin"]) => {
            may(ctx, CONFIGURE)?;
            let p: Pin = body(req)?;
            site_open(k, ctx, p.site).await?;
            if p.pin.len() < 6 || !p.pin.chars().all(|c| c.is_ascii_digit()) {
                return Err(Error::field("pin", "at least six digits"));
            }
            let mut tx = k.db.tenant_tx(ctx.tenant).await?;
            sqlx::query(
                "insert into op_operator_pin (tenant_id, site_id, user_id, pin_hash, set_by) values ($1, $2, $3, $4, $5) \
                 on conflict (tenant_id, site_id, user_id) do update set pin_hash = excluded.pin_hash, set_by = excluded.set_by, set_at = now()",
            )
            .bind(ctx.tenant)
            .bind(p.site)
            .bind(p.user)
            .bind(identity::pin_hash(&p.pin))
            .bind(ctx.user)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(SiteResponse::json(204, &Value::Null))
        }
        _ => Err(Error::NotFound("route".into())),
    }
}

async fn public(k: &Kernel, req: &SiteRequest) -> Result<SiteResponse> {
    match (req.method.as_str(), req.path.trim_matches('/')) {
        ("POST", "enroll") => {
            let e: Enroll = body(req)?;
            Ok(ok(serde_json::to_value(identity::enroll(k, req.tenant, &e).await?).map_err(|e| Error::internal(e.to_string()))?))
        }
        ("POST", "sync") => match identity::verify(k, req).await {
            Ok(device) => {
                let request: SyncRequest = body(req)?;
                let response = sync::exchange(k, req.tenant, device, request).await?;
                Ok(ok(serde_json::to_value(response).map_err(|e| Error::internal(e.to_string()))?))
            }
            Err(Refused::Clock(now)) => Ok(SiteResponse::json(401, &json!({"error": "this site's clock is too far from the server's", "server_time": now}))),
            Err(Refused::Key(why)) => Ok(SiteResponse::json(401, &json!({"error": why}))),
            Err(Refused::Other(e)) => Err(e),
        },
        _ => Err(Error::NotFound("route".into())),
    }
}

#[async_trait]
impl Site for OpsSite {
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        match &req.user {
            Some(ctx) => signed_in(k, ctx, &req).await,
            None => public(k, &req).await,
        }
    }

    fn max_body_for(&self, _method: &str, path: &str) -> usize {
        if path.trim_matches('/') == "sync" { MAX_SYNC_BYTES } else { 64 << 10 }
    }
}
