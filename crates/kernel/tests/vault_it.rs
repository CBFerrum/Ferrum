//! The vault against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the public schema and the vault's are reset first,
//! and the vault's dropped again at the end): its slots open it, and each way opening can fail is refused with its
//! reason; replicas starting together make one installation key; nothing that does not go through the vault's module
//! sees a row of it; and the vault's settings never outlive their transaction. Its keys are made on first use, once,
//! by replicas at once; each owner has its own, a handle another registry gave out opens nothing, and a key's row
//! copied to another place does not open. Secrets keep to their rules: a value is revealed only where its rule says,
//! signatures verify through a rotation's overlap, every use is on the audit trail under its cap, and people change
//! them only as administrators who have just confirmed it is them. Transit's part runs when `FERRUM_KMS_TEST_TRANSIT`
//! is set (OpenBao, as kms_it.rs).

use ferrum_kernel::controls::Proof;
use ferrum_kernel::vault::{self, Kdf, New, Opened, Opener, Rule, Scope, SlotKind, Vault};
use ferrum_kernel::{Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;
use zeroize::Zeroizing;

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

/// A replica: a pool of its own, and its own hold on the vault.
async fn replica() -> Db {
    Db::connect(DbConfig { url: url(), max_connections: 4, ..Default::default() }).await.expect("connect")
}

/// What a process with this password, key file and key command opens with (the address is never connected to:
/// only its password counts). Argon2id's costs are kept low for the tests.
fn opener(password: Option<&str>, file: &Path, command: Option<String>) -> Opener {
    Opener {
        database_url: match password {
            Some(p) => format!("postgres://ferrum:{p}@localhost:5432/ferrum"),
            None => "postgres://ferrum@localhost:5432/ferrum".into(),
        },
        file: file.to_path_buf(),
        transit: None,
        command,
        kdf: Kdf { m_kib: 64, t: 1, p: 1 },
    }
}

async fn reset(db: &Db) {
    sqlx::raw_sql("drop schema if exists vault cascade; drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    db.migrate().await.expect("migrate");
}

/// Runs `sql` as the vault's module would (the setting only it sets), to look at or damage its rows.
async fn as_vault(db: &Db, sql: &str) -> i64 {
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

fn refusal(r: ferrum_kernel::Result<Opened>) -> String {
    match r {
        Err(Error::Unavailable(why)) => why,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn key_file(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("{}\n", ferrum_kernel::kms::new_key())).unwrap();
    path
}

#[test]
fn the_vault_opens_with_its_slots_and_says_why_it_does_not() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let dir = std::env::temp_dir().join(format!("ferrum-vault-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let no_file = dir.join("absent.key");
    let db = replica().await;

    // ── a fresh database with nothing to open it with: refused, saying so ──
    reset(&db).await;
    let why = refusal(vault::open(&db, opener(None, &no_file, None), false).await);
    assert!(why.contains("nothing to open it with") && why.contains("DATABASE_URL has no password") && why.contains("absent.key"), "{why}");
    assert_eq!(as_vault(&db, "select count(*) from vault.root_slot").await, 0, "and nothing was made");

    // ── two replicas starting together on a fresh database make one installation key ──
    let (a, b) = (replica().await, replica().await);
    let both = tokio::join!(vault::open(&a, opener(Some("ferrum"), &no_file, None), false), vault::open(&b, opener(Some("ferrum"), &no_file, None), false));
    assert_eq!((both.0.unwrap(), both.1.unwrap()), (Opened::With(SlotKind::Credential), Opened::With(SlotKind::Credential)));
    assert_eq!(as_vault(&db, "select count(*) from vault.root_slot").await, 1, "one slot, one installation key");
    assert_eq!(as_vault(&db, "select count(*) from vault.root_slot where kind = 'credential' and installation_version = 1").await, 1);

    // ── the credential slot: the password opens it; another password, none, or a damaged row are refused, each saying why ──
    assert_eq!(vault::open(&replica().await, opener(Some("ferrum"), &no_file, None), false).await.unwrap(), Opened::With(SlotKind::Credential));
    let why = refusal(vault::open(&replica().await, opener(Some("changed"), &no_file, None), false).await);
    assert!(why.contains("the credential slot") && why.contains("not the one it was made with") && why.contains("vault_rewrap"), "{why}");
    let why = refusal(vault::open(&replica().await, opener(None, &no_file, None), false).await);
    assert!(why.contains("DATABASE_URL has none"), "{why}");
    let wrapped: Vec<u8> = {
        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
        let w = sqlx::query_scalar("select wrapped from vault.root_slot where kind = 'credential'").fetch_one(&mut *tx).await.unwrap();
        // Ended here: a transaction only dropped stays open, with its locks, until its connection is used again.
        tx.commit().await.unwrap();
        w
    };
    as_vault(&db, "with u as (update vault.root_slot set wrapped = set_byte(wrapped, 20, get_byte(wrapped, 20) # 1) where kind = 'credential' returning 1) select count(*) from u").await;
    let why = refusal(vault::open(&replica().await, opener(Some("ferrum"), &no_file, None), false).await);
    assert!(why.contains("damaged") && why.contains("restore"), "a damaged row is told from a wrong password: {why}");
    {
        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
        sqlx::query("update vault.root_slot set wrapped = $1 where kind = 'credential'").bind(&wrapped).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    }

    // ── the file slot, added to the open vault: tried first; a file gone or replaced is said, and the next slot opens ──
    let file = key_file(&dir, "root.key");
    let open_db = replica().await;
    vault::open(&open_db, opener(Some("ferrum"), &file, None), false).await.unwrap();
    assert_eq!(vault::put_slot(&open_db, SlotKind::File).await.unwrap().map(|p| p.to_string()), None);
    assert_eq!(vault::open(&replica().await, opener(Some("ferrum"), &file, None), false).await.unwrap(), Opened::With(SlotKind::File), "a file first");
    assert_eq!(vault::open(&replica().await, opener(Some("ferrum"), &no_file, None), false).await.unwrap(), Opened::With(SlotKind::Credential));
    let other = key_file(&dir, "other.key");
    let why = refusal(vault::open(&replica().await, opener(Some("changed"), &other, None), false).await);
    assert!(
        why.contains("the file slot: the key in") && why.contains("other.key is not the one it was made with") && why.contains("the credential slot"),
        "{why}"
    );
    let why = refusal(vault::open(&replica().await, opener(None, &no_file, None), false).await);
    assert!(why.contains("there is no key file at") && why.contains("DATABASE_URL has none"), "{why}");

    // ── the kms slot through a command: its key opens it; no command, or one that prints another key, is refused ──
    let kms_key = ferrum_kernel::kms::new_key();
    let printing = |key: &str| Some(format!("printf '%s' {key}"));
    let open_db = replica().await;
    vault::open(&open_db, opener(Some("ferrum"), &no_file, printing(&kms_key)), false).await.unwrap();
    vault::put_slot(&open_db, SlotKind::Kms).await.unwrap();
    assert_eq!(vault::open(&replica().await, opener(None, &no_file, printing(&kms_key)), false).await.unwrap(), Opened::With(SlotKind::Kms));
    assert_eq!(
        vault::open(&replica().await, opener(Some("ferrum"), &file, printing(&kms_key)), false).await.unwrap(),
        Opened::With(SlotKind::File),
        "file, then kms"
    );
    let why = refusal(vault::open(&replica().await, opener(None, &no_file, printing(&ferrum_kernel::kms::new_key())), false).await);
    assert!(why.contains("the kms slot: the key-management service gave back another key"), "{why}");
    let why = refusal(vault::open(&replica().await, opener(None, &no_file, None), false).await);
    assert!(why.contains("FERRUM_SECRET_KEY_COMMAND, which is not set here"), "{why}");
    let why = refusal(vault::open(&replica().await, opener(None, &no_file, Some("exit 7".into())), false).await);
    assert!(why.contains("the kms slot: FERRUM_SECRET_KEY_COMMAND failed"), "a command that fails is said: {why}");

    // ── the recovery slot: printed once, never tried at start, and named when nothing else opens ──
    let printed = vault::put_slot(&open_db, SlotKind::Recovery).await.unwrap().expect("a recovery key to print");
    assert_eq!(printed.len(), 64, "{} characters", printed.len());
    let why = refusal(vault::open(&replica().await, opener(None, &no_file, None), false).await);
    assert!(why.contains("recovery slot opens it only with the recovery key"), "{why}");
    assert!(!why.contains(printed.as_str()));

    // ── nothing that does not go through the vault's module sees its rows: not the system, not a workspace, not a bare query ──
    assert_eq!(as_vault(&db, "select count(*) from vault.root_slot").await, 4);
    let mut tx = db.system_tx().await.unwrap();
    let seen: i64 = sqlx::query_scalar("select count(*) from vault.root_slot").fetch_one(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();
    let mut tx = db.tenant_tx(Uuid::now_v7()).await.unwrap();
    let seen_by_workspace: i64 = sqlx::query_scalar("select count(*) from vault.root_slot").fetch_one(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();
    let bare: i64 = sqlx::query_scalar("select count(*) from vault.root_slot").fetch_one(&db.pool).await.unwrap();
    assert_eq!((seen, seen_by_workspace, bare), (0, 0, 0));
    let mut tx = db.system_tx().await.unwrap();
    let written =
        sqlx::query("insert into vault.root_slot (kind, installation_version, wrapped, check_value) values ('file', 9, '', '')").execute(&mut *tx).await;
    assert!(written.is_err(), "nor writes one");
    tx.rollback().await.unwrap();

    // ── the vault's setting lasts as long as its transaction: no connection keeps it (a pooler in transaction mode) ──
    for _ in 0..12 {
        let left: Option<String> = sqlx::query_scalar("select current_setting('ferrum.vault', true)").fetch_one(&open_db.side).await.unwrap();
        assert!(left.unwrap_or_default().is_empty(), "a connection the vault used kept its setting");
    }

    // ── a database not migrated yet: the vault opens once the kernel has migrated it ──
    let fresh = replica().await;
    sqlx::raw_sql("drop schema if exists vault cascade; drop schema if exists public cascade; create schema public;").execute(&fresh.pool).await.unwrap();
    assert_eq!(vault::open(&fresh, opener(Some("ferrum"), &no_file, None), false).await.unwrap(), Opened::Later);
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let k = Kernel::boot(fresh, Catalog::from_packages(vec![core]).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    assert!(vault::put_slot(&k.db, SlotKind::Recovery).await.unwrap().is_some(), "open once booted");
    assert_eq!(as_vault(&db, "select count(*) from vault.root_slot").await, 2, "made then: credential and recovery");

    keys_and_handles(&no_file).await;
    secrets_and_people(&no_file).await;

    if let Ok(base) = std::env::var("FERRUM_KMS_TEST_TRANSIT") {
        transit(&base, &no_file).await;
    } else {
        assert!(std::env::var("CI").is_err(), "CI runs this against OpenBao: set FERRUM_KMS_TEST_TRANSIT");
        eprintln!("Transit's part skipped: FERRUM_KMS_TEST_TRANSIT is not set");
    }

    sqlx::raw_sql("drop schema if exists vault cascade").execute(&db.pool).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The kms slot through Transit: a new vault's best slot when nothing else is set, opened back with the service; the
/// service's key unknown, or the service away, is said.
async fn transit(base: &str, no_file: &Path) {
    let base = base.trim_end_matches('/').to_string();
    let root = std::env::var("FERRUM_KMS_TEST_TOKEN").unwrap_or_else(|_| "ferrum-dev-root".into());
    let mount = format!("ferrum-vault-it-{}", std::process::id());
    let admin = |path: String, body: serde_json::Value| {
        let (base, root) = (base.clone(), root.clone());
        async move { reqwest::Client::new().post(format!("{base}/v1/{path}")).header("x-vault-token", root).json(&body).send().await.unwrap().status() }
    };
    assert!(admin(format!("sys/mounts/{mount}"), serde_json::json!({"type": "transit"})).await.is_success());
    assert!(admin(format!("{mount}/keys/root"), serde_json::json!({})).await.is_success());
    let token = root.clone();
    let env = move |name: &str| (name == "BAO_TOKEN").then(|| token.clone());
    let with = |key: &str| Opener {
        transit: Some(ferrum_kernel::kms::Transit::from_env(&format!("{base}/{mount}/{key}"), env.clone()).unwrap()),
        ..opener(None, no_file, None)
    };
    let db = replica().await;
    reset(&db).await;
    assert_eq!(vault::open(&db, with("root"), false).await.unwrap(), Opened::With(SlotKind::Kms), "Transit, as nothing else is set");
    assert_eq!(vault::open(&replica().await, with("root"), false).await.unwrap(), Opened::With(SlotKind::Kms));
    let why = refusal(vault::open(&replica().await, with("missing"), false).await);
    assert!(why.contains("the kms slot"), "{why}");
    let away =
        Opener { transit: Some(ferrum_kernel::kms::Transit::from_env("http://127.0.0.1:9/transit/root", env.clone()).unwrap()), ..opener(None, no_file, None) };
    let why = refusal(vault::open(&replica().await, away, false).await);
    assert!(why.contains("cannot be reached"), "{why}");
    let _ = reqwest::Client::new().delete(format!("{base}/v1/sys/mounts/{mount}")).header("x-vault-token", root).send().await;
}

fn core() -> Catalog {
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    Catalog::from_packages(vec![core]).unwrap()
}

/// A replica's kernel, with apps `crm` and `hr` registered and their handles, its vault opened with the password.
async fn kernel_with_apps(no_file: &Path) -> (Arc<Kernel>, Vault, Vault) {
    let db = replica().await;
    vault::open(&db, opener(Some("ferrum"), no_file, None), false).await.unwrap();
    let mut hooks = HookRegistry::default();
    let (mut crm, mut hr) = (None, None);
    hooks.app("crm", &[], |h| crm = Some(h.vault()));
    hooks.app("hr", &[], |h| hr = Some(h.vault()));
    let k = Kernel::boot(db, core(), hooks, KernelSettings::default(), None).await.expect("boot");
    (k, crm.unwrap(), hr.unwrap())
}

async fn workspace(k: &Kernel, slug: &str) -> Uuid {
    k.create_tenant(ferrum_kernel::tenant::NewTenant {
        slug: slug.into(),
        name: slug.into(),
        admin_email: format!("admin@{slug}.test"),
        admin_password: "Correct-Horse-9".into(),
        admin_name: None,
        packages: vec![],
        settings: None,
    })
    .await
    .unwrap()
}

/// Copies one key row's wrapped key over another's, as someone with the database could.
async fn copy_wrapped(db: &Db, from: (Uuid, &str), to: (Uuid, &str)) -> Vec<u8> {
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    let was: Vec<u8> = sqlx::query_scalar("select wrapped from vault.key where scope = $1 and owner = $2 and version = 1")
        .bind(to.0)
        .bind(to.1)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    sqlx::query("update vault.key set wrapped = (select wrapped from vault.key where scope = $1 and owner = $2 and version = 1) where scope = $3 and owner = $4 and version = 1")
        .bind(from.0)
        .bind(from.1)
        .bind(to.0)
        .bind(to.1)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    was
}

async fn put_wrapped(db: &Db, at: (Uuid, &str), wrapped: &[u8]) {
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    sqlx::query("update vault.key set wrapped = $3 where scope = $1 and owner = $2 and version = 1")
        .bind(at.0)
        .bind(at.1)
        .bind(wrapped)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn keys_and_handles(no_file: &Path) {
    let db = replica().await;
    reset(&db).await;
    let (a, crm_a, hr_a) = kernel_with_apps(no_file).await;
    let (b, crm_b, _) = kernel_with_apps(no_file).await;
    let (w, w2) = (workspace(&a, "acme").await, workspace(&a, "globex").await);
    let ws = Scope::Workspace(w);
    assert!(matches!(crm_a.derive(&a, Scope::Workspace(Uuid::now_v7()), "links").await, Err(Error::NotFound(_))), "no keys for a workspace that is not there");

    // ── a workspace's keys are made on first use, once, by replicas asking at the same moment ──
    let (x, y) = tokio::join!(crm_a.derive(&a, ws, "links"), crm_b.derive(&b, ws, "links"));
    let (x, y) = (x.unwrap(), y.unwrap());
    assert_eq!((x.bytes(), x.version()), (y.bytes(), 1), "one owner key, whichever replica made it");
    assert_eq!(as_vault(&db, &format!("select count(*) from vault.key where scope = '{w}'")).await, 2, "a workspace key and crm's key");
    let at_once = futures::future::join_all((0..8).map(|i| {
        let (k, v) = if i % 2 == 0 { (&a, &crm_a) } else { (&b, &crm_b) };
        v.derive(k, Scope::Workspace(w2), "links")
    }))
    .await;
    let first = at_once[0].as_ref().unwrap().bytes().to_vec();
    assert!(at_once.iter().all(|d| d.as_ref().unwrap().bytes() == first.as_slice()));
    assert_eq!(as_vault(&db, &format!("select count(*) from vault.key where scope = '{w2}'")).await, 2);

    // ── each owner, scope and purpose its own key; the installation's own scope beside the workspaces' ──
    let hr_x = hr_a.derive(&a, ws, "links").await.unwrap();
    let other = crm_a.derive(&a, ws, "other").await.unwrap();
    let inst = crm_a.derive(&a, Scope::Installation, "links").await.unwrap();
    assert!(hr_x.bytes() != x.bytes() && other.bytes() != x.bytes() && inst.bytes() != x.bytes() && first.as_slice() != x.bytes());
    assert_eq!(as_vault(&db, "select count(*) from vault.key where scope = '00000000-0000-0000-0000-000000000000'").await, 2);
    assert_eq!(crm_a.derive_at(&a, ws, "links", 1).await.unwrap().bytes(), x.bytes());
    assert!(matches!(crm_a.derive_at(&a, ws, "links", 2).await, Err(Error::NotFound(_))), "a version that is not there");
    assert_eq!(format!("{x:?}"), "Derived(version 1)", "a derived key never shows itself");

    // ── sealed data opens for its owner, scope and context, from any replica, and nowhere else ──
    let sealed = crm_a.seal(&a, ws, "deal/1", b"the deal's terms").await.unwrap();
    assert!(sealed.starts_with("fv1.1.") && !sealed.contains("terms"), "{sealed}");
    assert_eq!(&**crm_b.open(&b, ws, "deal/1", &sealed).await.unwrap(), b"the deal's terms");
    assert!(hr_a.open(&a, ws, "deal/1", &sealed).await.is_err(), "another owner");
    assert!(crm_a.open(&a, Scope::Workspace(w2), "deal/1", &sealed).await.is_err(), "another workspace");
    assert!(crm_a.open(&a, ws, "deal/2", &sealed).await.is_err(), "another context");
    assert!(crm_a.open(&a, ws, "deal/1", &sealed.replace("fv1.1.", "fv1.2.")).await.is_err(), "another version");
    let mut changed = sealed.clone().into_bytes();
    let last = changed.len() - 3;
    changed[last] = if changed[last] == b'A' { b'B' } else { b'A' };
    assert!(crm_a.open(&a, ws, "deal/1", &String::from_utf8(changed).unwrap()).await.is_err(), "changed");
    assert_eq!(
        as_vault(&db, &format!("select count(*) from vault.key where scope = '{w}' and owner = 'crm' and outside")).await,
        1,
        "kept, as it sealed what is kept outside"
    );

    // ── a handle is good with the kernel whose registry gave it out, and no other ──
    let mut stray = HookRegistry::default();
    let mut fake = None;
    stray.app("crm", &[], |h| fake = Some(h.vault()));
    assert!(matches!(fake.unwrap().derive(&a, ws, "links").await, Err(Error::Forbidden(_))), "crm's name, another registry's handle");
    assert!(matches!(crm_a.derive(&b, ws, "links").await, Err(Error::Forbidden(_))), "another replica's handle");
    let outside = HookRegistry::default().vault();
    assert!(matches!(outside.derive(&a, ws, "links").await, Err(Error::Forbidden(_))), "a handle asked for outside a registration");

    // ── a key's row copied to another owner or another workspace does not open there (a replica with nothing cached) ──
    let was = copy_wrapped(&db, (w, "crm"), (w, "hr")).await;
    let (c, _, hr_c) = kernel_with_apps(no_file).await;
    let e = hr_c.derive(&c, ws, "links").await.unwrap_err();
    assert!(e.to_string().contains("hr's key") && e.to_string().contains("does not open"), "{e}");
    put_wrapped(&db, (w, "hr"), &was).await;
    let was = copy_wrapped(&db, (w, ""), (w2, "")).await;
    let (c, crm_c, _) = kernel_with_apps(no_file).await;
    let e = crm_c.derive(&c, Scope::Workspace(w2), "links").await.unwrap_err();
    assert!(e.to_string().contains("workspace key") && e.to_string().contains("does not open"), "{e}");
    put_wrapped(&db, (w2, ""), &was).await;
    let (c, crm_c, hr_c) = kernel_with_apps(no_file).await;
    assert_eq!(crm_c.derive(&c, Scope::Workspace(w2), "links").await.unwrap().bytes(), first.as_slice(), "put back, they open again");
    assert_eq!(hr_c.derive(&c, ws, "links").await.unwrap().bytes(), hr_x.bytes());
}

fn value(s: &str) -> New {
    New::Value(Zeroizing::new(s.as_bytes().to_vec()))
}

/// The workspace's vault events, as its trail holds them: (action, what it says).
async fn trail(k: &Kernel, tenant: Uuid) -> Vec<(String, Value)> {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let rows: Vec<(String, Value)> = sqlx::query_as("select action, diff from audit_event where tenant_id = $1 and entity = 'Vault' order by id")
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    rows
}

async fn secrets_and_people(no_file: &Path) {
    let db = replica().await;
    reset(&db).await;
    let (k, crm, hr) = kernel_with_apps(no_file).await;
    let w = workspace(&k, "initech").await;
    let ws = Scope::Workspace(w);

    // ── a value is revealed only where its rule says so; an owner sees only its own ──
    let d = crm.set(&k, ws, "stripe.key", value("sk_live_51Hx"), Rule::default()).await.unwrap();
    assert_eq!((d.version, d.owner.as_str(), d.workspace), (1, "crm", Some(w)));
    assert!(!serde_json::to_string(&d).unwrap().contains("sk_live"), "a description holds no value");
    let e = crm.reveal(&k, ws, "stripe.key").await.unwrap_err();
    assert!(matches!(&e, Error::Forbidden(m) if m.contains("used, never revealed")), "{e}");
    crm.set(&k, ws, "smtp.password", value("p4ss-w0rd"), Rule { reveal: true, ..Default::default() }).await.unwrap();
    assert_eq!(&**crm.reveal(&k, ws, "smtp.password").await.unwrap(), b"p4ss-w0rd");
    assert!(matches!(hr.reveal(&k, ws, "smtp.password").await, Err(Error::NotFound(_))), "hr has none of crm's");
    assert!(matches!(hr.sign(&k, ws, "stripe.key", b"m").await, Err(Error::NotFound(_))));
    assert!(hr.list(&k, ws).await.unwrap().is_empty() && crm.list(&k, ws).await.unwrap().len() == 2);
    assert!(hr.describe(&k, ws, "smtp.password").await.unwrap().is_none());
    assert!(matches!(crm.reveal(&k, Scope::Workspace(Uuid::now_v7()), "smtp.password").await, Err(Error::NotFound(_))), "nor another workspace's");

    // ── HMAC: a signature verifies through a rotation's overlap, and not past it; a rotation without one ends it ──
    crm.set(&k, ws, "hooks", New::Random(32), Rule { overlap_secs: 3600, ..Default::default() }).await.unwrap();
    let first = crm.sign(&k, ws, "hooks", b"payload").await.unwrap();
    assert!(crm.verify(&k, ws, "hooks", b"payload", &first).await.unwrap());
    assert!(!crm.verify(&k, ws, "hooks", b"other", &first).await.unwrap());
    let d = crm.rotate(&k, ws, "hooks", New::Random(32)).await.unwrap();
    assert!(d.version == 2 && d.previous_until.is_some(), "{d:?}");
    let second = crm.sign(&k, ws, "hooks", b"payload").await.unwrap();
    assert_ne!(first, second);
    assert!(crm.verify(&k, ws, "hooks", b"payload", &first).await.unwrap(), "the version before, within the overlap");
    assert!(crm.verify(&k, ws, "hooks", b"payload", &second).await.unwrap());
    as_vault(&db, &format!("with u as (update vault.secret set valid_until = now() - interval '1 second' where scope = '{w}' and name = 'hooks' and version = 1 returning 1) select count(*) from u")).await;
    assert!(!crm.verify(&k, ws, "hooks", b"payload", &first).await.unwrap(), "past the overlap");
    crm.set(&k, ws, "plain", New::Random(32), Rule::default()).await.unwrap();
    let before = crm.sign(&k, ws, "plain", b"x").await.unwrap();
    crm.rotate(&k, ws, "plain", New::Random(32)).await.unwrap();
    assert!(!crm.verify(&k, ws, "plain", b"x", &before).await.unwrap(), "no overlap: the version before is gone");
    assert!(matches!(crm.rotate(&k, ws, "missing", New::Random(32)).await, Err(Error::NotFound(_))));

    // ── Ed25519: the vault makes the pair, signs, and anyone checks with the public half ──
    let d = crm.set(&k, ws, "signing", New::Ed25519, Rule::default()).await.unwrap();
    let public = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, d.public_key.unwrap()).unwrap();
    let sig = crm.sign(&k, ws, "signing", b"document").await.unwrap();
    assert!(ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &public).verify(b"document", &sig).is_ok());
    assert!(crm.verify(&k, ws, "signing", b"document", &sig).await.unwrap() && !crm.verify(&k, ws, "signing", b"other", &sig).await.unwrap());
    assert!(matches!(crm.rotate(&k, ws, "signing", value("not a key pair")).await, Err(Error::Invalid(_))), "another kind under its name");
    assert!(matches!(crm.reveal(&k, ws, "signing").await, Err(Error::Forbidden(_))), "its private half stays");

    // ── an expired secret is used no more; a removed one is gone ──
    as_vault(&db, &format!("with u as (update vault.secret set rule = rule || '{{\"expires_at\": \"2020-01-01T00:00:00Z\"}}' where scope = '{w}' and name = 'smtp.password' returning 1) select count(*) from u")).await;
    assert!(matches!(crm.reveal(&k, ws, "smtp.password").await, Err(Error::Forbidden(m)) if m.contains("expired on 2020-01-01")));
    assert!(crm.remove(&k, ws, "smtp.password").await.unwrap() && !crm.remove(&k, ws, "smtp.password").await.unwrap());
    assert!(matches!(crm.reveal(&k, ws, "smtp.password").await, Err(Error::NotFound(_))));

    // ── every use is on the trail, never a value, under the cap: past it counted, and summed up once the hour is over ──
    crm.set(&k, ws, "api", value("Zx-2468-secret"), Rule { reveal: true, ..Default::default() }).await.unwrap();
    for _ in 0..vault::USES_PER_HOUR + 5 {
        crm.reveal(&k, ws, "api").await.unwrap();
    }
    let events = trail(&k, w).await;
    let reveals = events.iter().filter(|(a, d)| a == "reveal" && d["name"] == "api").count();
    assert_eq!(reveals, vault::USES_PER_HOUR as usize, "the first of the hour, one by one");
    assert!(!format!("{events:?}").contains("Zx-2468") && !format!("{events:?}").contains("sk_live"), "never a value");
    assert!(events.iter().any(|(a, d)| a == "set" && d == &json!({"owner": "crm", "name": "api", "version": 1})));
    assert!(events.iter().any(|(a, d)| a == "rotate" && d["name"] == "hooks") && events.iter().any(|(a, d)| a == "remove" && d["name"] == "smtp.password"));
    assert!(events.iter().any(|(a, d)| a == "sign" && d["name"] == "signing"));
    as_vault(&db, "with u as (update vault.use_count set hour = hour - interval '1 hour' returning 1) select count(*) from u").await;
    crm.reveal(&k, ws, "api").await.unwrap();
    let events = trail(&k, w).await;
    let summary = events.iter().find(|(a, d)| a == "reveal_summary" && d["name"] == "api").expect("a summary of the hour");
    assert_eq!(summary.1["count"], json!(5));
    assert_eq!(crm.describe(&k, ws, "api").await.unwrap().unwrap().last_used, Some(chrono::Utc::now().date_naive()), "kept coarsely");
    crm.set(&k, Scope::Installation, "license", value("L-1"), Rule::default()).await.unwrap();
    let first_trail = trail(&k, w).await;
    assert!(first_trail.iter().any(|(a, d)| a == "set" && d["scope"] == json!("installation")), "the installation's, in the first workspace's trail");

    // ── people: an administrator who has just confirmed it is them; a value the vault makes is shown once ──
    let session = k.login("initech", "admin@initech.test", "Correct-Horse-9").await.unwrap();
    // As a request from a person's browser session makes it: a staff member, not yet confirmed lately.
    let admin = Ctx { proof: Proof::Person { at: None, staff: true }, ..k.load_ctx(w, session.user, None, "it".into()).await.unwrap() };
    let e = k.vault_set(&admin, "crm", "partner.token", New::Random(32), Rule::default()).await.unwrap_err();
    assert_eq!(e.code(), "step_up", "not confirmed lately: {e}");
    let proved = admin.clone().proved(Some(chrono::Utc::now()));
    let changed = k.vault_set(&proved, "crm", "partner.token", New::Random(32), Rule { reveal: true, ..Default::default() }).await.unwrap();
    let shown = changed.shown.as_ref().expect("shown once").to_string();
    assert_eq!(shown.len(), 43);
    assert!(!format!("{changed:?}").contains(&shown), "what is shown never shows in a description of it");
    assert_eq!(&**crm.reveal(&k, ws, "partner.token").await.unwrap(), shown.as_bytes(), "the value shown is the one kept");
    assert!(k.vault_rotate(&proved, "crm", "partner.token", New::Random(32)).await.unwrap().shown.is_some());
    assert!(k.vault_set(&proved, "platform", "smtp.password", value("relay-pw"), Rule::default()).await.is_ok());
    assert!(k.vault_set(&proved, "plugin:mail", "token", value("t"), Rule::default()).await.is_ok());
    assert!(matches!(k.vault_set(&proved, "nobody", "x", value("y"), Rule::default()).await, Err(Error::NotFound(_))), "an owner this server has not");
    let listed = k.vault_secrets(&admin).await.unwrap();
    assert!(listed.iter().any(|d| d.owner == "platform" && d.name == "smtp.password") && listed.iter().any(|d| d.owner == "crm" && d.name == "partner.token"));
    assert!(!serde_json::to_string(&listed).unwrap().contains(&shown) && !serde_json::to_string(&listed).unwrap().contains("relay-pw"));
    let set_by_person = trail(&k, w).await.into_iter().filter(|(a, d)| a == "set" && d["name"] == "partner.token").count();
    assert_eq!(set_by_person, 1);
    for (who, why) in [
        (Ctx { is_admin: false, ..proved.clone() }, "not an administrator"),
        (Ctx { outside: true, ..proved.clone() }, "from outside"),
        (Ctx { proof: Proof::Machine, ..proved.clone() }, "a key, which cannot confirm it is anyone"),
        (Ctx { read_only: true, ..proved.clone() }, "a restricted workspace"),
    ] {
        assert!(matches!(k.vault_set(&who, "crm", "x", value("y"), Rule::default()).await, Err(Error::Forbidden(_))), "{why}");
        assert!(matches!(k.vault_remove(&who, "crm", "partner.token").await, Err(Error::Forbidden(_))), "{why}");
    }
    assert!(matches!(k.vault_secrets(&Ctx { is_admin: false, ..admin.clone() }).await, Err(Error::Forbidden(_))));
    assert!(k.vault_remove(&proved, "crm", "partner.token").await.unwrap());
    assert!(trail(&k, w).await.iter().any(|(a, d)| a == "remove" && d["name"] == "partner.token"));
}
