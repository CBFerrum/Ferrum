//! SCIM 2.0 over HTTP (RFC 7644): `/api/scim/v2/…`, for an identity provider that keeps a workspace's
//! people and groups. It signs in with an API key of scope `scim`, made by an administrator, and
//! acts as that administrator; a key of any other kind does not work here. What each request does
//! is in the kernel (crates/kernel/src/scim.rs); this is the protocol's shape: its media type, its
//! error messages and its discovery endpoints. See docs/identity-lifecycle.md.

use crate::extract::{bearer, client_ip, request_id};
use crate::state::app_state;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::scim::{GROUP_URN, MAX_PAGE, USER_URN};
use ferrum_kernel::{Ctx, Error, Kernel};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

const MEDIA_TYPE: &str = "application/scim+json";
const ERROR_URN: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
/// The field of the error for a body that is not JSON.
const NOT_JSON: &str = "body";

fn scim(status: StatusCode, body: &Value) -> Response {
    let mut r = (status, body.to_string()).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(MEDIA_TYPE));
    r
}

/// RFC 7644 §3.12: an error in SCIM's own shape. What the rest of the API answers with 422, SCIM
/// answers with 400 and a `scimType`.
fn failure(e: Error) -> Response {
    let scim_type = match &e {
        Error::Validation(f) if f.first().is_some_and(|f| f.field == "filter") => Some("invalidFilter"),
        Error::Validation(f) if f.first().is_some_and(|f| f.field == NOT_JSON) => Some("invalidSyntax"),
        Error::Validation(_) | Error::Invalid(_) => Some("invalidValue"),
        Error::Conflict(_) => Some("uniqueness"),
        _ => None,
    };
    let detail = match &e {
        Error::Validation(fields) => fields.iter().map(|f| format!("{} {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        // These messages are the provider's to act on; internal ones stay in the log.
        Error::Db(_) | Error::Internal(_) => {
            let id = Uuid::now_v7();
            tracing::error!(error_id = %id, "SCIM request failed: {e}");
            format!("an internal error occurred ({id})")
        }
        other => other.to_string(),
    };
    let status = match scim_type {
        Some("uniqueness") => StatusCode::CONFLICT,
        Some(_) => StatusCode::BAD_REQUEST,
        None => StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
    };
    let mut body = json!({"schemas": [ERROR_URN], "status": status.as_u16().to_string(), "detail": detail});
    if let Some(t) = scim_type {
        body["scimType"] = json!(t);
    }
    scim(status, &body)
}

/// The administrator a request's key acts as.
async fn signed_in(app: &AppContext, headers: &HeaderMap) -> ferrum_kernel::Result<(Arc<Kernel>, Ctx)> {
    let st = app_state(app).map_err(|e| e.0)?;
    let refused = || Error::Unauthorized("use an API key of scope scim".into());
    let attempt = async {
        let token = bearer(headers).ok_or_else(refused)?;
        let (session, scope) = st.kernel.verify_api_key(&token).await.map_err(|_| refused())?;
        if scope.as_deref() != Some("scim") {
            return Err(refused());
        }
        st.user_limiter.check_local(&format!("{}|{}", session.tenant, session.user))?;
        st.kernel.load_ctx(session.tenant, session.user, Some(session.token_version), request_id(headers)).await
    };
    match attempt.await {
        Ok(ctx) if ctx.is_admin => Ok((st.kernel.clone(), ctx)),
        Ok(_) => Err(Error::Forbidden("this key's owner is no longer an administrator".into())),
        Err(e @ Error::Unauthorized(_)) => {
            // Wrong keys count against the address, like failed sign-ins.
            st.login_limiter.check(&format!("{}|scim", client_ip(headers))).await?;
            Err(e)
        }
        Err(e) => Err(e),
    }
}

/// Every request's shape: sign in, do `work`, answer in SCIM's terms. `Value::Null` means there is
/// nothing to say (204).
async fn handled<F, Fut>(app: &AppContext, headers: &HeaderMap, status: StatusCode, work: F) -> Response
where
    F: FnOnce(Arc<Kernel>, Ctx) -> Fut,
    Fut: Future<Output = ferrum_kernel::Result<Value>>,
{
    let done = match signed_in(app, headers).await {
        Ok((k, ctx)) => work(k, ctx).await,
        Err(e) => Err(e),
    };
    match done {
        Ok(Value::Null) => StatusCode::NO_CONTENT.into_response(),
        Ok(v) => scim(status, &v),
        Err(e) => failure(e),
    }
}

fn body_of(bytes: &Bytes) -> ferrum_kernel::Result<Value> {
    serde_json::from_slice(bytes).map_err(|e| Error::field(NOT_JSON, e.to_string()))
}

/// A resource's id; one that is no id names nothing.
fn id_of(raw: &str) -> ferrum_kernel::Result<Uuid> {
    raw.parse().map_err(|_| Error::NotFound("resource".into()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListParams {
    filter: Option<String>,
    start_index: Option<i64>,
    count: Option<i64>,
}

async fn list_users(State(app): State<AppContext>, headers: HeaderMap, Query(q): Query<ListParams>) -> Response {
    handled(&app, &headers, StatusCode::OK, |k, ctx| async move { k.scim_users(&ctx, q.filter.as_deref(), q.start_index, q.count).await }).await
}

async fn create_user(State(app): State<AppContext>, headers: HeaderMap, bytes: Bytes) -> Response {
    handled(&app, &headers, StatusCode::CREATED, |k, ctx| async move { k.scim_create_user(&ctx, &body_of(&bytes)?).await }).await
}

async fn get_user(State(app): State<AppContext>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    handled(&app, &headers, StatusCode::OK, |k, ctx| async move { k.scim_user(&ctx, id_of(&id)?).await }).await
}

async fn replace_user(State(app): State<AppContext>, headers: HeaderMap, Path(id): Path<String>, bytes: Bytes) -> Response {
    handled(&app, &headers, StatusCode::OK, |k, ctx| async move { k.scim_replace_user(&ctx, id_of(&id)?, &body_of(&bytes)?).await }).await
}

async fn patch_user(State(app): State<AppContext>, headers: HeaderMap, Path(id): Path<String>, bytes: Bytes) -> Response {
    handled(&app, &headers, StatusCode::OK, |k, ctx| async move { k.scim_patch_user(&ctx, id_of(&id)?, &body_of(&bytes)?).await }).await
}

async fn delete_user(State(app): State<AppContext>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    handled(&app, &headers, StatusCode::NO_CONTENT, |k, ctx| async move { k.scim_delete_user(&ctx, id_of(&id)?).await.map(|()| Value::Null) }).await
}

async fn list_groups(State(app): State<AppContext>, headers: HeaderMap, Query(q): Query<ListParams>) -> Response {
    handled(&app, &headers, StatusCode::OK, |k, ctx| async move { k.scim_groups(&ctx, q.filter.as_deref(), q.start_index, q.count).await }).await
}

async fn create_group(State(app): State<AppContext>, headers: HeaderMap, bytes: Bytes) -> Response {
    handled(&app, &headers, StatusCode::CREATED, |k, ctx| async move { k.scim_create_group(&ctx, &body_of(&bytes)?).await }).await
}

async fn get_group(State(app): State<AppContext>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    handled(&app, &headers, StatusCode::OK, |k, ctx| async move { k.scim_group(&ctx, id_of(&id)?).await }).await
}

async fn replace_group(State(app): State<AppContext>, headers: HeaderMap, Path(id): Path<String>, bytes: Bytes) -> Response {
    handled(&app, &headers, StatusCode::OK, |k, ctx| async move { k.scim_replace_group(&ctx, id_of(&id)?, &body_of(&bytes)?).await }).await
}

async fn patch_group(State(app): State<AppContext>, headers: HeaderMap, Path(id): Path<String>, bytes: Bytes) -> Response {
    handled(&app, &headers, StatusCode::OK, |k, ctx| async move { k.scim_patch_group(&ctx, id_of(&id)?, &body_of(&bytes)?).await }).await
}

async fn delete_group(State(app): State<AppContext>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    handled(&app, &headers, StatusCode::NO_CONTENT, |k, ctx| async move { k.scim_delete_group(&ctx, id_of(&id)?).await.map(|()| Value::Null) }).await
}

async fn service_provider_config() -> Response {
    let off = json!({"supported": false});
    scim(
        StatusCode::OK,
        &json!({
            "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
            "documentationUri": "https://github.com/CBFerrum/Ferrum/blob/main/docs/identity-lifecycle.md",
            "patch": {"supported": true}, "bulk": {"supported": false, "maxOperations": 0, "maxPayloadSize": 0},
            "filter": {"supported": true, "maxResults": MAX_PAGE}, "changePassword": off, "sort": off, "etag": off,
            "authenticationSchemes": [{"type": "oauthbearertoken", "name": "API key", "description": "An API key of scope scim, sent as a bearer token"}],
        }),
    )
}

async fn resource_types() -> Response {
    let kind = |name: &str, plural: &str, urn: &str| json!({"schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"], "id": name, "name": name, "endpoint": plural, "schema": urn});
    scim(
        StatusCode::OK,
        &json!({"schemas": ["urn:ietf:params:scim:api:messages:2.0:ListResponse"], "totalResults": 2, "Resources": [kind("User", "/Users", USER_URN), kind("Group", "/Groups", GROUP_URN)]}),
    )
}

async fn schemas() -> Response {
    let attr = |name: &str, kind: &str, required: bool, mutability: &str| json!({"name": name, "type": kind, "multiValued": false, "required": required, "caseExact": false, "mutability": mutability, "returned": "default", "uniqueness": "none"});
    let user = json!({"id": USER_URN, "name": "User", "description": "A member of the workspace", "attributes": [
        attr("userName", "string", true, "readWrite"), attr("displayName", "string", false, "readWrite"),
        attr("externalId", "string", false, "readWrite"), attr("active", "boolean", false, "readWrite"),
    ]});
    let group = json!({"id": GROUP_URN, "name": "Group", "description": "A group at the identity provider", "attributes": [
        attr("displayName", "string", true, "readWrite"), attr("externalId", "string", false, "readWrite"),
    ]});
    scim(StatusCode::OK, &json!({"schemas": ["urn:ietf:params:scim:api:messages:2.0:ListResponse"], "totalResults": 2, "Resources": [user, group]}))
}

pub fn routes() -> Routes {
    use axum::routing::get;
    Routes::new()
        .prefix("/api/scim/v2")
        .add("/ServiceProviderConfig", get(service_provider_config))
        .add("/ResourceTypes", get(resource_types))
        .add("/Schemas", get(schemas))
        .add("/Users", get(list_users).post(create_user))
        .add("/Users/{id}", get(get_user).put(replace_user).patch(patch_user).delete(delete_user))
        .add("/Groups", get(list_groups).post(create_group))
        .add("/Groups/{id}", get(get_group).put(replace_group).patch(patch_group).delete(delete_group))
}
