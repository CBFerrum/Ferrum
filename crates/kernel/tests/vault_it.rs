//! The vault against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the public schema and the vault's are reset first,
//! and again at the end): its slots open it, and each way opening can fail is refused with its
//! reason; replicas starting together make one installation key; nothing that does not go through the vault's module
//! sees a row of it; and the vault's settings never outlive their transaction. Its keys are made on first use, once,
//! by replicas at once; each owner has its own, a handle another registry gave out opens nothing, and a key's row
//! copied to another place does not open. Secrets keep to their rules: a value is revealed only where its rule says,
//! signatures verify through a rotation's overlap, every use is on the audit trail under its cap, and people change
//! them only as administrators who have just confirmed it is them. Every level rotates while two replicas keep using
//! the secrets, a background re-seal stopped half way is taken up again by the other, and a changed database password is
//! made good with the old one or the recovery key. A secret's row moved anywhere else does not open; and every table of
//! the database, searched for every value and every key (each made again from the rows and the key file, as someone
//! with a dump and that file could), holds none of them. Transit's part runs when `FERRUM_KMS_TEST_TRANSIT` is set
//! (OpenBao, as kms_it.rs).

use ferrum_kernel::controls::Proof;
use ferrum_kernel::vault::{self, Kdf, New, Opened, Opener, RecoveryKey, Rewrap, Rule, Scope, SlotKind, Vault};
use ferrum_kernel::{Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
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

    // ── a fresh database with nothing here to make its first slot: a server starts with the vault closed, saying so ──
    reset(&db).await;
    let closed = replica().await;
    assert_eq!(vault::open(&closed, opener(None, &no_file, None), false).await.unwrap(), Opened::Closed);
    let e = vault::put_slot(&closed, SlotKind::Recovery).await.unwrap_err();
    assert!(e.to_string().contains("the vault is closed: nothing here can make its first slot") && e.to_string().contains("absent.key"), "{e}");
    assert_eq!(as_vault(&db, "select count(*) from vault.root_slot").await, 0, "and nothing was made");
    let mut hooks = HookRegistry::default();
    let mut crm = None;
    hooks.app("crm", &[], |h| crm = Some(h.vault()));
    let k = Kernel::boot(closed, core(), hooks, KernelSettings::default(), None).await.expect("it starts all the same");
    let crm = crm.unwrap();
    let w = workspace(&k, "closed").await;
    for e in [
        crm.derive(&k, Scope::Workspace(w), "links").await.map(|_| ()).unwrap_err(),
        crm.describe(&k, Scope::Workspace(w), "x").await.map(|_| ()).unwrap_err(),
        crm.set(&k, Scope::Workspace(w), "x", value("y"), Rule::default()).await.map(|_| ()).unwrap_err(),
        k.rotate_owner_key(Scope::Workspace(w), "crm").await.map(|_| ()).unwrap_err(),
    ] {
        assert!(e.to_string().starts_with("the vault is closed: nothing here can make its first slot"), "every use says so: {e}");
    }
    // Erasing someone who opted out, with neither the vault nor the old key: refused, saying what to do; nothing is erased.
    let session = k.login("closed", "admin@closed.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(w, session.user, None, "it".into()).await.unwrap();
    k.set_message_opt_out(&admin, "zoe@example.com", "email", true).await.unwrap();
    let zoe = ferrum_kernel::privacy::Person { email: "zoe@example.com".into(), ..Default::default() };
    let e = k.erase_personal_data_of(&admin, &zoe, "request 1").await.unwrap_err();
    let said = e.to_string();
    assert!(matches!(e, Error::Unavailable(_)), "{said}");
    assert!(
        said.starts_with("erasing someone who opted out needs the vault, and the vault is closed: nothing here can make its first slot")
            && said.contains("Give it one of these, and the next start makes it"),
        "what to do, in plain words: {said}"
    );
    let kept = k.message_opt_outs(&admin, None, None, 10, 0).await.unwrap();
    assert_eq!(kept["opt_outs"][0]["address"], json!("zoe@example.com"), "nothing erased: {kept}");
    let found = k.vault_findings().await.unwrap();
    assert!(
        found.len() == 1 && found[0].level == ferrum_kernel::doctor::Level::Warning && found[0].detail.contains("first slot"),
        "a finding, not a blocker: {found:?}"
    );
    // A later start with a key file makes it, once, though replicas start together; so does one with a key-management
    // service, or with the database password (below).
    let first = key_file(&dir, "first.key");
    let (a, b) = (replica().await, replica().await);
    let both = tokio::join!(vault::open(&a, opener(None, &first, None), false), vault::open(&b, opener(None, &first, None), false));
    assert_eq!((both.0.unwrap(), both.1.unwrap()), (Opened::With(SlotKind::File), Opened::With(SlotKind::File)));
    assert_eq!(as_vault(&db, "select count(*) from vault.root_slot").await, 1, "one slot, one installation key");
    let why = refusal(vault::open(&replica().await, opener(None, &no_file, None), false).await);
    assert!(why.contains("no slot opens the vault"), "once made, a start whose slot does not open here is refused: {why}");
    reset(&db).await;
    let printing_first = Some(format!("printf '%s' {}", ferrum_kernel::kms::new_key()));
    assert_eq!(vault::open(&replica().await, opener(None, &no_file, printing_first), false).await.unwrap(), Opened::With(SlotKind::Kms));
    reset(&db).await;

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
    as_vault(&db, "with u as (update vault.root_slot set check_value = set_byte(check_value, 3, get_byte(check_value, 3) # 1) where kind = 'credential' returning 1) select count(*) from u").await;
    let why = refusal(vault::open(&replica().await, opener(Some("ferrum"), &no_file, None), false).await);
    assert!(why.contains("damaged") && why.contains("check value"), "an unreadable check value is refused, as damage, not as a wrong password: {why}");
    as_vault(&db, "with u as (update vault.root_slot set check_value = set_byte(check_value, 3, get_byte(check_value, 3) # 1) where kind = 'credential' returning 1) select count(*) from u").await;

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
    // The root's own events, in the installation's record: made, then each slot added.
    let actions: Vec<String> = vault::events(&open_db, None, 50).await.unwrap().into_iter().map(|e| e.action).collect();
    assert!(actions.contains(&"made".to_string()) && actions.iter().filter(|a| *a == "slot_added").count() == 3, "{actions:?}");

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
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&fresh.pool).await.unwrap();
    assert_eq!(vault::open(&fresh, opener(Some("ferrum"), &no_file, None), false).await.unwrap(), Opened::Later);
    let k = Kernel::boot(fresh, core(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    assert!(vault::put_slot(&k.db, SlotKind::Recovery).await.unwrap().is_some(), "open once booted");
    assert_eq!(as_vault(&db, "select count(*) from vault.root_slot").await, 2, "made then: credential and recovery");
    // The public schema made again under a vault that is there (a test's reset, a restore of the public schema alone):
    // opened once migrated, as before, never before the tables its first steps write to are there.
    let again = replica().await;
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&again.pool).await.unwrap();
    assert_eq!(vault::open(&again, opener(Some("ferrum"), &no_file, None), false).await.unwrap(), Opened::Later);
    let k = Kernel::boot(again, core(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    assert!(vault::put_slot(&k.db, SlotKind::Recovery).await.unwrap().is_some(), "opened with the slots it had");
    // The vault's schema gone behind the migrations' back: said, never made again empty.
    sqlx::raw_sql("drop schema vault cascade").execute(&db.pool).await.unwrap();
    let why = refusal(vault::open(&replica().await, opener(Some("ferrum"), &no_file, None), false).await);
    assert!(why.contains("the vault's tables are gone"), "{why}");

    keys_and_handles(&no_file).await;
    secrets_and_people(&no_file).await;
    rotation_under_use(&dir, &no_file).await;
    moved_rows_do_not_open(&no_file).await;
    dump_holds_nothing(&dir).await;

    if let Ok(base) = std::env::var("FERRUM_KMS_TEST_TRANSIT") {
        transit(&base, &no_file).await;
    } else {
        assert!(std::env::var("CI").is_err(), "CI runs this against OpenBao: set FERRUM_KMS_TEST_TRANSIT");
        eprintln!("Transit's part skipped: FERRUM_KMS_TEST_TRANSIT is not set");
    }

    // Left as the next suite expects a database: nothing of the vault, and nothing that says it was made.
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.unwrap();
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
    // The platform's own owner key is there too: a start with no old key makes it, for the apps that still read one
    // (crates/kernel/src/vault/keyed.rs).
    assert_eq!(as_vault(&db, "select count(*) from vault.key where scope = '00000000-0000-0000-0000-000000000000'").await, 3);
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
    // A signature names its version, and each kept version's public half can be read (Cloud's issuer keys).
    crm.set(&k, ws, "issuer", New::Ed25519, Rule { overlap_secs: 3600, ..Default::default() }).await.unwrap();
    crm.rotate(&k, ws, "issuer", New::Ed25519).await.unwrap();
    let (v2, sig2) = crm.sign_at(&k, ws, "issuer", b"jwt", None).await.unwrap();
    let (v1, sig1) = crm.sign_at(&k, ws, "issuer", b"jwt", Some(1)).await.unwrap();
    assert_eq!((v1, v2), (1, 2), "the current one unless asked for another");
    for (v, sig) in [(1, &sig1), (2, &sig2)] {
        let half = crm.public_key(&k, ws, "issuer", Some(v)).await.unwrap().unwrap();
        let half = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, half).unwrap();
        assert!(ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &half).verify(b"jwt", sig).is_ok(), "version {v}");
    }
    assert_eq!(crm.public_key(&k, ws, "issuer", None).await.unwrap(), crm.describe(&k, ws, "issuer").await.unwrap().unwrap().public_key);
    assert!(matches!(crm.sign_at(&k, ws, "issuer", b"jwt", Some(7)).await, Err(Error::NotFound(_))), "a version not kept");
    assert_eq!(crm.public_key(&k, ws, "hooks", None).await.unwrap(), None, "a value has no public half");
    // What an app sealed names the key version, beside the one it seals with now: what to seal again.
    let sealed = crm.seal(&k, ws, "crm.note/1", b"kept outside").await.unwrap();
    assert_eq!(Vault::sealed_version(&sealed), Some(crm.key_version(&k, ws).await.unwrap()));

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
    // The installation's own: on no workspace's trail, in the installation's record, under the same cap.
    crm.set(&k, Scope::Installation, "license", value("L-1122-installation"), Rule { reveal: true, ..Default::default() }).await.unwrap();
    for _ in 0..vault::USES_PER_HOUR + 3 {
        crm.reveal(&k, Scope::Installation, "license").await.unwrap();
    }
    assert!(!trail(&k, w).await.iter().any(|(_, d)| d["name"] == "license"), "the installation's are on no workspace's trail");
    let record = vault::events(&k.db, None, 1000).await.unwrap();
    assert!(record.iter().any(|e| e.action == "set" && e.owner == "crm" && e.name == "license" && e.details["version"] == json!(1)), "{record:?}");
    assert_eq!(record.iter().filter(|e| e.action == "reveal" && e.name == "license").count(), vault::USES_PER_HOUR as usize, "the first of the hour");
    assert!(!format!("{record:?}").contains("L-1122"), "never a value");
    as_vault(&db, "with u as (update vault.use_count set hour = hour - interval '1 hour' where scope = '00000000-0000-0000-0000-000000000000' returning 1) select count(*) from u").await;
    crm.reveal(&k, Scope::Installation, "license").await.unwrap();
    let summary = vault::events(&k.db, None, 5).await.unwrap().into_iter().find(|e| e.action == "reveal_summary").expect("the hour's summary");
    assert_eq!((summary.name.as_str(), summary.details["count"].clone()), ("license", json!(3)));
    // Append-only, even for the vault's own transactions.
    for sql in ["update vault.event set action = 'nothing'", "delete from vault.event", "truncate vault.event"] {
        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
        let done = sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string())).execute(&mut *tx).await;
        tx.rollback().await.unwrap();
        assert!(done.is_err(), "{sql}");
    }

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

    // ── a secret its app manages: people's ways leave it to the app and say so; the app goes on using and changing it ──
    fn refused(r: Result<(), Error>) -> bool {
        // A code and the app's id of its own, for a screen to say it in the person's language; the sentence for the rest.
        matches!(&r, Err(e @ Error::Refused { code: "managed_by_app", status: 409, pointer: Some(app), .. })
            if app == "crm" && e.to_string().contains("managed by its app, crm"))
    }
    crm.set(&k, ws, "issuer.key", New::Ed25519, Rule::default()).await.unwrap();
    crm.manage(&k, ws, "issuer.key", true).await.unwrap();
    assert_eq!(crm.describe(&k, ws, "issuer.key").await.unwrap().unwrap().managed_by.as_deref(), Some("crm"));
    assert!(k.vault_secrets(&admin).await.unwrap().iter().any(|d| d.name == "issuer.key" && d.managed_by.as_deref() == Some("crm")));
    assert!(k.vault_secrets(&admin).await.unwrap().iter().any(|d| d.name == "partner.token" && d.managed_by.is_none()), "people's own");
    assert!(refused(k.vault_set(&proved, "crm", "issuer.key", New::Ed25519, Rule::default()).await.map(|_| ())));
    assert!(refused(k.vault_rotate(&proved, "crm", "issuer.key", New::Ed25519).await.map(|_| ())));
    assert!(refused(k.vault_rule(&proved, "crm", "issuer.key", Rule::default()).await.map(|_| ())));
    assert!(refused(k.vault_remove(&proved, "crm", "issuer.key").await.map(|_| ())));
    assert!(refused(k.vault_regenerate(ws, "crm", "issuer.key").await.map(|_| ())), "an operator's regenerate too");
    assert_eq!(crm.rotate(&k, ws, "issuer.key", New::Ed25519).await.unwrap().managed_by.as_deref(), Some("crm"), "its app rotates it");
    assert!(crm.sign(&k, ws, "issuer.key", b"m").await.is_ok(), "and uses it");
    crm.manage(&k, ws, "issuer.key", false).await.unwrap();
    assert!(k.vault_rule(&proved, "crm", "issuer.key", Rule::default()).await.is_ok(), "people's again once its app lets go");
    assert!(matches!(crm.manage(&k, ws, "missing", true).await, Err(Error::NotFound(_))));
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

/// Counts rows as the vault's module sees them.
async fn count(db: &Db, sql: String) -> i64 {
    as_vault(db, &sql).await
}

async fn rotation_under_use(dir: &Path, no_file: &Path) {
    let db = replica().await;
    reset(&db).await;
    let (a, crm_a, hr_a) = kernel_with_apps(no_file).await;
    let (b, crm_b, _) = kernel_with_apps(no_file).await;
    let (_stop_listening, listening) = tokio::sync::watch::channel(false);
    let _a_hears = a.spawn_listeners(listening.clone());
    let _b_hears = b.spawn_listeners(listening.clone());
    let w = workspace(&a, "rotor").await;
    let ws = Scope::Workspace(w);
    let printed = vault::put_slot(&a.db, SlotKind::Recovery).await.unwrap().unwrap().to_string();
    const MANY: usize = 1500;
    for i in 0..MANY {
        crm_a.set(&a, ws, &format!("v{i}"), value(&format!("value-{i}")), Rule { reveal: true, ..Default::default() }).await.unwrap();
    }
    for i in 0..3 {
        hr_a.set(&a, ws, &format!("h{i}"), value("hr's"), Rule::default()).await.unwrap();
    }
    crm_a.set(&a, ws, "hooks", New::Random(32), Rule { overlap_secs: 3600, ..Default::default() }).await.unwrap();
    crm_a.set(&a, ws, "pair", New::Ed25519, Rule::default()).await.unwrap();
    crm_a.set(&a, Scope::Installation, "license", value("L-42"), Rule { reveal: true, ..Default::default() }).await.unwrap();
    let sealed = crm_a.seal(&a, ws, "doc/1", b"kept outside").await.unwrap();
    let derived = crm_a.derive(&a, ws, "links").await.unwrap().bytes().to_vec();
    let hook_sig = crm_a.sign(&a, ws, "hooks", b"event").await.unwrap();

    // Two readers on each replica, using every kind of secret until told to stop: none of their uses may fail.
    let stop = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(Mutex::new(Vec::<String>::new()));
    let uses = Arc::new(AtomicUsize::new(0));
    let mut readers = Vec::new();
    for (n, (k, v)) in [(a.clone(), crm_a.clone()), (a.clone(), crm_a.clone()), (b.clone(), crm_b.clone()), (b.clone(), crm_b.clone())].into_iter().enumerate()
    {
        let (stop, failed, uses, sealed, hook_sig, derived) = (stop.clone(), failed.clone(), uses.clone(), sealed.clone(), hook_sig.clone(), derived.clone());
        readers.push(tokio::spawn(async move {
            let mut i = n;
            while !stop.load(Ordering::Relaxed) {
                i = (i + 7) % MANY;
                let checks: Vec<(&str, std::result::Result<bool, Error>)> = vec![
                    ("reveal", v.reveal(&k, ws, &format!("v{i}")).await.map(|x| *x == *format!("value-{i}").as_bytes())),
                    ("verify across the overlap", v.verify(&k, ws, "hooks", b"event", &hook_sig).await),
                    (
                        "sign and verify",
                        match v.sign(&k, ws, "pair", b"m").await {
                            Ok(sig) => v.verify(&k, ws, "pair", b"m", &sig).await,
                            Err(e) => Err(e),
                        },
                    ),
                    ("open what was sealed outside", v.open(&k, ws, "doc/1", &sealed).await.map(|x| &**x == b"kept outside")),
                    ("derive", v.derive_at(&k, ws, "links", 1).await.map(|d| d.bytes() == derived.as_slice())),
                    ("the installation's", v.reveal(&k, Scope::Installation, "license").await.map(|x| &**x == b"L-42")),
                ];
                for (what, r) in checks {
                    match r {
                        Ok(true) => {}
                        Ok(false) => failed.lock().unwrap().push(format!("reader {n}: {what} gave something else")),
                        Err(e) => failed.lock().unwrap().push(format!("reader {n}: {what}: {e}")),
                    }
                }
                uses.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    let used_by = |n: usize| {
        let uses = uses.clone();
        async move {
            let start = uses.load(Ordering::Relaxed);
            while uses.load(Ordering::Relaxed) < start + n {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
    };
    used_by(8).await;

    // ── a slot: made again with a new salt; one row changes ──
    vault::put_slot(&a.db, SlotKind::Credential).await.unwrap();
    used_by(8).await;

    // ── the installation key: every slot and every workspace key wrapped again in one go ──
    assert!(
        matches!(vault::rotate_installation_key(&a.db, None).await, Err(Error::Invalid(m)) if m.contains("recovery")),
        "a recovery slot asks what becomes of it"
    );
    let wrong = RecoveryKey::Keep(Zeroizing::new("AAAA-".repeat(13)));
    assert!(vault::rotate_installation_key(&a.db, Some(wrong)).await.is_err(), "not this vault's recovery key");
    assert_eq!(vault::rotate_installation_key(&a.db, Some(RecoveryKey::Keep(Zeroizing::new(printed.clone())))).await.unwrap().map(|p| p.to_string()), None);
    assert_eq!(count(&db, "select count(*) from vault.root_slot where installation_version <> 2".into()).await, 0);
    assert_eq!(count(&db, "select count(*) from vault.key where owner = '' and parent_version <> 2".into()).await, 0);
    used_by(8).await;
    let (c, crm_c, _) = kernel_with_apps(no_file).await;
    assert_eq!(&**crm_c.reveal(&c, ws, "v3").await.unwrap(), b"value-3", "a replica started now opens with the new one");
    let w2 = workspace(&b, "rotor-two").await;
    crm_b.set(&b, Scope::Workspace(w2), "fresh", value("x"), Rule::default()).await.unwrap();
    assert_eq!(
        count(&db, format!("select count(*) from vault.key where scope = '{w2}' and owner = '' and parent_version = 2")).await,
        1,
        "one that held the old key makes new keys with the new"
    );

    // ── a workspace key: its owner keys wrapped again in the same transaction, the old version gone ──
    assert_eq!(a.rotate_workspace_key(ws).await.unwrap(), 2);
    assert_eq!(count(&db, format!("select count(*) from vault.key where scope = '{w}' and owner = ''")).await, 1);
    assert_eq!(count(&db, format!("select count(*) from vault.key where scope = '{w}' and owner <> '' and parent_version <> 2")).await, 0);
    used_by(8).await;

    // ── an owner key: the re-seal in the background, stopped half way, taken up by the other replica ──
    assert_eq!(a.rotate_owner_key(ws, "crm").await.unwrap(), 2);
    let under = |v: i32| count(&db, format!("select count(*) from vault.secret where scope = '{w}' and owner = 'crm' and key_version = {v}"));
    assert_eq!(under(2).await, 0, "nothing yet: it is the job's to do");
    let worker = {
        let b = b.clone();
        tokio::spawn(async move { b.run_jobs_once(1).await })
    };
    while under(2).await == 0 {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    // The worker loses its job, as when its replica stops: it ends at its next batch.
    let mut tx = a.db.system_tx().await.unwrap();
    sqlx::query("update job set lease_token = gen_random_uuid() where tenant_id = $1 and kind = 'platform.vault_reseal'")
        .bind(w)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    worker.await.unwrap().unwrap();
    let (half, left) = (under(2).await, under(1).await);
    assert!(half > 0 && left > 0, "stopped half way: {half} sealed again, {left} left");
    used_by(8).await;
    let mut tx = a.db.system_tx().await.unwrap();
    sqlx::query("update job set lease_until = now() - interval '1 second' where tenant_id = $1 and kind = 'platform.vault_reseal'")
        .bind(w)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(a.run_jobs_once(1).await.unwrap(), 1, "the job taken over");
    assert_eq!((under(1).await, under(2).await), (0, MANY as i64 + 2));
    assert_eq!(
        count(&db, format!("select count(*) from vault.key where scope = '{w}' and owner = 'crm' and version = 1")).await,
        1,
        "kept: it sealed what is kept outside"
    );
    used_by(8).await;
    // One with nothing outside: both replicas seal again at once, and the old version goes.
    assert_eq!(a.rotate_owner_key(ws, "hr").await.unwrap(), 2);
    let (x, y) = tokio::join!(a.vault_reseal(ws, "hr"), b.vault_reseal(ws, "hr"));
    // Each skips the rows the other holds, and says how many it left to it.
    assert!(x.unwrap() + y.unwrap() <= 3);
    assert_eq!(a.vault_reseal(ws, "hr").await.unwrap(), 0);
    assert_eq!(count(&db, format!("select count(*) from vault.secret where scope = '{w}' and owner = 'hr' and key_version = 2")).await, 3);
    assert_eq!(count(&db, format!("select count(*) from vault.key where scope = '{w}' and owner = 'hr'")).await, 1, "the old version gone");
    // The installation's scope: no job queue; the task (or the upkeep) seals again.
    assert_eq!(a.rotate_owner_key(Scope::Installation, "crm").await.unwrap(), 2);
    assert_eq!(a.vault_reseal(Scope::Installation, "crm").await.unwrap(), 0);
    assert!(matches!(a.rotate_owner_key(ws, "nobody").await, Err(Error::NotFound(_))));

    // ── one secret: a new version, the old signature still good through the overlap ──
    crm_a.rotate(&a, ws, "hooks", New::Random(32)).await.unwrap();
    used_by(8).await;
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.await.unwrap();
    }
    let failed = failed.lock().unwrap().clone();
    assert!(failed.is_empty(), "{} uses failed during the rotations: {:?}", failed.len(), &failed[..failed.len().min(10)]);
    assert!(uses.load(Ordering::Relaxed) > 40);
    // Changes to one secret from both replicas at once: none lost, none made twice, one current version.
    let before = crm_a.describe(&a, ws, "hooks").await.unwrap().unwrap().version;
    let changes = futures::future::join_all((0..8).map(|i| {
        let (k, v) = if i % 2 == 0 { (&a, &crm_a) } else { (&b, &crm_b) };
        v.rotate(k, ws, "hooks", New::Random(32))
    }))
    .await;
    let mut versions: Vec<i32> = changes.into_iter().map(|c| c.unwrap().version).collect();
    versions.sort();
    assert_eq!(versions, (before + 1..=before + 8).collect::<Vec<_>>(), "each its own version, one after another");
    assert_eq!(count(&db, format!("select count(*) from vault.secret where scope = '{w}' and name = 'hooks' and valid_until is null")).await, 1);
    assert_eq!(count(&db, format!("select count(*) from vault.secret where scope = '{w}' and name = 'hooks'")).await, 2, "the current, and the one before it");
    a.maintenance().await.unwrap();
    assert_eq!(&**crm_b.reveal(&b, ws, "v9").await.unwrap(), b"value-9");

    // ── slots: never the last a server opens with ──
    assert!(matches!(vault::remove_slot(&a.db, SlotKind::Credential).await, Err(Error::Conflict(_))), "the recovery slot alone opens no server");
    let file = key_file(dir, "rotor.key");
    let with_file = replica().await;
    vault::open(&with_file, opener(Some("ferrum"), &file, None), false).await.unwrap();
    vault::put_slot(&with_file, SlotKind::File).await.unwrap();
    vault::remove_slot(&with_file, SlotKind::Credential).await.unwrap();
    assert!(matches!(vault::remove_slot(&with_file, SlotKind::File).await, Err(Error::Conflict(_))));
    vault::put_slot(&with_file, SlotKind::Credential).await.unwrap();
    vault::remove_slot(&with_file, SlotKind::File).await.unwrap();
    assert!(matches!(vault::remove_slot(&with_file, SlotKind::File).await, Err(Error::NotFound(_))));

    // ── the database password changed: made good with the old one, given once; then with the recovery key ──
    let changed = replica().await;
    assert!(vault::open(&changed, opener(Some("n3w-pass"), no_file, None), true).await.is_err(), "the new password does not open it yet");
    let e = vault::rewrap(&changed, Rewrap::OldPassword(Zeroizing::new("not-it".into()))).await.unwrap_err();
    assert!(e.to_string().contains("not the password"), "{e}");
    vault::rewrap(&changed, Rewrap::OldPassword(Zeroizing::new("ferrum".into()))).await.unwrap();
    assert_eq!(vault::open(&replica().await, opener(Some("n3w-pass"), no_file, None), false).await.unwrap(), Opened::With(SlotKind::Credential));
    assert!(vault::open(&replica().await, opener(Some("ferrum"), no_file, None), false).await.is_err(), "the old one no longer does");
    let lost = replica().await;
    assert!(vault::open(&lost, opener(Some("ferrum"), no_file, None), true).await.is_err());
    assert!(vault::rewrap(&lost, Rewrap::Recovery(Zeroizing::new("AAAA-".repeat(13)))).await.is_err(), "another recovery key");
    vault::rewrap(&lost, Rewrap::Recovery(Zeroizing::new(printed.to_lowercase()))).await.unwrap();
    assert_eq!(vault::open(&replica().await, opener(Some("ferrum"), no_file, None), false).await.unwrap(), Opened::With(SlotKind::Credential));
    let (d, crm_d, _) = kernel_with_apps(no_file).await;
    assert_eq!(&**crm_d.reveal(&d, ws, "v1").await.unwrap(), b"value-1", "and everything opens as before");

    // ── what an operator and the doctor are told: never a key ──
    let st = vault::status(&d.db).await.unwrap();
    assert!(st.open && st.opened_with == Some(SlotKind::Credential) && st.installation_version == Some(2), "{st:?}");
    assert_eq!(st.slots.iter().map(|s| s.kind).collect::<Vec<_>>(), [SlotKind::Credential, SlotKind::Recovery]);
    assert!(st.workspace_keys >= 2 && st.owner_keys >= 3 && st.secrets as usize > MANY && st.resealing.is_empty(), "{st:?}");
    let said = serde_json::to_string(&st).unwrap();
    assert!(!said.contains(&printed) && !said.contains("value-"), "{said}");
    let level = |f: &[ferrum_kernel::doctor::Finding], check: &str| f.iter().find(|x| x.check == check).map(|x| x.level);
    use ferrum_kernel::doctor::Level;
    let found = d.vault_findings().await.unwrap();
    for check in ["vault", "vault.slots", "vault.recovery", "vault.slot.credential", "vault.keys", "vault.resealing"] {
        assert_eq!(level(&found, check), Some(Level::Ok), "{check}: {found:?}");
    }
    d.rotate_owner_key(ws, "hr").await.unwrap();
    as_vault(&db, &format!("with u as (update vault.key set created_at = now() - interval '2 days' where scope = '{w}' and owner = 'hr' and version = 3 returning 1) select count(*) from u")).await;
    assert_eq!(level(&d.vault_findings().await.unwrap(), "vault.resealing"), Some(Level::Warning), "still waiting a day on");
    d.vault_reseal(ws, "hr").await.unwrap();
    let closed = replica().await;
    let _ = vault::open(&closed, opener(Some("wrong"), no_file, None), true).await;
    let k_closed = Kernel::boot(closed, core(), HookRegistry::default(), KernelSettings::default(), None).await.unwrap();
    let found = k_closed.vault_findings().await.unwrap();
    assert_eq!(level(&found, "vault"), Some(Level::Blocker), "{found:?}");
    assert!(found[0].detail.contains("not the one it was made with"), "the reason, as at start: {found:?}");
    let never = Kernel::boot(replica().await, core(), HookRegistry::default(), KernelSettings::default(), None).await.unwrap();
    assert_eq!(level(&never.vault_findings().await.unwrap(), "vault"), Some(Level::Off), "a process that never opened it");
    vault::remove_slot(&d.db, SlotKind::Recovery).await.unwrap();
    assert_eq!(level(&d.vault_findings().await.unwrap(), "vault.recovery"), Some(Level::Warning), "no recovery key");

    // ── a server's start keeps the slots whole by itself: the mounted key file gains its slot, and a changed password
    //    its credential slot again, each once however many replicas start together ──
    let key = key_file(dir, "settle.key");
    let first = replica().await;
    assert_eq!(vault::open(&first, opener(Some("ferrum"), &key, None), false).await.unwrap(), Opened::With(SlotKind::Credential));
    assert_eq!(vault::settle(&first).await.unwrap().len(), 1, "an installation that had only its credential slot gains a file slot");
    assert!(vault::settle(&first).await.unwrap().is_empty(), "once");
    // The password changes (an import, a copy, a recovery, a provider's reset): the file slot opens the vault, and two
    // replicas starting together make the credential slot again for the new password once.
    let (r1, r2) = (replica().await, replica().await);
    for r in [&r1, &r2] {
        assert_eq!(vault::open(r, opener(Some("n3w-pass"), &key, None), false).await.unwrap(), Opened::With(SlotKind::File));
    }
    let k_changed = Kernel::boot(replica().await, core(), HookRegistry::default(), KernelSettings::default(), None).await.unwrap();
    vault::open(&k_changed.db, opener(Some("n3w-pass"), &key, None), false).await.unwrap();
    let advice = k_changed.vault_findings().await.unwrap().into_iter().find(|f| f.check == "vault.slot.credential").unwrap();
    assert!(advice.level == Level::Warning && advice.fix.is_some_and(|f| f.contains("Start a server")), "advice that works: {advice:?}");
    let (a1, a2) = tokio::join!(vault::settle(&r1), vault::settle(&r2));
    assert_eq!(a1.unwrap().len() + a2.unwrap().len(), 1, "made again once");
    assert_eq!(as_vault(&db, "select count(*) from vault.event where action = 'credential_remade'").await, 1);
    assert_eq!(
        vault::open(&replica().await, opener(Some("n3w-pass"), no_file, None), false).await.unwrap(),
        Opened::With(SlotKind::Credential),
        "the new password opens it alone"
    );
    // A key file other than the slot's is left alone; a credential slot an operator removed stays removed.
    let other = key_file(dir, "other.key");
    let elsewhere = replica().await;
    assert_eq!(vault::open(&elsewhere, opener(Some("n3w-pass"), &other, None), false).await.unwrap(), Opened::With(SlotKind::Credential));
    assert!(vault::settle(&elsewhere).await.unwrap().is_empty(), "the file slot is not replaced");
    vault::remove_slot(&elsewhere, SlotKind::Credential).await.unwrap();
    let after = replica().await;
    assert_eq!(vault::open(&after, opener(Some("an0ther"), &key, None), false).await.unwrap(), Opened::With(SlotKind::File));
    assert!(vault::settle(&after).await.unwrap().is_empty(), "none is made where none is");
}

/// Opens what the vault sealed, as its format says (nonce, ciphertext, tag; the place as associated data): what
/// someone with the rows and a slot's key could do.
fn open_as_stored(key: &[u8], place: &str, sealed: &[u8]) -> Option<Vec<u8>> {
    use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
    let (nonce, body) = sealed.split_at(NONCE_LEN);
    let mut body = body.to_vec();
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).ok()?);
    let plain = key.open_in_place(Nonce::try_assume_unique_for_key(nonce).ok()?, Aad::from(place.as_bytes()), &mut body).ok()?;
    Some(plain.to_vec())
}

/// The tables of `schema` that hold any of `needles` (each looked for as it is written in the row's text: bytes as
/// hex), with how many rows.
async fn holding(db: &Db, schema: &str, needles: &[String]) -> Vec<String> {
    let like: Vec<String> = needles.iter().map(|s| format!("%{s}%")).collect();
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true), set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    let tables: Vec<String> =
        sqlx::query_scalar("select table_name::text from information_schema.tables where table_schema = $1 and table_type = 'BASE TABLE' order by 1")
            .bind(schema)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    let mut out = Vec::new();
    for table in tables {
        let hits: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from \"{schema}\".\"{table}\" x where x::text ilike any($1)")))
            .bind(&like)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        if hits > 0 {
            out.push(format!("{schema}.{table}: {hits}"));
        }
    }
    tx.commit().await.unwrap();
    out
}

/// Every way a key or a value can be written down: its bytes as hex, base64 and base64url, and as text.
fn written(bytes: &[u8]) -> Vec<String> {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    let mut out = vec![hex::encode(bytes), STANDARD.encode(bytes).trim_end_matches('=').to_string(), URL_SAFE_NO_PAD.encode(bytes)];
    if let Ok(text) = std::str::from_utf8(bytes) {
        out.push(text.to_string());
    }
    out
}

async fn moved_rows_do_not_open(no_file: &Path) {
    let db = replica().await;
    reset(&db).await;
    let (k, crm, hr) = kernel_with_apps(no_file).await;
    let (w, w2) = (workspace(&k, "movers").await, workspace(&k, "elsewhere").await);
    let ws = Scope::Workspace(w);
    let open = Rule { reveal: true, ..Default::default() };
    crm.set(&k, ws, "api", value("moved-nowhere"), open.clone()).await.unwrap();
    crm.set(&k, ws, "other", value("x"), open.clone()).await.unwrap();
    hr.set(&k, ws, "api", value("hr's own"), open.clone()).await.unwrap();
    crm.set(&k, Scope::Workspace(w2), "api", value("there"), open.clone()).await.unwrap();
    // Copies crm's `api` in `w` (its sealed value, under the version of crm's key that sealed it) over the row at
    // (scope, owner, name, version), as someone with the database could; then asks for that row's secret.
    let copy = |scope: Uuid, owner: &'static str, name: &'static str, version: i32| {
        let db = db.clone();
        async move {
            let mut tx = db.pool.begin().await.unwrap();
            sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
            let sealed: Vec<u8> = sqlx::query_scalar("select sealed from vault.secret where scope = $1 and owner = 'crm' and name = 'api'")
                .bind(w)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            let target_kv: i32 = sqlx::query_scalar("select max(version) from vault.key where scope = $1 and owner = $2")
                .bind(scope)
                .bind(owner)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            sqlx::query("delete from vault.secret where scope = $1 and owner = $2 and name = $3")
                .bind(scope)
                .bind(owner)
                .bind(name)
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("insert into vault.secret (scope, owner, name, version, key_version, kind, sealed, rule) values ($1, $2, $3, $4, $5, 'value', $6, '{\"reveal\": true}')")
                .bind(scope)
                .bind(owner)
                .bind(name)
                .bind(version)
                .bind(target_kv)
                .bind(&sealed)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
    };
    let refused = |r: ferrum_kernel::Result<Zeroizing<Vec<u8>>>, how: &str| match r {
        Ok(_) => panic!("{how}: it opened"),
        Err(e) => assert!(e.to_string().contains("does not open"), "{how}: {e}"),
    };
    copy(w, "crm", "other", 1).await;
    refused(crm.reveal(&k, ws, "other").await, "to another name, under the same key");
    copy(w, "crm", "other", 2).await;
    refused(crm.reveal(&k, ws, "other").await, "to another version");
    copy(w, "hr", "api", 1).await;
    refused(hr.reveal(&k, ws, "api").await, "to another owner");
    copy(w2, "crm", "api", 1).await;
    refused(crm.reveal(&k, Scope::Workspace(w2), "api").await, "to another workspace");
    assert_eq!(&**crm.reveal(&k, ws, "api").await.unwrap(), b"moved-nowhere", "where it was sealed, it opens");

    // Nothing of any of the vault's tables is seen by a query that does not go through it.
    for table in ["root_slot", "key", "secret", "use_count", "event"] {
        assert!(count(&db, format!("select count(*) from vault.{table}")).await > 0 || table == "use_count", "{table} holds rows");
        for (who, tx) in [("the system", db.system_tx().await.unwrap()), ("a workspace", db.tenant_tx(w).await.unwrap())] {
            let mut tx = tx;
            let seen: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from vault.{table}"))).fetch_one(&mut *tx).await.unwrap();
            tx.rollback().await.unwrap();
            assert_eq!(seen, 0, "{who} sees {table}");
        }
    }
}

async fn dump_holds_nothing(dir: &Path) {
    let db = replica().await;
    reset(&db).await;
    // A vault opened with a key file only: what someone with a dump and this file could open, the test opens too.
    let file = key_file(dir, "dump.key");
    let file_key = Zeroizing::new(base64::Engine::decode(&base64::engine::general_purpose::STANDARD, std::fs::read_to_string(&file).unwrap().trim()).unwrap());
    let db0 = replica().await;
    assert_eq!(vault::open(&db0, opener(Some("ferrum"), &file, None), false).await.unwrap(), Opened::With(SlotKind::File), "a key file mounted: the best slot");
    let printed = vault::put_slot(&db0, SlotKind::Recovery).await.unwrap().unwrap().to_string();
    let mut hooks = HookRegistry::default();
    let mut crm = None;
    hooks.app("crm", &[], |h| crm = Some(h.vault()));
    let k = Kernel::boot(db0, core(), hooks, KernelSettings::default(), None).await.unwrap();
    let crm = crm.unwrap();
    let w = workspace(&k, "dumped").await;
    let ws = Scope::Workspace(w);
    let mut values: Vec<Vec<u8>> = Vec::new();
    for (name, v) in [("api", "Pq-2468-vault-secret"), ("smtp", "Zz-9753-mail-pass")] {
        crm.set(&k, ws, name, value(v), Rule { reveal: true, ..Default::default() }).await.unwrap();
        values.push(v.as_bytes().to_vec());
    }
    crm.set(&k, Scope::Installation, "license", value("Lc-1122-installation"), Rule::default()).await.unwrap();
    values.push(b"Lc-1122-installation".to_vec());
    let session = k.login("dumped", "admin@dumped.test", "Correct-Horse-9").await.unwrap();
    let admin = Ctx { proof: Proof::Person { at: None, staff: true }, ..k.load_ctx(w, session.user, None, "it".into()).await.unwrap() }
        .proved(Some(chrono::Utc::now()));
    let shown = k.vault_set(&admin, "crm", "partner", New::Random(32), Rule::default()).await.unwrap().shown.unwrap().to_string();
    values.push(shown.as_bytes().to_vec());
    crm.set(&k, ws, "pair", New::Ed25519, Rule::default()).await.unwrap();
    crm.set(&k, ws, "hooks", New::Random(32), Rule { overlap_secs: 600, ..Default::default() }).await.unwrap();
    let sealed = crm.seal(&k, ws, "doc/9", b"Od-4680-kept-outside").await.unwrap();
    values.push(b"Od-4680-kept-outside".to_vec());
    let derived = crm.derive(&k, ws, "links").await.unwrap().bytes().to_vec();
    for _ in 0..3 {
        crm.reveal(&k, ws, "api").await.unwrap();
        crm.sign(&k, ws, "pair", b"m").await.unwrap();
    }
    // Every level rotated, so that old and new versions are both there to look for.
    let first_printed = printed;
    let printed = vault::rotate_installation_key(&k.db, Some(RecoveryKey::New)).await.unwrap().expect("a new recovery key").to_string();
    k.rotate_workspace_key(ws).await.unwrap();
    k.rotate_owner_key(ws, "crm").await.unwrap();
    crm.rotate(&k, ws, "hooks", New::Random(32)).await.unwrap();
    assert_eq!(k.vault_reseal(ws, "crm").await.unwrap(), 0);
    assert_eq!(&**crm.open(&k, ws, "doc/9", &sealed).await.unwrap(), b"Od-4680-kept-outside");

    // Every key, made again from the rows and the key file alone.
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    let (version, wrapped): (i32, Vec<u8>) =
        sqlx::query_as("select installation_version, wrapped from vault.root_slot where kind = 'file'").fetch_one(&mut *tx).await.unwrap();
    let installation = open_as_stored(&file_key, &format!("ferrum-vault/root/file/{version}"), &wrapped).expect("the installation key, with the file");
    let mut keys: std::collections::HashMap<(Uuid, String, i32), Vec<u8>> = Default::default();
    let rows: Vec<(Uuid, String, i32, i32, Vec<u8>)> =
        sqlx::query_as("select scope, owner, version, parent_version, wrapped from vault.key order by owner = '' desc").fetch_all(&mut *tx).await.unwrap();
    for (scope, owner, v, parent, wrapped) in &rows {
        let above = if owner.is_empty() { installation.clone() } else { keys[&(*scope, String::new(), *parent)].clone() };
        let key = open_as_stored(&above, &format!("ferrum-vault/key/{scope}/{owner}/{v}/{parent}"), wrapped).expect("a key, with the one above it");
        keys.insert((*scope, owner.clone(), *v), key);
    }
    let secrets: Vec<(Uuid, String, String, i32, i32, Vec<u8>)> =
        sqlx::query_as("select scope, owner, name, version, key_version, sealed from vault.secret").fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let mut opened = Vec::new();
    for (scope, owner, name, v, kv, sealed) in &secrets {
        let plain = open_as_stored(&keys[&(*scope, owner.clone(), *kv)], &format!("ferrum-vault/secret/{scope}/{owner}/{name}/{v}/{kv}"), sealed)
            .expect("a secret, with its key");
        opened.push(plain);
    }
    for v in &values[..values.len() - 1] {
        assert!(opened.contains(v), "the format is as this test reads it");
    }
    assert!(keys.len() >= 4 && opened.len() >= 7, "{} keys, {} secrets", keys.len(), opened.len());

    // Not one of them, nor a value, nor the slots' keys, is anywhere in the database as it is.
    let mut needles: Vec<String> = Vec::new();
    for bytes in values.iter().chain(opened.iter()).chain(keys.values()).chain([&installation, &*file_key, &derived]) {
        needles.extend(written(bytes));
    }
    for p in [&printed, &first_printed] {
        needles.push(p.clone());
        needles.push(p.replace('-', ""));
    }
    let needles: Vec<String> = needles.into_iter().filter(|n| n.len() >= 12).collect();
    // The search finds what is there: a public half kept as bytes, a workspace's name kept as text.
    let public = crm.describe(&k, ws, "pair").await.unwrap().unwrap().public_key.unwrap();
    let public = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, public).unwrap();
    assert_eq!(holding(&db, "vault", &[hex::encode(&public)]).await, ["vault.secret: 1"]);
    assert!(holding(&db, "public", &["dumped".to_string()]).await.iter().any(|t| t.starts_with("public.tenant:")));
    assert_eq!(holding(&db, "public", &needles).await, Vec::<String>::new(), "a value or a key in a workspace's tables");
    assert_eq!(holding(&db, "vault", &needles).await, Vec::<String>::new(), "a value or an unwrapped key in the vault's own");
    // And without the key file (or the recovery key), the dump opens nothing: the slot's check says so.
    let why = refusal(vault::open(&replica().await, opener(Some("not-the-password"), &dir.join("missing.key"), None), false).await);
    assert!(why.contains("no slot opens the vault"), "{why}");
}
