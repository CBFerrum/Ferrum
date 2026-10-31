//! The compiler: a [`Spec`] is resolved against the effective metadata and the caller's
//! permissions into a [`Plan`], which renders parameterized SQL.
//!
//! Rules that make it safe to expose to every user and to AI agents:
//! * every identifier comes from metadata (entity tables, field columns, `date_trunc` units);
//!   every value is a bound parameter;
//! * the entity, every linked entity it joins and every field it touches — in columns, measures,
//!   filters, sorts and search — must be readable by the caller; the row scopes (own records,
//!   company and other user scopes) of the base entity and of every joined entity are applied in
//!   the query itself;
//! * only whitelisted aggregates exist; results are capped.
//!
//! Shape: the "source" is `select <per-row values> from base a [join child c] [left join link lN]
//! where ...`; statements group or list over it (`... from (<source>) s`).

use crate::formula::{Formula, MAX_FORMULAS};
use crate::spec::{Agg, Bucket, Compare, Dir, MAX_PIVOT_CELLS, MAX_PIVOT_COLUMNS, Spec, resolve_date};
use chrono::NaiveDate;
use ferrum_kernel::ctx::RowScope;
use ferrum_kernel::ddl::q;
use ferrum_kernel::query::{Filter, parse_filters, push_filter};
use ferrum_kernel::store::{SYSTEM_KEYS, entity_predicate, field_expr};
use ferrum_kernel::{Action, Ctx, Error, FieldError, Result, row_scope};
use ferrum_meta::{EffEntity, EntityKind, FieldDef, FieldType, Schema};
use serde_json::{Map, Value, json};
use sqlx::{Postgres, QueryBuilder};
use std::sync::Arc;

pub(crate) fn bad(at: &str, msg: impl Into<String>) -> Error {
    Error::Validation(vec![FieldError::new(at, msg)])
}

/// Title source of `created_by` / `updated_by` values.
pub(crate) const USERS: &str = "@user";

/// How a value is typed for the UI and decoded from Postgres.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ty {
    Field(FieldType),
    /// Record ids.
    Id,
    /// `created_by` / `updated_by`: user ids, shown by name.
    User,
    /// `number`, `workflow_state`.
    Text,
    /// `docstatus`, `version`.
    Int,
    /// `created_at`, `updated_at`.
    Timestamp,
}

impl Ty {
    fn system(name: &str) -> Option<Ty> {
        Some(match name {
            "id" | "amended_from" => Ty::Id,
            "created_by" | "updated_by" => Ty::User,
            "number" | "workflow_state" => Ty::Text,
            "docstatus" | "version" => Ty::Int,
            "created_at" | "updated_at" => Ty::Timestamp,
            _ => return None,
        })
    }

    /// The column type the UI formats by (the metadata field type names).
    pub(crate) fn name(self) -> String {
        match self {
            Ty::Field(ft) => serde_json::to_value(ft).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "data".into()),
            Ty::Id | Ty::User | Ty::Text => "data".into(),
            Ty::Int => "int".into(),
            Ty::Timestamp => "datetime".into(),
        }
    }

    pub(crate) fn is_decimal(self) -> bool {
        matches!(self, Ty::Field(ft) if ft.is_decimal())
    }
    pub(crate) fn is_int(self) -> bool {
        matches!(self, Ty::Int | Ty::Field(FieldType::Int))
    }
    fn summable(self) -> bool {
        matches!(self, Ty::Field(ft) if ft.is_numeric())
    }
    fn is_date(self) -> bool {
        self == Ty::Field(FieldType::Date)
    }
    pub(crate) fn is_datetime(self) -> bool {
        matches!(self, Ty::Field(FieldType::Datetime) | Ty::Timestamp)
    }
    fn ordered(self) -> bool {
        self.summable() || self.is_date() || self.is_datetime() || self == Ty::Int
    }
    fn groupable(self) -> bool {
        !matches!(self, Ty::Field(FieldType::Text | FieldType::Markdown | FieldType::Code | FieldType::Json | FieldType::Table | FieldType::Secret))
    }
    /// Aggregates allowed on this type, for the builder UI.
    pub(crate) fn aggregates(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self != Ty::Field(FieldType::Json) {
            out.push("count_distinct");
        }
        if self.summable() {
            out.extend(["sum", "avg"]);
        }
        if self.ordered() {
            out.extend(["min", "max"]);
        }
        out
    }
    pub(crate) fn buckets(self) -> bool {
        self.is_date() || self.is_datetime()
    }
}

/// Where a resolved field lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Loc {
    Base,
    /// Through a link field of the base entity.
    Link(String),
    /// In a child table of the base entity.
    Child(String),
    /// Through a link field of a child table's rows.
    ChildLink(String, String),
}

impl Loc {
    fn table(&self) -> Option<&str> {
        match self {
            Loc::Child(t) | Loc::ChildLink(t, _) => Some(t),
            _ => None,
        }
    }
}

/// A field path checked against the metadata and the caller's permissions.
#[derive(Debug, Clone)]
pub(crate) struct Res {
    pub path: String,
    pub label: String,
    pub ty: Ty,
    pub def: Option<FieldDef>,
    pub loc: Loc,
    /// The field's name on the entity it lives on.
    pub field: String,
}

impl Res {
    /// Column metadata for the UI: type, link target, select options, precision.
    pub(crate) fn describe(&self, m: &mut Map<String, Value>) {
        m.insert("type".into(), json!(self.ty.name()));
        if let Some(d) = &self.def {
            if let Some(to) = &d.to {
                m.insert("to".into(), json!(to));
            }
            if !d.options.is_empty() {
                m.insert("options".into(), json!(d.options));
            }
            if let Some(p) = d.precision {
                m.insert("precision".into(), json!(p));
            }
        }
    }
}

/// Resolves field paths for one base entity and one caller.
pub(crate) struct Resolver<'a> {
    pub ctx: &'a Ctx,
    pub schema: &'a Schema,
    pub base: Arc<EffEntity>,
}

impl<'a> Resolver<'a> {
    /// The base entity of a query, which the caller must be able to read.
    pub fn new(ctx: &'a Ctx, entity: &str, at: &str) -> Result<Resolver<'a>> {
        let schema = ctx.schema.as_ref();
        let base = schema.entity(entity).cloned().ok_or_else(|| bad(at, format!("unknown entity '{entity}'")))?;
        match base.def.kind {
            EntityKind::Child => {
                return Err(bad(at, format!("{} rows belong to their parent document: query the parent and use its table field", base.def.label)));
            }
            EntityKind::Ledger => return Err(bad(at, format!("{} is a ledger: use the built-in reports for it", base.def.label))),
            _ => {}
        }
        if base.table.is_empty() {
            return Err(bad(at, format!("{entity} has no table")));
        }
        readable(ctx, &base, at)?;
        Ok(Resolver { ctx, schema, base })
    }

    fn can_read(&self, ent: &EffEntity, f: &FieldDef, at: &str) -> Result<()> {
        if self.ctx.perms.can_read_field(&ent.def.name, f) {
            Ok(())
        } else {
            Err(Error::Forbidden(format!("{at}: you are not permitted to read {} of {}", f.label(), ent.def.label)))
        }
    }

    fn link_target(&self, ent: &EffEntity, f: &FieldDef, at: &str) -> Result<Arc<EffEntity>> {
        let target = f.to.as_deref().and_then(|t| self.schema.entity(t)).cloned().ok_or_else(|| bad(at, format!("{} links to an unknown entity", f.name)))?;
        if target.table.is_empty() || target.def.kind == EntityKind::Ledger {
            return Err(bad(at, format!("{} of {} cannot be followed", f.name, ent.def.label)));
        }
        readable(self.ctx, &target, at)?;
        Ok(target)
    }

    fn child(&self, ent: &EffEntity, table: &str, at: &str) -> Result<Arc<EffEntity>> {
        let info = ent.children.get(table).ok_or_else(|| bad(at, format!("'{table}' is not a child table of {}", ent.def.label)))?;
        self.schema.entity(&info.entity).cloned().ok_or_else(|| bad(at, format!("child entity {} is missing", info.entity)))
    }

    /// The entity a location's fields belong to.
    pub fn entity_of(&self, loc: &Loc) -> Result<Arc<EffEntity>> {
        let link = |ent: &EffEntity, l: &str| -> Result<Arc<EffEntity>> {
            ent.field(l).and_then(|f| f.to.as_deref()).and_then(|t| self.schema.entity(t)).cloned().ok_or_else(|| Error::internal(format!("link {l} vanished")))
        };
        Ok(match loc {
            Loc::Base => self.base.clone(),
            Loc::Link(l) => link(&self.base, l)?,
            Loc::Child(t) => self.child(&self.base, t, "")?,
            Loc::ChildLink(t, l) => {
                let child = self.child(&self.base, t, "")?;
                link(&child, l)?
            }
        })
    }

    /// Resolves `field`, `link.field`, `table.field` or `table.link.field`; `at` names the spot in
    /// the spec for error messages.
    pub fn resolve(&self, path: &str, at: &str) -> Result<Res> {
        let segs: Vec<&str> = path.split('.').collect();
        if path.trim().is_empty() || segs.iter().any(|s| s.is_empty()) {
            return Err(bad(at, format!("'{path}' is not a field path")));
        }
        let base = self.base.clone();
        let hop = |ent: &EffEntity, name: &str| -> Result<FieldDef> {
            let f = ent.field(name).cloned().ok_or_else(|| bad(at, format!("unknown field '{name}' on {}", ent.def.label)))?;
            self.can_read(ent, &f, at)?;
            Ok(f)
        };
        match segs.as_slice() {
            [f] => self.leaf(&base, f, Loc::Base, String::new(), path, at),
            [h, f] => {
                let hf = hop(&base, h)?;
                match hf.ftype {
                    FieldType::Link => {
                        let target = self.link_target(&base, &hf, at)?;
                        self.leaf(&target, f, Loc::Link(h.to_string()), format!("{} › ", hf.label()), path, at)
                    }
                    FieldType::Table => {
                        let child = self.child(&base, h, at)?;
                        self.leaf(&child, f, Loc::Child(h.to_string()), format!("{} › ", hf.label()), path, at)
                    }
                    _ => Err(bad(at, format!("'{h}' is neither a link nor a child table, so '{path}' cannot be followed"))),
                }
            }
            [t, l, f] => {
                let tf = hop(&base, t)?;
                if tf.ftype != FieldType::Table {
                    return Err(bad(at, format!("'{t}' is not a child table: three-part paths are table.link.field")));
                }
                let child = self.child(&base, t, at)?;
                let lf = hop(&child, l)?;
                if lf.ftype != FieldType::Link {
                    return Err(bad(at, format!("'{l}' of {} is not a link", tf.label())));
                }
                let target = self.link_target(&child, &lf, at)?;
                self.leaf(&target, f, Loc::ChildLink(t.to_string(), l.to_string()), format!("{} › {} › ", tf.label(), lf.label()), path, at)
            }
            _ => Err(bad(at, format!("'{path}' is too deep: paths are field, link.field, table.field or table.link.field"))),
        }
    }

    fn leaf(&self, ent: &EffEntity, name: &str, loc: Loc, prefix: String, path: &str, at: &str) -> Result<Res> {
        let child = ent.def.kind == EntityKind::Child;
        if SYSTEM_KEYS.contains(&name) {
            if child {
                return Err(bad(at, format!("child rows have no '{name}'")));
            }
            let ty = Ty::system(name).ok_or_else(|| Error::internal("system key without a type"))?;
            return Ok(Res { path: path.into(), label: format!("{prefix}{}", ferrum_meta::humanize(name)), ty, def: None, loc, field: name.into() });
        }
        let f = ent.field(name).cloned().ok_or_else(|| bad(at, format!("unknown field '{name}' on {}", ent.def.label)))?;
        match f.ftype {
            FieldType::Table => return Err(bad(at, format!("'{name}' is a child table: use {name}.<field>"))),
            FieldType::Secret => return Err(bad(at, format!("'{name}' is a secret and cannot be queried"))),
            _ => {}
        }
        self.can_read(ent, &f, at)?;
        if ent.storage_of(name).is_none() {
            return Err(bad(at, format!("'{name}' is not stored")));
        }
        Ok(Res { path: path.into(), label: format!("{prefix}{}", f.label()), ty: Ty::Field(f.ftype), def: Some(f), loc, field: name.into() })
    }
}

/// The caller must be able to read `ent` (own records count).
pub(crate) fn readable(ctx: &Ctx, ent: &EffEntity, at: &str) -> Result<RowScope> {
    row_scope(ctx, ent, Action::Read).map_err(|_| Error::Forbidden(format!("{at}: you are not permitted to read {}", ent.def.label)))
}

struct Join {
    key: String,
    alias: String,
    target: Arc<EffEntity>,
    /// SQL of the link value being followed.
    on: String,
    scope: RowScope,
}

struct Grain {
    table: String,
    sql_table: String,
}

/// One filter, placed where it applies.
enum FilterPlan {
    /// A condition on an alias of the source (the base, the child grain or a joined link).
    On { at: String, ent: Arc<EffEntity>, alias: String, f: Filter },
    /// Some child row matches (for child tables that are not the query's grain).
    Exists {
        at: String,
        sql_table: String,
        child: Arc<EffEntity>,
        alias: String,
        link: Option<(String, Arc<EffEntity>, String, RowScope)>,
        cond_ent: Arc<EffEntity>,
        cond_alias: String,
        f: Filter,
    },
}

/// A grouping dimension, a pivot dimension or a plain column.
#[derive(Debug, Clone)]
pub(crate) struct Dim {
    pub name: String,
    pub label: String,
    pub res: Res,
    pub bucket: Option<Bucket>,
    /// SQL over the source's aliases.
    pub expr: String,
}

impl Dim {
    /// The type of the output values.
    pub(crate) fn ty(&self) -> Ty {
        if self.bucket.is_some() { Ty::Field(FieldType::Date) } else { self.res.ty }
    }
    pub(crate) fn column(&self, role: &str) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), json!(self.name));
        m.insert("label".into(), json!(self.label));
        self.res.describe(&mut m);
        if let Some(b) = self.bucket {
            m.insert("type".into(), json!("date"));
            m.remove("to");
            m.remove("options");
            m.insert("bucket".into(), json!(b));
        }
        m.insert("role".into(), json!(role));
        m.insert("field".into(), json!(self.res.path));
        Value::Object(m)
    }
    /// Values whose display is a title (links, users).
    pub(crate) fn titled(&self) -> Option<String> {
        if self.bucket.is_some() {
            return None;
        }
        match self.res.ty {
            Ty::User => Some(USERS.into()),
            Ty::Field(FieldType::Link) => self.res.def.as_ref().and_then(|d| d.to.clone()),
            _ => None,
        }
    }
    /// Whether ordering by value is meaningful (dates, numbers) rather than by size.
    fn ordinal(&self) -> bool {
        self.bucket.is_some() || self.res.ty.ordered()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Meas {
    pub name: String,
    pub label: String,
    pub agg: Agg,
    pub res: Option<Res>,
    pub expr: Option<String>,
}

impl Meas {
    pub(crate) fn ty(&self) -> Ty {
        match (self.agg, &self.res) {
            (Agg::Count | Agg::CountDistinct, _) => Ty::Int,
            (Agg::Avg, Some(r)) if r.ty.is_int() => Ty::Field(FieldType::Decimal),
            (_, Some(r)) => r.ty,
            (_, None) => Ty::Int,
        }
    }
    /// The aggregate over the statement's `s.vN` input, for ordering.
    pub(crate) fn agg_sql(&self, i: usize) -> String {
        match self.agg {
            Agg::Count if self.res.is_none() => "count(*)".into(),
            Agg::Count => format!("count(s.v{i})"),
            Agg::CountDistinct => format!("count(distinct s.v{i})"),
            Agg::Sum => format!("sum(s.v{i})"),
            Agg::Avg => format!("avg(s.v{i})"),
            Agg::Min => format!("min(s.v{i})"),
            Agg::Max => format!("max(s.v{i})"),
        }
    }
    /// The aggregate as output: numbers travel as exact text (no float on the way).
    pub(crate) fn out_sql(&self, i: usize) -> String {
        let numeric_in = self.res.as_ref().is_some_and(|r| r.ty.summable());
        match self.agg {
            Agg::Count | Agg::CountDistinct => self.agg_sql(i),
            Agg::Avg => format!("round(avg(s.v{i}), 9)::text"),
            Agg::Sum => format!("{}::text", self.agg_sql(i)),
            Agg::Min | Agg::Max if numeric_in => format!("{}::text", self.agg_sql(i)),
            Agg::Min | Agg::Max => self.agg_sql(i),
        }
    }
    /// The value of an empty group: zero for counts and sums, unknown otherwise.
    pub(crate) fn empty(&self) -> Value {
        match (self.agg, self.ty().is_int()) {
            (a, true) if a.additive() => json!(0),
            (a, false) if a.additive() => json!("0"),
            _ => Value::Null,
        }
    }
    pub(crate) fn column(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), json!(self.name));
        m.insert("label".into(), json!(self.label));
        m.insert("type".into(), json!(self.ty().name()));
        if let Some(p) = self.res.as_ref().and_then(|r| r.def.as_ref()).and_then(|d| d.precision) {
            m.insert("precision".into(), json!(p));
        }
        m.insert("role".into(), json!("measure"));
        m.insert("fn".into(), json!(self.agg));
        if let Some(r) = &self.res {
            m.insert("field".into(), json!(r.path));
        }
        Value::Object(m)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Plain record rows (a table report).
    Raw,
    /// Grouped and/or aggregated.
    Grouped,
}

#[derive(Debug, Clone)]
pub(crate) enum Order {
    Dim(usize, Dir),
    Meas(usize, Dir),
    /// A raw column, or a hidden ordering value `oN`.
    Raw(usize, Dir),
    Hidden(usize, Dir),
}

/// The date window a query is restricted to.
#[derive(Debug, Clone)]
pub(crate) struct Range {
    pub res: Res,
    pub expr: String,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
}

#[derive(Debug, Clone)]
pub(crate) struct TimeSeries {
    pub bucket: Bucket,
    pub fill_zero: bool,
}

pub struct Plan {
    tenant: uuid::Uuid,
    schema: Arc<Schema>,
    pub(crate) base: Arc<EffEntity>,
    scope: RowScope,
    grain: Option<Grain>,
    joins: Vec<Join>,
    filters: Vec<FilterPlan>,
    search: Option<(String, Vec<String>)>,
    /// The workspace's time zone, when an expression needs local dates of a timestamp.
    tz: Option<String>,
    pub(crate) timezone: String,
    /// The fiscal year's first month (fiscal buckets).
    pub(crate) fy_month: u32,
    pub(crate) range: Option<Range>,
    pub(crate) mode: Mode,
    /// Row dimensions; with a time series its dimension comes first.
    pub(crate) dims: Vec<Dim>,
    pub(crate) pivot: Option<Dim>,
    pub(crate) pivot_cap: u32,
    pub(crate) measures: Vec<Meas>,
    pub(crate) raw: Vec<Dim>,
    hidden: Vec<String>,
    pub(crate) order: Vec<Order>,
    pub(crate) limit: u32,
    pub(crate) time_series: Option<TimeSeries>,
    pub(crate) compare: Option<Compare>,
    /// Calculated columns: name, label and formula, in the order they are worked out.
    pub(crate) formulas: Vec<(String, String, Formula)>,
    pub(crate) subtotals: bool,
}

struct Builder<'a> {
    r: Resolver<'a>,
    grain: Option<Grain>,
    joins: Vec<Join>,
    tz: bool,
    fy: u32,
}

impl Builder<'_> {
    fn join(&mut self, key: String, from_ent: &EffEntity, from_alias: &str, link: &str) -> Result<String> {
        if let Some(j) = self.joins.iter().find(|j| j.key == key) {
            return Ok(j.alias.clone());
        }
        let f = from_ent.field(link).ok_or_else(|| Error::internal(format!("link {link} vanished")))?;
        let target = f.to.as_deref().and_then(|t| self.r.schema.entity(t)).cloned().ok_or_else(|| Error::internal("link target vanished"))?;
        let scope = readable(self.r.ctx, &target, &key)?;
        let on = field_expr(from_ent, link, from_alias).ok_or_else(|| Error::internal(format!("link {link} has no column")))?;
        let alias = format!("l{}", self.joins.len() + 1);
        self.joins.push(Join { key, alias: alias.clone(), target, on, scope });
        Ok(alias)
    }

    /// SQL for a resolved field within the source (adds the joins it needs).
    fn expr(&mut self, r: &Res, at: &str) -> Result<String> {
        let (ent, alias) = match &r.loc {
            Loc::Base => (self.r.base.clone(), "a".to_string()),
            Loc::Link(l) => {
                let base = self.r.base.clone();
                let alias = self.join(l.clone(), &base, "a", l)?;
                (self.r.entity_of(&r.loc)?, alias)
            }
            Loc::Child(t) => {
                self.require_grain(t, at)?;
                (self.r.entity_of(&r.loc)?, "c".to_string())
            }
            Loc::ChildLink(t, l) => {
                self.require_grain(t, at)?;
                let child = self.r.entity_of(&Loc::Child(t.clone()))?;
                let alias = self.join(format!("{t}.{l}"), &child, "c", l)?;
                (self.r.entity_of(&r.loc)?, alias)
            }
        };
        field_expr(&ent, &r.field, &alias).ok_or_else(|| bad(at, format!("'{}' cannot be queried", r.path)))
    }

    fn require_grain(&self, table: &str, at: &str) -> Result<()> {
        match &self.grain {
            Some(g) if g.table == table => Ok(()),
            _ => Err(bad(at, format!("'{table}' rows are not part of this query's grouping"))),
        }
    }

    fn dim(&mut self, path: &str, bucket: Option<Bucket>, label: Option<&str>, at: &str) -> Result<Dim> {
        let res = self.r.resolve(path, &format!("{at}.field"))?;
        let e = self.expr(&res, at)?;
        let expr = match bucket {
            None if res.ty.is_datetime() => {
                return Err(bad(at, format!("'{path}' is a date-time: group it by a bucket (day, week, month, quarter, year, fiscal_quarter or fiscal_year)")));
            }
            None if !res.ty.groupable() => return Err(bad(at, format!("{} fields such as '{path}' cannot be grouped", res.ty.name()))),
            None => e,
            Some(b) if res.ty.is_date() => b.sql(&format!("({e})::timestamp"), self.fy),
            Some(b) if res.ty.is_datetime() => {
                self.tz = true;
                b.sql(&format!("(({e}) at time zone z.tz)"), self.fy)
            }
            Some(_) => return Err(bad(at, format!("buckets apply to date and date-time fields; '{path}' is {}", res.ty.name()))),
        };
        let name = match bucket {
            Some(b) => format!("{}({path})", b.name()),
            None => path.to_string(),
        };
        let label = match (label, bucket) {
            (Some(l), _) if !l.trim().is_empty() => l.trim().to_string(),
            (_, Some(b)) => format!("{} ({})", res.label, b.label()),
            _ => res.label.clone(),
        };
        Ok(Dim { name, label, res, bucket, expr })
    }

    fn raw(&mut self, path: &str, label: Option<&str>, at: &str) -> Result<Dim> {
        let res = self.r.resolve(path, &format!("{at}.field"))?;
        let expr = self.expr(&res, at)?;
        let label = label.map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).unwrap_or_else(|| res.label.clone());
        Ok(Dim { name: path.to_string(), label, res, bucket: None, expr })
    }

    fn measure(&mut self, m: &crate::spec::Measure, at: &str) -> Result<Meas> {
        let Some(path) = m.field.as_deref() else {
            if m.func != Agg::Count {
                return Err(bad(at, format!("{} needs a field", m.func.name())));
            }
            let label = m.label.as_deref().map(str::trim).filter(|l| !l.is_empty()).unwrap_or("Count").to_string();
            return Ok(Meas { name: "count".into(), label, agg: Agg::Count, res: None, expr: None });
        };
        let res = self.r.resolve(path, &format!("{at}.field"))?;
        let fits = match m.func {
            Agg::Sum | Agg::Avg => res.ty.summable(),
            Agg::Min | Agg::Max => res.ty.ordered(),
            Agg::Count | Agg::CountDistinct => res.ty != Ty::Field(FieldType::Json),
        };
        if !fits {
            return Err(bad(at, format!("{} cannot be applied to '{path}' ({}); allowed: {}", m.func.name(), res.ty.name(), res.ty.aggregates().join(", "))));
        }
        // Grouping by child rows repeats the parent's values once per row: totals of parent fields
        // would be inflated, so only duplicate-proof aggregates may use them.
        if let Some(g) = &self.grain
            && res.loc.table() != Some(g.table.as_str())
            && matches!(m.func, Agg::Sum | Agg::Avg | Agg::Count)
        {
            return Err(bad(
                at,
                format!(
                    "{}({path}) would count each {} once per {} row; measure a field of {}, or use count_distinct, min or max",
                    m.func.name(),
                    self.r.base.def.label,
                    g.table,
                    g.table
                ),
            ));
        }
        let expr = self.expr(&res, at)?;
        let label = match m.label.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            Some(l) => l.to_string(),
            None if m.func == Agg::Count => format!("Count of {}", res.label),
            None => format!("{} {}", m.func.label(), res.label),
        };
        Ok(Meas { name: format!("{}({path})", m.func.name()), label, agg: m.func, res: Some(res), expr: Some(expr) })
    }

    fn filter(&mut self, f: Filter, i: usize) -> Result<FilterPlan> {
        let at = format!("filters[{i}]");
        if f.field == "_tag" {
            return Ok(FilterPlan::On { at, ent: self.r.base.clone(), alias: "a".into(), f });
        }
        let res = self.r.resolve(&f.field, &at)?;
        let leaf = Filter { field: res.field.clone(), op: f.op, value: f.value };
        let ent = self.r.entity_of(&res.loc)?;
        let grain_table = self.grain.as_ref().map(|g| g.table.clone());
        Ok(match &res.loc {
            Loc::Base => FilterPlan::On { at, ent, alias: "a".into(), f: leaf },
            Loc::Link(l) => {
                let base = self.r.base.clone();
                let alias = self.join(l.clone(), &base, "a", l)?;
                FilterPlan::On { at, ent, alias, f: leaf }
            }
            Loc::Child(t) if grain_table.as_deref() == Some(t) => FilterPlan::On { at, ent, alias: "c".into(), f: leaf },
            Loc::ChildLink(t, l) if grain_table.as_deref() == Some(t) => {
                let child = self.r.entity_of(&Loc::Child(t.clone()))?;
                let alias = self.join(format!("{t}.{l}"), &child, "c", l)?;
                FilterPlan::On { at, ent, alias, f: leaf }
            }
            Loc::Child(t) => {
                let child = self.r.entity_of(&res.loc)?;
                let sql_table = self.r.base.children.get(t).map(|c| c.table.clone()).ok_or_else(|| Error::internal("child table vanished"))?;
                let alias = format!("x{i}");
                FilterPlan::Exists { at, sql_table, child, alias: alias.clone(), link: None, cond_ent: ent, cond_alias: alias, f: leaf }
            }
            Loc::ChildLink(t, l) => {
                let child = self.r.entity_of(&Loc::Child(t.clone()))?;
                let sql_table = self.r.base.children.get(t).map(|c| c.table.clone()).ok_or_else(|| Error::internal("child table vanished"))?;
                let scope = readable(self.r.ctx, &ent, &at)?;
                let alias = format!("x{i}");
                let link_alias = format!("y{i}");
                FilterPlan::Exists {
                    at,
                    sql_table,
                    child,
                    alias,
                    link: Some((l.clone(), ent.clone(), link_alias.clone(), scope)),
                    cond_ent: ent,
                    cond_alias: link_alias,
                    f: leaf,
                }
            }
        })
    }
}

/// Compiles a spec for `ctx`: every check that can fail does so here, before any SQL runs.
pub fn compile(ctx: &Ctx, spec: &Spec) -> Result<Plan> {
    let r = Resolver::new(ctx, &spec.entity, "entity")?;
    let scope = readable(ctx, &r.base, "entity")?;
    let limit = spec.limit()?;
    let mode =
        if spec.group_by.is_empty() && spec.measures.is_empty() && spec.pivot.is_none() && spec.time_series.is_none() { Mode::Raw } else { Mode::Grouped };
    if mode == Mode::Grouped && spec.measures.is_empty() {
        return Err(bad("measures", "add at least one measure, e.g. {\"fn\": \"count\"}"));
    }
    if mode == Mode::Grouped && !spec.fields.is_empty() {
        return Err(bad("fields", "plain fields cannot be combined with group_by or measures; group by them instead"));
    }
    if spec.measures.len() > 20 || spec.group_by.len() > 6 || spec.fields.len() > 100 || spec.sort.len() > 10 {
        return Err(bad("spec", "at most 20 measures, 6 group-by fields, 100 fields and 10 sorts"));
    }
    if spec.time_series.is_some() && spec.date_range.is_some() {
        return Err(bad("date_range", "use either date_range or time_series (which carries its own from/to)"));
    }
    if spec.compare.is_some() && spec.pivot.is_some() {
        return Err(bad("compare", "compare cannot be combined with pivot"));
    }
    if spec.compare.is_some() && mode == Mode::Raw {
        return Err(bad("compare", "compare needs measures"));
    }

    // The grain: grouping or measuring child-table fields turns rows into child rows. Two child
    // tables at once would multiply each other's rows, so that is refused.
    let mut grain: Option<(String, String)> = None;
    {
        let mut paths: Vec<(String, String)> = Vec::new();
        for (i, g) in spec.group_by.iter().enumerate() {
            paths.push((g.field.clone(), format!("group_by[{i}].field")));
        }
        for (i, m) in spec.measures.iter().enumerate() {
            if let Some(f) = &m.field {
                paths.push((f.clone(), format!("measures[{i}].field")));
            }
        }
        if mode == Mode::Raw {
            for (i, f) in spec.fields.iter().enumerate() {
                paths.push((f.field.clone(), format!("fields[{i}].field")));
            }
        }
        if let Some(p) = &spec.pivot {
            paths.push((p.field.clone(), "pivot.field".into()));
        }
        if let Some(t) = &spec.time_series {
            paths.push((t.field.clone(), "time_series.field".into()));
        }
        if let Some(d) = &spec.date_range {
            paths.push((d.field.clone(), "date_range.field".into()));
        }
        for (path, at) in paths {
            let res = r.resolve(&path, &at)?;
            if let Some(t) = res.loc.table() {
                match &grain {
                    Some((g, _)) if g != t => {
                        return Err(bad(&at, format!("this query already uses the child table '{g}'; one child table can be grouped or measured at a time")));
                    }
                    _ => grain = Some((t.to_string(), at)),
                }
            }
        }
    }
    let grain = match grain {
        Some((t, _)) => {
            let sql_table = r.base.children.get(&t).map(|c| c.table.clone()).ok_or_else(|| Error::internal("child table vanished"))?;
            Some(Grain { table: t, sql_table })
        }
        None => None,
    };
    let fy = ctx.fy_start_month.clamp(1, 12);
    let mut b = Builder { r, grain, joins: vec![], tz: false, fy };
    let today = ctx.today();
    let date = |v: &Option<String>, at: &str| -> Result<Option<NaiveDate>> {
        v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| resolve_date(s, today, fy).map_err(|m| bad(at, m))).transpose()
    };

    // Dimensions: the time series first.
    let mut dims = Vec::new();
    let mut range = None;
    let mut time_series = None;
    if let Some(ts) = &spec.time_series {
        let d = b.dim(&ts.field, Some(ts.bucket), None, "time_series")?;
        let (from, to) = (date(&ts.from, "time_series.from")?, date(&ts.to, "time_series.to")?);
        let expr = b.expr(&d.res, "time_series")?;
        range = Some(Range { res: d.res.clone(), expr, from, to });
        time_series = Some(TimeSeries { bucket: ts.bucket, fill_zero: ts.fill_zero });
        dims.push(d);
    }
    if let Some(dr) = &spec.date_range {
        let res = b.r.resolve(&dr.field, "date_range.field")?;
        if !(res.ty.is_date() || res.ty.is_datetime()) {
            return Err(bad("date_range.field", format!("'{}' is not a date or date-time field", dr.field)));
        }
        let expr = b.expr(&res, "date_range")?;
        range = Some(Range { res, expr, from: date(&dr.from, "date_range.from")?, to: date(&dr.to, "date_range.to")? });
    }
    if let Some(rg) = &range {
        if let (Some(f), Some(t)) = (rg.from, rg.to)
            && f > t
        {
            return Err(bad("date_range", format!("from ({f}) is after to ({t})")));
        }
        if rg.res.ty.is_datetime() {
            b.tz = true;
        }
    }
    if spec.compare.is_some() && !range.as_ref().is_some_and(|r| r.from.is_some() && r.to.is_some()) {
        return Err(bad("compare", "comparing with a previous period needs a date_range (or time_series) with both from and to"));
    }
    for (i, g) in spec.group_by.iter().enumerate() {
        let d = b.dim(&g.field, g.bucket, g.label.as_deref(), &format!("group_by[{i}]"))?;
        if dims.iter().any(|x: &Dim| x.name == d.name) {
            return Err(bad(&format!("group_by[{i}]"), format!("'{}' is already grouped", d.name)));
        }
        dims.push(d);
    }
    let pivot = match &spec.pivot {
        Some(p) => {
            let d = b.dim(&p.field, p.bucket, None, "pivot")?;
            if dims.iter().any(|x| x.name == d.name) {
                return Err(bad("pivot.field", format!("'{}' is already a row dimension", d.name)));
            }
            Some(d)
        }
        None => None,
    };
    let pivot_cap = match spec.pivot.as_ref().and_then(|p| p.max_columns) {
        None => crate::spec::DEFAULT_PIVOT_COLUMNS,
        Some(n) if (1..=MAX_PIVOT_COLUMNS).contains(&n) => n,
        Some(_) => return Err(bad("pivot.max_columns", format!("must be between 1 and {MAX_PIVOT_COLUMNS}"))),
    };
    if pivot.is_some() && (limit as u64) * (pivot_cap as u64) * (spec.measures.len().max(1) as u64) > MAX_PIVOT_CELLS {
        return Err(bad("pivot", format!("a pivot holds at most {MAX_PIVOT_CELLS} cells (rows × columns × measures): lower limit or max_columns")));
    }
    let mut measures: Vec<Meas> = Vec::new();
    for (i, m) in spec.measures.iter().enumerate() {
        let at = format!("measures[{i}]");
        let mm = b.measure(m, &at)?;
        if measures.iter().any(|x| x.name == mm.name) {
            return Err(bad(&at, format!("'{}' is measured twice", mm.name)));
        }
        if dims.iter().any(|d| d.label == mm.label) || measures.iter().any(|x| x.label == mm.label) {
            return Err(bad(&format!("{at}.label"), format!("the label '{}' is already used", mm.label)));
        }
        measures.push(mm);
    }
    let mut raw = Vec::new();
    if mode == Mode::Raw {
        if spec.fields.is_empty() {
            // The entity's own list columns, as its list view shows them.
            let mut cols: Vec<String> = vec!["number".into()];
            let listed = b.r.base.def.list.columns.iter().cloned().chain(b.r.base.def.title_field.clone().filter(|_| b.r.base.def.list.columns.is_empty()));
            for c in listed {
                if !cols.contains(&c) {
                    cols.push(c);
                }
            }
            for c in cols {
                if let Ok(d) = b.raw(&c, None, "fields") {
                    raw.push(d);
                }
            }
        }
        for (i, f) in spec.fields.iter().enumerate() {
            let d = b.raw(&f.field, f.label.as_deref(), &format!("fields[{i}]"))?;
            if raw.iter().any(|x: &Dim| x.name == d.name) {
                return Err(bad(&format!("fields[{i}]"), format!("'{}' is listed twice", d.name)));
            }
            raw.push(d);
        }
    }

    // Sorting: a time series is always chronological first.
    let mut order = Vec::new();
    let mut hidden = Vec::new();
    if time_series.is_some() {
        order.push(Order::Dim(0, Dir::Asc));
    }
    for (i, s) in spec.sort.iter().enumerate() {
        let at = format!("sort[{i}].by");
        let by = s.by.trim();
        let o = match mode {
            Mode::Grouped => {
                let dim =
                    dims.iter().position(|d| d.name == by || d.label == by || (d.res.path == by && dims.iter().filter(|x| x.res.path == by).count() == 1));
                let meas = measures.iter().position(|m| m.name == by || m.label == by);
                match (dim, meas) {
                    (Some(d), _) => Order::Dim(d, s.dir),
                    (None, Some(m)) => Order::Meas(m, s.dir),
                    (None, None) => {
                        let names: Vec<&str> = dims.iter().map(|d| d.name.as_str()).chain(measures.iter().map(|m| m.name.as_str())).collect();
                        return Err(bad(&at, format!("'{by}' is not a column of this query; use one of: {}", names.join(", "))));
                    }
                }
            }
            Mode::Raw => match raw.iter().position(|d| d.name == by || d.label == by) {
                Some(k) => Order::Raw(k, s.dir),
                None => {
                    let res = b.r.resolve(by, &at)?;
                    if res.loc.table().is_some() && b.grain.is_none() {
                        return Err(bad(&at, "sorting by a child-table field needs that table among the fields"));
                    }
                    if !res.ty.groupable() {
                        return Err(bad(&at, format!("cannot sort by {} fields", res.ty.name())));
                    }
                    hidden.push(b.expr(&res, &at)?);
                    Order::Hidden(hidden.len() - 1, s.dir)
                }
            },
        };
        order.push(o);
    }
    if spec.sort.is_empty() {
        match mode {
            Mode::Grouped => {
                let dated: Vec<usize> =
                    dims.iter().enumerate().filter(|(i, d)| d.bucket.is_some() && !(time_series.is_some() && *i == 0)).map(|(i, _)| i).collect();
                if !dated.is_empty() {
                    order.extend(dated.into_iter().map(|i| Order::Dim(i, Dir::Asc)));
                } else if time_series.is_none() && !dims.is_empty() {
                    order.push(Order::Meas(0, Dir::Desc));
                }
            }
            Mode::Raw => {
                let (field, desc) = ferrum_kernel::query::parse_sort(&b.r.base, None).unwrap_or(("created_at".into(), true));
                if let Ok(res) = b.r.resolve(&field, "sort") {
                    hidden.push(b.expr(&res, "sort")?);
                    order.push(Order::Hidden(hidden.len() - 1, if desc { Dir::Desc } else { Dir::Asc }));
                }
            }
        }
    }

    let mut filters = Vec::new();
    let parsed = parse_filters(&spec.filters).map_err(|e| bad("filters", e.to_string()))?;
    for (i, f) in parsed.into_iter().enumerate() {
        filters.push(b.filter(f, i)?);
    }
    let search = match spec.search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(s) if s.chars().count() > 100 => return Err(bad("search", "at most 100 characters")),
        Some(s) => {
            // Only fields the caller can read are searched: matching would reveal hidden values.
            let base = b.r.base.clone();
            let mut cols = vec!["a.number::text".to_string()];
            for name in base.def.title_field.iter().chain(base.def.search_fields.iter()) {
                if let Some(f) = base.field(name)
                    && ctx.perms.can_read_field(&base.def.name, f)
                    && let Some(e) = field_expr(&base, name, "a")
                {
                    cols.push(format!("{e}::text"));
                }
            }
            cols.dedup();
            Some((format!("%{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")), cols))
        }
    };

    let formulas = formulas(spec, mode, &dims, &raw, &measures, pivot.is_some())?;
    if spec.subtotals && (mode != Mode::Grouped || dims.len() < 2 || pivot.is_some() || spec.compare.is_some()) {
        return Err(bad("subtotals", "subtotals need two or more groupings (a time series counts), without a pivot or a comparison"));
    }

    let Builder { r, grain, joins, tz, .. } = b;
    let plan = Plan {
        tenant: ctx.tenant,
        schema: ctx.schema.clone(),
        base: r.base.clone(),
        scope,
        grain,
        joins,
        filters,
        search,
        tz: tz.then(|| ctx.schema.timezone.clone()),
        timezone: ctx.schema.timezone.clone(),
        fy_month: fy,
        range,
        mode,
        dims,
        pivot,
        pivot_cap,
        measures,
        raw,
        hidden,
        order,
        limit,
        time_series,
        compare: spec.compare,
        formulas,
        subtotals: spec.subtotals,
    };
    // Filter values are typed by their fields; render once so bad values fail now.
    let mut probe = QueryBuilder::new("");
    plan.push_source(&mut probe, None)?;
    Ok(plan)
}

/// Compiles the calculated columns: references are the result's columns (and earlier formulas)
/// by name or label.
fn formulas(spec: &Spec, mode: Mode, dims: &[Dim], raw: &[Dim], measures: &[Meas], pivot: bool) -> Result<Vec<(String, String, Formula)>> {
    if spec.formulas.is_empty() {
        return Ok(vec![]);
    }
    if spec.formulas.len() > MAX_FORMULAS {
        return Err(bad("formulas", format!("at most {MAX_FORMULAS} calculated columns")));
    }
    if pivot {
        return Err(bad("formulas", "calculated columns cannot be combined with a pivot"));
    }
    let shown = if mode == Mode::Raw { raw } else { dims };
    let mut known: Vec<(String, String)> = shown.iter().map(|d| (d.name.clone(), d.label.clone())).collect();
    known.extend(measures.iter().map(|m| (m.name.clone(), m.label.clone())));
    if spec.compare.is_some() {
        known.extend(measures.iter().map(|m| (format!("previous({})", m.name), format!("{} (previous)", m.label))));
    }
    let mut out = Vec::new();
    for (i, f) in spec.formulas.iter().enumerate() {
        let label = f.label.trim().to_string();
        if label.is_empty() || label.chars().count() > 100 {
            return Err(bad("formulas", "each calculated column needs a label of at most 100 characters"));
        }
        let lookup = |r: &str| {
            known
                .iter()
                .find(|(n, l)| n.eq_ignore_ascii_case(r) || l.eq_ignore_ascii_case(r))
                .map(|(n, _)| n.clone())
                .ok_or_else(|| format!("no column [{r}]: refer to a column by its name or label"))
        };
        let parsed = Formula::parse(&f.expr, &lookup).map_err(|m| bad("formulas", format!("{label}: {m}")))?;
        let name = format!("calc{}", i + 1);
        known.push((name.clone(), label.clone()));
        out.push((name, label, parsed));
    }
    Ok(out)
}

impl Plan {
    pub(crate) fn grain(&self) -> Option<&str> {
        self.grain.as_ref().map(|g| g.table.as_str())
    }

    fn source_columns(&self) -> String {
        let mut cols = vec!["a.id as _id".to_string()];
        for (i, d) in self.dims.iter().enumerate() {
            cols.push(format!("{} as d{i}", d.expr));
        }
        if let Some(p) = &self.pivot {
            cols.push(format!("{} as p", p.expr));
        }
        for (i, m) in self.measures.iter().enumerate() {
            if let Some(e) = &m.expr {
                cols.push(format!("{e} as v{i}"));
            }
        }
        for (i, d) in self.raw.iter().enumerate() {
            cols.push(format!("{} as r{i}", d.expr));
        }
        for (i, h) in self.hidden.iter().enumerate() {
            cols.push(format!("{h} as o{i}"));
        }
        if self.grain.is_some() {
            cols.push("c.idx as _idx".into());
        }
        cols.join(", ")
    }

    /// `(select <per-row values> from ... where ...) s`; `range` replaces the spec's dates (for
    /// the comparison period).
    pub(crate) fn push_source(&self, qb: &mut QueryBuilder<Postgres>, range: Option<(NaiveDate, NaiveDate)>) -> Result<()> {
        qb.push("(select ").push(self.source_columns()).push(" from ");
        if let Some(tz) = &self.tz {
            qb.push("(select ").push_bind(tz.clone()).push("::text as tz) z cross join ");
        }
        qb.push(format!("{} a", q(&self.base.table)));
        if let Some(g) = &self.grain {
            qb.push(format!(" join {} c on c.tenant_id = a.tenant_id and c.parent_id = a.id", q(&g.sql_table)));
        }
        for j in &self.joins {
            qb.push(format!(
                " left join {t} {a} on {a}.tenant_id = a.tenant_id and {a}.id = {on}{pred}",
                t = q(&j.target.table),
                a = j.alias,
                on = j.on,
                pred = entity_predicate(&j.target, &j.alias)
            ));
            j.scope.push_sql(qb, &j.alias);
        }
        qb.push(" where a.tenant_id = ").push_bind(self.tenant);
        qb.push(entity_predicate(&self.base, "a"));
        self.scope.push_sql(qb, "a");
        for f in &self.filters {
            // The kernel reports bad filter values as `invalid`; say which filter it was.
            let with_at = |at: &str, e: Error| match e {
                Error::Invalid(m) => bad(at, m),
                other => other,
            };
            match f {
                FilterPlan::On { at, ent, alias, f } => push_filter(qb, &self.schema, ent, alias, f).map_err(|e| with_at(at, e))?,
                FilterPlan::Exists { at, sql_table, child, alias, link, cond_ent, cond_alias, f } => {
                    qb.push(format!(" and exists (select 1 from {} {alias}", q(sql_table)));
                    if let Some((l, target, la, scope)) = link {
                        let on = field_expr(child, l, alias).ok_or_else(|| Error::internal("link column vanished"))?;
                        qb.push(format!(
                            " left join {} {la} on {la}.tenant_id = {alias}.tenant_id and {la}.id = {on}{}",
                            q(&target.table),
                            entity_predicate(target, la)
                        ));
                        scope.push_sql(qb, la);
                    }
                    qb.push(format!(" where {alias}.tenant_id = a.tenant_id and {alias}.parent_id = a.id"));
                    push_filter(qb, &self.schema, cond_ent, cond_alias, f).map_err(|e| with_at(at, e))?;
                    qb.push(")");
                }
            }
        }
        if let Some((pattern, cols)) = &self.search {
            qb.push(" and (");
            for (i, c) in cols.iter().enumerate() {
                if i > 0 {
                    qb.push(" or ");
                }
                qb.push(format!("{c} ilike ")).push_bind(pattern.clone());
            }
            qb.push(")");
        }
        if let Some(rg) = &self.range {
            let (from, to) = match range {
                Some((f, t)) => (Some(f), Some(t)),
                None => (rg.from, rg.to),
            };
            let local = rg.res.ty.is_datetime();
            if let Some(f) = from {
                qb.push(format!(" and {} >= ", rg.expr)).push_bind(f);
                qb.push(if local { "::date::timestamp at time zone z.tz" } else { "::date" });
            }
            if let Some(t) = to {
                if local {
                    qb.push(format!(" and {} < (", rg.expr)).push_bind(t).push("::date + 1)::timestamp at time zone z.tz");
                } else {
                    qb.push(format!(" and {} <= ", rg.expr)).push_bind(t).push("::date");
                }
            }
        }
        qb.push(") s");
        Ok(())
    }

    /// Output of the source column `col` (decimals as exact text).
    fn dim_out(d: &Dim, col: &str) -> String {
        format!("s.{col}{} as {col}", if d.ty().is_decimal() { "::text" } else { "" })
    }

    /// The requested order, then every dimension (or the record id) so equal rows never swap.
    fn order_sql(&self) -> String {
        let dir = |d: Dir| if d == Dir::Desc { "desc nulls last" } else { "asc nulls last" };
        let mut parts: Vec<String> = Vec::new();
        for o in &self.order {
            match o {
                Order::Dim(i, d) => parts.push(format!("s.d{i} {}", dir(*d))),
                Order::Meas(i, d) => parts.push(format!("{} {}", self.measures[*i].agg_sql(*i), dir(*d))),
                Order::Raw(i, d) => parts.push(format!("s.r{i} {}", dir(*d))),
                Order::Hidden(i, d) => parts.push(format!("s.o{i} {}", dir(*d))),
            }
        }
        match self.mode {
            Mode::Grouped => parts.extend((0..self.dims.len()).map(|i| format!("s.d{i} asc nulls last"))),
            Mode::Raw => {
                parts.push("s._id asc".into());
                if self.grain.is_some() {
                    parts.push("s._idx asc".into());
                }
            }
        }
        parts.join(", ")
    }

    /// Text key of a row's dimensions (`dims` indexes), matched exactly across statements.
    fn key_sql(dims: &[usize]) -> String {
        format!("jsonb_build_array({})", dims.iter().map(|i| format!("s.d{i}::text")).collect::<Vec<_>>().join(", "))
    }

    fn measures_out(&self) -> Vec<String> {
        self.measures.iter().enumerate().map(|(i, m)| format!("{} as m{i}", m.out_sql(i))).collect()
    }

    /// Grouped rows. `keys`: only rows whose key over `key_dims` is in this JSON array of text
    /// arrays (comparison periods, pivot cells).
    pub(crate) fn grouped(
        &self,
        range: Option<(NaiveDate, NaiveDate)>,
        key_dims: &[usize],
        keys: Option<&Value>,
        limit: u32,
    ) -> Result<QueryBuilder<Postgres>> {
        let mut qb = QueryBuilder::new("select to_jsonb(x) from (select ");
        let mut cols: Vec<String> = self.dims.iter().enumerate().map(|(i, d)| Self::dim_out(d, &format!("d{i}"))).collect();
        if !key_dims.is_empty() {
            cols.push(format!("{} as _k", Self::key_sql(key_dims)));
        }
        cols.extend(self.measures_out());
        qb.push(cols.join(", ")).push(" from ");
        self.push_source(&mut qb, range)?;
        if let Some(k) = keys {
            qb.push(format!(" where {} in (select value from jsonb_array_elements(", Self::key_sql(key_dims))).push_bind(k.clone()).push("::jsonb))");
        }
        if !self.dims.is_empty() {
            qb.push(format!(" group by {}", (0..self.dims.len()).map(|i| format!("s.d{i}")).collect::<Vec<_>>().join(", ")));
            let order = self.order_sql();
            if !order.is_empty() {
                qb.push(format!(" order by {order}"));
            }
        }
        qb.push(" limit ").push_bind(limit as i64 + 1).push(") x");
        Ok(qb)
    }

    /// The measures per value of the first dimension, for the first-dimension keys listed.
    pub(crate) fn subtotals(&self, keys: &Value) -> Result<QueryBuilder<Postgres>> {
        let first = self.dims.first().ok_or_else(|| Error::internal("subtotals without a dimension"))?;
        let mut qb = QueryBuilder::new("select to_jsonb(x) from (select ");
        let mut cols = vec![Self::dim_out(first, "d0"), format!("{} as _k", Self::key_sql(&[0]))];
        cols.extend(self.measures_out());
        qb.push(cols.join(", ")).push(" from ");
        self.push_source(&mut qb, None)?;
        qb.push(format!(" where {} in (select value from jsonb_array_elements(", Self::key_sql(&[0])))
            .push_bind(keys.clone())
            .push("::jsonb)) group by s.d0) x");
        Ok(qb)
    }

    /// The measures over every matching row.
    pub(crate) fn totals(&self, range: Option<(NaiveDate, NaiveDate)>) -> Result<QueryBuilder<Postgres>> {
        let mut qb = QueryBuilder::new("select to_jsonb(x) from (select ");
        qb.push(self.measures_out().join(", ")).push(" from ");
        self.push_source(&mut qb, range)?;
        qb.push(") x");
        Ok(qb)
    }

    /// Pivot columns with their totals, in display order, one more than the cap.
    pub(crate) fn pivot_columns(&self) -> Result<QueryBuilder<Postgres>> {
        let p = self.pivot.as_ref().ok_or_else(|| Error::internal("not a pivot"))?;
        let mut qb = QueryBuilder::new("select to_jsonb(x) from (select ");
        let mut cols = vec![Self::dim_out(p, "p"), "s.p::text as _p".to_string()];
        cols.extend(self.measures_out());
        qb.push(cols.join(", ")).push(" from ");
        self.push_source(&mut qb, None)?;
        // Dates and numbers read in order; categories biggest first, so the cap keeps the top ones.
        let order = if p.ordinal() { "s.p asc nulls last".to_string() } else { format!("{} desc nulls last, s.p asc nulls last", self.measures[0].agg_sql(0)) };
        qb.push(format!(" group by s.p order by {order} limit ")).push_bind(self.pivot_cap as i64 + 1).push(") x");
        Ok(qb)
    }

    /// Pivot cells for the given row keys and pivot values (`_p` texts; `null_col` adds empties).
    pub(crate) fn pivot_cells(&self, row_keys: &Value, cols: &[String], null_col: bool) -> Result<QueryBuilder<Postgres>> {
        let all: Vec<usize> = (0..self.dims.len()).collect();
        let mut qb = QueryBuilder::new("select to_jsonb(x) from (select ");
        let mut out = vec![format!("{} as _k", Self::key_sql(&all)), "s.p::text as _p".to_string()];
        out.extend(self.measures_out());
        qb.push(out.join(", ")).push(" from ");
        self.push_source(&mut qb, None)?;
        qb.push(format!(" where {} in (select value from jsonb_array_elements(", Self::key_sql(&all))).push_bind(row_keys.clone()).push("::jsonb))");
        qb.push(" and (s.p::text = any(").push_bind(cols.to_vec()).push(")");
        if null_col {
            qb.push(" or s.p is null");
        }
        qb.push(")");
        let mut group: Vec<String> = all.iter().map(|i| format!("s.d{i}")).collect();
        group.push("s.p".into());
        qb.push(format!(" group by {}) x", group.join(", ")));
        Ok(qb)
    }

    /// Plain rows.
    pub(crate) fn rows(&self) -> Result<QueryBuilder<Postgres>> {
        let mut qb = QueryBuilder::new("select to_jsonb(x) from (select s._id");
        for (i, d) in self.raw.iter().enumerate() {
            qb.push(", ").push(Self::dim_out(d, &format!("r{i}")));
        }
        qb.push(" from ");
        self.push_source(&mut qb, None)?;
        qb.push(format!(" order by {} limit ", self.order_sql())).push_bind(self.limit as i64 + 1).push(") x");
        Ok(qb)
    }

    /// Sums of the plain number columns over every matching row (percentages do not add up).
    pub(crate) fn raw_totals(&self) -> Result<Option<QueryBuilder<Postgres>>> {
        let summed: Vec<String> = self
            .raw
            .iter()
            .enumerate()
            .filter(|(_, d)| matches!(d.res.ty, Ty::Field(FieldType::Int | FieldType::Decimal | FieldType::Currency)))
            .map(|(i, _)| format!("sum(s.r{i})::text as r{i}"))
            .collect();
        if summed.is_empty() {
            return Ok(None);
        }
        let mut qb = QueryBuilder::new("select to_jsonb(x) from (select ");
        qb.push(summed.join(", ")).push(" from ");
        self.push_source(&mut qb, None)?;
        qb.push(") x");
        Ok(Some(qb))
    }
}

/// Groupable and aggregatable fields of an entity for the builder UI: the entity's own fields,
/// one hop through links, child-table fields and their links' fields — only what `ctx` may read.
pub fn fields(ctx: &Ctx, entity: &str) -> Result<Value> {
    let r = Resolver::new(ctx, entity, "entity")?;
    let base = r.base.clone();
    let mut out: Vec<Value> = Vec::new();
    let mut push = |res: Res, group: String| {
        if res.ty == Ty::Field(FieldType::Json) && res.loc != Loc::Base {
            return;
        }
        let mut m = Map::new();
        m.insert("path".into(), json!(res.path));
        m.insert("label".into(), json!(res.label));
        res.describe(&mut m);
        m.insert("group".into(), json!(group));
        if let Some(t) = res.loc.table() {
            m.insert("child_table".into(), json!(t));
        }
        m.insert("groupable".into(), json!(res.ty.groupable()));
        m.insert("aggregates".into(), json!(res.ty.aggregates()));
        m.insert("buckets".into(), if res.ty.buckets() { json!(Bucket::ALL) } else { json!([]) });
        m.insert("bucket_required".into(), json!(res.ty.is_datetime()));
        out.push(Value::Object(m));
    };
    let own = |ent: &EffEntity| -> Vec<String> {
        let mut names: Vec<String> = ent.def.fields.iter().filter(|f| f.ftype.is_stored()).map(|f| f.name.clone()).collect();
        if ent.def.kind != EntityKind::Child {
            names.extend(["number", "docstatus", "workflow_state", "created_at", "created_by", "updated_at", "updated_by"].map(String::from));
        }
        names
    };
    for name in own(&base) {
        if let Ok(res) = r.resolve(&name, "") {
            push(res, base.def.label.clone());
        }
    }
    for lf in base.def.fields.iter().filter(|f| f.ftype == FieldType::Link) {
        let Some(target) = lf.to.as_deref().and_then(|t| ctx.schema.entity(t)) else { continue };
        if !ctx.perms.can_read_field(&base.def.name, lf) || readable(ctx, target, "").is_err() {
            continue;
        }
        for name in own(target).into_iter().filter(|n| !matches!(n.as_str(), "created_by" | "updated_by" | "workflow_state")) {
            if let Ok(res) = r.resolve(&format!("{}.{name}", lf.name), "") {
                push(res, format!("{} ({})", target.def.label, lf.label()));
            }
        }
    }
    for tf in base.def.fields.iter().filter(|f| f.ftype == FieldType::Table) {
        let Ok(child) = r.child(&base, &tf.name, "") else { continue };
        for name in own(&child) {
            if let Ok(res) = r.resolve(&format!("{}.{name}", tf.name), "") {
                push(res, tf.label());
            }
        }
        for lf in child.def.fields.iter().filter(|f| f.ftype == FieldType::Link) {
            let Some(target) = lf.to.as_deref().and_then(|t| ctx.schema.entity(t)) else { continue };
            if !ctx.perms.can_read_field(&child.def.name, lf) || readable(ctx, target, "").is_err() {
                continue;
            }
            for name in own(target).into_iter().filter(|n| !matches!(n.as_str(), "created_by" | "updated_by" | "workflow_state")) {
                if let Ok(res) = r.resolve(&format!("{}.{}.{name}", tf.name, lf.name), "") {
                    push(res, format!("{} › {} ({})", tf.label(), target.def.label, lf.label()));
                }
            }
        }
    }
    Ok(json!({
        "entity": base.def.name,
        "label": base.def.label,
        "fields": out,
        "filters": {"tag": "_tag", "operators": ["=", "!=", ">", ">=", "<", "<=", "in", "not in", "like", "not like", "between", "is"]},
        "functions": ["count", "count_distinct", "sum", "avg", "min", "max"],
        "buckets": Bucket::ALL,
        "compare": ["previous_period", "previous_year"],
        "dates": "YYYY-MM-DD or today, start_of_week|month|quarter|year|fiscal_year, end_of_..., with offsets such as -30d, +1w, -11m, -1q, -1y",
        "formulas": "{label, expr}: arithmetic over the result's columns by name or label in brackets, e.g. round(([Revenue] - [Cost]) / [Revenue] * 100, 1); + - * /, round, abs, min, max, coalesce",
        "subtotals": "true with two or more groupings: the measures per value of the first, returned as `subtotals`",
        "limits": {"rows": crate::spec::MAX_LIMIT, "pivot_columns": MAX_PIVOT_COLUMNS, "pivot_cells": MAX_PIVOT_CELLS, "formulas": MAX_FORMULAS},
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::spec::Spec;
    use ferrum_kernel::ctx::{Access, EntityAccess, PermSet};
    use std::collections::BTreeMap;

    pub(crate) fn schema() -> Arc<Schema> {
        let mut pkgs = ferrum_domain::packages().unwrap();
        pkgs.extend(ferrum_app_erp::packages().unwrap());
        pkgs.extend(crate::packages().unwrap());
        let catalog = ferrum_meta::Catalog::from_packages(pkgs).unwrap();
        let mut s = ferrum_meta::effective::build(&catalog, &["selling".into(), "buying".into(), "studio".into()], &[], 1).unwrap();
        s.timezone = "Asia/Kolkata".into();
        Arc::new(s)
    }

    /// A user reading `entities` (all records unless listed in `own`), with `groups` field groups.
    pub(crate) fn user(schema: Arc<Schema>, entities: &[&str], own: &[&str], groups: &[&str]) -> Ctx {
        let mut perms = PermSet::default();
        for e in entities {
            let read = if own.contains(e) { Access::Own } else { Access::All };
            perms.entities.insert(e.to_string(), EntityAccess { read, read_groups: groups.iter().map(|g| g.to_string()).collect(), ..Default::default() });
        }
        Ctx {
            tenant: uuid::Uuid::now_v7(),
            user: uuid::Uuid::now_v7(),
            email: "u@x.test".into(),
            full_name: "U".into(),
            roles: vec![],
            is_admin: false,
            is_system: false,
            request_id: "t".into(),
            scopes: BTreeMap::new(),
            default_company: None,
            fy_start_month: 4,
            schema,
            perms: Arc::new(perms),
            depth: 0,
        }
    }

    pub(crate) fn sales(schema: Arc<Schema>) -> Ctx {
        user(schema, &["SalesInvoice", "Customer", "CustomerGroup", "Item", "ItemGroup", "Company"], &[], &[])
    }

    fn spec(v: Value) -> Spec {
        Spec::parse(&v).unwrap()
    }

    fn sql(qb: &QueryBuilder<Postgres>) -> String {
        qb.sql().as_str().to_string()
    }

    fn err(ctx: &Ctx, v: Value) -> Error {
        compile(ctx, &spec(v)).err().expect("should be refused")
    }

    #[test]
    fn child_measures_group_through_joins() {
        let ctx = sales(schema());
        let plan = compile(
            &ctx,
            &spec(json!({"entity": "SalesInvoice", "filters": [["docstatus", "=", 1], ["customer.customer_group", "=", uuid::Uuid::nil().to_string()]],
                "group_by": [{"field": "items.item.item_group"}], "measures": [{"fn": "sum", "field": "items.amount"}, {"fn": "count_distinct", "field": "customer"}]})),
        )
        .unwrap();
        assert_eq!(plan.grain(), Some("items"));
        let s = sql(&plan.grouped(None, &[], None, 500).unwrap());
        assert!(s.contains(r#"from "e_sales_invoice" a join "e_sales_invoice__items" c on c.tenant_id = a.tenant_id and c.parent_id = a.id"#), "{s}");
        assert!(s.contains(r#"left join "e_item" l1 on l1.tenant_id = a.tenant_id and l1.id = c."item""#), "{s}");
        assert!(s.contains(r#"left join "e_customer" l2 on l2.tenant_id = a.tenant_id and l2.id = a."customer""#), "{s}");
        assert!(s.contains(r#"l1."item_group" as d0"#) && s.contains(r#"c."amount" as v0"#), "{s}");
        assert!(s.contains("sum(s.v0)::text as m0") && s.contains("count(distinct s.v1) as m1"), "{s}");
        assert!(s.contains("group by s.d0 order by sum(s.v0) desc nulls last, s.d0 asc nulls last limit $"), "{s}");
        // Values are parameters, never SQL text.
        assert!(s.contains(r#"a.docstatus = $"#) && s.contains(r#"l2."customer_group" = $"#), "{s}");
        assert!(!s.contains("00000000-0000-0000-0000-000000000000"), "{s}");
        assert_eq!(s.matches('$').count(), 4, "tenant, two filter values, limit: {s}");
    }

    #[test]
    fn child_filters_are_row_level_at_child_grain_and_exists_otherwise() {
        let ctx = sales(schema());
        let item = uuid::Uuid::now_v7().to_string();
        let rows = compile(&ctx, &spec(json!({"entity": "SalesInvoice", "filters": [["items.item", "=", item]], "group_by": [{"field": "items.item"}], "measures": [{"fn": "sum", "field": "items.qty"}]}))).unwrap();
        let s = sql(&rows.grouped(None, &[], None, 10).unwrap());
        assert!(s.contains(r#" and c."item" = $"#) && !s.contains("exists"), "{s}");
        let docs =
            compile(&ctx, &spec(json!({"entity": "SalesInvoice", "filters": [["items.item.item_group", "=", item]], "measures": [{"fn": "count"}]}))).unwrap();
        let s = sql(&docs.totals(None).unwrap());
        assert!(
            s.contains(r#"and exists (select 1 from "e_sales_invoice__items" x0 left join "e_item" y0 on y0.tenant_id = x0.tenant_id and y0.id = x0."item" where x0.tenant_id = a.tenant_id and x0.parent_id = a.id and y0."item_group" = $"#),
            "{s}"
        );
        let tagged = compile(&ctx, &spec(json!({"entity": "SalesInvoice", "filters": [["_tag", "=", "urgent"]], "measures": [{"fn": "count"}]}))).unwrap();
        assert!(sql(&tagged.totals(None).unwrap()).contains("exists (select 1 from record_tag t where"));
    }

    #[test]
    fn unreadable_fields_and_entities_are_refused_everywhere() {
        let s = schema();
        let ctx = sales(s.clone());
        let forbidden = |v: Value| matches!(err(&ctx, v), Error::Forbidden(_));
        // credit_limit is in the `credit` field group, which this user lacks.
        assert!(forbidden(json!({"entity": "SalesInvoice", "measures": [{"fn": "sum", "field": "customer.credit_limit"}]})));
        assert!(forbidden(json!({"entity": "SalesInvoice", "filters": [["customer.credit_limit", ">", 0]], "measures": [{"fn": "count"}]})));
        assert!(forbidden(json!({"entity": "SalesInvoice", "group_by": [{"field": "customer.credit_limit"}], "measures": [{"fn": "count"}]})));
        assert!(forbidden(json!({"entity": "SalesInvoice", "sort": [{"by": "customer.credit_limit"}]})));
        assert!(forbidden(json!({"entity": "Customer", "fields": [{"field": "credit_limit"}]})));
        // Entities: the base and every joined one.
        assert!(forbidden(json!({"entity": "PurchaseInvoice", "measures": [{"fn": "count"}]})));
        assert!(forbidden(json!({"entity": "SalesInvoice", "group_by": [{"field": "items.warehouse.warehouse_name"}], "measures": [{"fn": "count"}]})));
        assert!(forbidden(json!({"entity": "SalesInvoice", "filters": [["project.project_name", "=", "x"]], "measures": [{"fn": "count"}]})));
        // With the group, the same query compiles.
        let credit = user(s, &["SalesInvoice", "Customer"], &[], &["credit"]);
        assert!(compile(&credit, &spec(json!({"entity": "SalesInvoice", "measures": [{"fn": "sum", "field": "customer.credit_limit"}]}))).is_ok());
    }

    #[test]
    fn own_records_scope_the_base_and_joined_entities() {
        let ctx = user(schema(), &["SalesInvoice", "Customer"], &["SalesInvoice", "Customer"], &[]);
        let plan =
            compile(&ctx, &spec(json!({"entity": "SalesInvoice", "group_by": [{"field": "customer.customer_name"}], "measures": [{"fn": "count"}]}))).unwrap();
        let s = sql(&plan.grouped(None, &[], None, 10).unwrap());
        assert!(s.contains("l1.id = a.\"customer\" and (l1.created_by = $"), "{s}");
        assert!(s.contains("where a.tenant_id = $") && s.contains(" and (a.created_by = $"), "{s}");
    }

    #[test]
    fn buckets_use_the_workspace_time_zone() {
        let ctx = user(schema(), &["SalesInvoice"], &[], &[]);
        let plan = compile(
            &ctx,
            &spec(json!({"entity": "SalesInvoice", "group_by": [{"field": "created_at", "bucket": "month"}, {"field": "posting_date", "bucket": "week"}], "measures": [{"fn": "count"}],
                "date_range": {"field": "created_at", "from": "2026-01-01", "to": "2026-03-31"}})),
        )
        .unwrap();
        assert_eq!(plan.tz.as_deref(), Some("Asia/Kolkata"));
        let s = sql(&plan.grouped(None, &[], None, 10).unwrap());
        assert!(s.contains("from (select $1::text as tz) z cross join \"e_sales_invoice\" a"), "{s}");
        assert!(s.contains("date_trunc('month', ((a.created_at) at time zone z.tz))::date as d0"), "{s}");
        assert!(s.contains(r#"date_trunc('week', (a."posting_date")::timestamp)::date as d1"#), "{s}");
        assert!(s.contains("a.created_at >= $3::date::timestamp at time zone z.tz and a.created_at < ($4::date + 1)::timestamp at time zone z.tz"), "{s}");
        assert!(!s.contains("Asia/Kolkata"));
        // Dates alone need no zone.
        let plain =
            compile(&ctx, &spec(json!({"entity": "SalesInvoice", "group_by": [{"field": "posting_date", "bucket": "year"}], "measures": [{"fn": "count"}]})))
                .unwrap();
        assert!(plain.tz.is_none() && !sql(&plain.totals(None).unwrap()).contains("z.tz"));
        let e = err(&ctx, json!({"entity": "SalesInvoice", "group_by": [{"field": "created_at"}], "measures": [{"fn": "count"}]}));
        assert!(matches!(&e, Error::Validation(f) if f[0].field == "group_by[0]" && f[0].message.contains("bucket")), "{e:?}");
        let e = err(&ctx, json!({"entity": "SalesInvoice", "group_by": [{"field": "customer_name", "bucket": "month"}], "measures": [{"fn": "count"}]}));
        assert!(matches!(&e, Error::Validation(f) if f[0].message.contains("date and date-time")), "{e:?}");
    }

    #[test]
    fn pivot_columns_are_capped_and_ordered() {
        let ctx = sales(schema());
        let by_customer = compile(
            &ctx,
            &spec(json!({"entity": "SalesInvoice", "group_by": [{"field": "customer.customer_group"}], "pivot": {"field": "customer", "max_columns": 2}, "measures": [{"fn": "sum", "field": "grand_total"}]})),
        )
        .unwrap();
        let s = sql(&by_customer.pivot_columns().unwrap());
        assert!(s.contains("group by s.p order by sum(s.v0) desc nulls last, s.p asc nulls last limit $"), "categories biggest first: {s}");
        let by_month = compile(
            &ctx,
            &spec(json!({"entity": "SalesInvoice", "group_by": [{"field": "customer"}], "pivot": {"field": "posting_date", "bucket": "month"}, "measures": [{"fn": "count"}]})),
        )
        .unwrap();
        assert!(sql(&by_month.pivot_columns().unwrap()).contains("group by s.p order by s.p asc nulls last limit $"), "dates in order");
        assert_eq!(by_month.pivot_cap, crate::spec::DEFAULT_PIVOT_COLUMNS);
        let cells = sql(&by_month.pivot_cells(&json!([["x"]]), &["2026-01-01".into()], true).unwrap());
        assert!(
            cells.contains("where jsonb_build_array(s.d0::text) in (select value from jsonb_array_elements($")
                && cells.contains("s.p::text = any($")
                && cells.contains("or s.p is null"),
            "{cells}"
        );
        let e = err(&ctx, json!({"entity": "SalesInvoice", "pivot": {"field": "customer", "max_columns": 101}, "measures": [{"fn": "count"}]}));
        assert!(matches!(&e, Error::Validation(f) if f[0].field == "pivot.max_columns"), "{e:?}");
        let e = err(
            &ctx,
            json!({"entity": "SalesInvoice", "group_by": [{"field": "customer"}], "pivot": {"field": "status", "max_columns": 100}, "limit": 10000, "measures": [{"fn": "count"}]}),
        );
        assert!(matches!(&e, Error::Validation(f) if f[0].field == "pivot" && f[0].message.contains("cells")), "{e:?}");
        let e =
            err(&ctx, json!({"entity": "SalesInvoice", "group_by": [{"field": "customer"}], "pivot": {"field": "customer"}, "measures": [{"fn": "count"}]}));
        assert!(matches!(&e, Error::Validation(f) if f[0].field == "pivot.field"), "{e:?}");
    }

    #[test]
    fn sorts_by_measures_labels_and_dimensions() {
        let ctx = sales(schema());
        let plan = compile(
            &ctx,
            &spec(json!({"entity": "SalesInvoice", "group_by": [{"field": "customer"}], "measures": [{"fn": "count"}, {"fn": "sum", "field": "grand_total", "label": "Revenue"}],
                "sort": [{"by": "Revenue", "dir": "desc"}, {"by": "count"}, {"by": "customer", "dir": "desc"}]})),
        )
        .unwrap();
        let s = sql(&plan.grouped(None, &[], None, 10).unwrap());
        assert!(s.contains("order by sum(s.v1) desc nulls last, count(*) asc nulls last, s.d0 desc nulls last, s.d0 asc nulls last"), "{s}");
        let e = err(&ctx, json!({"entity": "SalesInvoice", "group_by": [{"field": "customer"}], "measures": [{"fn": "count"}], "sort": [{"by": "nope"}]}));
        assert!(matches!(&e, Error::Validation(f) if f[0].field == "sort[0].by" && f[0].message.contains("customer, count")), "{e:?}");
        // A time series is chronological first, whatever else is sorted.
        let ts = compile(
            &ctx,
            &spec(json!({"entity": "SalesInvoice", "time_series": {"field": "posting_date", "bucket": "month"}, "group_by": [{"field": "status"}], "measures": [{"fn": "count"}], "sort": [{"by": "count", "dir": "desc"}]})),
        )
        .unwrap();
        assert!(
            sql(&ts.grouped(None, &[], None, 10).unwrap())
                .contains("order by s.d0 asc nulls last, count(*) desc nulls last, s.d0 asc nulls last, s.d1 asc nulls last")
        );
    }

    #[test]
    fn spec_mistakes_name_their_place() {
        let ctx = sales(schema());
        let at = |v: Value| match err(&ctx, v) {
            Error::Validation(f) => format!("{}: {}", f[0].field, f[0].message),
            other => panic!("{other:?}"),
        };
        assert!(at(json!({"entity": "Nope"})).starts_with("entity: unknown entity"));
        assert!(at(json!({"entity": "SalesInvoiceItem"})).contains("parent"));
        assert!(
            at(json!({"entity": "SalesInvoice", "group_by": [{"field": "nope"}], "measures": [{"fn": "count"}]}))
                .starts_with("group_by[0].field: unknown field 'nope' on Sales Invoice")
        );
        assert!(
            at(json!({"entity": "SalesInvoice", "group_by": [{"field": "items"}], "measures": [{"fn": "count"}]})).contains("child table: use items.<field>")
        );
        assert!(at(json!({"entity": "SalesInvoice", "group_by": [{"field": "a.b.c.d"}], "measures": [{"fn": "count"}]})).contains("too deep"));
        assert!(at(json!({"entity": "SalesInvoice", "measures": [{"fn": "sum", "field": "customer_name"}]})).starts_with("measures[0]: sum cannot be applied"));
        assert!(at(json!({"entity": "SalesInvoice", "measures": [{"fn": "sum"}]})).contains("needs a field"));
        assert!(at(json!({"entity": "SalesInvoice", "group_by": [{"field": "customer"}]})).starts_with("measures:"));
        assert!(
            at(json!({"entity": "SalesInvoice", "group_by": [{"field": "items.item"}, {"field": "taxes.account"}], "measures": [{"fn": "count"}]}))
                .contains("one child table")
        );
        assert!(
            at(json!({"entity": "SalesInvoice", "group_by": [{"field": "items.item"}], "measures": [{"fn": "sum", "field": "grand_total"}]}))
                .contains("once per items row")
        );
        assert!(at(json!({"entity": "SalesInvoice", "measures": [{"fn": "count"}], "filters": [["customer", "=", "not-an-id"]]})).starts_with("filters[0]:"));
        assert!(at(json!({"entity": "SalesInvoice", "measures": [{"fn": "count"}], "compare": "previous_period"})).starts_with("compare:"));
        assert!(at(json!({"entity": "SalesInvoice", "measures": [{"fn": "count"}], "date_range": {"field": "posting_date", "from": "2026-02-01", "to": "2026-01-01"}})).contains("after"));
        assert!(
            at(json!({"entity": "SalesInvoice", "measures": [{"fn": "count"}], "date_range": {"field": "posting_date", "from": "last week"}}))
                .starts_with("date_range.from:")
        );
        assert!(at(json!({"entity": "SalesInvoice", "measures": [{"fn": "count"}, {"fn": "count"}]})).contains("twice"));
        assert!(at(json!({"entity": "SalesInvoice", "limit": 0})).starts_with("limit:"));
        // Child-grain duplicates are fine for duplicate-proof aggregates.
        assert!(compile(&ctx, &spec(json!({"entity": "SalesInvoice", "group_by": [{"field": "items.item"}], "measures": [{"fn": "count_distinct", "field": "id"}, {"fn": "max", "field": "grand_total"}]}))).is_ok());
    }

    #[test]
    fn plain_rows_list_columns_with_number_totals() {
        let ctx = sales(schema());
        let plan = compile(&ctx, &spec(json!({"entity": "SalesInvoice", "fields": [{"field": "customer"}, {"field": "grand_total"}, {"field": "customer.customer_group"}], "search": "50%_off"}))).unwrap();
        assert_eq!(plan.mode, Mode::Raw);
        let s = sql(&plan.rows().unwrap());
        assert!(s.contains("order by s.o0 desc nulls last, s._id asc limit $"), "the entity's list sort: {s}");
        assert!(s.contains("a.number::text ilike $"), "{s}");
        assert!(s.contains("select s._id, s.r0 as r0, s.r1::text as r1, s.r2 as r2 from"), "decimals travel as text: {s}");
        assert!(sql(&plan.raw_totals().unwrap().unwrap()).contains("sum(s.r1)::text as r1"));
        let default = compile(&ctx, &spec(json!({"entity": "SalesInvoice"}))).unwrap();
        let names: Vec<&str> = default.raw.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["number", "customer_name", "posting_date", "grand_total", "outstanding_amount", "status"]);
    }

    #[test]
    fn field_catalog_follows_permissions() {
        let s = schema();
        let f = fields(&sales(s.clone()), "SalesInvoice").unwrap();
        let paths: Vec<&str> = f["fields"].as_array().unwrap().iter().map(|x| x["path"].as_str().unwrap()).collect();
        for p in ["posting_date", "customer", "customer.customer_group", "items.amount", "items.item.item_group", "created_at"] {
            assert!(paths.contains(&p), "{p}");
        }
        assert!(!paths.contains(&"customer.credit_limit"), "field groups hide fields");
        assert!(!paths.iter().any(|p| p.starts_with("project.")), "unreadable links are not offered");
        let date = f["fields"].as_array().unwrap().iter().find(|x| x["path"] == "posting_date").unwrap();
        assert_eq!(date["buckets"].as_array().unwrap().len(), 7);
        let amount = f["fields"].as_array().unwrap().iter().find(|x| x["path"] == "items.amount").unwrap();
        assert_eq!(amount["aggregates"], json!(["count_distinct", "sum", "avg", "min", "max"]));
        assert!(matches!(fields(&sales(s), "PurchaseInvoice"), Err(Error::Forbidden(_))));
    }
}
