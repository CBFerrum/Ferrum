//! What connections keep of a person, for their data report and erasure (privacy.rs; docs/builder/decisions.md
//! BD-35): the calls they made or that were about a record of theirs, calls and inbound deliveries whose kept text
//! (an error, a mapped result, a debug capture, a payload) carries one of their addresses, and their own sign-ins.
//! Calls and deliveries stay as the log's lines (when, which operation, how it went) without what named them; a
//! sign-in's tokens are destroyed here and revoked at the provider after the erasure commits.

use crate::db::Tx;
use crate::error::Result;
use serde_json::Value;
use uuid::Uuid;

/// A call is theirs: made by their account, about one of their records, or its kept text carries an address of theirs
/// (as written, or percent-encoded in a captured address).
const CALL_THEIRS: &str = "l.tenant_id = $1 and (l.user_id = $2 \
     or (l.entity, l.record_id) in (select * from unnest($3::text[], $4::uuid[])) \
     or exists (select 1 from unnest($5::text[]) e, lower(concat(l.error, l.result::text, l.debug::text)) t \
       where strpos(t, e) > 0 or strpos(t, replace(e, '@', '%40')) > 0))";

/// A delivery is theirs: its payload or its error carries an address of theirs.
const INBOUND_THEIRS: &str = "i.tenant_id = $1 and exists (select 1 from unnest($2::text[]) e where strpos(lower(concat(i.payload::text, i.error)), e) > 0)";

fn split(records: &[(String, Uuid)]) -> (Vec<String>, Vec<Uuid>) {
    records.iter().cloned().unzip()
}

/// What connections hold of a person (`account`, `emails`, their `records`), oldest first: `{kind: call | inbound |
/// sign_in, connection, …}`, never a token, a payload or a body.
pub(crate) async fn held_report(tx: &mut Tx, tenant: Uuid, account: Option<Uuid>, emails: &[String], records: &[(String, Uuid)]) -> Result<Vec<Value>> {
    let (entities, ids) = split(records);
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select v from ( \
           select jsonb_build_object('kind', 'call', 'connection', c.name, 'operation', l.operation, 'at', l.at, 'outcome', l.outcome, \
             'entity', l.entity, 'record', l.record_id, 'made_by_them', $2::uuid is not null and l.user_id = $2) v, l.at \
           from conn_call l join conn c on c.tenant_id = l.tenant_id and c.id = l.conn_id where {CALL_THEIRS} \
         union all select jsonb_build_object('kind', 'inbound', 'connection', c.name, 'received_at', i.received_at, 'state', i.state), i.received_at \
           from conn_inbound i join conn c on c.tenant_id = i.tenant_id and c.id = i.conn_id \
           where i.tenant_id = $1 and exists (select 1 from unnest($5::text[]) e where strpos(lower(concat(i.payload::text, i.error)), e) > 0) \
         union all select jsonb_build_object('kind', 'sign_in', 'connection', c.name, 'account', t.account, 'status', t.status, 'updated_at', t.updated_at), t.updated_at \
           from conn_token t join conn c on c.tenant_id = t.tenant_id and c.id = t.conn_id where t.tenant_id = $1 and t.user_id = $2 \
         ) h order by at"
    )))
    .bind(tenant)
    .bind(account)
    .bind(&entities)
    .bind(&ids)
    .bind(emails)
    .fetch_all(&mut **tx)
    .await?)
}

/// What an erasure did to connections' rows.
#[derive(Debug, Default)]
pub(crate) struct Erased {
    pub calls: u64,
    pub inbound: u64,
    /// Their own sign-ins taken, for revoking at the provider once the erasure commits: connection, sealed tokens.
    pub sign_ins: Vec<(Uuid, String)>,
}

/// Erases what connections hold of a person: their calls and those about their (erased) `records` lose who made them,
/// their errors, kept results and captures; deliveries carrying an address lose their payload and error; their own
/// sign-ins and sign-ins under way go, the tokens with them.
pub(crate) async fn erase_held(tx: &mut Tx, tenant: Uuid, account: Option<Uuid>, emails: &[String], records: &[(String, Uuid)]) -> Result<Erased> {
    let (entities, ids) = split(records);
    let calls = sqlx::query(sqlx::AssertSqlSafe(format!(
        "update conn_call l set user_id = null, error = null, result = null, debug = null, input_digest = null where {CALL_THEIRS}"
    )))
    .bind(tenant)
    .bind(account)
    .bind(&entities)
    .bind(&ids)
    .bind(emails)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    let inbound = sqlx::query(sqlx::AssertSqlSafe(format!("update conn_inbound i set payload = null, error = null where {INBOUND_THEIRS}")))
        .bind(tenant)
        .bind(emails)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    let mut sign_ins = Vec::new();
    if let Some(person) = account {
        sqlx::query("delete from conn_signin where tenant_id = $1 and user_id = $2").bind(tenant).bind(person).execute(&mut **tx).await?;
        sign_ins = sqlx::query_as("delete from conn_token where tenant_id = $1 and user_id = $2 returning conn_id, sealed")
            .bind(tenant)
            .bind(person)
            .fetch_all(&mut **tx)
            .await?;
    }
    Ok(Erased { calls, inbound, sign_ins })
}
