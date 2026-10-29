//! HTTP endpoints under `/api/app/studio/` (signed-in users; there are no public ones).
//!
//! | Method & path | Body / query → response |
//! |---|---|
//! | `POST /query` | a spec → `{columns, rows, totals, meta}` |
//! | `GET /fields/{entity}` | groupable/aggregatable fields, functions, buckets |
//! | `GET /sources` | built-in report catalog (chart sources) and readable entities |
//! | `GET /reports`, `/charts`, `/cards`, `/dashboards`, `/spaces` | records the caller may open (`?module=`, `?entity=`) |
//! | `GET\|POST /reports/{id}/run` | `{filters?, search?, sort?, limit?, from?, to?}` → `{report, data}` |
//! | `GET\|POST /reports/{id}/export` | same, `format=csv` → a CSV file |
//! | `GET\|POST /charts/{id}/data`, `/cards/{id}/data` | runtime settings, `dashboard?` + `values?` |
//! | `POST /charts/preview`, `/cards/preview` | `{record, filter_defs?, values?, ...}` for the builder |
//! | `GET\|POST /dashboards/{id}/data` | `{values?}` (dashboard filter values) → every widget |
//! | `GET /spaces/{id}/data` | every block, resolved for the caller |
//!
//! GET variants take the same settings as query parameters (`filters`, `values`, `sort` as JSON).

use crate::builder::{self, CARD, CHART, DASHBOARD, REPORT, Runtime, SPACE, WidgetRequest};
use crate::query;
use crate::run::run;
use crate::spec::Spec;
use async_trait::async_trait;
use ferrum_kernel::{Access, Action, Ctx, Error, Kernel, Result, Site, SiteRequest, SiteResponse};
use ferrum_meta::EntityKind;
use serde_json::{Map, Value, json};
use uuid::Uuid;

pub struct StudioSite;

fn ok(v: Value) -> SiteResponse {
    SiteResponse::json(200, &v)
}

fn uuid(seg: &str) -> Result<Uuid> {
    seg.parse().map_err(|_| Error::NotFound("record".into()))
}

fn body(req: &SiteRequest) -> Result<Value> {
    if req.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&req.body).map_err(|e| Error::invalid(format!("the request body must be JSON: {e}")))
}

fn param<'a>(req: &'a SiteRequest, k: &str) -> Option<&'a str> {
    req.query.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

/// The request's settings: the JSON body, or for GET the query parameters (JSON-valued ones
/// parsed).
fn settings(req: &SiteRequest) -> Result<Value> {
    if req.method != "GET" {
        return body(req);
    }
    let mut m = Map::new();
    for (k, v) in &req.query {
        let Some(text) = v.as_str() else { continue };
        let value = match k.as_str() {
            "format" => continue,
            "filters" | "values" | "sort" => serde_json::from_str(text).map_err(|e| Error::field(k, format!("must be JSON: {e}")))?,
            "limit" => json!(text.parse::<u32>().map_err(|_| Error::field("limit", "must be a whole number"))?),
            _ => json!(text),
        };
        m.insert(k.clone(), value);
    }
    Ok(Value::Object(m))
}

fn kind(seg: &str) -> Option<&'static str> {
    Some(match seg {
        "reports" => REPORT,
        "charts" => CHART,
        "cards" => CARD,
        "dashboards" => DASHBOARD,
        "spaces" => SPACE,
        _ => return None,
    })
}

/// What can feed a chart: the built-in reports and the entities the caller can query.
fn sources(ctx: &Ctx) -> Value {
    let entities: Vec<Value> = ctx
        .schema
        .entities
        .values()
        .filter(|e| !e.table.is_empty() && !matches!(e.def.kind, EntityKind::Child | EntityKind::Ledger))
        .filter(|e| ctx.perms.access(&e.def.name, Action::Read) != Access::None)
        .map(|e| json!({"name": e.def.name, "label": e.def.label, "module": e.def.module}))
        .collect();
    json!({"builtin": ferrum_domain::reports::catalog_for(ctx), "entities": entities})
}

#[async_trait]
impl Site for StudioSite {
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        let Some(ctx) = req.user.as_ref() else { return Err(Error::NotFound("page".into())) };
        let path: Vec<&str> = req.path.split('/').filter(|p| !p.is_empty()).collect();
        let m = req.method.as_str();
        Ok(match (m, path.as_slice()) {
            ("POST", ["query"]) => ok(run(k, ctx, &Spec::parse(&body(&req)?)?).await?),
            ("GET", ["fields", entity]) => ok(query::fields(ctx, entity)?),
            ("GET", ["sources"]) => ok(sources(ctx)),
            ("GET", [seg]) if kind(seg).is_some() => {
                ok(builder::list(k, ctx, kind(seg).unwrap_or(REPORT), param(&req, "module"), param(&req, "entity")).await?)
            }
            ("GET" | "POST", ["reports", id, "run"]) => ok(builder::run_report(k, ctx, uuid(id)?, &Runtime::parse(&settings(&req)?)?).await?),
            ("GET" | "POST", ["reports", id, "export"]) => {
                match param(&req, "format").unwrap_or("csv") {
                    "csv" => {}
                    "xlsx" => return Err(Error::field("format", "xlsx export is not available in this build; use csv")),
                    other => return Err(Error::field("format", format!("unknown format '{other}'; use csv"))),
                }
                let (file, text) = builder::export_report(k, ctx, uuid(id)?, &Runtime::parse(&settings(&req)?)?).await?;
                SiteResponse {
                    status: 200,
                    content_type: "text/csv; charset=utf-8".into(),
                    headers: vec![("content-disposition".into(), format!("attachment; filename=\"{file}\"")), ("cache-control".into(), "no-store".into())],
                    body: text.into_bytes(),
                }
            }
            ("POST", [seg @ ("charts" | "cards"), "preview"]) => {
                let entity = kind(seg).unwrap_or(CHART);
                ok(builder::widget_data(k, ctx, entity, None, &WidgetRequest::parse(&body(&req)?)?).await?)
            }
            ("GET" | "POST", [seg @ ("charts" | "cards"), id, "data"]) => {
                let entity = kind(seg).unwrap_or(CHART);
                ok(builder::widget_data(k, ctx, entity, Some(uuid(id)?), &WidgetRequest::parse(&settings(&req)?)?).await?)
            }
            ("GET" | "POST", ["dashboards", id, "data"]) => {
                let s = settings(&req)?;
                let values = match s.get("values") {
                    None | Some(Value::Null) => Map::new(),
                    Some(Value::Object(o)) => o.clone(),
                    Some(_) => return Err(Error::field("values", "must be an object of dashboard filter values")),
                };
                if let Some(extra) = s.as_object().and_then(|o| o.keys().find(|k| k.as_str() != "values")) {
                    return Err(Error::field(extra, "unknown setting; send dashboard filter values as {\"values\": {...}}"));
                }
                ok(builder::dashboard_data(k, ctx, uuid(id)?, &values).await?)
            }
            ("GET", ["spaces", id, "data"]) => ok(builder::space_data(k, ctx, uuid(id)?).await?),
            _ => return Err(Error::NotFound("endpoint".into())),
        })
    }
}
