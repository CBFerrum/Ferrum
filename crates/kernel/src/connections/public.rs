//! A short-lived public address of a stored file, for a service that pulls media by address rather than taking bytes
//! (BD-64): an operation's `public_files` gives its templates `public_files.<name>`, good for one file, for a while,
//! readable without signing in at `{PUBLIC_URL}/files/pull/{id}?s={signature}`, and recorded with the call it was made
//! for. The signature is an HMAC of the address's id under the server's key, so the address cannot be changed to reach
//! anything else; the row says which file, until when, and whether it was ended early.

use super::{Loaded, More, render};
use crate::error::{Error, Result};
use crate::{Ctx, Kernel};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::LazyLock;
use uuid::Uuid;

/// Where the addresses are served, under the server's public address.
pub const PULL_PATH: &str = "/files/pull/";
/// How long an address is good for unless the operation says.
pub const DEFAULT_MINUTES: u32 = 60;
/// The longest an operation may make one good for.
pub const MAX_MINUTES: u32 = 360;
/// How long ended and expired addresses are kept in the record.
const KEEP_DAYS: i32 = 90;

/// One public address an operation makes for a call (`public_files: {<name>: {file, minutes?, end_with_call?}}`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicFile {
    /// A template giving the stored file's id (`{{ inputs.media }}`); the caller must be able to read the file.
    pub file: String,
    /// How long it is good for, in minutes: [`DEFAULT_MINUTES`] unless said, at most [`MAX_MINUTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<u32>,
    /// Ended as soon as the call has its answer (worked or refused), rather than at its time; a call that waits for the
    /// service keeps it until it ends.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub end_with_call: bool,
}

impl PublicFile {
    pub(crate) fn problem(&self, name: &str) -> Option<String> {
        if !ferrum_meta::is_ident(name) {
            return Some(format!("public_files: {name} is not a name (lower-case letters, digits, '_')"));
        }
        if self.minutes.is_some_and(|m| !(1..=MAX_MINUTES).contains(&m)) {
            return Some(format!("public_files.{name}: minutes is 1 to {MAX_MINUTES}"));
        }
        super::template_problem(&self.file)
    }
}

/// The signature in an address (`s=` and 64 hex digits, as written or percent-encoded): what masking hides wherever an
/// address appears in a capture, an error or a result.
static SIGNATURE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"(s(?:=|%3[dD]))[0-9a-f]{64}").expect("signature"));

/// `s` with every address's signature hidden.
pub(crate) fn mask_addresses(s: &str) -> String {
    SIGNATURE.replace_all(s, format!("${{1}}{}", super::HIDDEN)).into_owned()
}

/// The addresses made for one call: `public_files` as its templates read it, and those to end with it.
#[derive(Debug, Default)]
pub(crate) struct Made {
    pub vars: Value,
    pub ends: Vec<Uuid>,
}

/// A stored file an address leads to.
#[derive(Debug, Clone)]
pub struct Pulled {
    pub tenant: Uuid,
    pub name: String,
    pub content_type: String,
    pub size: i64,
    pub key: String,
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Kernel {
    fn address_signature(&self, id: Uuid) -> Result<String> {
        let key = self
            .settings
            .secret_key
            .as_ref()
            .ok_or_else(|| Error::Unavailable("a public address of a file needs the server's secret key (FERRUM_SECRET_KEY)".into()))?;
        Ok(key.fingerprint("pulled-file", &id.to_string()))
    }

    /// The addresses an operation's `public_files` makes for a call: each file read as the caller may read it, good
    /// until its time. A keyed call made again (a retry, a wait resumed) gets the addresses it gave before while they
    /// have ten minutes left, so the service is not handed a second one.
    pub(crate) async fn public_files(
        &self,
        ctx: &Ctx,
        conn: &Loaded,
        operation: &str,
        (inputs, key): (&Value, Option<&str>),
        about: Option<(&str, Uuid)>,
        more: &More,
    ) -> Result<Made> {
        if more.public_files.is_empty() {
            return Ok(Made::default());
        }
        let base = self.settings.public_url.as_deref().map(|u| u.trim_end_matches('/').to_string()).ok_or_else(|| {
            Error::Unavailable("this server has no public address (PUBLIC_URL), and a service cannot pull a file from localhost: an operator sets one".into())
        })?;
        let vars = json!({"inputs": inputs, "key": key});
        let mut out = Map::new();
        let mut ends = Vec::new();
        for (name, p) in &more.public_files {
            let given = render(&p.file, &vars)?;
            let file: Uuid = given.trim().parse().map_err(|_| Error::invalid(format!("public_files.{name}: {given} is not a stored file")))?;
            self.file_info(ctx, file).await.map_err(|_| Error::invalid(format!("public_files.{name}: the file {file} is not one you may send")))?;
            let minutes = i32::try_from(p.minutes.unwrap_or(DEFAULT_MINUTES).min(MAX_MINUTES)).unwrap_or(60);
            let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
            let kept: Option<Uuid> = match key {
                Some(k) => {
                    sqlx::query_scalar(
                        "select id from conn_file_address where tenant_id = $1 and conn_id = $2 and operation = $3 and call_key = $4 and name = $5 \
                       and file_id = $6 and ended_at is null and expires_at > now() + interval '10 minutes' order by at desc limit 1",
                    )
                    .bind(conn.tenant)
                    .bind(conn.id)
                    .bind(operation)
                    .bind(k)
                    .bind(name)
                    .bind(file)
                    .fetch_optional(&mut *tx)
                    .await?
                }
                None => None,
            };
            let id = match kept {
                Some(id) => id,
                None => {
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "insert into conn_file_address (tenant_id, id, conn_id, operation, name, call_key, file_id, entity, record_id, user_id, expires_at) \
                         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now() + make_interval(mins => $11))",
                    )
                    .bind(conn.tenant)
                    .bind(id)
                    .bind(conn.id)
                    .bind(operation)
                    .bind(name)
                    .bind(key)
                    .bind(file)
                    .bind(about.map(|a| a.0))
                    .bind(about.map(|a| a.1))
                    .bind((!ctx.user.is_nil()).then_some(ctx.user))
                    .bind(minutes)
                    .execute(&mut *tx)
                    .await?;
                    id
                }
            };
            tx.commit().await?;
            out.insert(name.clone(), json!(format!("{base}{PULL_PATH}{id}?s={}", self.address_signature(id)?)));
            if p.end_with_call {
                ends.push(id);
            }
        }
        Ok(Made { vars: Value::Object(out), ends })
    }

    /// Ends addresses early (their call has its answer).
    pub(crate) async fn end_public_files(&self, tenant: Uuid, ids: &[Uuid]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        sqlx::query("update conn_file_address set ended_at = now() where tenant_id = $1 and id = any($2) and ended_at is null")
            .bind(tenant)
            .bind(ids)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The file an address leads to, for anyone holding it: its signature right, not ended, not past its time, its
    /// workspace active. Anything else is not found, alike.
    pub async fn pulled_file(&self, id: Uuid, signature: &str) -> Result<Pulled> {
        let gone = || Error::NotFound("file".into());
        if self.settings.secret_key.is_none() || !same(&self.address_signature(id)?, signature) {
            return Err(gone());
        }
        let mut tx = self.db.system_tx().await?;
        let row: Option<(Uuid, String, String, i64, String)> = sqlx::query_as(
            "select a.tenant_id, f.file_name, f.content_type, f.size_bytes, f.storage_key from conn_file_address a \
               join file_object f on f.tenant_id = a.tenant_id and f.id = a.file_id join tenant t on t.id = a.tenant_id \
             where a.id = $1 and a.ended_at is null and a.expires_at > now() and t.status = 'active'",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let (tenant, name, content_type, size, key) = row.ok_or_else(gone)?;
        Ok(Pulled { tenant, name, content_type, size, key })
    }

    /// The addresses a connection's calls made, newest first: `{at, operation, name, key, file, entity, record_id,
    /// user_id, expires_at, ended_at}` (never the address itself).
    pub async fn connection_addresses(&self, ctx: &Ctx, conn: Uuid, limit: i64) -> Result<Vec<Value>> {
        super::admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows = sqlx::query_scalar(
            "select jsonb_build_object('at', at, 'operation', operation, 'name', name, 'key', call_key, 'file', file_id, 'entity', entity, \
               'record_id', record_id, 'user_id', user_id, 'expires_at', expires_at, 'ended_at', ended_at) \
             from conn_file_address where tenant_id = $1 and conn_id = $2 order by at desc limit $3",
        )
        .bind(ctx.tenant)
        .bind(conn)
        .bind(limit.clamp(1, 500))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Upkeep: addresses past their time (or ended) longer than [`KEEP_DAYS`] ago.
    pub(crate) async fn purge_public_files(&self) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let n = sqlx::query("delete from conn_file_address where coalesce(ended_at, expires_at) < now() - make_interval(days => $1::int)")
            .bind(KEEP_DAYS)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn signatures_are_hidden_as_written_and_encoded() {
        let sig = "a".repeat(64);
        let s = format!("https://f.example/files/pull/x?s={sig} and image_url=https%3A%2F%2Ff%2Ffiles%2Fpull%2Fx%3Fs%3D{sig}");
        let m = super::mask_addresses(&s);
        assert!(!m.contains(&sig), "{m}");
        assert!(m.contains("?s=••••••") && m.contains("%3Fs%3D••••••"), "{m}");
    }
}
