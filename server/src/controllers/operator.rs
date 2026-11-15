//! The installation's settings for its operator, over HTTP (docs/api.md, "Settings"; the Operations screen calls these):
//! read them, set one, unset one. Allowed with the operator's token (`X-Ferrum-Operator-Token`, the secret
//! `FERRUM_OPERATOR_TOKEN`), compared in constant time as tenant creation compares the bootstrap token; without the
//! token set, they answer nobody. A change is checked and recorded as `ferrum task set` does (crate::settings::change).

use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, app_state};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::Error;
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};

/// The operator, or a refusal that says the same whether a token is set or not: the environment's token, else the
/// vault's (crate::secrets).
pub(crate) async fn operator(st: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let given = headers.get("x-ferrum-operator-token").and_then(|v| v.to_str().ok()).unwrap_or_default();
    if crate::secrets::token(st, "operator_token", &st.settings.operator_token, given).await != crate::secrets::Token::Good {
        return Err(ApiError(Error::Forbidden("the settings need the operator's token".into())));
    }
    Ok(())
}

fn no_store(mut res: Response) -> Response {
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// `GET /api/operator/settings`: every setting as this replica has it, the table's rows, and the replicas running.
async fn list(State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    operator(&st, &headers).await?;
    Ok(no_store(Json(crate::settings::listing(&st).await?).into_response()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetBody {
    value: Value,
    #[serde(default)]
    pool: Option<String>,
    /// For an address that is given a secret (`sends:`): that secret again, else it is cleared from the vault.
    #[serde(default)]
    secret: Option<String>,
}

#[derive(Deserialize)]
struct PoolQuery {
    #[serde(default)]
    pool: Option<String>,
}

async fn changed(st: &AppState, change: crate::settings::Change, secret: Option<String>) -> ApiResult<Response> {
    let secrets = match (secret, crate::settings::find(&change.name).and_then(|d| d.sends.clone())) {
        (Some(value), Some(sends)) => vec![(sends, ferrum_kernel::vault::Zeroizing::new(value))],
        (Some(_), None) => return Err(ApiError(Error::field("secret", "only an address that is given a secret takes one"))),
        (None, _) => Vec::new(),
    };
    let (done, tied) = crate::settings::change_with(st, &[change], &secrets, "operator").await?;
    let tied: Vec<Value> = tied.into_iter().map(|(key, what)| json!({"secret": key, "now": what})).collect();
    Ok(no_store(Json(json!({"changed": done, "secrets": tied})).into_response()))
}

/// `PUT /api/operator/settings/{key}` `{"value": ..., "pool": ...}`: sets one setting.
async fn set(State(app): State<AppContext>, headers: HeaderMap, Path(key): Path<String>, Json(body): Json<SetBody>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    operator(&st, &headers).await?;
    let value = match body.value {
        Value::String(s) => s,
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        _ => return Err(ApiError(Error::Invalid("value is text, a number, or true or false; DELETE unsets a setting".into()))),
    };
    changed(&st, crate::settings::Change { name: key, pool: body.pool, value: Some(value) }, body.secret).await
}

/// `DELETE /api/operator/settings/{key}[?pool=]`: unsets one setting (the environment's value or the default applies).
async fn unset(State(app): State<AppContext>, headers: HeaderMap, Path(key): Path<String>, Query(q): Query<PoolQuery>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    operator(&st, &headers).await?;
    changed(&st, crate::settings::Change { name: key, pool: q.pool, value: None }, None).await
}

pub fn routes() -> Routes {
    use axum::routing::{get, put};
    Routes::new().add("/api/operator/settings", get(list)).add("/api/operator/settings/{key}", put(set).delete(unset))
}
