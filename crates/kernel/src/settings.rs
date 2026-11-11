//! A workspace's settings in expressions (docs/builder/decisions.md BD-17): `settings.<Entity>.<field>` is that field
//! of the one record of a settings entity, the workspace's own or a package's alike; until the record is first
//! saved, the field's default.
//!
//! Read as the workspace, whoever saves or presses: a rule must hold for someone who may not open the settings (as
//! BD-10 reads aggregates). Secret fields are never read (refused when saved, and left out here). Read in the
//! caller's transaction, once for each place that evaluates (a save, an action's checks, one effect, an automation's
//! run), and only for the entities its expressions name: a changed setting is seen by the next evaluation, on every
//! replica, with nothing cached.

use crate::Kernel;
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::Result;
use ferrum_meta::{EffEntity, Expr, FieldType};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::Arc;

/// The settings entities `exprs` read, each once.
pub(crate) fn used<'a>(exprs: impl IntoIterator<Item = &'a Expr>) -> BTreeSet<String> {
    exprs.into_iter().flat_map(|e| e.settings().iter().map(|(n, _)| n.clone())).collect()
}

/// `ctx` with the settings of `names` loaded, for expressions evaluated with its `user_value()`; `ctx` itself when
/// they name none.
pub(crate) async fn with<'a>(k: &Kernel, ctx: &'a Ctx, tx: &mut Tx, names: BTreeSet<String>) -> Result<Cow<'a, Ctx>> {
    if names.is_empty() {
        return Ok(Cow::Borrowed(ctx));
    }
    let loaded = load(k, ctx, tx, &names).await?;
    Ok(Cow::Owned(Ctx { settings: Some(Arc::new(loaded)), ..ctx.clone() }))
}

/// How many times settings were loaded, for the test that they are loaded once per place and only when named.
static LOADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[doc(hidden)]
pub fn loads() -> usize {
    LOADS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `{Entity: record}` for each of `names`: its one record, or its fields' defaults while it has none, as an expression
/// sees a record, every field present and no secret. A name that is not (or no longer) a settings entity is left out.
async fn load(k: &Kernel, ctx: &Ctx, tx: &mut Tx, names: &BTreeSet<String>) -> Result<Value> {
    LOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let workspace = Ctx::system(ctx.tenant, ctx.schema.clone());
    let mut out = Map::new();
    for name in names {
        let Some(ent) = ctx.schema.entity(name).filter(|e| e.def.kind == ferrum_meta::EntityKind::Settings && !e.def.company_scoped) else {
            continue;
        };
        let found = k.find_tx(&workspace, tx, name, &[], None, 1).await?.into_iter().next();
        let record = match found {
            // Its lines come with a read of the record.
            Some(d) if !ent.children.is_empty() => k.get_tx(&workspace, tx, name, crate::store::doc_id(&d)?, false).await?,
            Some(d) => d,
            None => Map::new(),
        };
        out.insert(name.clone(), view(ent, &record, &ctx.schema));
    }
    Ok(Value::Object(out))
}

/// A settings record as expressions read it: every field there (its default where the record has none yet), secrets
/// left out.
fn view(ent: &EffEntity, record: &Map<String, Value>, schema: &ferrum_meta::Schema) -> Value {
    let mut full = Map::new();
    for f in ent.def.fields.iter().filter(|f| f.ftype != FieldType::Secret) {
        let v = match record.get(&f.name) {
            Some(v) => v.clone(),
            None if record.is_empty() => f.default.clone().unwrap_or(Value::Null),
            None => Value::Null,
        };
        full.insert(f.name.clone(), v);
    }
    let mut view = ent.cel_view(&Value::Object(full), schema);
    // Nor on its lines.
    for t in ent.table_fields() {
        let (Some(cent), Some(Value::Array(rows))) = (t.of.as_deref().and_then(|c| schema.entity(c)), view.get_mut(&t.name)) else { continue };
        for r in rows.iter_mut().filter_map(Value::as_object_mut) {
            for s in cent.secret_fields() {
                r.remove(&s.name);
            }
        }
    }
    view
}
