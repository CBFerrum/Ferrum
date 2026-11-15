//! A workspace's vault follows the workspace (docs/operations.md, "The vault"): its keys and secrets go with it when it
//! is deleted or leaves for good, and a move carries them to the installation it goes to without either installation
//! learning the other's root. (A sandbox's are made as each secret's rule says: secrets.rs, `sandbox`.)
//!
//! A move runs at the target, which reads the source's database and cannot open the source's root. So at the source an
//! operator first makes a transfer (`ferrum task vault_transfer`): every version of the workspace's key, wrapped by a
//! key made for this transfer alone, kept in `vault.transfer`; that key is shown to the operator once and never stored.
//! Given it (among the source's keys, `FERRUM_MOVE_SOURCE_SECRET_KEYS`), the move opens the workspace's keys, wraps them
//! with this installation's key, and copies the owner keys and secrets below them as they are: each is bound to the
//! workspace's id, which a move keeps, and to nothing of the source's root.

use super::root::{current_version, lock, notify};
use super::seal::{Key, key_place, transfer_check, transfer_place};
use crate::db::{Tx, with_retry};
use crate::error::{Error, Result};
use crate::secret::SecretKey;
use crate::{Db, Kernel};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;
use zeroize::Zeroizing;

/// Makes `tx` see the vault's tables (the setting only this module sets), for the statements that follow, in a
/// transaction a workspace's deletion or a move opened; [`left`] says it no more.
async fn entered(tx: &mut Tx) -> Result<()> {
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut **tx).await?;
    Ok(())
}

async fn left(tx: &mut Tx) -> Result<()> {
    sqlx::query("select set_config('ferrum.vault', '', true)").execute(&mut **tx).await?;
    Ok(())
}

/// Removes the workspace `tenant`'s keys, secrets and transfer in `tx`, the transaction that deletes its rows, so that
/// neither goes without the other; says so in the installation's record (`why`). Every replica lets go of what it
/// holds of them once `tx` commits. How many keys and secrets went.
pub(crate) async fn drop_scope(tx: &mut Tx, tenant: Uuid, why: &str) -> Result<(u64, u64)> {
    entered(tx).await?;
    let secrets = sqlx::query("delete from vault.secret where scope = $1").bind(tenant).execute(&mut **tx).await?.rows_affected();
    sqlx::query("delete from vault.use_count where scope = $1").bind(tenant).execute(&mut **tx).await?;
    sqlx::query("delete from vault.transfer where scope = $1").bind(tenant).execute(&mut **tx).await?;
    // Owner keys first: each names the workspace key that wraps it.
    let keys = sqlx::query("delete from vault.key where scope = $1 and owner <> ''").bind(tenant).execute(&mut **tx).await?.rows_affected()
        + sqlx::query("delete from vault.key where scope = $1").bind(tenant).execute(&mut **tx).await?.rows_affected();
    if keys + secrets > 0 {
        super::installation_event(tx, "workspace_vault_removed", json!({"workspace": tenant, "why": why, "keys": keys, "secrets": secrets})).await?;
        notify(tx, &tenant.to_string()).await?;
    }
    left(tx).await?;
    Ok((keys, secrets))
}

/// `ferrum task vault_transfer workspace:<slug>`, at the source of a move: every version of the workspace's key, wrapped
/// by a new transfer key and kept in `vault.transfer`, in place of a transfer made before. The key is returned to be
/// shown once, as base64 of its 32 bytes (as `FERRUM_SECRET_KEY` is written): the move takes it among the source's keys.
/// A workspace with nothing in the vault needs none.
pub async fn transfer_out(k: &Kernel, tenant: Uuid) -> Result<Zeroizing<String>> {
    let root = &k.db.vault;
    root.installation()?;
    let key = Key::random()?;
    let id = Uuid::now_v7();
    with_retry(|| async {
        let mut tx = k.db.vault_tx(None).await?;
        let rows: Vec<(i32, i32, Vec<u8>)> =
            sqlx::query_as("select version, parent_version, wrapped from vault.key where scope = $1 and owner = '' order by version for share")
                .bind(tenant)
                .fetch_all(&mut *tx)
                .await?;
        if rows.is_empty() {
            return Err(Error::Invalid("this workspace has nothing in the vault: its move needs no transfer".into()));
        }
        sqlx::query("delete from vault.transfer where scope = $1").bind(tenant).execute(&mut *tx).await?;
        let check = key.check_value(&transfer_check(tenant, id));
        for (version, parent, wrapped) in &rows {
            let workspace_key = root.workspace_key_from(&k.db, tenant, *version, *parent, wrapped).await?;
            sqlx::query("insert into vault.transfer (scope, version, id, check_value, wrapped) values ($1, $2, $3, $4, $5)")
                .bind(tenant)
                .bind(version)
                .bind(id)
                .bind(&check)
                .bind(key.wrap(&transfer_place(tenant, id, *version), &workspace_key)?)
                .execute(&mut *tx)
                .await?;
        }
        super::installation_event(&mut tx, "transfer_made", json!({"workspace": tenant, "versions": rows.len()})).await?;
        tx.commit().await?;
        Ok(())
    })
    .await?;
    Ok(Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(key.bytes())))
}

/// The workspace keys of `tenant` in the source's vault, opened with whichever of `keys` is the key of its transfer
/// there: by version. `None`: it has nothing in the source's vault. Read in `src`, a transaction of the source's.
async fn opened_transfer(src: &mut Tx, tenant: Uuid, keys: &[SecretKey]) -> Result<Option<HashMap<i32, Key>>> {
    entered(src).await?;
    let versions: Vec<i32> =
        sqlx::query_scalar("select version from vault.key where scope = $1 and owner = '' order by version").bind(tenant).fetch_all(&mut **src).await?;
    if versions.is_empty() {
        left(src).await?;
        return Ok(None);
    }
    let rows: Vec<(i32, Uuid, Vec<u8>, Vec<u8>)> =
        sqlx::query_as("select version, id, check_value, wrapped from vault.transfer where scope = $1").bind(tenant).fetch_all(&mut **src).await?;
    left(src).await?;
    let Some((_, id, check, _)) = rows.first() else {
        return Err(Error::Conflict(
            "the workspace has keys and secrets in the source's vault: make a transfer of them there (ferrum task vault_transfer workspace:<slug>) \
             and give the key it prints in FERRUM_MOVE_SOURCE_SECRET_KEYS"
                .into(),
        ));
    };
    let key = keys.iter().filter_map(|k| Key::from_slice(k.key_bytes())).find(|k| k.checks(&transfer_check(tenant, *id), check)).ok_or_else(|| {
        Error::Invalid(
            "no key in FERRUM_MOVE_SOURCE_SECRET_KEYS is the key of this workspace's transfer: give the one `ferrum task vault_transfer` printed at the source"
                .into(),
        )
    })?;
    let mut out = HashMap::new();
    for version in versions {
        let Some((_, _, _, wrapped)) = rows.iter().find(|r| r.0 == version) else {
            return Err(Error::Conflict("the workspace's key changed at the source after its transfer was made: make the transfer again there".into()));
        };
        let opened = key
            .unwrap(&transfer_place(tenant, *id, version), wrapped)
            .ok_or_else(|| Error::Conflict("the workspace's transfer at the source does not open: it was changed or damaged; make it again there".into()))?;
        out.insert(version, opened);
    }
    Ok(Some(out))
}

/// Whether a move can carry the vault of `tenant` from `source`: nothing in it, or a transfer there that one of `keys`
/// opens and that holds every version of its key. Asked before anything is frozen.
pub(crate) async fn may_carry(source: &Db, tenant: Uuid, keys: &[SecretKey]) -> Result<()> {
    let mut src = source.pool.begin().await?;
    opened_transfer(&mut src, tenant, keys).await?;
    src.rollback().await?;
    Ok(())
}

/// What the source's vault holds of `tenant`, to tell whether it changed after a move copied it: how many keys and
/// secrets, and a digest of them (the day each was last used left out, which a use moves).
pub(crate) async fn digest(src: &mut Tx, tenant: Uuid) -> Result<Value> {
    entered(src).await?;
    let (rows, digest): (i64, String) = sqlx::query_as(
        "select count(*), coalesce(md5(string_agg(r, ',' order by r)), '') from ( \
           select concat_ws('/', 'key', owner, version, parent_version, encode(wrapped, 'hex')) r from vault.key where scope = $1 \
           union all select concat_ws('/', 'secret', owner, name, version, key_version, encode(sealed, 'hex'), rule::text, valid_until::text) \
           from vault.secret where scope = $1) x",
    )
    .bind(tenant)
    .fetch_one(&mut **src)
    .await?;
    left(src).await?;
    Ok(json!({"rows": rows, "digest": digest}))
}

/// What a move brings of `tenant`'s vault, in the copy's transaction `tx` (here) from its snapshot `src` (the source's
/// database): every version of its key, opened with the transfer key among `keys` and wrapped with this installation's
/// key, and the owner keys and secrets below them as they are; each owner key is opened on the way, so that a transfer
/// of anything else carries nothing. Says what it carried as the copy says a table ([`digest`]).
pub(crate) async fn carry_in(k: &Kernel, src: &mut Tx, tx: &mut Tx, tenant: Uuid, keys: &[SecretKey]) -> Result<Value> {
    let Some(opened) = opened_transfer(src, tenant, keys).await? else { return Ok(json!({"rows": 0, "digest": ""})) };
    let installation = k.db.vault.installation()?;
    entered(src).await?;
    entered(tx).await?;
    lock(tx, false).await?;
    if current_version(tx).await? != Some(installation.version) {
        return Err(Error::Conflict("this installation's vault key changed while the workspace was copied: run the move again".into()));
    }
    let here: bool = sqlx::query_scalar("select exists (select 1 from vault.key where scope = $1)").bind(tenant).fetch_one(&mut **tx).await?;
    if here {
        return Err(Error::Conflict(
            "this installation's vault holds keys of this workspace already: abort the move, and remove what is left of it here".into(),
        ));
    }
    let workspace_keys: Vec<(i32, DateTime<Utc>)> =
        sqlx::query_as("select version, created_at from vault.key where scope = $1 and owner = '' order by version").bind(tenant).fetch_all(&mut **src).await?;
    for (version, at) in &workspace_keys {
        let key = opened.get(version).ok_or_else(|| Error::Conflict("the workspace's key changed at the source during the copy: run the move again".into()))?;
        sqlx::query("insert into vault.key (scope, owner, version, parent_version, wrapped, created_at) values ($1, '', $2, $3, $4, $5)")
            .bind(tenant)
            .bind(version)
            .bind(installation.version)
            .bind(installation.key.wrap(&key_place(tenant, "", *version, installation.version), key)?)
            .bind(at)
            .execute(&mut **tx)
            .await?;
    }
    let owner_keys: Vec<(String, i32, i32, Vec<u8>, bool, DateTime<Utc>)> =
        sqlx::query_as("select owner, version, parent_version, wrapped, outside, created_at from vault.key where scope = $1 and owner <> ''")
            .bind(tenant)
            .fetch_all(&mut **src)
            .await?;
    for (owner, version, parent, wrapped, outside, at) in &owner_keys {
        let above = opened.get(parent).ok_or_else(|| super::keys::damaged(tenant, owner, *version))?;
        above
            .unwrap(&key_place(tenant, owner, *version, *parent), wrapped)
            .ok_or_else(|| Error::Conflict("the workspace's keys at the source do not open with its transfer: make the transfer again there".into()))?;
        sqlx::query(
            "insert into vault.key (scope, owner, version, parent_version, parent_owner, wrapped, outside, created_at) values ($1, $2, $3, $4, '', $5, $6, $7)",
        )
        .bind(tenant)
        .bind(owner)
        .bind(version)
        .bind(parent)
        .bind(wrapped)
        .bind(outside)
        .bind(at)
        .execute(&mut **tx)
        .await?;
    }
    let secrets: Vec<Value> = sqlx::query_scalar("select to_jsonb(s) from vault.secret s where scope = $1").bind(tenant).fetch_all(&mut **src).await?;
    for row in &secrets {
        sqlx::query("insert into vault.secret select * from jsonb_populate_record(null::vault.secret, $1)").bind(row).execute(&mut **tx).await?;
    }
    super::installation_event(tx, "workspace_arrived", json!({"workspace": tenant, "keys": workspace_keys.len() + owner_keys.len(), "secrets": secrets.len()}))
        .await?;
    left(src).await?;
    left(tx).await?;
    digest(src, tenant).await
}
