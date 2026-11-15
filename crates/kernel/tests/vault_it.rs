//! The vault against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the public schema and the vault's are reset first,
//! and the vault's dropped again at the end): its slots open it, and each way opening can fail is refused with its
//! reason; replicas starting together make one installation key; nothing that does not go through the vault's module
//! sees a row of it; and the vault's settings never outlive their transaction. Transit's part runs when
//! `FERRUM_KMS_TEST_TRANSIT` is set (OpenBao, as kms_it.rs).

use ferrum_kernel::vault::{self, Kdf, Opened, Opener, SlotKind};
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use std::path::{Path, PathBuf};
use uuid::Uuid;

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
        sqlx::query_scalar("select wrapped from vault.root_slot where kind = 'credential'").fetch_one(&mut *tx).await.unwrap()
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
