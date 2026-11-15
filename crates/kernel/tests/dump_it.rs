//! A whole database dumped and restored as the server's own role, no superuser (D7; docs/operations.md, "Backups and
//! restore"): `deploy/dump.sh` against real PostgreSQL, from `FERRUM_TEST_DATABASE_URL` into the empty
//! `FERRUM_RESTORE_TEST_DATABASE_URL` (both reset first, the vault's schema with them, and again at the end), with
//! pg_dump and pg_restore from `PG_BIN` or the PATH. Every row of every table of every schema (the settings' tables,
//! and the vault's: its slots, keys, secrets, counts of use and its record), and every sequence, comes back as it was;
//! the vault's guards come back with it; and the server starts on the copy, holding as after any restore, its vault
//! opening as it did and its secrets reading as they were. Restored where the role has another password, the vault's
//! credential slot does not open and the start is refused, saying so; each way out the guide gives ("The vault",
//! *After a restore, a copy or a move*) makes it good: the old password, the recovery key, a key file or a
//! key-management service.

use ferrum_kernel::outbox;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::vault::{self, Kdf, New, Opened, Opener, Rewrap, Rule, Scope, SlotKind, Vault, Zeroizing};
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

fn url(env: &str, database: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| format!("postgres://ferrum:ferrum@localhost:5432/{database}"))
}

fn catalog() -> Catalog {
    Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog")
}

async fn database(url: &str, reset: bool) -> Db {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 4, ..Default::default() }).await.expect("connect");
    if reset {
        // The vault's schema too: it lives beside public (crates/kernel/src/vault), and a restore goes into an empty database.
        sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;")
            .execute(&db.pool)
            .await
            .expect("reset");
    }
    db
}

/// How a process opens the vault: its credential slot from `address`'s password (never connected to), and the key file
/// and key-management command it has. Argon2id is kept cheap here.
fn opener(address: &str, file: &Path, command: Option<String>) -> Opener {
    Opener { database_url: address.to_string(), file: file.to_path_buf(), transit: None, command, kdf: Kdf { m_kib: 64, t: 1, p: 1 } }
}

/// `address` with another password, as a database's own where its role was given a new one.
fn with_password(address: &str, password: &str) -> String {
    let mut u = reqwest::Url::parse(address).unwrap();
    u.set_password(Some(password)).unwrap();
    u.to_string()
}

/// A server's start on `address`: its vault opened as `opener` has it (or the start refused, saying why), and its kernel
/// booted with an app's handle on the vault.
async fn start(address: &str, opener: Opener) -> Result<(Arc<Kernel>, Vault), String> {
    let db = database(address, false).await;
    vault::open(&db, opener, false).await.map_err(|e| e.to_string())?;
    let mut hooks = HookRegistry::default();
    let mut crm = None;
    hooks.app("crm", &[], |h| crm = Some(h.vault()));
    let k = Kernel::boot(db, catalog(), hooks, KernelSettings::default(), None).await.expect("boot");
    Ok((k, crm.unwrap()))
}

/// The slot the vault of `address` opens with for `opener`, or why it does not, as a start would say it.
async fn opens(address: &str, opener: Opener) -> Result<Opened, String> {
    vault::open(&database(address, false).await, opener, false).await.map_err(|e| e.to_string())
}

/// `deploy/dump.sh` on the database at `args[1]`, by PostgreSQL's own address: through a pooler the script refuses (it
/// needs one session, and the setting it gives pg_dump's), so where suites run through one (scripts/local_ci.sh managed)
/// the direct host stands in.
fn script(args: &[&str]) -> std::process::Output {
    let mut args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    if let Ok(host) = std::env::var("FERRUM_TEST_DATABASE_DIRECT_HOST") {
        args[1] = ferrum_kernel::db::with_host(&args[1], &host).expect("a direct address");
    }
    Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/dump.sh")).args(&args).output().expect("deploy/dump.sh")
}

/// Every table's rows (as the system and the vault see them) and every sequence, by name: a count and a digest that do
/// not depend on the order rows come in.
async fn contents(db: &Db) -> BTreeMap<String, (i64, String)> {
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true), set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    let tables: Vec<String> = sqlx::query_scalar(
        "select format('%I.%I', n.nspname, c.relname) from pg_class c join pg_namespace n on n.oid = c.relnamespace \
         where c.relkind in ('r', 'p') and not c.relispartition and n.nspname not in ('pg_catalog', 'information_schema') \
         and n.nspname !~ '^pg_' and not exists (select 1 from pg_depend d where d.objid = c.oid and d.deptype = 'e') order by 1",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    let mut out = BTreeMap::new();
    for t in tables {
        let row: (i64, Option<String>) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select count(*), md5(string_agg(h, '' order by h)) from (select md5(to_jsonb(x)::text) h from {t} x) s"
        )))
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        out.insert(t, (row.0, row.1.unwrap_or_default()));
    }
    let sequences: Vec<(String, Option<i64>)> =
        sqlx::query_as("select format('%I.%I', schemaname, sequencename), last_value from pg_sequences order by 1").fetch_all(&mut *tx).await.unwrap();
    for (s, last) in sequences {
        out.insert(format!("sequence {s}"), (last.unwrap_or(0), String::new()));
    }
    tx.commit().await.unwrap();
    out
}

fn value(s: &str) -> New {
    New::Value(Zeroizing::new(s.as_bytes().to_vec()))
}

#[test]
fn a_dump_as_the_servers_own_role_holds_everything_and_restores() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let live = url("FERRUM_TEST_DATABASE_URL", "ferrum_test");
    let copy = url("FERRUM_RESTORE_TEST_DATABASE_URL", "ferrum_restore_test");
    let dir = std::env::temp_dir().join(format!("ferrum-dump-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (key_file, no_file) = (dir.join("root.key"), dir.join("absent.key"));
    std::fs::write(&key_file, format!("{}\n", ferrum_kernel::kms::new_key())).unwrap();
    let kms = Some(format!("printf '%s' {}", ferrum_kernel::kms::new_key()));
    let db = database(&live, true).await;
    let (superuser, bypass): (bool, bool) =
        sqlx::query_as("select rolsuper, rolbypassrls from pg_roles where rolname = current_user").fetch_one(&db.pool).await.unwrap();
    assert!(!superuser && !bypass, "the server's own role, as it runs");
    db.close().await;
    // The vault as a server makes it on a new database (here from its key file), with every other kind of slot added as
    // an operator adds them: the database password's, the key-management service's, and a recovery key printed once.
    let (k, crm) = start(&live, opener(&live, &key_file, kms.clone())).await.expect("the first start makes the vault");
    for kind in [SlotKind::Credential, SlotKind::Kms] {
        vault::put_slot(&k.db, kind).await.unwrap();
    }
    let recovery = vault::put_slot(&k.db, SlotKind::Recovery).await.unwrap().expect("a recovery key, printed once");
    // Two workspaces with people, events waiting and done, the audit trail, and an installation's setting.
    let mut acme = None;
    for slug in ["acme", "bravo"] {
        let tenant = k
            .create_tenant(NewTenant {
                slug: slug.into(),
                name: slug.into(),
                admin_email: format!("admin@{slug}.test"),
                admin_password: "Correct-Horse-9".into(),
                admin_name: None,
                packages: vec![],
                settings: None,
            })
            .await
            .unwrap();
        acme.get_or_insert(tenant);
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        for n in 0..3 {
            outbox::enqueue(&mut tx, tenant, "pay", None, None, json!({"n": n})).await.unwrap();
        }
        tx.commit().await.unwrap();
    }
    let acme = Scope::Workspace(acme.unwrap());
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("insert into setting (scope, scope_name, key, value, updated_by) values ('installation', '', 'session_days', '\"7\"', 'it')")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Secrets of a workspace and of the installation, one of them used: their keys, their counts and their record.
    let shown = Rule { reveal: true, ..Default::default() };
    crm.set(&k, acme, "smtp.password", value("p4ss-before-the-dump"), shown.clone()).await.unwrap();
    crm.set(&k, Scope::Installation, "license", value("L-before-the-dump"), shown).await.unwrap();
    crm.reveal(&k, Scope::Installation, "license").await.unwrap();
    let before = contents(&k.db).await;
    assert!(before["public.app_user"].0 >= 2 && before["public.outbox"].0 >= 6 && before["public.setting"].0 == 1, "{before:?}");
    for (table, least) in [("vault.root_slot", 4), ("vault.key", 3), ("vault.secret", 2), ("vault.event", 5)] {
        assert!(before[table].0 >= least, "{table}: {before:?}");
    }

    // Without the system's setting the role's dump would hold no workspace's rows, and say nothing: the script sets it.
    let file = dir.join("ferrum.dump");
    let file = file.to_str().unwrap();
    let done = script(&["dump", &live, file]);
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));

    // A database that is not empty is refused; an empty one the role owns takes it, all of it.
    let target = database(&copy, true).await;
    sqlx::query("create table left_over (x int)").execute(&target.pool).await.unwrap();
    let refused = script(&["restore", &copy, file]);
    assert!(!refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("empty database"), "{refused:?}");
    sqlx::query("drop table left_over").execute(&target.pool).await.unwrap();
    target.close().await;
    let done = script(&["restore", &copy, file]);
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));

    let restored = database(&copy, false).await;
    let after = contents(&restored).await;
    let differ: Vec<&String> = before.keys().chain(after.keys()).filter(|t| before.get(*t) != after.get(*t)).collect();
    assert!(differ.is_empty(), "these came back otherwise: {differ:?}");
    // Still isolated: without the settings the role sees no workspace's rows and none of the vault's.
    let seen: i64 = sqlx::query_scalar("select count(*) from app_user").fetch_one(&restored.pool).await.unwrap();
    assert_eq!(seen, 0, "row-level security came back forced");
    let seen: i64 = sqlx::query_scalar("select count(*) from vault.root_slot").fetch_one(&restored.pool).await.unwrap();
    assert_eq!(seen, 0, "and on the vault's tables, which only the vault's own setting opens");
    // The vault's record is still append-only, even to the vault's own transactions.
    for change in ["update vault.event set action = 'changed'", "delete from vault.event", "truncate vault.event"] {
        let mut tx = restored.pool.begin().await.unwrap();
        sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
        let e = sqlx::raw_sql(change).execute(&mut *tx).await.expect_err(change).to_string();
        assert!(e.contains("append-only"), "{change}: {e}");
        tx.rollback().await.unwrap();
    }
    restored.close().await;

    // The server starts on it as its own role, with its password: the vault opens as it did (the key file and the
    // service are not here), the secrets read as they were, and what reaches outside is held, as after any restore.
    let (k2, crm2) = start(&copy, opener(&copy, &no_file, None)).await.expect("the vault opens on the copy");
    assert_eq!(&**crm2.reveal(&k2, acme, "smtp.password").await.unwrap(), b"p4ss-before-the-dump");
    assert_eq!(&**crm2.reveal(&k2, Scope::Installation, "license").await.unwrap(), b"L-before-the-dump");
    assert!(k2.restore_held().await.unwrap(), "a restored database is held until a person releases it");
    k2.login("acme", "admin@acme.test", "Correct-Horse-9").await.expect("people sign in as before");
    k2.db.close().await;

    // Restored where the role has another password (a new cluster's, an instance made again), the credential slot does
    // not open, and the start is refused, saying what to do. Each way out, as an operator takes it:
    let old = Zeroizing::new(reqwest::Url::parse(&copy).unwrap().password().unwrap_or_default().to_string());
    let new = with_password(&copy, "n3w-pass");
    let why = opens(&copy, opener(&new, &no_file, None)).await.unwrap_err();
    assert!(why.contains("not the one it was made with") && why.contains("vault_rewrap"), "{why}");
    // 1. `ferrum task vault_rewrap` with the new address, given the old password once: the credential slot is made
    // again, and the server starts with everything readable.
    let db = database(&copy, false).await;
    assert!(vault::open(&db, opener(&new, &no_file, None), true).await.is_err());
    vault::rewrap(&db, Rewrap::OldPassword(old.clone())).await.unwrap();
    db.close().await;
    let (k3, crm3) = start(&copy, opener(&new, &no_file, None)).await.expect("the new password opens it");
    assert_eq!(&**crm3.reveal(&k3, acme, "smtp.password").await.unwrap(), b"p4ss-before-the-dump");
    k3.db.close().await;
    assert!(opens(&copy, opener(&copy, &no_file, None)).await.is_err(), "the old one no longer does");
    // 2. `ferrum task vault_rewrap with:recovery`, given the recovery key, when the old password is gone as well.
    let third = with_password(&copy, "th1rd-pass");
    let db = database(&copy, false).await;
    assert!(vault::open(&db, opener(&third, &no_file, None), true).await.is_err());
    vault::rewrap(&db, Rewrap::Recovery(recovery.clone())).await.unwrap();
    db.close().await;
    assert_eq!(opens(&copy, opener(&third, &no_file, None)).await, Ok(Opened::With(SlotKind::Credential)));
    // 3. The key file, mounted where the server runs: it opens the vault whatever the password. The credential slot is
    // then made again for the new password, taken out first: a task never replaces one its own password does not open.
    let fourth = with_password(&copy, "f0urth-pass");
    let db = database(&copy, false).await;
    assert_eq!(vault::open(&db, opener(&fourth, &key_file, None), false).await.unwrap(), Opened::With(SlotKind::File));
    let e = vault::put_slot(&db, SlotKind::Credential).await.unwrap_err().to_string();
    assert!(e.contains("does not open the credential slot"), "{e}");
    vault::remove_slot(&db, SlotKind::Credential).await.unwrap();
    vault::put_slot(&db, SlotKind::Credential).await.unwrap();
    db.close().await;
    assert_eq!(opens(&copy, opener(&fourth, &no_file, None)).await, Ok(Opened::With(SlotKind::Credential)));
    // 4. The key-management service, set where the server runs, opens it the same way.
    assert_eq!(opens(&copy, opener(&with_password(&copy, "f1fth-pass"), &no_file, kms)).await, Ok(Opened::With(SlotKind::Kms)));

    // As found: no vault is left whose slots a later suite could not open.
    k.db.close().await;
    for address in [&live, &copy] {
        database(address, true).await.close().await;
    }
    let _ = std::fs::remove_dir_all(&dir);
}
