//! Cluster-level schema sync: entity tables from metadata, then package SQL.
//!
//! Additive and idempotent: creates tables, adds missing columns, foreign keys, indexes,
//! exclusion constraints and RLS policies. It never drops or retypes columns (expand/contract);
//! type drift and orphaned columns are reported. Replicas serialize on an advisory lock and skip
//! the work entirely when the catalog fingerprint is unchanged.

use crate::db::Db;
use crate::error::{Error, Result};
use crate::store::SYSTEM_KEYS;
use ferrum_meta::{Catalog, EffEntity, EntityKind, FieldDef, FieldType, OnDelete, Schema, Storage, effective};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, PgConnection};
use std::collections::{BTreeMap, HashMap, HashSet};

const SYNC_LOCK: i64 = 0x0FE2_2077;

/// The longest the sync waits for something it cannot hurry: another replica's sync (its advisory lock), and the
/// transactions with a snapshot older than an index build, which `create index concurrently` lets finish before each of
/// its phases. Longer than the 10 s other statements wait for a lock: a wait here queues for no table's lock, so no
/// writer waits behind it.
/// A transaction of this server's own seldom lasts that long (a statement ends at its statement timeout, an idle
/// transaction at 60 s: `db::IDLE_IN_TRANSACTION`); one that does (a dump, a report from a console) fails the start,
/// naming the index, and the next start builds it again.
const BUILD_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SyncReport {
    pub skipped: bool,
    pub statements: usize,
    pub sql_files_applied: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

pub fn column_type(f: &FieldDef) -> &'static str {
    match f.ftype {
        FieldType::Int => "bigint",
        FieldType::Decimal | FieldType::Currency | FieldType::Percent => "numeric(28,9)",
        FieldType::Date => "date",
        FieldType::Datetime => "timestamptz",
        FieldType::Check => "boolean",
        FieldType::Link => "uuid",
        FieldType::Json => "jsonb",
        FieldType::Table | FieldType::Secret => "",
        _ => "text",
    }
}

fn pg_type_name(f: &FieldDef) -> &'static str {
    match f.ftype {
        FieldType::Int => "bigint",
        FieldType::Decimal | FieldType::Currency | FieldType::Percent => "numeric",
        FieldType::Date => "date",
        FieldType::Datetime => "timestamp with time zone",
        FieldType::Check => "boolean",
        FieldType::Link => "uuid",
        FieldType::Json => "jsonb",
        _ => "text",
    }
}

fn short_hash(s: &str) -> String {
    hex::encode(&Sha256::digest(s.as_bytes())[..4])
}

/// What this file makes of a catalog, beside the catalog itself: a new version syncs every database
/// once, though no package changed. 2: indexes in lists' default order.
const DDL_VERSION: u32 = 3;

/// Constraint/index name that fits Postgres' 63-byte limit and is stable.
fn obj_name(kind: &str, table: &str, cols: &[&str]) -> String {
    let raw = format!("{kind}_{table}_{}", cols.join("_"));
    if raw.len() <= 63 { raw } else { format!("{kind}_{}_{}", &table[..table.len().min(40)], short_hash(&raw)) }
}

/// What people read when an entity's unique index or exclusion refuses a write, in place of the
/// constraint's name: "another Permit Type already has this Type Name". Set on every sync, skipped
/// or not, as the words are kept in this process alone.
fn constraint_messages(schema: &Schema) {
    let managed = schema.entities.values().filter(|e| !e.custom && e.def.kind != EntityKind::Child && e.def.kind != EntityKind::Ledger);
    for ent in managed {
        let (t, kind) = (&ent.table, if ent.def.submittable { "uqv" } else { "uq" });
        let taken = |labels: Vec<String>| {
            let words = match labels.split_last() {
                Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
                _ => labels.concat(),
            };
            format!("another {} already has this {words}", ent.def.label)
        };
        for f in ent.value_fields().filter(|f| f.unique && ent.storage_of(&f.name) == Some(Storage::Column)) {
            crate::error::set_constraint_message(&obj_name(kind, t, &[&f.name]), &taken(vec![f.label()]));
        }
        for group in &ent.def.unique {
            let cols: Vec<&str> = group.iter().map(String::as_str).collect();
            let labels = cols.iter().map(|c| ent.field(c).map_or_else(|| ferrum_meta::humanize(c), FieldDef::label)).collect();
            crate::error::set_constraint_message(&obj_name(kind, t, &cols), &taken(labels));
        }
        for (i, x) in ent.def.exclusion.iter().enumerate() {
            if let Some(m) = &x.message {
                crate::error::set_constraint_message(&obj_name("ex", t, &[&i.to_string()]), m);
            }
        }
    }
}

struct Ddl<'a> {
    conn: &'a mut PgConnection,
    /// A dry run ([`lacks`]): statements are collected here instead of run.
    dry: Option<Vec<String>>,
    report: SyncReport,
    constraints: HashSet<String>,
    /// Foreign keys that exist but were never validated (an earlier start's validation failed): validated again.
    unvalidated: HashSet<String>,
    policies: HashSet<String>,
    columns: HashMap<String, HashMap<String, String>>,
    /// The schema's indexes by name, whether each is valid (a build that failed leaves an invalid one).
    indexes: HashMap<String, bool>,
    /// Tables this run created: empty and unseen by anyone, so their indexes are built the plain way.
    new_tables: HashSet<String>,
}

impl<'a> Ddl<'a> {
    fn new(conn: &'a mut PgConnection, dry: bool) -> Self {
        Ddl {
            conn,
            dry: dry.then(Vec::new),
            report: SyncReport::default(),
            constraints: HashSet::new(),
            unvalidated: HashSet::new(),
            policies: HashSet::new(),
            columns: HashMap::new(),
            indexes: HashMap::new(),
            new_tables: HashSet::new(),
        }
    }
}

impl Ddl<'_> {
    async fn exec(&mut self, sql: String) -> Result<()> {
        if let Some(planned) = &mut self.dry {
            planned.push(sql);
            return Ok(());
        }
        tracing::debug!("ddl: {sql}");
        sqlx::raw_sql(AssertSqlSafe(sql.clone())).execute(&mut *self.conn).await.map_err(|e| Error::Internal(format!("schema sync failed on `{sql}`: {e}")))?;
        self.report.statements += 1;
        Ok(())
    }

    async fn load_state(&mut self) -> Result<()> {
        self.constraints = sqlx::query_scalar::<_, String>(
            "select conname::text from pg_constraint c join pg_namespace n on n.oid = c.connamespace where n.nspname = current_schema()",
        )
        .fetch_all(&mut *self.conn)
        .await?
        .into_iter()
        .collect();
        self.unvalidated = sqlx::query_scalar::<_, String>(
            "select conname::text from pg_constraint c join pg_namespace n on n.oid = c.connamespace where n.nspname = current_schema() and not c.convalidated",
        )
        .fetch_all(&mut *self.conn)
        .await?
        .into_iter()
        .collect();
        self.indexes = sqlx::query_as::<_, (String, bool)>(
            "select c.relname::text, i.indisvalid from pg_index i join pg_class c on c.oid = i.indexrelid join pg_namespace n on n.oid = c.relnamespace \
             where n.nspname = current_schema()",
        )
        .fetch_all(&mut *self.conn)
        .await?
        .into_iter()
        .collect();
        self.policies =
            sqlx::query_scalar::<_, String>("select tablename::text || '.' || policyname::text from pg_policies where schemaname = current_schema()")
                .fetch_all(&mut *self.conn)
                .await?
                .into_iter()
                .collect();
        let cols: Vec<(String, String, String)> =
            sqlx::query_as("select table_name::text, column_name::text, data_type::text from information_schema.columns where table_schema = current_schema()")
                .fetch_all(&mut *self.conn)
                .await?;
        for (t, c, ty) in cols {
            self.columns.entry(t).or_default().insert(c, ty);
        }
        Ok(())
    }

    async fn rls(&mut self, table: &str) -> Result<()> {
        if self.policies.contains(&format!("{table}.tenant_isolation")) {
            return Ok(());
        }
        let t = q(table);
        self.exec(format!(
            "alter table {t} enable row level security; alter table {t} force row level security; \
             create policy tenant_isolation on {t} using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) \
             with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))"
        ))
        .await?;
        self.policies.insert(format!("{table}.tenant_isolation"));
        Ok(())
    }

    async fn add_columns(&mut self, table: &str, ent: &EffEntity) -> Result<()> {
        let existing = self.columns.get(table).cloned().unwrap_or_default();
        for f in ent.value_fields() {
            if ent.storage_of(&f.name) != Some(Storage::Column) {
                continue;
            }
            match existing.get(&f.name) {
                Some(ty) if ty != pg_type_name(f) => self.report.warnings.push(format!(
                    "{table}.{}: column type is {ty} but metadata says {} (not changed automatically)",
                    f.name,
                    pg_type_name(f)
                )),
                Some(_) => {}
                None => {
                    let extra = if f.ftype == FieldType::Check { " not null default false" } else { "" };
                    self.exec(format!("alter table {} add column if not exists {} {}{extra}", q(table), q(&f.name), column_type(f))).await?;
                }
            }
        }
        let known: HashSet<&str> = ent.value_fields().map(|f| f.name.as_str()).collect();
        for col in existing.keys() {
            let system = [
                "tenant_id",
                "id",
                "number",
                "docstatus",
                "version",
                "workflow_state",
                "amended_from",
                "created_at",
                "created_by",
                "updated_at",
                "updated_by",
                "ext",
                "parent_id",
                "idx",
            ];
            if !known.contains(col.as_str()) && !system.contains(&col.as_str()) {
                self.report.warnings.push(format!("{table}.{col}: column no longer defined in metadata (kept)"));
            }
        }
        Ok(())
    }

    async fn index(&mut self, table: &str, cols: &[&str], unique: bool, partial: Option<&str>) -> Result<()> {
        self.index_named(&obj_name(if unique { "uq" } else { "ix" }, table, cols), table, cols, unique, partial).await
    }

    /// A unique index over documents that are not cancelled: a cancelled document is void, so its
    /// values must not block new or amended ones. Replaces the plain one made while the entity was not
    /// submittable (a package release or a workspace's own definition can make it so): dropped once the
    /// new one stands, so that the values are never left unchecked while a large table's index is built.
    async fn unique_live(&mut self, table: &str, cols: &[&str], partial: Option<&str>) -> Result<()> {
        let cond = partial.map_or("docstatus <> 2".to_string(), |p| format!("{p} and docstatus <> 2"));
        self.index_named(&obj_name("uqv", table, cols), table, cols, true, Some(&cond)).await?;
        let old = obj_name("uq", table, cols);
        if self.indexes.remove(&old).is_some() {
            self.exec(format!("drop index concurrently if exists {}", q(&old))).await?;
        }
        Ok(())
    }

    async fn index_named(&mut self, name: &str, table: &str, cols: &[&str], unique: bool, partial: Option<&str>) -> Result<()> {
        let collist = std::iter::once("tenant_id").chain(cols.iter().copied()).map(q).collect::<Vec<_>>().join(", ");
        let where_ = partial.map(|p| format!(" where {p}")).unwrap_or_default();
        self.make_index(name, table, unique, &format!("on {} ({collist}){where_}", q(table))).await
    }

    /// Makes index `name` from `rest` (`on <table> (...)`, what follows its name), without blocking writes: on a table
    /// that exists already it is built `concurrently` (outside any transaction: this connection is in none, each
    /// statement is sent alone), waiting at most [`BUILD_WAIT`] for older transactions and saying every 30 s that it
    /// waits. An index of that name left invalid by a build that failed is dropped and built again. A build that fails
    /// (rows that repeat a value in a unique one, the wait run out) leaves nothing invalid behind and fails the sync,
    /// naming the index. One that stands already costs nothing.
    async fn make_index(&mut self, name: &str, table: &str, unique: bool, rest: &str) -> Result<()> {
        let create = |when: &str| format!("create {}index {when}if not exists {} {rest}", if unique { "unique " } else { "" }, q(name));
        if self.dry.is_some() || self.new_tables.contains(table) {
            return self.exec(create("")).await;
        }
        match self.indexes.get(name) {
            Some(true) => return Ok(()),
            Some(false) => self.exec(format!("drop index concurrently if exists {}", q(name))).await?,
            None => {}
        }
        let sql = create("concurrently ");
        tracing::debug!("ddl: {sql}");
        sqlx::raw_sql(AssertSqlSafe(format!("set lock_timeout = {}", BUILD_WAIT.as_millis()))).execute(&mut *self.conn).await?;
        let started = std::time::Instant::now();
        let built = {
            let run = sqlx::raw_sql(AssertSqlSafe(sql.clone())).execute(&mut *self.conn);
            tokio::pin!(run);
            loop {
                match tokio::time::timeout(std::time::Duration::from_secs(30), &mut run).await {
                    Ok(done) => break done,
                    Err(_) => tracing::info!("schema sync: still building {name} on {table} ({}s)", started.elapsed().as_secs()),
                }
            }
        };
        let _ = sqlx::raw_sql("reset lock_timeout").execute(&mut *self.conn).await;
        if let Err(e) = built {
            let _ = sqlx::raw_sql(AssertSqlSafe(format!("drop index concurrently if exists {}", q(name)))).execute(&mut *self.conn).await;
            let detail =
                e.as_database_error().and_then(|d| d.try_downcast_ref::<sqlx::postgres::PgDatabaseError>()).and_then(|d| d.detail()).unwrap_or_default();
            let repeated =
                if unique && e.as_database_error().is_some_and(|d| d.code().as_deref() == Some("23505")) { ": rows already repeat a value" } else { "" };
            return Err(Error::Internal(format!("schema sync failed on `{sql}`{repeated}: {e} {detail}")));
        }
        self.report.statements += 1;
        self.indexes.insert(name.to_string(), true);
        Ok(())
    }

    /// An entity's lists read in their default order from an index, rather than by sorting every
    /// row of the workspace for each page (`query::order_by` makes the order with the same words).
    async fn sort_index(&mut self, table: &str, ent: &EffEntity) -> Result<()> {
        let Ok((field, desc)) = crate::query::parse_sort(ent, None) else { return Ok(()) };
        let column = SYSTEM_KEYS.contains(&field.as_str()) || ent.storage_of(&field) == Some(Storage::Column);
        // The `(tenant_id, created_at, id)` index serves `created_at` both ways.
        if !column || field == "created_at" {
            return Ok(());
        }
        let dir = if desc { "desc" } else { "asc" };
        let name = obj_name("ixs", table, &[&field, dir]);
        let rest = format!("on {} (tenant_id, {} {}, id {dir})", q(table), q(&field), crate::query::direction(&field, desc));
        self.make_index(&name, table, false, &rest).await
    }

    async fn links(&mut self, table: &str, ent: &EffEntity, schema: &Schema) -> Result<()> {
        for f in ent.value_fields().filter(|f| f.ftype == FieldType::Link) {
            if ent.storage_of(&f.name) != Some(Storage::Column) {
                continue;
            }
            let target = f.to.as_deref().and_then(|t| schema.entity(t));
            let Some(target) = target else { continue };
            if target.custom || target.def.kind == EntityKind::Ledger || target.table.is_empty() {
                continue;
            }
            self.index(table, &[&f.name], false, None).await?;
            let name = obj_name("fk", table, &[&f.name]);
            if self.constraints.contains(&name) {
                self.validate(table, &name).await?;
                continue;
            }
            let on_delete = match f.on_delete {
                OnDelete::Restrict => "restrict".to_string(),
                OnDelete::SetNull => format!("set null ({})", q(&f.name)),
                OnDelete::Cascade => "cascade".to_string(),
            };
            // Not validated as it is added, then validated by itself: the first holds the table for an instant,
            // the second reads it with writes carrying on.
            self.exec(format!(
                "alter table {} add constraint {} foreign key (tenant_id, {}) references {} (tenant_id, id) on delete {on_delete} deferrable initially immediate not valid",
                q(table),
                q(&name),
                q(&f.name),
                q(&target.table)
            ))
            .await?;
            self.constraints.insert(name.clone());
            self.unvalidated.insert(name.clone());
            self.validate(table, &name).await?;
        }
        Ok(())
    }

    /// A foreign key a start before this one left unvalidated (its validation failed on rows that point at nothing).
    async fn validate(&mut self, table: &str, name: &str) -> Result<()> {
        if self.unvalidated.remove(name) {
            self.exec(format!("alter table {} validate constraint {}", q(table), q(name))).await?;
        }
        Ok(())
    }

    async fn search_indexes(&mut self, table: &str, ent: &EffEntity) -> Result<()> {
        let mut cols: Vec<&str> = ent.def.search_fields.iter().map(String::as_str).collect();
        if let Some(t) = &ent.def.title_field {
            cols.push(t);
        }
        cols.sort();
        cols.dedup();
        for c in cols {
            let is_text_col = ent.field(c).is_some_and(|f| f.ftype.is_text()) && ent.storage_of(c) == Some(Storage::Column);
            if is_text_col {
                let name = obj_name("trgm", table, &[c]);
                self.make_index(&name, table, false, &format!("on {} using gin ({} gin_trgm_ops)", q(table), q(c))).await?;
            }
        }
        Ok(())
    }
}

const SYSTEM_COLUMNS: &str = "\
  tenant_id uuid not null references tenant(id) on delete cascade deferrable initially immediate,
  id uuid not null,
  number text,
  docstatus smallint not null default 0 check (docstatus between 0 and 2),
  version int not null default 1 check (version >= 1),
  workflow_state text,
  amended_from uuid,
  created_at timestamptz not null default now(),
  created_by uuid,
  updated_at timestamptz not null default now(),
  updated_by uuid,
  ext jsonb not null default '{}'::jsonb,
  primary key (tenant_id, id)";

/// The schema with every package installed and no tenant overlays: what physical tables must exist.
pub fn cluster_schema(catalog: &Catalog) -> Result<Schema> {
    let all: Vec<String> = catalog.packages().map(|p| p.name().to_string()).collect();
    effective::build(catalog, &all, &[], 0).map_err(|e| Error::Internal(format!("catalog: {e}")))
}

/// The words for constraint refusals, for a start that does not sync (a guest, or one that does not migrate).
pub fn words(catalog: &Catalog) -> Result<()> {
    constraint_messages(&cluster_schema(catalog)?);
    Ok(())
}

fn fingerprint(catalog: &Catalog) -> String {
    format!("{}:{DDL_VERSION}", catalog.fingerprint())
}

/// Whether the schema was last synced for exactly this catalog: what `sync` would skip. Writes nothing.
pub async fn is_current(db: &Db, catalog: &Catalog) -> Result<bool> {
    let current: Option<String> = sqlx::query_scalar("select value from cluster_state where key = 'catalog_fingerprint'").fetch_optional(&db.pool).await?;
    Ok(current.as_deref() == Some(fingerprint(catalog).as_str()))
}

/// The entities that have tables of their own.
fn managed(schema: &Schema) -> Vec<&EffEntity> {
    schema.entities.values().map(|e| e.as_ref()).filter(|e| !e.custom && e.def.kind != EntityKind::Child && e.def.kind != EntityKind::Ledger).collect()
}

/// Pass 1 of the sync: every table and column the catalog needs.
async fn tables_and_columns(ddl: &mut Ddl<'_>, schema: &Schema, managed: &[&EffEntity]) -> Result<()> {
    for ent in managed {
        if !ddl.columns.contains_key(&ent.table) {
            ddl.exec(format!("create table if not exists {} (\n{SYSTEM_COLUMNS}\n)", q(&ent.table))).await?;
            ddl.columns.insert(ent.table.clone(), HashMap::new());
            ddl.new_tables.insert(ent.table.clone());
        }
        ddl.add_columns(&ent.table, ent).await?;
        for child in ent.children.values() {
            let Some(cent) = schema.entity(&child.entity) else { continue };
            if !ddl.columns.contains_key(&child.table) {
                ddl.exec(format!(
                    "create table if not exists {ct} (\n  tenant_id uuid not null,\n  id uuid not null,\n  parent_id uuid not null,\n  \
                     idx int not null check (idx >= 0),\n  created_at timestamptz not null default now(),\n  \
                     updated_at timestamptz not null default now(),\n  ext jsonb not null default '{{}}'::jsonb,\n  \
                     primary key (tenant_id, id),\n  foreign key (tenant_id, parent_id) references {pt} (tenant_id, id) on delete cascade deferrable initially immediate\n)",
                    ct = q(&child.table),
                    pt = q(&ent.table)
                ))
                .await?;
                ddl.columns.insert(child.table.clone(), HashMap::new());
                ddl.new_tables.insert(child.table.clone());
                ddl.index(&child.table, &["parent_id", "idx"], false, None).await?;
            }
            ddl.add_columns(&child.table, cent).await?;
        }
    }
    Ok(())
}

/// What the database lacks for `catalog`, changing nothing: the tables and columns the sync would create (a dry run of
/// its pass 1), and package SQL files it has not applied. Empty when the database holds everything the catalog needs.
pub async fn lacks(db: &Db, catalog: &Catalog) -> Result<Vec<String>> {
    let schema = cluster_schema(catalog)?;
    let mut conn = db.pool.acquire().await?;
    let mut ddl = Ddl::new(&mut conn, true);
    ddl.load_state().await?;
    tables_and_columns(&mut ddl, &schema, &managed(&schema)).await?;
    let mut out = ddl.dry.take().unwrap_or_default();
    let applied: HashSet<(String, String)> =
        sqlx::query_as::<_, (String, String)>("select package, file from cluster_package_sql").fetch_all(&mut *conn).await?.into_iter().collect();
    for pkg in catalog.packages() {
        for (file, _) in &pkg.sql {
            if !applied.contains(&(pkg.name().to_string(), file.clone())) {
                out.push(format!("package SQL {}/{file}", pkg.name()));
            }
        }
    }
    Ok(out)
}

/// In every trigger's WHEN (D7): a transaction that sets `ferrum.copy` (only a copy of a workspace does, and the deletions
/// that go with one: recovery.rs, `copying`) runs with the server's triggers standing aside, in place of
/// `session_replication_role = replica`, which takes a grant only an administrator can give.
pub const STANDS_ASIDE: &str = "current_setting('ferrum.copy'::text, true) IS DISTINCT FROM 'on'::text";

/// `def`, a trigger as `pg_get_triggerdef` writes it, made again to stand aside while a copy runs: [`STANDS_ASIDE`] added
/// to its WHEN, or made its WHEN, the rest as it was. `None` when it stands aside already, or is not written as expected.
pub fn stand_aside(def: &str) -> Option<String> {
    if def.contains("ferrum.copy") {
        return None;
    }
    let at = def.rfind(" EXECUTE FUNCTION ").or_else(|| def.rfind(" EXECUTE PROCEDURE "))?;
    let (head, tail) = def.split_at(at);
    let each = head.find(" FOR EACH ROW").or_else(|| head.find(" FOR EACH STATEMENT"))?;
    let head = match head[each..].find(" WHEN (") {
        Some(w) => {
            let w = each + w;
            let cond = head[w + " WHEN (".len()..].strip_suffix(')')?;
            format!("{} WHEN (({cond}) AND ({STANDS_ASIDE}))", &head[..w])
        }
        None => format!("{head} WHEN ({STANDS_ASIDE})"),
    };
    let head = match head.strip_prefix("CREATE TRIGGER ") {
        Some(rest) => format!("CREATE OR REPLACE TRIGGER {rest}"),
        None if head.starts_with("CREATE CONSTRAINT TRIGGER ") => head,
        None => return None,
    };
    Some(format!("{head}{tail}"))
}

/// What keeps a copy from running as the server's own role: foreign keys that cannot wait for the commit, and triggers
/// that do not stand aside ([`copy_ready`] makes them so), each named.
pub async fn not_ready(conn: &mut PgConnection) -> Result<(Vec<String>, Vec<String>)> {
    let keys: Vec<String> = sqlx::query_scalar(
        "select format('%s.%I', conrelid::regclass, conname) from pg_constraint \
         where contype = 'f' and not condeferrable and connamespace = 'public'::regnamespace and conparentid = 0 order by 1",
    )
    .fetch_all(&mut *conn)
    .await?;
    let triggers: Vec<String> = sqlx::query_scalar(
        "select format('%I on %s', t.tgname, t.tgrelid::regclass) from pg_trigger t join pg_class c on c.oid = t.tgrelid \
         where not t.tgisinternal and t.tgparentid = 0 and c.relnamespace = 'public'::regnamespace \
           and pg_get_triggerdef(t.oid) not like '%ferrum.copy%' order by 1",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok((keys, triggers))
}

/// Makes every foreign key of the schema able to wait for the commit (still checked at once unless a transaction asks
/// otherwise, so nothing changes for anyone else) and every trigger stand aside while a copy runs: what lets a copy of a
/// workspace run as the server's own role (D7). The kernel's migrations, packages' SQL and older syncs made them
/// otherwise; what the sync makes from now on is made so. Run as a migrating server starts, after the sync, a table at a
/// time; with nothing to do it costs two queries. What it could not change is returned (a copy then refuses, naming
/// it), never fatal to the start: a role that does not own a table, say.
pub async fn copy_ready(db: &Db) -> Result<Vec<String>> {
    let mut conn = db.direct.acquire().await?;
    let mut problems = Vec::new();
    let keys: Vec<(String, String)> = sqlx::query_as(
        "select conrelid::regclass::text, quote_ident(conname) from pg_constraint \
         where contype = 'f' and not condeferrable and connamespace = 'public'::regnamespace and conparentid = 0 order by 1, 2",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut by_table: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (table, key) in keys {
        by_table.entry(table).or_default().push(key);
    }
    for (table, keys) in &by_table {
        let each: Vec<String> = keys.iter().map(|k| format!("alter constraint {k} deferrable initially immediate")).collect();
        if let Err(e) = sqlx::raw_sql(AssertSqlSafe(format!("alter table {table} {}", each.join(", ")))).execute(&mut *conn).await {
            problems.push(format!("{table}'s foreign keys: {e}"));
        }
    }
    let triggers: Vec<(String, String, String, bool, String)> = sqlx::query_as(
        "select quote_ident(t.tgname), t.tgrelid::regclass::text, pg_get_triggerdef(t.oid), t.tgconstraint <> 0, t.tgenabled::text \
         from pg_trigger t join pg_class c on c.oid = t.tgrelid \
         where not t.tgisinternal and t.tgparentid = 0 and c.relnamespace = 'public'::regnamespace \
           and pg_get_triggerdef(t.oid) not like '%ferrum.copy%' order by 2, 1",
    )
    .fetch_all(&mut *conn)
    .await?;
    for (name, table, def, constraint, enabled) in triggers {
        let Some(again) = stand_aside(&def) else {
            problems.push(format!("trigger {name} on {table}: not written as expected ({def})"));
            continue;
        };
        // A constraint trigger cannot be replaced in place: dropped and made again, in one transaction.
        let mut sql = if constraint { format!("begin; drop trigger {name} on {table}; {again}; ") } else { format!("begin; {again}; ") };
        match enabled.as_str() {
            "D" => sql.push_str(&format!("alter table {table} disable trigger {name}; ")),
            "A" => sql.push_str(&format!("alter table {table} enable always trigger {name}; ")),
            "R" => sql.push_str(&format!("alter table {table} enable replica trigger {name}; ")),
            _ => {}
        }
        sql.push_str("commit");
        if let Err(e) = sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut *conn).await {
            let _ = sqlx::raw_sql("rollback").execute(&mut *conn).await;
            problems.push(format!("trigger {name} on {table}: {e}"));
        }
    }
    if by_table.is_empty() && problems.is_empty() {
        return Ok(problems);
    }
    tracing::info!(tables = by_table.len(), "foreign keys may wait for the commit, and triggers stand aside, while a copy runs");
    Ok(problems)
}

pub async fn sync(db: &Db, catalog: &Catalog, force: bool) -> Result<SyncReport> {
    // A session lock, and `create index concurrently` outside any transaction: the direct address.
    let mut conn = db.direct.acquire().await?;
    // Another replica's sync may be building an index: wait for it as long as a build waits, not the 10 s of a request.
    // Tried again and again rather than waited for in one statement: a statement that waits holds a snapshot, and the
    // other replica's index build waits for every older snapshot to end, so each would wait for the other (PostgreSQL
    // ends one of them with "deadlock detected").
    let waited = std::time::Instant::now();
    loop {
        if sqlx::query_scalar::<_, bool>("select pg_try_advisory_lock($1)").bind(SYNC_LOCK).fetch_one(&mut *conn).await? {
            break;
        }
        if waited.elapsed() > BUILD_WAIT {
            return Err(Error::Internal(format!("schema sync: another replica's sync still runs after {BUILD_WAIT:?}")));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let result = sync_locked(&mut conn, catalog, force).await;
    let _ = sqlx::query("select pg_advisory_unlock($1)").bind(SYNC_LOCK).execute(&mut *conn).await;
    result
}

async fn sync_locked(conn: &mut PgConnection, catalog: &Catalog, force: bool) -> Result<SyncReport> {
    // Only forward: a build older than the one that last synced changes nothing (window.rs).
    crate::window::refuse_older(conn).await?;
    let current: Option<String> = sqlx::query_scalar("select value from cluster_state where key = 'catalog_fingerprint'").fetch_optional(&mut *conn).await?;
    let fingerprint = fingerprint(catalog);
    let schema = cluster_schema(catalog)?;
    constraint_messages(&schema);
    if !force && current.as_deref() == Some(fingerprint.as_str()) {
        // A build with the same catalog still says that it is here, so older ones can tell.
        crate::window::record(conn).await?;
        return Ok(SyncReport { skipped: true, ..Default::default() });
    }
    let mut ddl = Ddl::new(conn, false);
    ddl.load_state().await?;

    // Pass 1: tables and columns.
    let managed = managed(&schema);
    tables_and_columns(&mut ddl, &schema, &managed).await?;

    // Pass 2: constraints, indexes, RLS (all tables exist now, so FK targets do too).
    for ent in &managed {
        let t = ent.table.clone();
        ddl.links(&t, ent, &schema).await?;
        ddl.index(&t, &["created_at", "id"], false, None).await?;
        ddl.sort_index(&t, ent).await?;
        if ent.def.numbering.is_some() {
            ddl.index(&t, &["number"], true, Some("number is not null")).await?;
        }
        if ent.def.kind == EntityKind::Settings {
            if ent.def.company_scoped {
                ddl.index(&t, &["company"], true, None).await?;
            } else {
                let name = obj_name("uq", &t, &["singleton"]);
                ddl.exec(format!("create unique index if not exists {} on {} (tenant_id)", q(&name), q(&t))).await?;
            }
        }
        for f in ent.value_fields().filter(|f| ent.storage_of(&f.name) == Some(Storage::Column)) {
            let not_null = format!("{} is not null", q(&f.name));
            if f.unique && ent.def.submittable {
                ddl.unique_live(&t, &[&f.name], Some(&not_null)).await?;
            } else if f.unique {
                ddl.index(&t, &[&f.name], true, Some(&not_null)).await?;
            } else if f.index {
                ddl.index(&t, &[&f.name], false, None).await?;
            }
        }
        for group in &ent.def.unique {
            let cols: Vec<&str> = group.iter().map(String::as_str).collect();
            if ent.def.submittable {
                ddl.unique_live(&t, &cols, None).await?;
            } else {
                ddl.index(&t, &cols, true, None).await?;
            }
        }
        for group in &ent.def.indexes {
            let cols: Vec<&str> = group.iter().map(String::as_str).collect();
            ddl.index(&t, &cols, false, None).await?;
        }
        for (i, x) in ent.def.exclusion.iter().enumerate() {
            let name = obj_name("ex", &t, &[&i.to_string()]);
            if ddl.constraints.contains(&name) {
                continue;
            }
            let eq: Vec<String> = std::iter::once("tenant_id".to_string()).chain(x.fields.iter().cloned()).map(|c| format!("{} with =", q(&c))).collect();
            // Dates are whole days (both ends count); date-times are half-open, so back-to-back
            // appointments (10:00-11:00, 11:00-12:00) do not overlap.
            let datetime = ent.field(&x.from).is_some_and(|f| f.ftype == FieldType::Datetime);
            let range =
                if datetime { format!("tstzrange({}, {}, '[)')", q(&x.from), q(&x.to)) } else { format!("daterange({}, {}, '[]')", q(&x.from), q(&x.to)) };
            // Cancelled documents are void and never overlap anything; nor do `when_not` values
            // (validated as plain words when the package loads).
            let mut conds: Vec<String> = Vec::new();
            if ent.def.submittable {
                conds.push("docstatus <> 2".into());
            }
            for (field, values) in &x.when_not {
                let list = values.iter().map(|v| format!("'{}'", v.replace('\'', "''"))).collect::<Vec<_>>().join(", ");
                conds.push(format!("{} is null or {} not in ({list})", q(field), q(field)));
            }
            let live =
                if conds.is_empty() { String::new() } else { format!(" where ({})", conds.iter().map(|c| format!("({c})")).collect::<Vec<_>>().join(" and ")) };
            ddl.exec(format!("alter table {} add constraint {} exclude using gist ({}, {range} with &&){live}", q(&t), q(&name), eq.join(", "))).await?;
            ddl.constraints.insert(name);
        }
        ddl.search_indexes(&t, ent).await?;
        ddl.rls(&t).await?;
        // A held record is not deleted, whoever's SQL asks (the kernel baseline migration), and the row as
        // it was before each change to it is kept while it is held.
        let named = ent.def.name.replace('\'', "''");
        ddl.exec(format!(
            "create or replace trigger ferrum_held before delete on {} for each row when ({STANDS_ASIDE}) execute function ferrum_held_guard('{named}')",
            q(&t)
        ))
        .await?;
        ddl.exec(format!(
            "create or replace trigger ferrum_held_keep before update on {} for each row when ({STANDS_ASIDE}) execute function ferrum_held_keep('{named}')",
            q(&t)
        ))
        .await?;
        for child in ent.children.values() {
            if let Some(cent) = schema.entity(&child.entity) {
                ddl.links(&child.table, cent, &schema).await?;
            }
            ddl.rls(&child.table).await?;
        }
    }

    // Pass 3: package SQL (engine tables, functions, triggers), each file applied once, checksummed.
    let applied: BTreeMap<(String, String), String> = sqlx::query_as::<_, (String, String, String)>("select package, file, checksum from cluster_package_sql")
        .fetch_all(&mut *ddl.conn)
        .await?
        .into_iter()
        .map(|(p, f, c)| ((p, f), c))
        .collect();
    for pkg in catalog.packages() {
        for (file, sql) in &pkg.sql {
            let checksum = hex::encode(Sha256::digest(sql.as_bytes()));
            match applied.get(&(pkg.name().to_string(), file.clone())) {
                Some(c) if *c == checksum => continue,
                Some(_) => {
                    return Err(Error::Internal(format!(
                        "package SQL {}/{file} changed after it was applied; add a new file instead of editing it",
                        pkg.name()
                    )));
                }
                None => {}
            }
            let mut tx = sqlx::Connection::begin(&mut *ddl.conn).await?;
            sqlx::raw_sql(AssertSqlSafe(sql.clone()))
                .execute(&mut *tx)
                .await
                .map_err(|e| Error::Internal(format!("package SQL {}/{file} failed: {e}", pkg.name())))?;
            sqlx::query("insert into cluster_package_sql (package, file, checksum) values ($1, $2, $3)")
                .bind(pkg.name())
                .bind(file)
                .bind(&checksum)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            ddl.report.sql_files_applied.push(format!("{}/{file}", pkg.name()));
        }
    }

    sqlx::query(
        "insert into cluster_state (key, value) values ('catalog_fingerprint', $1) \
         on conflict (key) do update set value = excluded.value, updated_at = now()",
    )
    .bind(&fingerprint)
    .execute(&mut *ddl.conn)
    .await?;
    crate::window::record(ddl.conn).await?;
    for w in &ddl.report.warnings {
        tracing::warn!("schema sync: {w}");
    }
    Ok(ddl.report)
}

#[cfg(test)]
mod copy_tests {
    use super::{STANDS_ASIDE, stand_aside};

    #[test]
    fn every_kind_of_trigger_stands_aside_with_the_rest_as_it_was() {
        let s = |d: &str| stand_aside(d).unwrap();
        assert_eq!(
            s("CREATE TRIGGER ferrum_held BEFORE DELETE ON public.e_item FOR EACH ROW EXECUTE FUNCTION ferrum_held_guard('Item')"),
            format!(
                "CREATE OR REPLACE TRIGGER ferrum_held BEFORE DELETE ON public.e_item FOR EACH ROW WHEN ({STANDS_ASIDE}) EXECUTE FUNCTION ferrum_held_guard('Item')"
            )
        );
        assert_eq!(
            s(
                "CREATE TRIGGER app_user_email_history AFTER UPDATE OF email ON public.app_user FOR EACH ROW WHEN ((old.email IS DISTINCT FROM new.email)) EXECUTE FUNCTION ferrum_person_identifier()"
            ),
            format!(
                "CREATE OR REPLACE TRIGGER app_user_email_history AFTER UPDATE OF email ON public.app_user FOR EACH ROW WHEN (((old.email IS DISTINCT FROM new.email)) AND ({STANDS_ASIDE})) EXECUTE FUNCTION ferrum_person_identifier()"
            ),
            "its own WHEN kept"
        );
        assert_eq!(
            s(
                "CREATE CONSTRAINT TRIGGER journal_entry_balanced AFTER INSERT ON public.journal_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION ferrum_check_journal_balance()"
            ),
            format!(
                "CREATE CONSTRAINT TRIGGER journal_entry_balanced AFTER INSERT ON public.journal_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN ({STANDS_ASIDE}) EXECUTE FUNCTION ferrum_check_journal_balance()"
            ),
            "made again, not replaced in place"
        );
        assert_eq!(
            s(
                "CREATE TRIGGER custom_record_lines_cascade AFTER DELETE ON public.custom_record REFERENCING OLD TABLE AS gone FOR EACH STATEMENT EXECUTE FUNCTION ferrum_custom_lines_cascade()"
            ),
            format!(
                "CREATE OR REPLACE TRIGGER custom_record_lines_cascade AFTER DELETE ON public.custom_record REFERENCING OLD TABLE AS gone FOR EACH STATEMENT WHEN ({STANDS_ASIDE}) EXECUTE FUNCTION ferrum_custom_lines_cascade()"
            )
        );
        assert!(
            s("CREATE TRIGGER audit_event_no_truncate BEFORE TRUNCATE ON public.audit_event FOR EACH STATEMENT EXECUTE FUNCTION ferrum_forbid_change()")
                .contains("STATEMENT WHEN (")
        );
        let once = s("CREATE TRIGGER t BEFORE UPDATE ON public.x FOR EACH ROW EXECUTE FUNCTION f()").replace("CREATE OR REPLACE", "CREATE");
        assert_eq!(stand_aside(&once), None, "once is enough");
        assert_eq!(stand_aside("something else"), None);
    }
}
