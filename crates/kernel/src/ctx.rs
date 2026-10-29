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
    /// Only some records: the user's own and/or those matching a rule's condition.
    Own,
    All,
}

/// One rule's reach for an action: the user's own records (`owner`) and/or records whose fields
/// hold given values (`condition`: field → accepted values, as text). Neither: every record.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Grant {
    pub owner: bool,
    pub condition: Vec<(String, Vec<String>)>,
}

impl Grant {
    fn is_whole(&self) -> bool {
        !self.owner && self.condition.is_empty()
    }
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
    /// Per action (in [`Action`] order), the grants of the rules allowing it.
    pub grants: [Vec<Grant>; 8],
}

fn action_index(a: Action) -> usize {
    match a {
        Action::Read => 0,
        Action::Write => 1,
        Action::Create => 2,
        Action::Delete => 3,
        Action::Submit => 4,
        Action::Cancel => 5,
        Action::Export => 6,
        Action::Import => 7,
    }
}

/// Every action, with the verb consents and declarations use for it.
const ACTIONS: [(Action, &str); 8] = [
    (Action::Read, "read"),
    (Action::Write, "write"),
    (Action::Create, "create"),
    (Action::Delete, "delete"),
    (Action::Submit, "submit"),
    (Action::Cancel, "cancel"),
    (Action::Export, "export"),
    (Action::Import, "import"),
];

impl EntityAccess {
    fn set(&mut self, a: Action, level: Access) {
        *match a {
            Action::Read => &mut self.read,
            Action::Write => &mut self.write,
            Action::Create => &mut self.create,
            Action::Delete => &mut self.delete,
            Action::Submit => &mut self.submit,
            Action::Cancel => &mut self.cancel,
            Action::Export => &mut self.export,
            Action::Import => &mut self.import,
        } = level;
    }

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
    match_rule: Option<Value>,
}

impl PermSet {
    pub async fn load(tx: &mut Tx, tenant: Uuid, roles: &[String], admin: bool) -> Result<PermSet> {
        if admin {
            return Ok(PermSet { admin: true, entities: BTreeMap::new() });
        }
        let rows: Vec<PermRow> = sqlx::query_as(
            "select entity, can_read, can_write, can_create, can_delete, can_submit, can_cancel, can_export, can_import, \
             if_owner, read_groups, write_groups, match_rule from role_permission where tenant_id = $1 and role = any($2)",
        )
        .bind(tenant)
        .bind(roles)
        .fetch_all(&mut **tx)
        .await?;
        let mut set = PermSet::default();
        for r in rows {
            // A condition that no longer parses (its field was removed) grants nothing.
            let condition = match r.match_rule {
                None => Some(Vec::new()),
                Some(v) => serde_json::from_value(v)
                    .ok()
                    .map(|condition| ferrum_meta::PermissionDef { entity: r.entity.clone(), condition, ..Default::default() }.condition_values()),
            };
            let Some(condition) = condition else { continue };
            let grant = Grant { owner: r.if_owner, condition };
            let level = if grant.is_whole() { Access::All } else { Access::Own };
            let e = set.entities.entry(r.entity).or_default();
            let allowed = [r.can_read, r.can_write, r.can_create, r.can_delete, r.can_submit, r.can_cancel, r.can_export, r.can_import];
            let levels = [&mut e.read, &mut e.write, &mut e.create, &mut e.delete, &mut e.submit, &mut e.cancel, &mut e.export, &mut e.import];
            for (i, (yes, cur)) in allowed.into_iter().zip(levels).enumerate() {
                if yes {
                    if level > *cur {
                        *cur = level;
                    }
                    if !e.grants[i].contains(&grant) {
                        e.grants[i].push(grant.clone());
                    }
                }
            }
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

    /// The grants an action has on `entity` when access is partial (`None`: every record).
    pub fn grants(&self, entity: &str, action: Action) -> Option<Vec<Grant>> {
        if self.access(entity, action) == Access::All {
            return None;
        }
        let i = action_index(action);
        let mut out: Vec<Grant> = [entity, "*"].iter().filter_map(|k| self.entities.get(*k)).flat_map(|e| e.grants[i].iter().cloned()).collect();
        out.sort();
        out.dedup();
        Some(out)
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

    /// What an app's screen may do for this user (docs/plugins.md, "Screens"): the user's access
    /// narrowed to what the app declared (entity or `*` → actions as its consent lists them:
    /// `read`, `write (own records)`, `read field group <g>`...) and to every action on `own`, the
    /// app's own entities. Field groups only as declared; never administrator power.
    pub fn limited_to(&self, declared: &BTreeMap<String, BTreeSet<String>>, own: &BTreeSet<String>) -> PermSet {
        let mut out = PermSet::default();
        let names: BTreeSet<&str> = self.entities.keys().chain(declared.keys()).chain(own).map(String::as_str).chain(["*"]).collect();
        for name in names {
            let decl: BTreeSet<&str> = [name, "*"].into_iter().filter_map(|k| declared.get(k)).flatten().map(String::as_str).collect();
            let mut e = EntityAccess::default();
            for (i, (action, verb)) in ACTIONS.into_iter().enumerate() {
                let plugin = if own.contains(name) || decl.contains(verb) {
                    Access::All
                } else if decl.contains(format!("{verb} (own records)").as_str()) {
                    Access::Own
                } else {
                    Access::None
                };
                let user = self.access(name, action);
                let level = user.min(plugin);
                // Partial access keeps every limit: the user's grants, each narrowed to their own
                // records when the app is limited to those.
                e.grants[i] = match (level, user, plugin) {
                    (Access::Own, Access::All, _) => vec![Grant { owner: true, condition: vec![] }],
                    (Access::Own, _, Access::All) => self.grants(name, action).unwrap_or_default(),
                    (Access::Own, ..) => self.grants(name, action).unwrap_or_default().into_iter().map(|g| Grant { owner: true, ..g }).collect(),
                    _ => vec![],
                };
                e.set(action, level);
            }
            let groups = |prefix: &str, mine: BTreeSet<&str>| -> BTreeSet<String> {
                decl.iter().filter_map(|d| d.strip_prefix(prefix)).filter(|g| self.admin || mine.contains(g)).map(String::from).collect()
            };
            e.read_groups = groups("read field group ", self.groups(name, false));
            e.write_groups = groups("write field group ", self.groups(name, true));
            if ACTIONS.iter().any(|(a, _)| e.get(*a) != Access::None) || !e.read_groups.is_empty() || !e.write_groups.is_empty() {
                out.entities.insert(name.to_string(), e);
            }
        }
        out
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
    /// Now, in the workspace's time zone (its `timezone` setting; UTC when unset).
    pub fn now_local(&self) -> chrono::DateTime<chrono::FixedOffset> {
        local_now(&self.schema.timezone)
    }

    /// Today's date where the workspace is — documents dated "today" use this, not UTC.
    pub fn today(&self) -> chrono::NaiveDate {
        self.now_local().date_naive()
    }

    pub fn user_value(&self) -> Value {
        let now = self.now_local();
        json!({
            "today": now.format("%Y-%m-%d").to_string(),
            "now": now.to_rfc3339(),
            "time": now.format("%H:%M:%S").to_string(),
            "timezone": self.schema.timezone,
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
    /// The user, when some grant reaches only their own records: matched against `created_by` ...
    pub owner: Option<Uuid>,
    /// ... or, by email, against the entity's `owner_fields`.
    pub owner_email: Option<String>,
    pub owner_fields: Vec<OwnerField>,
    /// Partial access: a record qualifies through any one of these (none: every record).
    pub grants: Vec<ScopeGrant>,
    pub conds: Vec<ScopeCond>,
}

/// A rule's reach, ready to test: the user's own records and/or fields holding given values.
#[derive(Debug, Clone, Default)]
pub struct ScopeGrant {
    pub owner: bool,
    pub fields: Vec<GrantField>,
}

/// A field of a grant's condition: its accepted values as text, and its SQL (`{a}`: the alias).
#[derive(Debug, Clone)]
pub struct GrantField {
    pub field: String,
    pub values: Vec<String>,
    expr: String,
    /// A checkbox: unset reads as `false`, in SQL and in [`RowScope::matches`] alike.
    check: bool,
}

impl ScopeGrant {
    /// The user's own records, nothing else.
    pub fn own() -> ScopeGrant {
        ScopeGrant { owner: true, fields: vec![] }
    }
}

/// A document value as the text a condition compares (empty when unset).
fn condition_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// An owner field and its SQL test (`{a}`: the table alias; the email is bound after `prefix`).
#[derive(Debug, Clone)]
pub struct OwnerField {
    pub field: String,
    pub child: Option<String>,
    prefix: String,
    suffix: String,
}

fn owner_fields(ctx: &Ctx, ent: &EffEntity) -> Vec<OwnerField> {
    ent.def
        .owner_fields
        .iter()
        .filter_map(|o| match o.split_once('.') {
            None => {
                let expr = crate::store::field_expr(ent, o, "{a}")?;
                Some(OwnerField { field: o.clone(), child: None, prefix: format!("lower({expr}) = "), suffix: String::new() })
            }
            Some((table, cf)) => {
                let info = ent.children.get(table)?;
                let cent = ctx.schema.entity(&info.entity)?;
                let expr = crate::store::field_expr(cent, cf, "oc")?;
                Some(OwnerField {
                    field: cf.to_string(),
                    child: Some(table.to_string()),
                    prefix: format!(
                        "exists (select 1 from {} oc where oc.tenant_id = {{a}}.tenant_id and oc.parent_id = {{a}}.id and lower({expr}) = ",
                        crate::ddl::q(&info.table)
                    ),
                    suffix: ")".into(),
                })
            }
        })
        .collect()
}

pub fn row_scope(ctx: &Ctx, ent: &EffEntity, action: Action) -> Result<RowScope> {
    ctx.require(&ent.def.name, action)?;
    let mut scope = RowScope::default();
    if ctx.is_admin || ctx.is_system {
        return Ok(scope);
    }
    if let Some(mut grants) = ctx.perms.grants(&ent.def.name, action) {
        if grants.is_empty() {
            // Partial access with no grant recorded (a hand-made permission set): own records.
            grants.push(Grant { owner: true, condition: vec![] });
        }
        if grants.iter().any(|g| g.owner) {
            scope.owner = Some(ctx.user);
            scope.owner_email = (!ctx.email.is_empty()).then(|| ctx.email.to_lowercase());
            scope.owner_fields = owner_fields(ctx, ent);
        }
        for g in grants {
            let fields: Option<Vec<GrantField>> = g
                .condition
                .into_iter()
                .map(|(field, values)| {
                    let check = ent.field(&field)?.ftype == FieldType::Check;
                    let expr = crate::store::field_expr(ent, &field, "{a}")?;
                    Some(GrantField { expr: if check { format!("coalesce({expr}, false)") } else { expr }, field, values, check })
                })
                .collect();
            // A condition on a field the entity no longer has reaches nothing (never more).
            if let Some(fields) = fields {
                scope.grants.push(ScopeGrant { owner: g.owner, fields });
            }
        }
        if scope.grants.is_empty() {
            // Every grant pointed at a missing field: nothing qualifies.
            scope
                .grants
                .push(ScopeGrant { owner: false, fields: vec![GrantField { field: "id".into(), values: vec![], expr: "{a}.id".into(), check: false }] });
        }
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
        self.grants.is_empty() && self.conds.is_empty()
    }

    /// Whether some grant limits access to the user's own records (no grant reaches them all).
    pub fn owner_only(&self) -> bool {
        self.grants.iter().any(|g| g.owner)
    }

    /// Whether the user is named on the record in one of its owner fields.
    fn named_owner(&self, doc: &Doc) -> bool {
        let Some(email) = &self.owner_email else { return false };
        let is_me = |v: Option<&Value>| v.and_then(Value::as_str).is_some_and(|s| s.trim().eq_ignore_ascii_case(email));
        self.owner_fields.iter().any(|o| match &o.child {
            None => is_me(doc.get(&o.field)),
            Some(t) => doc.get(t).and_then(Value::as_array).is_some_and(|rows| rows.iter().any(|r| is_me(r.get(&o.field)))),
        })
    }

    /// Whether the user owns the record: created it, or is named in an owner field. A record
    /// nobody created (system and background work) belongs to no one but those named.
    fn owns(&self, doc: &Doc) -> bool {
        let created_by = doc.get("created_by").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
        self.owner.is_some_and(|o| created_by == Some(o)) || self.named_owner(doc)
    }

    pub fn matches(&self, doc: &Doc) -> bool {
        if !self.grants.is_empty() {
            let holds = |f: &GrantField| {
                let v = doc.get(&f.field).filter(|v| !v.is_null());
                f.values.contains(&if f.check && v.is_none() { "false".into() } else { condition_text(v) })
            };
            let reached = self.grants.iter().any(|g| (!g.owner || self.owns(doc)) && g.fields.iter().all(holds));
            if !reached {
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

    /// `(created_by = user or <owner fields name them>)` for table alias `a`.
    fn push_owner(&self, qb: &mut QueryBuilder<Postgres>, alias: &str) {
        qb.push(format!("({alias}.created_by = ")).push_bind(self.owner.unwrap_or_else(Uuid::nil));
        if let Some(email) = &self.owner_email {
            for o in &self.owner_fields {
                qb.push(format!(" or {}", o.prefix.replace("{a}", alias))).push_bind(email.clone()).push(o.suffix.clone());
            }
        }
        qb.push(")");
    }

    /// One grant's predicate, parenthesized: `(owner)` for own records alone, else
    /// `(owner and field = any(values) and ...)`.
    fn push_grant(&self, qb: &mut QueryBuilder<Postgres>, g: &ScopeGrant, alias: &str) {
        if g.owner && g.fields.is_empty() {
            return self.push_owner(qb, alias);
        }
        qb.push("(");
        if g.owner {
            self.push_owner(qb, alias);
        }
        for (i, f) in g.fields.iter().enumerate() {
            if g.owner || i > 0 {
                qb.push(" and ");
            }
            qb.push(format!("({})::text = any(", f.expr.replace("{a}", alias))).push_bind(f.values.clone()).push(")");
        }
        if !g.owner && g.fields.is_empty() {
            qb.push("true");
        }
        qb.push(")");
    }

    /// Appends ` and (...)` predicates for table alias `a`.
    pub fn push_sql(&self, qb: &mut QueryBuilder<Postgres>, alias: &str) {
        match self.grants.as_slice() {
            [] => {}
            [one] => {
                qb.push(" and ");
                self.push_grant(qb, one, alias);
            }
            many => {
                qb.push(" and (");
                for (i, g) in many.iter().enumerate() {
                    if i > 0 {
                        qb.push(" or ");
                    }
                    self.push_grant(qb, g, alias);
                }
                qb.push(")");
            }
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

/// Now in an IANA time zone; unknown zones fall back to UTC.
pub fn local_now(tz: &str) -> chrono::DateTime<chrono::FixedOffset> {
    let utc = chrono::Utc::now();
    match tz.parse::<chrono_tz::Tz>() {
        Ok(zone) => utc.with_timezone(&zone).fixed_offset(),
        Err(_) => utc.fixed_offset(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(items: &[(&str, &[&str])]) -> BTreeMap<String, BTreeSet<String>> {
        items.iter().map(|(e, a)| (e.to_string(), a.iter().map(|s| s.to_string()).collect())).collect()
    }

    #[test]
    fn screens_get_the_user_narrowed_to_the_declaration() {
        let own = Grant { owner: true, condition: vec![] };
        let goods = Grant { owner: false, condition: vec![("kind".into(), vec!["goods".into()])] };
        let mut user = PermSet::default();
        let mut c = EntityAccess { read: Access::All, write: Access::All, ..Default::default() };
        c.grants[0] = vec![Grant::default()];
        c.grants[1] = vec![Grant::default()];
        c.read_groups.insert("credit".into());
        user.entities.insert("Customer".into(), c);
        let mut o = EntityAccess { read: Access::Own, ..Default::default() };
        o.grants[0] = vec![own.clone()];
        user.entities.insert("Order".into(), o);
        let mut i = EntityAccess { read: Access::Own, ..Default::default() };
        i.grants[0] = vec![goods.clone()];
        user.entities.insert("Item".into(), i);

        let declared = decl(&[
            ("Customer", &["read", "read field group credit", "read field group secret"]),
            ("Order", &["read", "write"]),
            ("Item", &["read (own records)"]),
        ]);
        let mine: BTreeSet<String> = ["XBoard".to_string()].into();
        let p = user.limited_to(&declared, &mine);
        assert!(!p.admin);
        assert_eq!(p.access("Customer", Action::Read), Access::All);
        assert_eq!(p.access("Customer", Action::Write), Access::None, "not declared");
        assert_eq!(p.access("Order", Action::Read), Access::Own);
        assert_eq!(p.grants("Order", Action::Read), Some(vec![own.clone()]));
        assert_eq!(p.access("Order", Action::Write), Access::None, "the user may not");
        assert_eq!(p.grants("Item", Action::Read), Some(vec![Grant { owner: true, ..goods.clone() }]), "both limits hold");
        assert_eq!(p.access("XBoard", Action::Delete), Access::None, "own entities still need the user's right");
        assert_eq!(p.access("Supplier", Action::Read), Access::None);
        assert_eq!(p.groups("Customer", false), BTreeSet::from(["credit"]));

        // An administrator gets exactly the declaration.
        let a = PermSet { admin: true, entities: BTreeMap::new() }.limited_to(&declared, &mine);
        assert!(!a.admin);
        assert_eq!(a.access("Customer", Action::Read), Access::All);
        assert_eq!(a.access("Customer", Action::Delete), Access::None);
        assert_eq!(a.grants("Item", Action::Read), Some(vec![own.clone()]));
        assert_eq!(a.access("XBoard", Action::Delete), Access::All);
        assert_eq!(a.groups("Customer", false), BTreeSet::from(["credit", "secret"]));

        // A wildcard declaration reaches entities named nowhere else, within the user's rights.
        let mut u = PermSet::default();
        u.entities.insert("*".into(), EntityAccess { read: Access::All, write: Access::All, ..Default::default() });
        let w = u.limited_to(&decl(&[("*", &["read"])]), &BTreeSet::new());
        assert_eq!(w.access("Anything", Action::Read), Access::All);
        assert_eq!(w.access("Anything", Action::Write), Access::None);
        assert_eq!(u.limited_to(&BTreeMap::new(), &BTreeSet::new()).access("Anything", Action::Read), Access::None);
    }

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
        let scope = RowScope { conds: vec![ScopeCond { field: Some("company".into()), ext: false, ids: vec![c1], strict: true }], ..Default::default() };
        let mut doc = Doc::new();
        doc.insert("company".into(), json!(c1.to_string()));
        assert!(scope.matches(&doc));
        doc.insert("company".into(), json!(c2.to_string()));
        assert!(!scope.matches(&doc));
        doc.remove("company");
        assert!(!scope.matches(&doc), "strict scopes reject empty values");
    }

    #[test]
    fn local_time_follows_the_workspace_zone() {
        let kolkata = local_now("Asia/Kolkata");
        assert_eq!(kolkata.offset().local_minus_utc(), 5 * 3600 + 1800);
        assert_eq!(local_now("Not/AZone").offset().local_minus_utc(), 0);
        let utc = chrono::Utc::now();
        assert_eq!(kolkata.date_naive(), (utc + chrono::Duration::minutes(330)).date_naive());
    }

    #[test]
    fn owners_are_creators_or_named_assignees() {
        let me = Uuid::now_v7();
        let someone = Uuid::now_v7();
        let field =
            |f: &str, child: Option<&str>| OwnerField { field: f.into(), child: child.map(str::to_string), prefix: String::new(), suffix: String::new() };
        let scope = RowScope {
            owner: Some(me),
            owner_email: Some("me@x.test".into()),
            owner_fields: vec![field("assigned_to", None), field("user", Some("assignees"))],
            grants: vec![ScopeGrant::own()],
            conds: vec![],
        };
        let doc = |v: Value| v.as_object().cloned().unwrap();
        assert!(scope.matches(&doc(json!({"created_by": me.to_string()}))));
        assert!(!scope.matches(&doc(json!({"created_by": someone.to_string()}))));
        assert!(scope.matches(&doc(json!({"created_by": someone.to_string(), "assigned_to": " ME@x.test "}))));
        assert!(scope.matches(&doc(json!({"created_by": someone.to_string(), "assignees": [{"user": "other@x.test"}, {"user": "me@x.test"}]}))));
        assert!(!scope.matches(&doc(json!({"created_by": someone.to_string(), "assignees": [{"user": "other@x.test"}]}))));
        let creator_only = RowScope { owner: Some(me), grants: vec![ScopeGrant::own()], ..Default::default() };
        assert!(!creator_only.matches(&doc(json!({"created_by": someone.to_string(), "assigned_to": "me@x.test"}))));
        assert!(!creator_only.matches(&doc(json!({"subject": "made by a background job"}))), "no creator is nobody's");
        assert!(scope.matches(&doc(json!({"assigned_to": "me@x.test"}))), "owner fields still grant system-created records");
    }
}
