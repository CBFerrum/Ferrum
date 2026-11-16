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

/// `GET /api/operator/vault`: the vault as `ferrum task vault_status` shows it: open or why not, its slots, its keys and
/// secrets counted. Never a key.
async fn vault_status(State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    operator(&st, &headers).await?;
    Ok(no_store(Json(ferrum_kernel::vault::status(&st.kernel.db).await?).into_response()))
}

/// The installation's secrets, by owner: names, rules, when set and last used. Never a value.
pub(crate) fn by_owner(secrets: Vec<ferrum_kernel::vault::Described>) -> Value {
    let mut owners: std::collections::BTreeMap<String, Vec<ferrum_kernel::vault::Described>> = Default::default();
    for d in secrets {
        owners.entry(d.owner.clone()).or_default().push(d);
    }
    Value::Array(owners.into_iter().map(|(owner, secrets)| json!({"owner": owner, "secrets": secrets})).collect())
}

/// `GET /api/operator/secrets`: the installation's secrets by owner, and the platform's own (the registry's), each
/// with where it comes from now (`environment`, `vault`, or `default`: none) and when a change of it applies.
async fn secrets(State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    operator(&st, &headers).await?;
    let owners = by_owner(st.vault.installation_secrets(&st.kernel).await?);
    let platform: Vec<Value> = crate::secrets::sources(&st)
        .await
        .into_iter()
        .filter_map(|(key, (source, _))| {
            let d = crate::secrets::find(&key)?;
            let applies = if d.vault == Some(crate::settings::Held::Library) { "start" } else { "live" };
            Some(json!({"key": key, "env": d.env, "held": d.vault, "source": source, "applies": applies}))
        })
        .collect();
    Ok(no_store(Json(json!({"owners": owners, "platform": platform})).into_response()))
}

/// What a person or the operator gives to set or rotate a secret: exactly one of a value, a random value the vault
/// makes (`random`: its bytes, shown once) or an Ed25519 pair; the rule, to set one.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SecretBody {
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    random: Option<usize>,
    #[serde(default)]
    ed25519: bool,
    #[serde(default)]
    pub(crate) rule: Option<ferrum_kernel::vault::Rule>,
    /// For a token of the platform's: how long the one before still works, in seconds.
    #[serde(default)]
    overlap_secs: Option<i64>,
}

impl SecretBody {
    pub(crate) fn new_value(&mut self) -> Result<ferrum_kernel::vault::New, ApiError> {
        use ferrum_kernel::vault::{New, Zeroizing};
        match (self.value.take(), self.random, self.ed25519) {
            (Some(v), None, false) => Ok(New::Value(Zeroizing::new(v.into_bytes()))),
            (None, Some(n), false) => Ok(New::Random(n)),
            (None, None, true) => Ok(New::Ed25519),
            _ => Err(ApiError(Error::field("value", "give one of value, random (its bytes) or ed25519"))),
        }
    }
}

/// What a change of a secret answers: it described, and a value the vault made, this once.
pub(crate) fn changed_secret(c: ferrum_kernel::vault::Changed) -> Response {
    let mut out = json!({"secret": c.secret});
    if let Some(shown) = &c.shown {
        out["shown"] = json!(shown.as_str());
    }
    no_store(Json(out).into_response())
}

/// `PUT /api/operator/secrets/{owner}/{name}` (and `POST …/rotate`): sets one of the installation's secrets. The
/// platform's own are the registry's, each as its kind says (crate::secrets::set): a token made here and shown once, a
/// new key that signs sessions, or a value given. Another owner's takes the body's value and rule.
async fn set_secret(
    State(app): State<AppContext>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(mut body): Json<SecretBody>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    operator(&st, &headers).await?;
    if owner == "platform" {
        let def = crate::secrets::find(&name).ok_or_else(|| ApiError(Error::NotFound(format!("platform secret {name}"))))?;
        let given = match body.value.take() {
            Some(v) => crate::secrets::Given::Value(ferrum_kernel::vault::Zeroizing::new(v)),
            None => crate::secrets::Given::Made,
        };
        let done = crate::secrets::set(&st, def, given, body.overlap_secs.unwrap_or(0)).await?;
        return Ok(no_store(Json(done).into_response()));
    }
    let value = body.new_value()?;
    Ok(changed_secret(match body.rule.take() {
        Some(rule) => st.vault.operator_set(&st.kernel, &owner, &name, value, rule).await?,
        None => st.vault.operator_rotate(&st.kernel, &owner, &name, value).await?,
    }))
}

/// What changes a secret's rule alone: `{"rule": {...}}`.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleBody {
    pub(crate) rule: ferrum_kernel::vault::Rule,
}

/// `PUT /api/operator/secrets/{owner}/{name}/rule` `{"rule": ...}`: changes only the rule of one of the installation's
/// secrets (another owner's than the platform's, whose rules the server keeps), recorded as the operator's.
async fn set_rule(
    State(app): State<AppContext>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(body): Json<RuleBody>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    operator(&st, &headers).await?;
    if owner == "platform" {
        return Err(ApiError(Error::Invalid(
            "the platform's secrets keep the rules the server gives them (bound to where each is sent): set the secret again instead".into(),
        )));
    }
    Ok(no_store(Json(json!({"secret": st.vault.operator_rule(&st.kernel, &owner, &name, body.rule).await?})).into_response()))
}

/// `DELETE /api/operator/secrets/{owner}/{name}`: removes one of the installation's secrets.
async fn remove_secret(State(app): State<AppContext>, headers: HeaderMap, Path((owner, name)): Path<(String, String)>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    operator(&st, &headers).await?;
    let removed = if owner == "platform" {
        let def = crate::secrets::find(&name).ok_or_else(|| ApiError(Error::NotFound(format!("platform secret {name}"))))?;
        st.vault.remove(&st.kernel, ferrum_kernel::vault::Scope::Installation, &def.key).await?
    } else {
        st.vault.operator_remove(&st.kernel, &owner, &name).await?
    };
    Ok(no_store(Json(json!({"removed": removed})).into_response()))
}

pub fn routes() -> Routes {
    use axum::routing::{get, post, put};
    Routes::new()
        .add("/api/operator/settings", get(list))
        .add("/api/operator/settings/{key}", put(set).delete(unset))
        .add("/api/operator/vault", get(vault_status))
        .add("/api/operator/secrets", get(secrets))
        .add("/api/operator/secrets/{owner}/{name}", put(set_secret).delete(remove_secret))
        .add("/api/operator/secrets/{owner}/{name}/rotate", post(set_secret))
        .add("/api/operator/secrets/{owner}/{name}/rule", put(set_rule))
}
