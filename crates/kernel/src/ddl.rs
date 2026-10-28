//! Cluster-level schema sync: entity tables from metadata, then package SQL.
//!
//! Additive and idempotent: creates tables, adds missing columns, foreign keys, indexes,
//! exclusion constraints and RLS policies. It never drops or retypes columns (expand/contract);
//! type drift and orphaned columns are reported. Replicas serialize on an advisory lock and skip
//! the work entirely when the catalog fingerprint is unchanged.

use crate::db::Db;
use crate::error::{Error, Result};
use ferrum_meta::{Catalog, EffEntity, EntityKind, FieldDef, FieldType, OnDelete, Schema, Storage, effective};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, PgConnection};
use std::collections::{BTreeMap, HashMap, HashSet};

const SYNC_LOCK: i64 = 0x0FE2_2077;

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
        FieldType::Table => "",
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

/// Constraint/index name that fits Postgres' 63-byte limit and is stable.
fn obj_name(kind: &str, table: &str, cols: &[&str]) -> String {
    let raw = format!("{kind}_{table}_{}", cols.join("_"));
    if raw.len() <= 63 { raw } else { format!("{kind}_{}_{}", &table[..table.len().min(40)], short_hash(&raw)) }
}

struct Ddl<'a> {
    conn: &'a mut PgConnection,
    report: SyncReport,
    constraints: HashSet<String>,
    policies: HashSet<String>,
    columns: HashMap<String, HashMap<String, String>>,
}

impl Ddl<'_> {
    async fn exec(&mut self, sql: String) -> Result<()> {
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
        let name = obj_name(if unique { "uq" } else { "ix" }, table, cols);
        let collist = std::iter::once("tenant_id").chain(cols.iter().copied()).map(q).collect::<Vec<_>>().join(", ");
        let where_ = partial.map(|p| format!(" where {p}")).unwrap_or_default();
        self.exec(format!("create {}index if not exists {} on {} ({collist}){where_}", if unique { "unique " } else { "" }, q(&name), q(table))).await
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
                continue;
            }
            let on_delete = match f.on_delete {
                OnDelete::Restrict => "restrict".to_string(),
                OnDelete::SetNull => format!("set null ({})", q(&f.name)),
                OnDelete::Cascade => "cascade".to_string(),
            };
            self.exec(format!(
                "alter table {} add constraint {} foreign key (tenant_id, {}) references {} (tenant_id, id) on delete {on_delete}",
                q(table),
                q(&name),
                q(&f.name),
                q(&target.table)
            ))
            .await?;
            self.constraints.insert(name);
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
                self.exec(format!("create index if not exists {} on {} using gin ({} gin_trgm_ops)", q(&name), q(table), q(c))).await?;
            }
        }
        Ok(())
    }
}

const SYSTEM_COLUMNS: &str = "\
  tenant_id uuid not null references tenant(id) on delete cascade,
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

pub async fn sync(db: &Db, catalog: &Catalog, force: bool) -> Result<SyncReport> {
    let mut conn = db.pool.acquire().await?;
    sqlx::query("select pg_advisory_lock($1)").bind(SYNC_LOCK).execute(&mut *conn).await?;
    let result = sync_locked(&mut conn, catalog, force).await;
    let _ = sqlx::query("select pg_advisory_unlock($1)").bind(SYNC_LOCK).execute(&mut *conn).await;
    result
}

async fn sync_locked(conn: &mut PgConnection, catalog: &Catalog, force: bool) -> Result<SyncReport> {
    let current: Option<String> = sqlx::query_scalar("select value from cluster_state where key = 'catalog_fingerprint'").fetch_optional(&mut *conn).await?;
    if !force && current.as_deref() == Some(catalog.fingerprint()) {
        return Ok(SyncReport { skipped: true, ..Default::default() });
    }
    let schema = cluster_schema(catalog)?;
    let mut ddl = Ddl { conn, report: SyncReport::default(), constraints: HashSet::new(), policies: HashSet::new(), columns: HashMap::new() };
    ddl.load_state().await?;

    // Pass 1: tables and columns.
    let managed: Vec<&EffEntity> =
        schema.entities.values().map(|e| e.as_ref()).filter(|e| !e.custom && e.def.kind != EntityKind::Child && e.def.kind != EntityKind::Ledger).collect();
    for ent in &managed {
        if !ddl.columns.contains_key(&ent.table) {
            ddl.exec(format!("create table if not exists {} (\n{SYSTEM_COLUMNS}\n)", q(&ent.table))).await?;
            ddl.columns.insert(ent.table.clone(), HashMap::new());
        }
        ddl.add_columns(&ent.table, ent).await?;
        for (field, child) in &ent.children {
            let Some(cent) = schema.entity(&child.entity) else { continue };
            if !ddl.columns.contains_key(&child.table) {
                ddl.exec(format!(
                    "create table if not exists {ct} (\n  tenant_id uuid not null,\n  id uuid not null,\n  parent_id uuid not null,\n  \
                     idx int not null check (idx >= 0),\n  created_at timestamptz not null default now(),\n  \
                     updated_at timestamptz not null default now(),\n  ext jsonb not null default '{{}}'::jsonb,\n  \
                     primary key (tenant_id, id),\n  foreign key (tenant_id, parent_id) references {pt} (tenant_id, id) on delete cascade\n)",
                    ct = q(&child.table),
                    pt = q(&ent.table)
                ))
                .await?;
                ddl.columns.insert(child.table.clone(), HashMap::new());
                ddl.index(&child.table, &["parent_id", "idx"], false, None).await?;
            }
            ddl.add_columns(&child.table, cent).await?;
            let _ = field;
        }
    }

    // Pass 2: constraints, indexes, RLS (all tables exist now, so FK targets do too).
    for ent in &managed {
        let t = ent.table.clone();
        ddl.links(&t, ent, &schema).await?;
        ddl.index(&t, &["created_at", "id"], false, None).await?;
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
            if f.unique {
                ddl.index(&t, &[&f.name], true, Some(&format!("{} is not null", q(&f.name)))).await?;
            } else if f.index {
                ddl.index(&t, &[&f.name], false, None).await?;
            }
        }
        for group in &ent.def.unique {
            let cols: Vec<&str> = group.iter().map(String::as_str).collect();
            ddl.index(&t, &cols, true, None).await?;
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
            ddl.exec(format!(
                "alter table {} add constraint {} exclude using gist ({}, daterange({}, {}, '[]') with &&)",
                q(&t),
                q(&name),
                eq.join(", "),
                q(&x.from),
                q(&x.to)
            ))
            .await?;
            ddl.constraints.insert(name);
        }
        ddl.search_indexes(&t, ent).await?;
        ddl.rls(&t).await?;
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
    .bind(catalog.fingerprint())
    .execute(&mut *ddl.conn)
    .await?;
    for w in &ddl.report.warnings {
        tracing::warn!("schema sync: {w}");
    }
    Ok(ddl.report)
}
