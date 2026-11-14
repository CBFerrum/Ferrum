//! Linked records in expressions (docs/builder/decisions.md BD-14): `links.<link field>` is the record that link
//! field names, with its lines, as an expression sees a record; null when the field is empty or the record is gone.
//!
//! Read as the workspace, whoever saves or presses: a check must not pass or fail by who runs it (as BD-10 reads
//! aggregates), within the caller's zone: a record of an entity the zone leaves out (crate::zones, level 2) reads as
//! absent, so nothing of it reaches a check or the message a refusal shows. Secret fields are left out. Read in the caller's transaction without a lock: a rule over a linked
//! record sees it as it is when the save runs. One level: a link of the linked record stays an id.

use crate::Kernel;
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use ferrum_meta::{EffEntity, Expr, FieldType};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use uuid::Uuid;

/// The link fields `exprs` read, each once.
pub(crate) fn used<'a>(exprs: impl IntoIterator<Item = &'a Expr>) -> BTreeSet<String> {
    exprs.into_iter().flat_map(|e| e.links().iter().cloned()).collect()
}

/// `links` for expressions over `doc` (a record of `ent`, as an expression sees it) that read `names`: one read per
/// name, none when there are none.
pub(crate) async fn load(k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &Value, names: &BTreeSet<String>) -> Result<Value> {
    let mut out = Map::new();
    if names.is_empty() {
        return Ok(Value::Object(out));
    }
    let workspace = Ctx::system_for(ctx);
    for name in names {
        let to = ent.field(name).filter(|f| f.ftype == FieldType::Link).and_then(|f| f.to.as_deref());
        let tent = to.and_then(|t| ctx.schema.entity(t)).cloned();
        let id = doc.get(name).and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
        let (Some(tent), Some(id)) = (tent, id) else {
            out.insert(name.clone(), Value::Null);
            continue;
        };
        let found = match k.get_tx(&workspace, tx, &tent.def.name, id, false).await {
            Ok(d) => d,
            Err(Error::NotFound(_)) => {
                out.insert(name.clone(), Value::Null);
                continue;
            }
            Err(e) => return Err(e),
        };
        let mut view = tent.cel_view(&Value::Object(found), &ctx.schema);
        // Secrets stay where they are, on the record and on its lines.
        if let Value::Object(m) = &mut view {
            for f in &tent.def.fields {
                if f.ftype == FieldType::Secret {
                    m.remove(&f.name);
                } else if f.ftype == FieldType::Table
                    && let (Some(cent), Some(Value::Array(rows))) = (f.of.as_deref().and_then(|c| ctx.schema.entity(c)), m.get_mut(&f.name))
                {
                    for r in rows.iter_mut().filter_map(Value::as_object_mut) {
                        for s in cent.secret_fields() {
                            r.remove(&s.name);
                        }
                    }
                }
            }
        }
        out.insert(name.clone(), view);
    }
    Ok(Value::Object(out))
}
