//! Generic document API for every entity: `/api/v1/{entity}[/{id}[/...]]`.

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, if_match};
use crate::state::app_state;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use ferrum_kernel::query::parse_filters;
use ferrum_kernel::value::canonical_json;
use ferrum_kernel::{Action, Doc, Error, ListQuery};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const EXPORT_MAX_ROWS: usize = 100_000;
const IMPORT_MAX_ROWS: usize = 5_000;

#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    pub filters: Option<String>,
    pub fields: Option<String>,
    pub sort: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub cursor: Option<String>,
    pub search: Option<String>,
    pub titles: Option<bool>,
    pub count: Option<bool>,
}

impl ListParams {
    pub fn to_query(&self) -> Result<ListQuery, ApiError> {
        let filters = match self.filters.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => parse_filters(&serde_json::from_str::<Value>(s)?)?,
            _ => vec![],
        };
        let fields = self.fields.as_ref().map(|f| f.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
        Ok(ListQuery {
            filters,
            fields,
            sort: self.sort.clone(),
            limit: self.limit,
            offset: self.offset,
            cursor: self.cursor.clone(),
            search: self.search.clone(),
            with_titles: self.titles.unwrap_or(false),
            with_count: self.count.unwrap_or(false),
        })
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct ReadParams {
    pub titles: Option<bool>,
}

pub fn body_doc(v: Value) -> Result<Doc, ApiError> {
    match v {
        Value::Object(m) => Ok(m),
        _ => Err(ApiError(Error::invalid("request body must be a JSON object"))),
    }
}

/// JSON response with `ETag: "v<version>"` so clients can send it back as `If-Match`.
pub fn doc_response(doc: Doc, status: StatusCode) -> Response {
    let etag = doc.get("version").and_then(Value::as_i64).map(|v| format!("\"v{v}\""));
    let mut resp = (status, Json(Value::Object(doc))).into_response();
    if let Some(e) = etag.and_then(|e| HeaderValue::from_str(&e).ok()) {
        resp.headers_mut().insert(header::ETAG, e);
    }
    resp
}

async fn list(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Query(p): Query<ListParams>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(st.kernel.list(&ctx, &entity, &p.to_query()?).await?).into_response())
}

async fn create(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path(entity): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let doc = body_doc(body)?;
    if let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()) {
        let mut h = Sha256::new();
        h.update(entity.as_bytes());
        h.update(canonical_json(&Value::Object(doc.clone())));
        let (d, replayed) = st.kernel.insert_idempotent(&ctx, &entity, doc, key, &hex::encode(h.finalize())).await?;
        let mut resp = doc_response(d, if replayed { StatusCode::OK } else { StatusCode::CREATED });
        if replayed {
            resp.headers_mut().insert("idempotent-replayed", HeaderValue::from_static("true"));
        }
        return Ok(resp);
    }
    Ok(doc_response(st.kernel.insert(&ctx, &entity, doc).await?, StatusCode::CREATED))
}

async fn read(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>, Query(p): Query<ReadParams>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(doc_response(st.kernel.get(&ctx, &entity, id, p.titles.unwrap_or(true)).await?, StatusCode::OK))
}

async fn update(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path((entity, id)): Path<(String, Uuid)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let expected = if_match(&headers)?;
    Ok(doc_response(st.kernel.update(&ctx, &entity, id, body_doc(body)?, expected).await?, StatusCode::OK))
}

async fn remove(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.kernel.delete(&ctx, &entity, id, if_match(&headers)?).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn submit(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(doc_response(st.kernel.submit(&ctx, &entity, id, if_match(&headers)?).await?, StatusCode::OK))
}

async fn cancel(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(doc_response(st.kernel.cancel(&ctx, &entity, id, if_match(&headers)?).await?, StatusCode::OK))
}

async fn amend(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(doc_response(st.kernel.amend(&ctx, &entity, id).await?, StatusCode::CREATED))
}

async fn action(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path((entity, id, action)): Path<(String, Uuid, String)>,
    body: Option<Json<Value>>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let params = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    Ok(Json(st.kernel.run_action(&ctx, &entity, id, &action, params).await?).into_response())
}

async fn transition(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path((entity, id, action)): Path<(String, Uuid, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(doc_response(st.kernel.transition(&ctx, &entity, id, &action, if_match(&headers)?).await?, StatusCode::OK))
}

async fn print(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let html = st.kernel.render_print(&ctx, &entity, id).await?;
    let mut resp = Html(html).into_response();
    resp.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; img-src data: https:; base-uri 'none'; form-action 'none'"),
    );
    Ok(resp)
}

async fn audit_trail(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    // Reading the record proves read permission (and tenant ownership) before showing its history.
    st.kernel.get(&ctx, &entity, id, false).await?;
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    let rows: Vec<(i64, chrono::DateTime<chrono::Utc>, Option<Uuid>, String, Option<i32>, Option<Value>, Option<String>)> = sqlx::query_as(
        "select e.id, e.at, e.actor, e.action, e.version, e.diff, u.full_name from audit_event e \
         left join app_user u on u.tenant_id = e.tenant_id and u.id = e.actor \
         where e.tenant_id = $1 and e.entity = $2 and e.record_id = $3 order by e.id",
    )
    .bind(ctx.tenant)
    .bind(&entity)
    .bind(id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::from)?;
    let broken_at = ferrum_kernel::audit::verify_record(&mut tx, ctx.tenant, &entity, id).await?;
    tx.commit().await.map_err(Error::from)?;
    let events: Vec<Value> = rows
        .into_iter()
        .map(|(eid, at, actor, action, version, diff, name)| {
            json!({"id": eid, "at": at, "actor": actor, "actor_name": name, "action": action, "version": version, "diff": diff})
        })
        .collect();
    Ok(Json(json!({"events": events, "chain_valid": broken_at.is_none(), "broken_at": broken_at})).into_response())
}

async fn calc(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Json(body): Json<Value>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(Value::Object(st.kernel.preview(&ctx, &entity, body_doc(body)?).await?)).into_response())
}

fn csv_cell(v: &Value) -> String {
    let raw = match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => v.to_string(),
        other => other.to_string(),
    };
    // Neutralize spreadsheet formula injection.
    let safe = if raw.starts_with(['=', '+', '-', '@', '\t', '\r']) && raw.parse::<f64>().is_err() { format!("'{raw}") } else { raw };
    if safe.contains([',', '"', '\n', '\r']) { format!("\"{}\"", safe.replace('"', "\"\"")) } else { safe }
}

async fn export(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Query(p): Query<ListParams>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    ctx.require(&entity, Action::Export)?;
    let ent = st.kernel.entity(&ctx, &entity)?;
    let mut q = p.to_query()?;
    q.limit = Some(500);
    q.offset = None;
    q.sort = Some("created_at desc".into());
    q.with_count = false;
    let columns: Vec<String> = match &q.fields {
        Some(f) => f.clone(),
        None => std::iter::once("number".to_string()).chain(ent.value_fields().filter(|f| !f.hidden).map(|f| f.name.clone())).collect(),
    };
    let mut out = String::new();
    out.push_str(&columns.iter().map(|c| csv_cell(&Value::String(c.clone()))).collect::<Vec<_>>().join(","));
    out.push('\n');
    let mut rows = 0usize;
    loop {
        let page = st.kernel.list(&ctx, &entity, &q).await?;
        for r in &page.rows {
            out.push_str(&columns.iter().map(|c| csv_cell(r.get(c).unwrap_or(&Value::Null))).collect::<Vec<_>>().join(","));
            out.push('\n');
            rows += 1;
        }
        match page.next_cursor {
            Some(c) if rows < EXPORT_MAX_ROWS => q.cursor = Some(c),
            _ => break,
        }
    }
    let mut resp = out.into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/csv; charset=utf-8"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{entity}.csv\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

async fn import(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Json(body): Json<Value>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    ctx.require(&entity, Action::Import)?;
    let rows = body.as_array().ok_or_else(|| ApiError(Error::invalid("expected a JSON array of records")))?;
    if rows.len() > IMPORT_MAX_ROWS {
        return Err(ApiError(Error::invalid(format!("at most {IMPORT_MAX_ROWS} rows per request"))));
    }
    let mut created = Vec::new();
    let mut errors = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let Some(doc) = r.as_object().cloned() else {
            errors.push(json!({"row": i, "error": "not an object"}));
            continue;
        };
        match st.kernel.insert(&ctx, &entity, doc).await {
            Ok(d) => created.push(d.get("id").cloned().unwrap_or(Value::Null)),
            Err(Error::Validation(f)) => errors.push(json!({"row": i, "error": "validation_failed", "fields": f})),
            Err(e) => errors.push(json!({"row": i, "error": e.code(), "message": e.to_string()})),
        }
    }
    let status = if errors.is_empty() { StatusCode::CREATED } else { StatusCode::MULTI_STATUS };
    Ok((status, Json(json!({"created": created.len(), "ids": created, "errors": errors}))).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{get, post};
    Routes::new()
        .prefix("/api/v1")
        .add("/{entity}", get(list).post(create))
        .add("/{entity}/_calc", post(calc))
        .add("/{entity}/_export", get(export))
        .add("/{entity}/_import", post(import))
        .add("/{entity}/{id}", get(read).patch(update).put(update).delete(remove))
        .add("/{entity}/{id}/submit", post(submit))
        .add("/{entity}/{id}/cancel", post(cancel))
        .add("/{entity}/{id}/amend", post(amend))
        .add("/{entity}/{id}/actions/{action}", post(action))
        .add("/{entity}/{id}/workflow/{action}", post(transition))
        .add("/{entity}/{id}/print", get(print))
        .add("/{entity}/{id}/audit", get(audit_trail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_cells_are_safe() {
        assert_eq!(csv_cell(&json!("a,b")), "\"a,b\"");
        assert_eq!(csv_cell(&json!("=SUM(A1)")), "'=SUM(A1)");
        assert_eq!(csv_cell(&json!("-12.5")), "-12.5");
        assert_eq!(csv_cell(&json!("say \"hi\"")), "\"say \"\"hi\"\"\"");
    }
}
