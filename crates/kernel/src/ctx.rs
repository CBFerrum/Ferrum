//! Request context and the permission model (roles → entity access, field groups, record scopes).
//!
//! One rule set drives both single-record checks (`RowScope::matches`) and list/report queries
//! (`RowScope::push_sql`), so a list can never show a record a direct read would refuse.

use crate::Doc;
use crate::db::Tx;
use crate::error::{Error, Result};
use ferrum_meta::{EffEntity, FieldDef, FieldType, Schema, Storage};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    #[default]
    None,
    /// Only records the user created.
    Own,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Read,
    Write,
    Create,
    Delete,
    Submit,
    Cancel,
    Export,
    Import,
}

impl Action {
    pub fn parse(s: &str) -> Option<Action> {
        Some(match s {
            "read" => Action::Read,
            "write" => Action::Write,
            "create" => Action::Create,
            "delete" => Action::Delete,
            "submit" => Action::Submit,
            "cancel" => Action::Cancel,
            "export" => Action::Export,
            "import" => Action::Import,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct EntityAccess {
    pub read: Access,
    pub write: Access,
    pub create: Access,
    pub delete: Access,
    pub submit: Access,
    pub cancel: Access,
    pub export: Access,
    pub import: Access,
    pub read_groups: BTreeSet<String>,
    pub write_groups: BTreeSet<String>,
}

impl EntityAccess {
    fn get(&self, a: Action) -> Access {
        match a {
            Action::Read => self.read,
            Action::Write => self.write,
            Action::Create => self.create,
            Action::Delete => self.delete,
            Action::Submit => self.submit,
            Action::Cancel => self.cancel,
            Action::Export => self.export,
            Action::Import => self.import,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PermSet {
    pub admin: bool,
    pub entities: BTreeMap<String, EntityAccess>,
}

#[derive(sqlx::FromRow)]
struct PermRow {
    entity: String,
    can_read: bool,
    can_write: bool,
    can_create: bool,
    can_delete: bool,
    can_submit: bool,
    can_cancel: bool,
    can_export: bool,
    can_import: bool,
    if_owner: bool,
    read_groups: Vec<String>,
    write_groups: Vec<String>,
}

impl PermSet {
    pub async fn load(tx: &mut Tx, tenant: Uuid, roles: &[String], admin: bool) -> Result<PermSet> {
        if admin {
            return Ok(PermSet { admin: true, entities: BTreeMap::new() });
        }
        let rows: Vec<PermRow> = sqlx::query_as(
            "select entity, can_read, can_write, can_create, can_delete, can_submit, can_cancel, can_export, can_import, \
             if_owner, read_groups, write_groups from role_permission where tenant_id = $1 and role = any($2)",
        )
        .bind(tenant)
        .bind(roles)
        .fetch_all(&mut **tx)
        .await?;
        let mut set = PermSet::default();
        for r in rows {
            let level = if r.if_owner { Access::Own } else { Access::All };
            let e = set.entities.entry(r.entity).or_default();
            let up = |cur: &mut Access, allowed: bool| {
                if allowed && level > *cur {
                    *cur = level;
                }
            };
            up(&mut e.read, r.can_read);
            up(&mut e.write, r.can_write);
            up(&mut e.create, r.can_create);
            up(&mut e.delete, r.can_delete);
            up(&mut e.submit, r.can_submit);
            up(&mut e.cancel, r.can_cancel);
            up(&mut e.export, r.can_export);
            up(&mut e.import, r.can_import);
            e.read_groups.extend(r.read_groups);
            e.write_groups.extend(r.write_groups);
        }
        Ok(set)
    }

    pub fn access(&self, entity: &str, action: Action) -> Access {
        if self.admin {
            return Access::All;
        }
        let specific = self.entities.get(entity).map(|e| e.get(action)).unwrap_or_default();
        let wildcard = self.entities.get("*").map(|e| e.get(action)).unwrap_or_default();
        specific.max(wildcard)
    }

    fn groups(&self, entity: &str, write: bool) -> BTreeSet<&str> {
        let mut out = BTreeSet::new();
        for key in [entity, "*"] {
            if let Some(e) = self.entities.get(key) {
                let g = if write { &e.write_groups } else { &e.read_groups };
                out.extend(g.iter().map(String::as_str));
            }
        }
        out
    }

    pub fn can_read_field(&self, entity: &str, f: &FieldDef) -> bool {
        match &f.perm_group {
            None => true,
            Some(g) => self.admin || self.groups(entity, false).contains(g.as_str()) || self.groups(entity, true).contains(g.as_str()),
        }
    }

    pub fn can_write_field(&self, entity: &str, f: &FieldDef) -> bool {
        match &f.perm_group {
            None => true,
            Some(g) => self.admin || self.groups(entity, true).contains(g.as_str()),
        }
    }
}

/// Everything known about the caller for one request.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub tenant: Uuid,
    pub user: Uuid,
    pub email: String,
    pub full_name: String,
    pub roles: Vec<String>,
    pub is_admin: bool,
    pub is_system: bool,
    pub request_id: String,
    /// Record scopes: entity → allowed record ids (e.g. `Company` → the user's companies).
    pub scopes: BTreeMap<String, Vec<Uuid>>,
    pub default_company: Option<Uuid>,
    pub fy_start_month: u32,
    pub schema: Arc<Schema>,
    pub perms: Arc<PermSet>,
    /// Automation recursion depth guard.
    pub depth: u8,
}

impl Ctx {
    /// Full-power context for engine work (seeding, automations, migrations).
    pub fn system(tenant: Uuid, schema: Arc<Schema>) -> Ctx {
        Ctx {
            tenant,
            user: Uuid::nil(),
            email: "system@ferrum.local".into(),
            full_name: "System".into(),
            roles: vec![],
            is_admin: true,
            is_system: true,
            request_id: format!("sys-{}", Uuid::now_v7()),
            scopes: BTreeMap::new(),
            default_company: None,
            fy_start_month: schema.fy_start_month,
            schema,
            perms: Arc::new(PermSet { admin: true, entities: BTreeMap::new() }),
            depth: 0,
        }
    }

    /// The `user` variable visible to CEL expressions.
    pub fn user_value(&self) -> Value {
        json!({
            "id": self.user.to_string(),
            "email": self.email,
            "full_name": self.full_name,
            "roles": self.roles,
            "is_admin": self.is_admin,
            "companies": self.scopes.get("Company").map(|v| v.iter().map(|u| u.to_string()).collect::<Vec<_>>()).unwrap_or_default(),
            "default_company": self.default_company.map(|u| u.to_string()),
        })
    }

    pub fn has_role(&self, role: &str) -> bool {
        self.is_admin || self.roles.iter().any(|r| r == role)
    }

    pub fn require(&self, entity: &str, action: Action) -> Result<Access> {
        match self.perms.access(entity, action) {
            Access::None => Err(Error::Forbidden(format!("not permitted to {action:?} {entity}").to_lowercase())),
            a => Ok(a),
        }
    }
}

/// A condition restricting which records an action applies to.
#[derive(Debug, Clone)]
pub struct ScopeCond {
    /// `None` = the record's own id.
    pub field: Option<String>,
    pub ext: bool,
    pub ids: Vec<Uuid>,
    /// Strict: the value must be in `ids`. Lenient: empty values also pass.
    pub strict: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RowScope {
    pub owner: Option<Uuid>,
    pub conds: Vec<ScopeCond>,
}

pub fn row_scope(ctx: &Ctx, ent: &EffEntity, action: Action) -> Result<RowScope> {
    let access = ctx.require(&ent.def.name, action)?;
    let mut scope = RowScope { owner: (access == Access::Own).then_some(ctx.user), conds: vec![] };
    if ctx.is_admin || ctx.is_system {
        return Ok(scope);
    }
    for (scope_entity, ids) in &ctx.scopes {
        if scope_entity == &ent.def.name {
            scope.conds.push(ScopeCond { field: None, ext: false, ids: ids.clone(), strict: true });
        }
        for f in ent.value_fields().filter(|f| f.ftype == FieldType::Link && f.to.as_deref() == Some(scope_entity.as_str())) {
            let strict = scope_entity == "Company" && f.name == "company";
            scope.conds.push(ScopeCond { field: Some(f.name.clone()), ext: ent.storage_of(&f.name) == Some(Storage::Ext), ids: ids.clone(), strict });
        }
    }
    Ok(scope)
}

impl RowScope {
    pub fn is_unrestricted(&self) -> bool {
        self.owner.is_none() && self.conds.is_empty()
    }

    pub fn matches(&self, doc: &Doc) -> bool {
        if let Some(owner) = self.owner {
            let created_by = doc.get("created_by").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
            if created_by.is_some_and(|c| c != owner) {
                return false;
            }
        }
        self.conds.iter().all(|c| {
            let key = c.field.as_deref().unwrap_or("id");
            match doc.get(key).and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) {
                Some(v) => c.ids.contains(&v),
                None => !c.strict,
            }
        })
    }

    /// Appends ` and (...)` predicates for table alias `a`.
    pub fn push_sql(&self, qb: &mut QueryBuilder<Postgres>, alias: &str) {
        if let Some(owner) = self.owner {
            qb.push(format!(" and {alias}.created_by = ")).push_bind(owner);
        }
        for c in &self.conds {
            let col = match (&c.field, c.ext) {
                (None, _) => format!("{alias}.id"),
                (Some(f), false) => format!("{alias}.{}", crate::ddl::q(f)),
                (Some(f), true) => format!("nullif({alias}.ext->>'{f}', '')::uuid"),
            };
            qb.push(" and (");
            if !c.strict {
                qb.push(format!("{col} is null or "));
            }
            qb.push(format!("{col} = any(")).push_bind(c.ids.clone()).push("))");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_levels_combine() {
        let mut set = PermSet::default();
        set.entities.insert("Invoice".into(), EntityAccess { read: Access::Own, ..Default::default() });
        set.entities.insert("*".into(), EntityAccess { read: Access::All, ..Default::default() });
        assert_eq!(set.access("Invoice", Action::Read), Access::All);
        assert_eq!(set.access("Invoice", Action::Write), Access::None);
        assert_eq!(PermSet { admin: true, ..Default::default() }.access("X", Action::Delete), Access::All);
    }

    #[test]
    fn row_scope_matches() {
        let c1 = Uuid::now_v7();
        let c2 = Uuid::now_v7();
        let scope = RowScope { owner: None, conds: vec![ScopeCond { field: Some("company".into()), ext: false, ids: vec![c1], strict: true }] };
        let mut doc = Doc::new();
        doc.insert("company".into(), json!(c1.to_string()));
        assert!(scope.matches(&doc));
        doc.insert("company".into(), json!(c2.to_string()));
        assert!(!scope.matches(&doc));
        doc.remove("company");
        assert!(!scope.matches(&doc), "strict scopes reject empty values");
    }
}
