//! Collection operations (A11 2.4, API-2): an action bound to no record (`collection: true`), the capability
//! `{pkg}.{entity}.{action}` with input `{params}`. Its rights are the action's (its permission, or a rule naming it,
//! which takes no `where`: there is no record); its parameters only those it declares; one that only reads runs in a
//! read-only transaction. What it writes goes through the record pipeline, audited there.

use crate::Kernel;
use crate::capability::{Invocation, snake};
use crate::ctx::Ctx;
use crate::db::{Tx, with_retry};
use crate::error::{Error, Result};
use crate::hooks::HookCtx;
use serde_json::{Value, json};

impl Kernel {
    /// Collection action `action` of `entity` as `ctx` with `params`, from a screen (`POST /api/v1/{E}/_actions/{action}`):
    /// `(answer, repeated)`. With `key` (the request's `Idempotency-Key`) through the gate, under a receipt that answers
    /// a repeat once; without, in a transaction of its own each time.
    pub async fn run_collection(&self, ctx: &Ctx, entity: &str, action: &str, params: Value, key: Option<&str>) -> Result<(Value, bool)> {
        let ent = self.entity(ctx, entity)?;
        let def = ent.def.actions.iter().find(|a| a.name == action && a.collection).ok_or_else(|| Error::NotFound(format!("collection action {action}")))?;
        if let Some(key) = key {
            let id = format!("{}.{}.{action}", def.origin.as_deref().unwrap_or(&ent.package), snake(&ent.def.name));
            let done = self.invoke(ctx, Invocation::new(id, json!({"params": params})).key(key)).await?;
            return Ok((done.result, done.repeated));
        }
        let read_only = def.read_only;
        let out = with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            if read_only {
                // The action promised not to write; the database holds it to that.
                sqlx::query("set transaction read only").execute(&mut *tx).await?;
            }
            let out = self.run_collection_tx(ctx, &mut tx, entity, action, &params).await?;
            tx.commit().await?;
            Ok(out)
        })
        .await
        .inspect_err(|e| self.tally(ctx, entity, action, e))?;
        Ok((out, false))
    }

    /// [`Kernel::run_collection`] in `tx`: the action's gate, its declared parameters only, then the first hook that
    /// answers ([`crate::hooks::DocHook::collection`]).
    pub async fn run_collection_tx(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, action: &str, params: &Value) -> Result<Value> {
        let ent = self.entity(ctx, entity)?;
        let def = ent.def.actions.iter().find(|a| a.name == action && a.collection).ok_or_else(|| Error::NotFound(format!("collection action {action}")))?;
        // A secret parameter is never shown back, in an error's text either.
        let secrets = def.secret_values(params);
        let run = async {
            crate::ctx::action_gate(ctx, &ent, def)?;
            let params = crate::doc::given_params(action, &def.params, params)?;
            for h in &self.hooks.for_tenant(&ent.def.name, &ctx.schema.packages) {
                let mut hc = HookCtx { kernel: self, ctx, tx: &mut *tx, entity: &ent };
                if let Some(out) = h.collection(&mut hc, action, &params).await? {
                    return Ok(out);
                }
            }
            Err(Error::NotFound(format!("handler for collection action {action}")))
        };
        run.await.map_err(|e| e.redact(&secrets))
    }
}
