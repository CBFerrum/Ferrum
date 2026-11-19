//! Request context and the permission model (roles → entity access, field groups, record scopes).
//!
//! One rule set drives both single-record checks (`RowScope::matches`) and list/report queries
//! (`RowScope::push_sql`), so a list can never show a record a direct read would refuse.

use crate::Doc;
use crate::db::Tx;
use crate::error::{Error, Result};
use ferrum_meta::via::ViaStep;
use ferrum_meta::{ConditionRange, EffEntity, FieldDef, FieldType, Schema, Storage};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, Postgres, QueryBuilder};
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
/// hold given values (`condition`: field → accepted values, as text) or fall in given ranges.
/// None of these: every record.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Grant {
    pub owner: bool,
    pub condition: Vec<(String, Vec<String>)>,
    pub ranges: Vec<(String, ConditionRange)>,
    /// Own records reached through this path to the person's email (BD-02), not by who made them.
    pub via: Option<String>,
}

impl Grant {
    fn is_whole(&self) -> bool {
        !self.owner && self.condition.is_empty() && self.ranges.is_empty()
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
    /// As rules and consents write it.
    pub fn verb(self) -> &'static str {
        ACTIONS.iter().find(|(a, _)| *a == self).map_or("read", |(_, verb)| verb)
    }

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

/// A rule reading an entity, as one record's fields are told by it (S9 R13): what it does not show on the records it
/// reaches, and of their lines, per line entity.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReadRule {
    pub grant: Grant,
    pub hide: BTreeSet<String>,
    pub line_hide: BTreeMap<String, BTreeSet<String>>,
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
    /// Actions rules name (BD-02), each with the grants of the rules naming it.
    pub actions: BTreeMap<String, Vec<Grant>>,
    /// What an outside person sees of the conversation on the records they read (BD-22): `Some(false)` the shared
    /// comments and files, `Some(true)` those and their own beside them.
    pub shared: Option<bool>,
    /// Each rule reading the entity, with the fields it does not show (BD-40's `hide`, and the grouped fields whose group
    /// its role does not grant, S9 R13) and those of its records' lines, to tell what one record shows.
    pub read_rules: Vec<ReadRule>,
    /// Fields some reading rule does not show and no rule reading every record shows: left out wherever a set of records
    /// is read (lists, filters, sorting, search, reports, exports, history, live updates).
    pub hidden: BTreeSet<String>,
    /// Fields a rule hides (BD-40), which no rule reading every record shows: never written.
    pub write_hidden: BTreeSet<String>,
    /// Per line entity, the line fields some reading rule of this entity does not show: left out of lines in sets.
    pub line_hidden: BTreeMap<String, BTreeSet<String>>,
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

/// The verbs consents, declarations and records keys name actions by.
pub fn action_verbs() -> impl Iterator<Item = &'static str> {
    ACTIONS.iter().map(|(_, verb)| *verb)
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
    /// Level 2 of zones (crate::zones): only these entities are reached, administrator or not. For a
    /// seen entity everything else here holds as it is; for any other, nothing does.
    #[serde(skip)]
    pub limit: Option<Arc<Limit>>,
}

/// What a context reaches in a zone that limits staff to some apps (crate::zones, level 2): the
/// entities of those apps' packages, of the packages they depend on, and (when the rule says
/// `custom: true`) the workspace's own. Anything else, an entity no package is known for included,
/// is not there.
#[derive(Debug, Clone, Default)]
pub struct Limit {
    pub zone: String,
    pub entities: BTreeSet<String>,
    /// The packages those entities come from (the apps' and those they depend on).
    pub packages: BTreeSet<String>,
    /// `FERRUM_ZONE_MODE=report`: nothing is hidden; what would be is counted.
    pub report: bool,
}

impl Limit {
    /// Within `rule` of zone `zone`, for a workspace of `schema`.
    pub fn of(zone: &str, rule: &crate::zones::Rule, schema: &Schema, report: bool) -> Limit {
        Limit {
            zone: zone.into(),
            entities: crate::zones::entities_seen(rule, schema),
            packages: crate::zones::packages_seen(rule, schema).into_iter().map(String::from).collect(),
            report,
        }
    }

    /// Whether package `package`'s own code and capabilities are reached.
    pub fn sees_package(&self, package: &str) -> bool {
        if self.packages.contains(package) {
            return true;
        }
        if self.report {
            crate::zones::count_hidden(&self.zone, "report", "entity");
        }
        self.report
    }

    pub fn sees(&self, entity: &str) -> bool {
        if self.entities.contains(entity) {
            return true;
        }
        if self.report {
            crate::zones::count_hidden(&self.zone, "report", "entity");
        }
        self.report
    }
}

#[derive(sqlx::FromRow)]
struct PermRow {
    role: String,
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
    via: Option<String>,
    actions: Vec<String>,
    shared: Option<String>,
    hide: Vec<String>,
}

impl PermSet {
    /// Whether a zone's limit lets the holder reach `entity` at all (no limit: yes).
    pub fn sees(&self, entity: &str) -> bool {
        self.limit.as_ref().is_none_or(|l| l.sees(entity))
    }

    /// This set, within `limit`.
    pub fn within(&self, limit: Option<Arc<Limit>>) -> PermSet {
        PermSet { limit, ..self.clone() }
    }

    /// The rights `roles` give in `tenant` (all of them for an administrator). Field groups follow the rule that reaches the
    /// record (S9 R13): a reading rule shows a grouped field of `schema`'s entity only when its own groups, or its role's
    /// group-only rows on the entity, grant the group; a line's grouped fields, when its role grants their group on the
    /// line entity. A role's group-only rows on an entity it reads nothing of grant nothing.
    pub async fn load(tx: &mut Tx, tenant: Uuid, roles: &[String], admin: bool, schema: &Schema) -> Result<PermSet> {
        if admin {
            return Ok(PermSet { admin: true, ..Default::default() });
        }
        let rows: Vec<PermRow> = sqlx::query_as(
            "select role, entity, can_read, can_write, can_create, can_delete, can_submit, can_cancel, can_export, can_import, \
             if_owner, read_groups, write_groups, match_rule, via, actions, shared, hide from role_permission where tenant_id = $1 and role = any($2)",
        )
        .bind(tenant)
        .bind(roles)
        .fetch_all(&mut **tx)
        .await?;
        // Each role's groups on each entity, from all its rows there (a line entity's are its group-only rows), and those
        // of its rows that grant no action (group-only rows), which join its reading rules on the entity.
        let mut role_groups: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
        let mut group_only: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
        for r in &rows {
            let groups = r.read_groups.iter().chain(&r.write_groups).cloned();
            role_groups.entry((r.role.clone(), r.entity.clone())).or_default().extend(groups.clone());
            let acts = [r.can_read, r.can_write, r.can_create, r.can_delete, r.can_submit, r.can_cancel, r.can_export, r.can_import];
            if !acts.contains(&true) && r.actions.is_empty() {
                group_only.entry((r.role.clone(), r.entity.clone())).or_default().extend(groups);
            }
        }
        // Grouped fields of an entity whose group `groups` leaves out.
        let ungranted = |entity: &str, groups: &BTreeSet<String>| -> BTreeSet<String> {
            let Some(ent) = schema.entity(entity) else { return BTreeSet::new() };
            ent.def.fields.iter().filter(|f| f.perm_group.as_ref().is_some_and(|g| !groups.contains(g))).map(|f| f.name.clone()).collect()
        };
        let mut set = PermSet::default();
        for r in rows {
            // A condition that no longer parses (its field was removed) grants nothing.
            let condition = match r.match_rule {
                None => Some((Vec::new(), Vec::new())),
                Some(v) => serde_json::from_value(v).ok().and_then(|condition| {
                    let def = ferrum_meta::PermissionDef { entity: r.entity.clone(), condition, ..Default::default() };
                    def.condition_ranges().map(|ranges| (def.condition_values(), ranges))
                }),
            };
            let Some((condition, ranges)) = condition else { continue };
            // A path makes a rule an own-record rule whatever its flag says.
            let via = r.via.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            let grant = Grant { owner: r.if_owner || via.is_some(), condition, ranges, via };
            let level = if grant.is_whole() { Access::All } else { Access::Own };
            let e = set.entities.entry(r.entity.clone()).or_default();
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
            for a in r.actions {
                let named = e.actions.entry(a).or_default();
                if !named.contains(&grant) {
                    named.push(grant.clone());
                }
            }
            e.read_groups.extend(r.read_groups.iter().cloned());
            e.write_groups.extend(r.write_groups.iter().cloned());
            let explicit: BTreeSet<String> = r.hide.into_iter().collect();
            e.write_hidden.extend(explicit.iter().cloned());
            if r.can_read {
                let mut groups: BTreeSet<String> = r.read_groups.into_iter().chain(r.write_groups).collect();
                groups.extend(group_only.get(&(r.role.clone(), r.entity.clone())).into_iter().flatten().cloned());
                let mut hide = explicit;
                hide.extend(ungranted(&r.entity, &groups));
                // Its records' lines, at every depth: the line entity's groups this rule's role grants.
                let mut line_hide = BTreeMap::new();
                let mut lines: Vec<String> = schema.entity(&r.entity).map(|e| e.children.values().map(|c| c.entity.clone()).collect()).unwrap_or_default();
                while let Some(l) = lines.pop() {
                    let granted = role_groups.get(&(r.role.clone(), l.clone())).cloned().unwrap_or_default();
                    let hid = ungranted(&l, &granted);
                    if !hid.is_empty() {
                        line_hide.insert(l.clone(), hid);
                    }
                    lines.extend(schema.entity(&l).map(|e| e.children.values().map(|c| c.entity.clone()).collect::<Vec<_>>()).unwrap_or_default());
                }
                e.hidden.extend(hide.iter().cloned());
                for (l, hid) in &line_hide {
                    e.line_hidden.entry(l.clone()).or_default().extend(hid.iter().cloned());
                }
                e.read_rules.push(ReadRule { grant: grant.clone(), hide, line_hide });
            } else {
                e.hidden.extend(explicit);
            }
            // Only a rule that reads says anything of the conversation; the widest of them holds.
            if r.can_read
                && let Some(add) = r.shared.as_deref().map(|s| s == "add")
            {
                e.shared = Some(e.shared.unwrap_or(false) || add);
            }
        }
        // A field a rule reading every record shows is shown everywhere: such a rule reaches each record.
        let every = set.entities.get("*").cloned();
        let whole = |e: &EntityAccess| -> Vec<ReadRule> {
            e.read_rules.iter().chain(every.iter().flat_map(|w| &w.read_rules)).filter(|r| r.grant.is_whole()).cloned().collect()
        };
        let mut lines_hidden: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for e in set.entities.values_mut() {
            let wholes = whole(e);
            e.hidden.retain(|f| !wholes.iter().any(|r| !r.hide.contains(f)));
            e.write_hidden.retain(|f| !wholes.iter().any(|r| !r.hide.contains(f)));
            for (l, hid) in e.line_hidden.iter_mut() {
                hid.retain(|f| !wholes.iter().any(|r| !r.line_hide.get(l).is_some_and(|h| h.contains(f))));
                lines_hidden.entry(l.clone()).or_default().extend(hid.iter().cloned());
            }
            e.line_hidden.retain(|_, h| !h.is_empty());
        }
        // A line's fields hidden in sets are its entity's too: filters and sorts through lines read them.
        for (l, hid) in lines_hidden.into_iter().filter(|(_, h)| !h.is_empty()) {
            set.entities.entry(l).or_default().hidden.extend(hid);
        }
        Ok(set)
    }

    /// Fields of `entity` hidden from the holder wherever a set of its records is read, and never written (BD-40).
    pub fn hidden(&self, entity: &str) -> Option<&BTreeSet<String>> {
        if self.admin || !self.sees(entity) {
            return None;
        }
        self.entities.get(entity).map(|e| &e.hidden).filter(|h| !h.is_empty())
    }

    /// Fields of `entity` a rule hides (BD-40), never written by the holder.
    pub fn write_hidden(&self, entity: &str) -> Option<&BTreeSet<String>> {
        if self.admin || !self.sees(entity) {
            return None;
        }
        self.entities.get(entity).map(|e| &e.write_hidden).filter(|h| !h.is_empty())
    }

    /// Whether some line of `entity`'s records hides a field in sets (S9 R13).
    pub fn hides_lines(&self, entity: &str) -> bool {
        !self.admin && self.entities.get(entity).is_some_and(|e| !e.line_hidden.is_empty())
    }

    pub fn access(&self, entity: &str, action: Action) -> Access {
        if !self.sees(entity) {
            return Access::None;
        }
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

    /// The grants of the rules naming action `action` of `entity` (`None`: none names it).
    pub fn action_grants(&self, entity: &str, action: &str) -> Option<Vec<Grant>> {
        if !self.sees(entity) {
            return None;
        }
        self.entities.get(entity).and_then(|e| e.actions.get(action)).cloned()
    }

    /// What the holder sees of the conversation on `entity`'s records they read (BD-22): `None` nothing, `Some(false)`
    /// the shared comments and files, `Some(true)` those and their own. Only outside people are asked.
    pub fn shared(&self, entity: &str) -> Option<bool> {
        if !self.sees(entity) {
            return None;
        }
        self.entities.get(entity).and_then(|e| e.shared)
    }

    /// The actions of `entity` some rule names, whatever their own permission asks.
    pub fn named_actions(&self, entity: &str) -> Vec<&str> {
        if !self.sees(entity) {
            return vec![];
        }
        self.entities.get(entity).map(|e| e.actions.keys().map(String::as_str).collect()).unwrap_or_default()
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
        self.sees(entity) && self.group_readable(entity, f) && self.hidden(entity).is_none_or(|h| !h.contains(&f.name))
    }

    /// Whether the field's group lets the holder read it, whatever rules hide.
    pub fn group_readable(&self, entity: &str, f: &FieldDef) -> bool {
        match &f.perm_group {
            None => true,
            Some(g) => self.admin || self.groups(entity, false).contains(g.as_str()) || self.groups(entity, true).contains(g.as_str()),
        }
    }

    pub fn can_write_field(&self, entity: &str, f: &FieldDef) -> bool {
        if !self.sees(entity) || self.write_hidden(entity).is_some_and(|h| h.contains(&f.name)) {
            return false;
        }
        match &f.perm_group {
            None => true,
            Some(g) => self.admin || self.groups(entity, true).contains(g.as_str()),
        }
    }

    /// What an app's screen may do for this user (docs/plugins.md, "Pages an app brings"): the user's access
    /// narrowed to what the app declared (entity or `*` → actions as its consent lists them:
    /// `read`, `write (own records)`, `read field group <g>`...) and to every action on `own`, the
    /// app's own entities. Field groups only as declared; never administrator power.
    pub fn limited_to(&self, declared: &BTreeMap<String, BTreeSet<String>>, own: &BTreeSet<String>) -> PermSet {
        // A zone's limit holds on whatever is narrowed from it.
        let mut out = PermSet { limit: self.limit.clone(), ..Default::default() };
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
                    (Access::Own, Access::All, _) => vec![Grant { owner: true, ..Default::default() }],
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
            if let Some(mine) = self.entities.get(name) {
                e.read_rules = mine.read_rules.clone();
                e.hidden = mine.hidden.clone();
                e.write_hidden = mine.write_hidden.clone();
                e.line_hidden = mine.line_hidden.clone();
            }
            let hides = !e.hidden.is_empty() || !e.line_hidden.is_empty();
            if ACTIONS.iter().any(|(a, _)| e.get(*a) != Access::None) || !e.read_groups.is_empty() || !e.write_groups.is_empty() || hides {
                out.entities.insert(name.to_string(), e);
            }
        }
        out
    }
}

/// Everything known about the caller for one request.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// Set while what is read goes on to an AI provider: see [`Egress`].
    pub egress: Option<Arc<Egress>>,
    /// The workspace's account is restricted (crate::account): nothing changes, everything can be
    /// read and taken away.
    pub read_only: bool,
    pub tenant: Uuid,
    pub user: Uuid,
    pub email: String,
    pub full_name: String,
    pub roles: Vec<String>,
    pub is_admin: bool,
    /// Since when they have been an administrator (BD-83): an `administrators` approval step counts them after seven
    /// days, and an app can ask the same of an asker. None for everyone else.
    pub admin_since: Option<chrono::DateTime<chrono::Utc>>,
    pub is_system: bool,
    /// A portal person or a guest (docs/builder/decisions.md BD-02): a record's comments, files,
    /// assignments and tags are the workspace members' business, never theirs.
    pub outside: bool,
    pub request_id: String,
    /// Record scopes: entity → allowed record ids (e.g. `Company` → the user's companies).
    pub scopes: BTreeMap<String, Vec<Uuid>>,
    pub default_company: Option<Uuid>,
    pub fy_start_month: u32,
    pub schema: Arc<Schema>,
    pub perms: Arc<PermSet>,
    /// Automation recursion depth guard.
    pub depth: u8,
    /// The workspace's settings the expressions at hand read (BD-17), loaded where they are evaluated
    /// (`crate::settings::with`) and seen by them as `settings`; none elsewhere.
    pub settings: Option<Arc<Value>>,
    /// Where the change being made came from, when not from a person here: `outside` for a record a connection's
    /// mapping fetched from another system (BD-50). Hooks read it here; the change's events carry it as `origin`, and
    /// automations' conditions and effects read it as `origin`, so that what a new record sets going (invitations,
    /// notifications) is not set going for one fetched from outside.
    pub origin: Option<String>,
    /// How the request behind this context came in (crate::zones): its zone, the caller's address
    /// and its credential. None for work with no request behind it, and for the system.
    pub zone: Option<Arc<crate::zones::Entry>>,
    /// What can be said of the person's proof of who they are (crate::controls): step-up rules ask it.
    pub proof: crate::controls::Proof,
    /// The app whose code acts, when an app's call made this context (`Kernel::call_tx` names the caller); none for a
    /// request, a job, the system, and every effect inside an app's call (`actions::system`), so an effect never acts as
    /// the app (S1-P0c, BD-83).
    pub app: Option<String>,
    /// How this context was made (BD-83): set where each context is made and nowhere else; [`Made::Screen`] only by
    /// `Kernel::request_ctx_as` for an interactive session credential.
    pub made: Made,
    /// The reads this request has recorded already (crate::controls), shared by its clones.
    pub reads: Arc<crate::controls::Seen>,
}

/// How a context was made (BD-83, `gapclose/design/S1-payments.md` 2.21): what is acting behind it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Made {
    /// A person at a screen in this request: the browser's session cookie, or the web app's page token bound to it.
    Screen,
    /// A program with a person's credential: an API key, a bare bearer token, an app token, an OAuth token, SCIM, DAV, a
    /// socket.
    Key,
    /// An automation, as the administrator who saved it.
    Automation,
    /// Work queued earlier, as its creator or the system; also any context loaded outside a request.
    Job,
    /// An approved action completing, as its requester.
    Approval,
    /// The assistant or an MCP client acting for a person.
    Assistant,
    /// An agent (A2-A3, AG-1).
    Agent,
    /// The platform or an app's code, a person's context elevated included.
    #[default]
    System,
}

/// What may leave for an AI provider. Being able to read something does not make it yours to send:
/// a person with access to pay records may look at them, and that says nothing about passing them to
/// a third party. So a context can carry this, and everything read through it is narrowed: entities
/// marked `internal` are refused, and fields in a permission group or holding a secret are left out,
/// in records, lists, filters, sorting, reports and audit trails alike. A workspace can widen it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Egress {
    /// Field groups that may go.
    #[serde(default)]
    pub groups: BTreeSet<String>,
    /// `internal` entities that may go all the same.
    #[serde(default)]
    pub entities: BTreeSet<String>,
}

impl Egress {
    pub fn allows_entity(&self, ent: &ferrum_meta::EntityDef) -> bool {
        !ent.internal || self.entities.contains(&ent.name)
    }

    pub fn allows_field(&self, f: &FieldDef) -> bool {
        f.ftype != FieldType::Secret && f.perm_group.as_ref().is_none_or(|g| self.groups.contains(g))
    }
}

impl Ctx {
    /// This context, for reading what goes on to an AI provider.
    pub fn for_provider(&self, egress: Egress) -> Ctx {
        Ctx { egress: Some(Arc::new(egress)), made: Made::Assistant, ..self.clone() }
    }

    /// The one test of "a decision only a person makes" (BD-83): a context `request_ctx_as` built for this HTTP request
    /// from an interactive session credential, not the system, carrying no app, not the assistant. An automation, a key,
    /// a job, an approval completing later, the assistant and an app's call are each refused where this is required.
    pub fn at_screen(&self) -> bool {
        self.made == Made::Screen && !self.is_system && self.app.is_none() && self.egress.is_none()
    }

    /// Whether the person's browser session proved who they are within the last `seconds` (its step-up, read from the
    /// session in the database when the request came in).
    pub fn stepped_up_within(&self, seconds: i64) -> bool {
        matches!(self.proof, crate::controls::Proof::Person { at: Some(at), .. } if chrono::Utc::now() - at <= chrono::Duration::seconds(seconds))
    }

    /// This context with `is_system` set: made [`Made::System`] with it, never a screen.
    pub fn elevated(mut self) -> Ctx {
        self.is_system = true;
        self.made = Made::System;
        self
    }

    /// Full-power context for engine work (seeding, automations, migrations).
    pub fn system(tenant: Uuid, schema: Arc<Schema>) -> Ctx {
        Ctx {
            egress: None,
            read_only: false,
            tenant,
            user: Uuid::nil(),
            email: "system@ferrum.local".into(),
            full_name: "System".into(),
            roles: vec![],
            is_admin: true,
            admin_since: None,
            is_system: true,
            made: Made::System,
            outside: false,
            request_id: format!("sys-{}", Uuid::now_v7()),
            scopes: BTreeMap::new(),
            default_company: None,
            fy_start_month: schema.fy_start_month,
            schema,
            perms: Arc::new(PermSet { admin: true, ..Default::default() }),
            depth: 0,
            settings: None,
            origin: None,
            zone: None,
            proof: crate::controls::Proof::Inside,
            app: None,
            reads: Default::default(),
        }
    }

    /// The system, acting where `ctx`'s request came in: every right, within the limit of `ctx`'s zone
    /// (crate::zones, level 2). What a limited person sets going reaches no more than their zone does.
    pub fn system_for(ctx: &Ctx) -> Ctx {
        let mut sys = Ctx::system(ctx.tenant, ctx.schema.clone());
        sys.zone = ctx.zone.clone();
        // What it changes for the person needs what the person would need (crate::controls).
        sys.proof = ctx.proof;
        sys.perms = Arc::new(PermSet { admin: true, limit: ctx.perms.limit.clone(), ..Default::default() });
        sys
    }

    /// The entities this context's zone shows, when it hides any (crate::zones, level 2; not in report mode): what
    /// SQL listing things about records (notifications, mentions) keeps to, as `$n::text[] is null or entity is null
    /// or entity = any($n)`.
    pub fn zone_entities(&self) -> Option<Vec<String>> {
        self.perms.limit.as_ref().filter(|l| !l.report).map(|l| l.entities.iter().cloned().collect())
    }

    /// Whether this context reaches package `package`'s own code at all in its zone (crate::zones, level 2): its
    /// signed-in app endpoints, its co-edited documents, its capabilities.
    pub fn sees_package(&self, package: &str) -> bool {
        self.perms.limit.as_ref().is_none_or(|l| l.sees_package(package))
    }

    /// Whether this context reaches `entity` at all in its zone (crate::zones, level 2).
    pub fn sees(&self, entity: &str) -> bool {
        self.perms.sees(entity)
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
        let mut user = json!({
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
        });
        // Bound as `settings` for the expressions that name them (ferrum_meta::Expr), as time is.
        if let Some(s) = &self.settings {
            user["settings"] = (**s).clone();
        }
        user
    }

    pub fn has_role(&self, role: &str) -> bool {
        self.is_admin || self.roles.iter().any(|r| r == role)
    }

    pub fn require(&self, entity: &str, action: Action) -> Result<Access> {
        if self.read_only && !matches!(action, Action::Read | Action::Export) {
            return Err(Error::Forbidden(
                "this workspace is read-only now that its account is closed or restricted; you can still read and export everything".into(),
            ));
        }
        match self.perms.access(entity, action) {
            Access::None => Err(Error::Forbidden(format!("not permitted to {action:?} {entity}").to_lowercase())),
            // Rights first: a step-up is asked only of whoever may do it (crate::controls).
            a => self.check_step_up(entity, crate::controls::work_of(action)).map(|()| a),
        }
    }
}

/// A condition restricting which records an action applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeCond {
    /// `None` = the record's own id.
    pub field: Option<String>,
    pub ext: bool,
    pub ids: Vec<Uuid>,
    /// Strict: the value must be in `ids`. Lenient: empty values also pass.
    pub strict: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

/// A rule's reach, ready to test: the user's own records and/or fields holding given values or
/// falling in given ranges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeGrant {
    pub owner: bool,
    pub fields: Vec<GrantField>,
    pub ranges: Vec<GrantRange>,
    /// Ownership through a path rather than by creator and owner fields.
    pub via: Option<ViaScope>,
}

/// An owner path compiled against the schema in force (BD-02): its SQL from the record (`{a}`, the
/// person's email bound between `prefix` and `suffix`), and how one record in memory is tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViaScope {
    pub path: String,
    prefix: String,
    suffix: String,
    first: ViaFirst,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ViaFirst {
    /// The record's own field holds the email.
    Field(String),
    /// A child table's rows hold it: the table field and the row field.
    Table(String, String),
    /// It lies past a link: looked up in the database, from the linked record (alias `vt`).
    Link { field: String, table: String, pred: String, prefix: String, suffix: String },
}

/// SQL testing an owner path from alias `alias`: a prefix and a suffix the email is bound between.
/// `None` when the schema no longer holds the path: the rule then reaches nothing.
fn via_sql(schema: &Schema, from: &EffEntity, steps: &[ViaStep], alias: &str, depth: usize) -> Option<(String, String)> {
    match steps.first()? {
        ViaStep::Email { field } => {
            let expr = crate::store::field_expr(from, field, alias)?;
            Some((format!("lower(trim({expr})) = "), String::new()))
        }
        ViaStep::Table { field, .. } => {
            let info = from.children.get(field)?;
            let cent = schema.entity(&info.entity)?;
            let Some(ViaStep::Email { field: email }) = steps.get(1) else { return None };
            let c = format!("vc{depth}");
            let expr = crate::store::field_expr(cent, email, &c)?;
            Some((
                format!(
                    "exists (select 1 from {} {c} where {c}.tenant_id = {alias}.tenant_id and {c}.parent_id = {alias}.id{} and lower(trim({expr})) = ",
                    crate::ddl::q(&info.table),
                    info.predicate(&c)
                ),
                ")".into(),
            ))
        }
        ViaStep::Link { field, to } => {
            let target = schema.entity(to)?;
            if target.table.is_empty() {
                return None;
            }
            let link = crate::store::field_expr(from, field, alias)?;
            let v = format!("vl{depth}");
            let (p, s) = via_sql(schema, target, &steps[1..], &v, depth + 1)?;
            Some((
                format!(
                    "exists (select 1 from {} {v} where {v}.tenant_id = {alias}.tenant_id{} and {v}.id = {link} and {p}",
                    crate::ddl::q(&target.table),
                    crate::store::entity_predicate(target, &v)
                ),
                format!("{s})"),
            ))
        }
    }
}

/// Compiles rule path `path` for entity `ent` against the schema in force.
fn compile_via(schema: &Schema, ent: &EffEntity, path: &str) -> Option<ViaScope> {
    let steps = ferrum_meta::via::resolve(&ent.def.name, path, |n| schema.entity(n).map(|e| &e.def)).ok()?;
    let (prefix, suffix) = via_sql(schema, ent, &steps, "{a}", 0)?;
    let first = match &steps[..] {
        [ViaStep::Email { field }] => ViaFirst::Field(field.clone()),
        [ViaStep::Table { field, .. }, ViaStep::Email { field: email }] => ViaFirst::Table(field.clone(), email.clone()),
        [ViaStep::Link { field, to }, rest @ ..] => {
            let target = schema.entity(to)?;
            let (prefix, suffix) = via_sql(schema, target, rest, "vt", 1)?;
            ViaFirst::Link { field: field.clone(), table: crate::ddl::q(&target.table), pred: crate::store::entity_predicate(target, "vt"), prefix, suffix }
        }
        _ => return None,
    };
    Some(ViaScope { path: path.to_string(), prefix, suffix, first })
}

/// A field of a grant's condition: its accepted values as text, and its SQL (`{a}`: the alias).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantField {
    pub field: String,
    pub values: Vec<String>,
    expr: String,
    /// A checkbox: unset reads as `false`, in SQL and in [`RowScope::matches`] alike.
    check: bool,
}

/// A range of a grant's condition, with days from today resolved: its ends as text, and its SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRange {
    pub field: String,
    low: Option<String>,
    high: Option<String>,
    /// Compared as dates, or else as numbers: in SQL and in [`RowScope::matches`] alike.
    date: bool,
    expr: String,
}

impl GrantRange {
    /// `None` when the field is gone: the rule then reaches nothing, never more.
    fn new(ctx: &Ctx, ent: &EffEntity, field: String, range: ConditionRange) -> Option<GrantRange> {
        let date = matches!(ent.field(&field)?.ftype, FieldType::Date | FieldType::Datetime);
        let expr = crate::store::field_expr(ent, &field, "{a}")?;
        let day = |n: Option<i32>| n.map(|n| (ctx.today() + chrono::Duration::days(n.into())).to_string());
        Some(GrantRange { low: range.gte.or(day(range.from_today)), high: range.lte.or(day(range.to_today)), date, expr, field })
    }

    /// Whether a document's value lies in the range. No value, or one that cannot be read, lies
    /// outside it, as SQL's `null` comparisons leave it.
    fn holds(&self, v: Option<&Value>) -> bool {
        let text = condition_text(v);
        let between = |low: Option<bool>, high: Option<bool>| low != Some(false) && high != Some(false);
        if self.date {
            let day = |s: &str| chrono::NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok();
            let Some(value) = day(&text) else { return false };
            between(self.low.as_deref().map(|l| day(l).is_some_and(|l| value >= l)), self.high.as_deref().map(|h| day(h).is_some_and(|h| value <= h)))
        } else {
            let number = |s: &str| s.trim().parse::<rust_decimal::Decimal>().ok();
            let Some(value) = number(&text) else { return false };
            between(self.low.as_deref().map(|l| number(l).is_some_and(|l| value >= l)), self.high.as_deref().map(|h| number(h).is_some_and(|h| value <= h)))
        }
    }
}

impl ScopeGrant {
    /// The user's own records, nothing else.
    pub fn own() -> ScopeGrant {
        ScopeGrant { owner: true, ..Default::default() }
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
                        "exists (select 1 from {} oc where oc.tenant_id = {{a}}.tenant_id and oc.parent_id = {{a}}.id{} and lower({expr}) = ",
                        crate::ddl::q(&info.table),
                        info.predicate("oc")
                    ),
                    suffix: ")".into(),
                })
            }
        })
        .collect()
}

pub fn row_scope(ctx: &Ctx, ent: &EffEntity, action: Action) -> Result<RowScope> {
    ctx.require(&ent.def.name, action)?;
    scope_of(ctx, ent, ctx.perms.grants(&ent.def.name, action))
}

/// The records action `name` of `ent` may run on (BD-02): those of the rules naming it when some
/// rule does, else those of the action's own permission `perm`, as [`row_scope`] gives them.
pub fn action_scope(ctx: &Ctx, ent: &EffEntity, name: &str, perm: Action) -> Result<RowScope> {
    match ctx.perms.action_grants(&ent.def.name, name) {
        Some(grants) if !ctx.is_admin && !ctx.is_system => {
            if ctx.read_only && !matches!(perm, Action::Read | Action::Export) {
                return Err(Error::Forbidden(
                    "this workspace is read-only now that its account is closed or restricted; you can still read and export everything".into(),
                ));
            }
            ctx.check_step_up(&ent.def.name, crate::controls::work_of(perm))?;
            scope_of(ctx, ent, Some(grants))
        }
        _ => row_scope(ctx, ent, perm),
    }
}

/// Whether `ctx` may press action `def` of `ent` at all, and on which records: its permission or a rule naming it
/// ([`action_scope`]), then a configured action's `roles` (an administrator always may). The run path and the
/// entity's description (`may_run`) both ask this.
pub fn action_gate(ctx: &Ctx, ent: &EffEntity, def: &ferrum_meta::ActionDef) -> Result<RowScope> {
    let scope = action_scope(ctx, ent, &def.name, Action::parse(&def.permission).unwrap_or(Action::Write))?;
    if !def.roles.is_empty() && !def.roles.iter().any(|r| ctx.has_role(r)) {
        return Err(Error::Forbidden(format!("'{}' is for {}", def.label, def.roles.join(", "))));
    }
    Ok(scope)
}

/// Whether `ctx` may take workflow transition `t` of `ent`, and on which records (the run path and the entity's
/// description, `may_take`, both ask this): one naming roles, a holder of one, on a record they may read; one
/// naming none, whoever may change the record. An outside person (BD-02) takes only one naming a role of theirs.
pub fn transition_gate(ctx: &Ctx, ent: &EffEntity, t: &ferrum_meta::WorkflowTransition) -> Result<RowScope> {
    if t.roles.is_empty() {
        if ctx.outside {
            return Err(Error::Forbidden(format!("'{}' is for the workspace's members", t.action)));
        }
        return row_scope(ctx, ent, Action::Write);
    }
    if !t.roles.iter().any(|r| ctx.has_role(r)) {
        return Err(Error::Forbidden(format!("'{}' requires one of the roles: {}", t.action, t.roles.join(", "))));
    }
    row_scope(ctx, ent, Action::Read)
}

/// The fields of one record hidden from `ctx` (BD-40): those every rule reaching the record hides. A field one rule
/// hides and another reaching the same record shows is shown. Empty when no rule hides anything.
pub async fn record_hidden(ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &Doc) -> Result<RecordHidden> {
    if ctx.perms.admin {
        return Ok(RecordHidden::default());
    }
    let Some(e) = ctx.perms.entities.get(&ent.def.name) else { return Ok(RecordHidden::default()) };
    let mut out = RecordHidden { fields: ctx.perms.hidden(&ent.def.name).cloned().unwrap_or_default(), lines: e.line_hidden.clone() };
    // Each rule reaching the record by its own arms shows what it does not hide; a share or the line would lift nothing.
    for rule in &e.read_rules {
        if out.fields.is_empty() && out.lines.values().all(BTreeSet::is_empty) {
            break;
        }
        let narrows = !out.fields.iter().all(|f| rule.hide.contains(f))
            || out.lines.iter().any(|(l, hid)| !hid.iter().all(|f| rule.line_hide.get(l).is_some_and(|h| h.contains(f))));
        if !narrows {
            continue;
        }
        if scope_of(ctx, ent, Some(vec![rule.grant.clone()]))?.admits(tx, ctx.tenant, doc).await? {
            out.fields.retain(|f| rule.hide.contains(f));
            for (l, hid) in out.lines.iter_mut() {
                hid.retain(|f| rule.line_hide.get(l).is_some_and(|h| h.contains(f)));
            }
        }
    }
    // A line entity's own rules' hides (BD-40: `{entity: Line, hide: [...]}`) hold on every line, whichever rule
    // reaches the record.
    for l in line_entities(&ctx.schema, ent) {
        if let Some(own) = ctx.perms.write_hidden(&l) {
            out.lines.entry(l).or_default().extend(own.iter().cloned());
        }
    }
    out.lines.retain(|_, h| !h.is_empty());
    Ok(out)
}

/// Whether one record of `ent` may hide something from `ctx` ([`record_hidden`] has anything to say): a field, or a
/// field of its lines at any depth.
pub fn may_hide(ctx: &Ctx, ent: &EffEntity) -> bool {
    !ctx.perms.admin
        && (ctx.perms.hidden(&ent.def.name).is_some()
            || ctx.perms.hides_lines(&ent.def.name)
            || line_entities(&ctx.schema, ent).iter().any(|l| ctx.perms.write_hidden(l).is_some()))
}

/// The entities of `ent`'s lines, at every depth.
pub fn line_entities(schema: &ferrum_meta::Schema, ent: &EffEntity) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut todo: Vec<String> = ent.children.values().map(|c| c.entity.clone()).collect();
    while let Some(l) = todo.pop() {
        if out.contains(&l) {
            continue;
        }
        todo.extend(schema.entity(&l).map(|e| e.children.values().map(|c| c.entity.clone()).collect::<Vec<_>>()).unwrap_or_default());
        out.push(l);
    }
    out
}

/// What one record does not show its reader ([`record_hidden`]): its fields, and its lines' per line entity.
#[derive(Debug, Clone, Default)]
pub struct RecordHidden {
    pub fields: BTreeSet<String>,
    pub lines: BTreeMap<String, BTreeSet<String>>,
}

/// The scope `grants` give on `ent` (`None`: every record), with the user's scopes on top.
fn scope_of(ctx: &Ctx, ent: &EffEntity, grants: Option<Vec<Grant>>) -> Result<RowScope> {
    if ctx.egress.as_ref().is_some_and(|e| !e.allows_entity(&ent.def)) {
        return Err(Error::Forbidden(format!("{} stays inside this workspace: it is not sent to the AI provider", ent.def.label)));
    }
    let mut scope = RowScope::default();
    if ctx.is_admin || ctx.is_system {
        return Ok(scope);
    }
    if let Some(mut grants) = grants {
        if grants.is_empty() {
            // Partial access with no grant recorded (a hand-made permission set): own records.
            grants.push(Grant { owner: true, ..Default::default() });
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
            let ranges: Option<Vec<GrantRange>> = g.ranges.into_iter().map(|(field, range)| GrantRange::new(ctx, ent, field, range)).collect();
            let via = match &g.via {
                None => Some(None),
                Some(path) => compile_via(&ctx.schema, ent, path).map(Some),
            };
            // A condition on a field the entity no longer has, or a path it no longer holds, reaches
            // nothing (never more).
            if let (Some(fields), Some(ranges), Some(via)) = (fields, ranges, via) {
                scope.grants.push(ScopeGrant { owner: g.owner, fields, ranges, via });
            }
        }
        if scope.grants.is_empty() {
            // Every grant pointed at a missing field: nothing qualifies.
            let nothing = GrantField { field: "id".into(), values: vec![], expr: "{a}.id".into(), check: false };
            scope.grants.push(ScopeGrant { owner: false, fields: vec![nothing], ranges: vec![], via: None });
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

    /// Whether the user owns the record by grant `g`: through its path when it has one (`None`: the
    /// path goes past a link, which only [`RowScope::admits`] can look up), else as [`RowScope::owns`].
    fn owns_by(&self, g: &ScopeGrant, doc: &Doc) -> Option<bool> {
        let Some(via) = &g.via else { return Some(self.owns(doc)) };
        let Some(email) = &self.owner_email else { return Some(false) };
        let is_me = |v: Option<&Value>| v.and_then(Value::as_str).is_some_and(|s| s.trim().eq_ignore_ascii_case(email));
        match &via.first {
            ViaFirst::Field(f) => Some(is_me(doc.get(f))),
            ViaFirst::Table(t, f) => Some(doc.get(t).and_then(Value::as_array).is_some_and(|rows| rows.iter().any(|r| is_me(r.get(f))))),
            ViaFirst::Link { .. } => None,
        }
    }

    /// Whether the record's fields satisfy grant `g`'s condition and ranges.
    fn holds(g: &ScopeGrant, doc: &Doc) -> bool {
        let field = |f: &GrantField| {
            let v = doc.get(&f.field).filter(|v| !v.is_null());
            f.values.contains(&if f.check && v.is_none() { "false".into() } else { condition_text(v) })
        };
        g.fields.iter().all(field) && g.ranges.iter().all(|r| r.holds(doc.get(&r.field).filter(|v| !v.is_null())))
    }

    /// Whether the record is within reach as far as memory tells: an owner path past a link counts
    /// as not reaching here. Checking one record, use [`RowScope::admits`], which looks that up.
    pub fn matches(&self, doc: &Doc) -> bool {
        if !self.grants.is_empty() {
            let reached = self.grants.iter().any(|g| (!g.owner || self.owns_by(g, doc) == Some(true)) && Self::holds(g, doc));
            if !reached {
                return false;
            }
        }
        self.conds_hold(doc)
    }

    /// The index of the first grant reaching the record, looking up owner paths past a link (for
    /// explaining access), or `None`.
    pub async fn reached_by(&self, tx: &mut Tx, tenant: Uuid, doc: &Doc) -> Result<Option<usize>> {
        if !self.conds_hold(doc) {
            return Ok(None);
        }
        for (i, g) in self.grants.iter().enumerate() {
            if !Self::holds(g, doc) {
                continue;
            }
            let owned = match self.owns_by(g, doc) {
                _ if !g.owner => true,
                Some(known) => known,
                None => self.owns_past_link(tx, tenant, g, doc).await?,
            };
            if owned {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }

    /// Whether the record is within reach (BD-02): as [`RowScope::matches`], and where an owner path
    /// goes past a link, looked up in `tx` from the record's link, never from a cache, so a person
    /// taken off the linked record loses this one at once. Every check of a single record uses this.
    pub async fn admits(&self, tx: &mut Tx, tenant: Uuid, doc: &Doc) -> Result<bool> {
        if self.matches(doc) {
            return Ok(true);
        }
        if !self.grants.iter().any(|g| matches!(g.via, Some(ViaScope { first: ViaFirst::Link { .. }, .. }))) {
            return Ok(false);
        }
        Ok(self.reached_by(tx, tenant, doc).await?.is_some())
    }

    /// Grant `g`'s owner path past a link, for one record: whether the linked record leads to the
    /// person's email.
    async fn owns_past_link(&self, tx: &mut Tx, tenant: Uuid, g: &ScopeGrant, doc: &Doc) -> Result<bool> {
        let (Some(ViaScope { first: ViaFirst::Link { field, table, pred, prefix, suffix }, .. }), Some(email)) = (&g.via, &self.owner_email) else {
            return Ok(false);
        };
        let Some(id) = doc.get(field).and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(false) };
        let sql = format!("select exists (select 1 from {table} vt where vt.tenant_id = $1{pred} and vt.id = $2 and {prefix}$3{suffix})");
        Ok(sqlx::query_scalar::<_, bool>(AssertSqlSafe(sql)).bind(tenant).bind(id).bind(email).fetch_one(&mut **tx).await?)
    }

    /// Whether the record satisfies the scopes every grant is subject to (companies and the like).
    fn conds_hold(&self, doc: &Doc) -> bool {
        self.conds.iter().all(|c| {
            let key = c.field.as_deref().unwrap_or("id");
            match doc.get(key).and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) {
                Some(v) => c.ids.contains(&v),
                None => !c.strict,
            }
        })
    }

    /// Grant `g`'s ownership for table alias `alias`: its path to the person's email, or else
    /// [`RowScope::push_owner`]. Someone with no email owns nothing by a path.
    fn push_ownership(&self, qb: &mut QueryBuilder<Postgres>, g: &ScopeGrant, alias: &str) {
        match (&g.via, &self.owner_email) {
            (None, _) => self.push_owner(qb, alias),
            (Some(_), None) => {
                qb.push("false");
            }
            (Some(v), Some(email)) => {
                qb.push(format!("({}", v.prefix.replace("{a}", alias))).push_bind(email.clone()).push(format!("{})", v.suffix));
            }
        }
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
    /// `(owner and field = any(values) and range and ...)`.
    fn push_grant(&self, qb: &mut QueryBuilder<Postgres>, g: &ScopeGrant, alias: &str) {
        if g.owner && g.fields.is_empty() && g.ranges.is_empty() {
            return self.push_ownership(qb, g, alias);
        }
        qb.push("(");
        let mut first = true;
        let mut and = |qb: &mut QueryBuilder<Postgres>| {
            if !std::mem::replace(&mut first, false) {
                qb.push(" and ");
            }
        };
        if g.owner {
            and(qb);
            self.push_ownership(qb, g, alias);
        }
        for f in &g.fields {
            and(qb);
            qb.push(format!("({})::text = any(", f.expr.replace("{a}", alias))).push_bind(f.values.clone()).push(")");
        }
        for r in &g.ranges {
            let cast = if r.date { "date" } else { "numeric" };
            let value = format!("({})::{cast}", r.expr.replace("{a}", alias));
            for (end, op) in [(&r.low, ">="), (&r.high, "<=")] {
                if let Some(end) = end {
                    and(qb);
                    qb.push(format!("{value} {op} ")).push_bind(end.clone()).push(format!("::{cast}"));
                }
            }
        }
        if first {
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
        let own = Grant { owner: true, ..Default::default() };
        let goods = Grant { condition: vec![("kind".into(), vec!["goods".into()])], ..Default::default() };
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
        let a = PermSet { admin: true, ..Default::default() }.limited_to(&declared, &mine);
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
