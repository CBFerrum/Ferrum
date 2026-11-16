//! The vault's platform half and a workspace's vault following the workspace, against two real PostgreSQL databases
//! (`FERRUM_TEST_DATABASE_URL` and `FERRUM_RESTORE_TEST_DATABASE_URL`, both reset first, the vault's schema too), each
//! an installation with a root of its own. The server's own handle is given once, before any app; what a replica finds
//! in its environment is stored once however many copy it at once; a plugin's handle adds its secret to a request for
//! the hosts the secret names and never shows it; the operator acts through the platform's handle alone. A deleted
//! workspace takes its keys and secrets with it; a sandbox has each secret as its rule says, with keys of its own; a
//! move carries the vault with a transfer key, refuses without it (or with a wrong one, or after the key changed),
//! resumes, sees a change made at the source after the copy, is undone, finishes (the source's copy then opens
//! nothing), and goes back. No table of either database holds a value, an unwrapped key or a transfer key.

use ferrum_kernel::controls::Proof;
use ferrum_kernel::sandbox::Copy;
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::vault::{self, Carry, Kdf, New, Opener, Rule, Sandbox, Scope, Vault};
use ferrum_kernel::workspace_move::{MoveSource, Run, Stop};
use ferrum_kernel::{Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use zeroize::Zeroizing;

const PASSWORD: &str = "Correct-Horse-9";

fn url(env: &str, database: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| format!("postgres://ferrum:ferrum@localhost:5432/{database}"))
}

fn superuser(address: &str) -> String {
    let base = std::env::var("FERRUM_TEST_SUPERUSER").unwrap_or_else(|_| format!("postgres://{}@localhost:5432", std::env::var("USER").unwrap_or_default()));
    format!("{base}/{}", address.rsplit('/').next().unwrap())
}

async fn as_superuser(address: &str, sql: &str) {
    let db = Db::connect(DbConfig { url: superuser(address), max_connections: 1, ..Default::default() }).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string())).execute(&db.pool).await.unwrap();
    db.pool.close().await;
}

/// Files in a directory, as a single host keeps them.
struct DirFiles(PathBuf);

#[async_trait::async_trait]
impl ferrum_kernel::files::FileStore for DirFiles {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        let path = self.0.join(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).map_err(|e| Error::Unavailable(e.to_string()))
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        std::fs::read(self.0.join(key)).map_err(|_| Error::NotFound(key.into()))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        let _ = std::fs::remove_file(self.0.join(key));
        Ok(())
    }
}

struct Installation {
    url: String,
    k: Arc<Kernel>,
    platform: Vault,
    crm: Vault,
    key: SecretKey,
    files: PathBuf,
}

/// An installation on `url`, reset, its vault made with the database password at its first start, with the server's
/// handle and app `crm`'s.
async fn installation(url: &str, key: SecretKey, files: &Path) -> Installation {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 8, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let opener = Opener { database_url: url.to_string(), file: files.join("absent.key"), kdf: Kdf { m_kib: 64, t: 1, p: 1 }, ..Default::default() };
    vault::open(&db, opener, false).await.expect("opened once migrated");
    let mut hooks = HookRegistry::default();
    let platform = hooks.platform_vault();
    let mut crm = None;
    hooks.app("crm", &[], |h| crm = Some(h.vault()));
    let catalog = ferrum_meta::Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let k = Kernel::boot(db, catalog, hooks, KernelSettings { secret_key: Some(key.clone()), ..Default::default() }, None).await.expect("boot");
    k.set_file_store(Arc::new(DirFiles(files.to_path_buf())));
    k.ensure_directory().await.unwrap();
    Installation { url: url.to_string(), k, platform, crm: crm.unwrap(), key, files: files.to_path_buf() }
}

async fn workspace(k: &Kernel, slug: &str) -> Uuid {
    k.create_tenant(NewTenant {
        slug: slug.into(),
        name: slug.into(),
        admin_email: format!("admin@{slug}.test"),
        admin_password: PASSWORD.into(),
        admin_name: None,
        packages: vec!["contacts".into()],
        settings: None,
    })
    .await
    .unwrap()
}

/// The workspace's administrator, having just confirmed it is them.
async fn admin(k: &Kernel, slug: &str, tenant: Uuid) -> Ctx {
    let s = k.login(slug, &format!("admin@{slug}.test"), PASSWORD).await.unwrap();
    Ctx { proof: Proof::Person { at: None, staff: true }, ..k.load_ctx(tenant, s.user, None, "it".into()).await.unwrap() }.proved(Some(chrono::Utc::now()))
}

fn value(s: &str) -> New {
    New::Value(Zeroizing::new(s.as_bytes().to_vec()))
}

fn shown(v: Zeroizing<Vec<u8>>) -> String {
    String::from_utf8(v.to_vec()).unwrap()
}

/// How many of the vault's rows of `scope` there are: keys, secrets, transfer rows.
async fn rows_of(db: &Db, scope: Uuid) -> (i64, i64, i64) {
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    let n = sqlx::query_as(
        "select (select count(*) from vault.key where scope = $1), (select count(*) from vault.secret where scope = $1), (select count(*) from vault.transfer where scope = $1)",
    )
    .bind(scope)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    n
}

async fn id_of(k: &Kernel, slug: &str) -> Uuid {
    let mut tx = k.db.system_tx().await.unwrap();
    let id = sqlx::query_scalar("select id from tenant where slug = $1").bind(slug).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    id
}

fn quiet(_: &str) {}

fn says(e: Error, what: &str) {
    assert!(e.to_string().contains(what), "{what:?}: {e}");
}

#[test]
fn a_workspaces_vault_follows_it() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let (a_url, b_url) = (url("FERRUM_TEST_DATABASE_URL", "ferrum_test"), url("FERRUM_RESTORE_TEST_DATABASE_URL", "ferrum_restore_test"));
    let dirs = std::env::temp_dir().join(format!("ferrum-vault-follows-{}", Uuid::now_v7()));
    let a = installation(&a_url, SecretKey::new([1; 32]), &dirs.join("a")).await;
    let b = installation(&b_url, SecretKey::new([2; 32]), &dirs.join("b")).await;
    for u in [&a_url, &b_url] {
        as_superuser(u, "grant set on parameter session_replication_role to ferrum").await;
    }
    let mut needles: Vec<String> = Vec::new();

    // ── the server's handle: once, before any app ──
    let mut r = HookRegistry::default();
    let _taken = r.platform_vault();
    r.platform_vault();
    says(Error::Invalid(r.settle().unwrap_err().join("; ")), "asked for again");
    let mut r = HookRegistry::default();
    r.app("crm", &[], |h| {
        h.platform_vault();
    });
    assert!(r.settle().is_err(), "not inside an app's registration");

    // ── what a replica's environment holds is stored once, however many store it at once ──
    let mail = Rule { reveal: true, hosts: vec!["smtp.example.com".into()], ..Default::default() };
    let (one, two) = tokio::join!(
        a.platform.adopt(&a.k, Scope::Installation, "mailer_password", value("Mp-1357-from-env"), mail.clone(), "environment"),
        a.platform.adopt(&a.k, Scope::Installation, "mailer_password", value("Mp-1357-from-env"), mail.clone(), "environment"),
    );
    assert_eq!([one.unwrap(), two.unwrap()].iter().filter(|w| **w).count(), 1, "one of the two wrote it");
    assert!(
        !a.platform.adopt(&a.k, Scope::Installation, "mailer_password", value("Mp-2468-later"), mail.clone(), "environment").await.unwrap(),
        "what is there stays"
    );
    needles.extend(["Mp-1357-from-env".to_string(), "Mp-2468-later".to_string()]);
    let events = vault::events(&a.k.db, None, 100).await.unwrap();
    let adopted: Vec<&vault::Event> = events.iter().filter(|e| e.action == "adopt" && e.name == "mailer_password").collect();
    assert_eq!(adopted.len(), 1, "recorded once");
    assert_eq!((adopted[0].owner.as_str(), adopted[0].details["by"].clone()), ("platform", json!("environment")));
    // Read as the server starts, by its own handle of its own registry only.
    let (v, rule) = a.platform.reveal_at_start(&a.k.db, &a.k.hooks, "mailer_password").await.unwrap().expect("there");
    assert_eq!((shown(v).as_str(), rule.hosts), ("Mp-1357-from-env", vec!["smtp.example.com".to_string()]));
    assert!(a.platform.reveal_at_start(&a.k.db, &a.k.hooks, "nothing_here").await.unwrap().is_none());
    says(a.crm.reveal_at_start(&a.k.db, &a.k.hooks, "mailer_password").await.unwrap_err(), "only the server's own handle");
    let stranger = HookRegistry::default().platform_vault();
    says(stranger.reveal_at_start(&a.k.db, &a.k.hooks, "mailer_password").await.unwrap_err(), "only the server's own handle");

    // ── a plugin's handle: its secret goes on a request to the hosts it names, and is never shown ──
    says(a.crm.plugin("pay").unwrap_err(), "only the platform's handle");
    let pay = a.platform.plugin("pay").unwrap();
    let w = workspace(&a.k, "plugged").await;
    let boss = admin(&a.k, "plugged", w).await;
    let stripe = Rule { hosts: vec!["api.stripe.com".into()], ..Default::default() };
    a.k.vault_set(&boss, "plugin:pay", "stripe", value("sk_live_Pq-3141-plugin"), stripe).await.unwrap();
    needles.push("sk_live_Pq-3141-plugin".into());
    says(pay.reveal(&a.k, Scope::Workspace(w), "stripe").await.unwrap_err(), "a plugin's secrets are used, never revealed");
    let to = |u: &str| reqwest::Url::parse(u).unwrap();
    let (h, v) = pay.carry(&a.k, Scope::Workspace(w), "stripe", &to("https://api.stripe.com/v1/charges"), &Carry::Bearer).await.unwrap();
    assert_eq!((h.as_str(), v.as_str()), ("authorization", "Bearer sk_live_Pq-3141-plugin"));
    says(pay.carry(&a.k, Scope::Workspace(w), "stripe", &to("https://api.stripe.com.evil.example/"), &Carry::Bearer).await.unwrap_err(), "may not be sent");
    assert!(
        matches!(a.crm.carry(&a.k, Scope::Workspace(w), "stripe", &to("https://api.stripe.com/"), &Carry::Bearer).await, Err(Error::NotFound(_))),
        "another owner's"
    );
    let (_, basic) = vault::carried(&["*.example.com".into()], "API.example.com", &Carry::Basic("u".into()), "p").unwrap();
    assert_eq!(basic.as_str(), "Basic dTpw", "subdomains, in any case");
    assert!(vault::carried(&["*.example.com".into()], "example.com", &Carry::Bearer, "p").is_err(), "the domain itself is not one of its subdomains");
    assert!(vault::carried(&["example.com".into()], "example.com", &Carry::Bearer, "a\nb").is_err(), "one line");
    let header = Carry::Header { name: "x-api-key".into(), prefix: "Key ".into() };
    assert_eq!(vault::carried(&["example.com".into()], "example.com", &header, "k").unwrap().1.as_str(), "Key k");

    // ── the operator, through the platform's handle alone ──
    let changed = a.platform.operator_set(&a.k, "crm", "license", value("Lc-7777-operator"), Rule { reveal: true, ..Default::default() }).await.unwrap();
    needles.push("Lc-7777-operator".into());
    assert!(changed.shown.is_none());
    assert_eq!(shown(a.crm.reveal(&a.k, Scope::Installation, "license").await.unwrap()), "Lc-7777-operator", "the app uses what the operator set");
    says(a.crm.operator_set(&a.k, "crm", "license", value("x-y-z"), Rule::default()).await.unwrap_err(), "only the platform's handle");
    let listed = serde_json::to_string(&a.platform.installation_secrets(&a.k).await.unwrap()).unwrap();
    assert!(listed.contains("\"license\"") && listed.contains("\"mailer_password\"") && !listed.contains("Lc-7777") && !listed.contains("Mp-1357"), "{listed}");
    let by_operator = vault::events(&a.k.db, None, 100).await.unwrap().into_iter().find(|e| e.name == "license" && e.action == "set").unwrap();
    assert_eq!(by_operator.details["by"], json!("operator"));
    // Managed by its app: the operator's ways refuse it, naming the app, until the app lets go.
    a.crm.manage(&a.k, Scope::Installation, "license", true).await.unwrap();
    let managed = |e: Error| matches!(&e, Error::Conflict(m) if m.contains("managed by its app, crm"));
    assert!(managed(a.platform.operator_set(&a.k, "crm", "license", value("x"), Rule::default()).await.unwrap_err()));
    assert!(managed(a.platform.operator_rotate(&a.k, "crm", "license", value("x")).await.unwrap_err()));
    assert!(managed(a.platform.operator_rule(&a.k, "crm", "license", Rule::default()).await.unwrap_err()));
    assert!(managed(a.platform.operator_remove(&a.k, "crm", "license").await.unwrap_err()));
    assert!(a.platform.installation_secrets(&a.k).await.unwrap().iter().any(|d| d.name == "license" && d.managed_by.as_deref() == Some("crm")));
    a.crm.manage(&a.k, Scope::Installation, "license", false).await.unwrap();
    assert!(a.platform.operator_remove(&a.k, "crm", "license").await.unwrap());

    deleted(&a, &mut needles).await;
    sandboxed(&a, &mut needles).await;
    moved(&a, &b, &mut needles).await;

    // ── no table of either database holds a value, an unwrapped key or a transfer key ──
    let needles: Vec<String> = needles.into_iter().filter(|n| n.len() >= 12).collect();
    for i in [&a, &b] {
        for schema in ["public", "vault"] {
            assert_eq!(holding(&i.k.db, schema, &needles).await, Vec::<String>::new(), "{} {schema}", i.url);
        }
    }
    let _ = std::fs::remove_dir_all(&dirs);
}

/// A deleted workspace takes its keys and secrets with it, and the installation's record says so.
async fn deleted(a: &Installation, needles: &mut Vec<String>) {
    let w = workspace(&a.k, "gone").await;
    a.crm.set(&a.k, Scope::Workspace(w), "api", value("Gn-4242-deleted"), Rule::default()).await.unwrap();
    needles.push("Gn-4242-deleted".into());
    assert_eq!(rows_of(&a.k.db, w).await, (2, 1, 0), "its workspace key, crm's key, one secret");
    let mut tx = a.k.db.system_tx().await.unwrap();
    sqlx::query("update tenant set account_state = 'closed', closed_at = now() - interval '40 days', retain_until = current_date - 1 where id = $1")
        .bind(w)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    a.k.delete_tenant("gone", "gone", true, "kept long enough").await.unwrap();
    assert_eq!(rows_of(&a.k.db, w).await, (0, 0, 0), "nothing of it is left in the vault");
    let said = vault::events(&a.k.db, None, 100).await.unwrap().into_iter().find(|e| e.action == "workspace_vault_removed").expect("recorded");
    assert_eq!((said.details["workspace"].clone(), said.details["why"].clone(), said.details["secrets"].clone()), (json!(w), json!("deleted"), json!(1)));
}

/// A sandbox has each secret as its rule says, with keys of its own; resumed, it makes nothing twice; deleted, it takes
/// its vault with it.
async fn sandboxed(a: &Installation, needles: &mut Vec<String>) {
    let w = workspace(&a.k, "acme").await;
    let ws = Scope::Workspace(w);
    let (keep, regenerate) = (Rule { reveal: true, sandbox: Sandbox::Keep, ..Default::default() }, Rule { sandbox: Sandbox::Regenerate, ..Default::default() });
    a.crm.set(&a.k, ws, "kept", value("Kp-5151-kept-in-sandbox"), keep.clone()).await.unwrap();
    a.crm.set(&a.k, ws, "pair", New::Ed25519, keep).await.unwrap();
    a.crm.set(&a.k, ws, "made", New::Random(32), regenerate.clone()).await.unwrap();
    a.crm.set(&a.k, ws, "made-pair", New::Ed25519, regenerate).await.unwrap();
    a.crm.set(&a.k, ws, "blank", value("Bl-6161-never-copied"), Rule::default()).await.unwrap();
    needles.extend(["Kp-5151-kept-in-sandbox".to_string(), "Bl-6161-never-copied".to_string()]);
    let until = chrono::Utc::now().date_naive() + chrono::Duration::days(10);
    let copy = |resume| Copy { from: None, until, base_domain: None, not_sealed: vec![], resume, halt: None };
    a.k.copy_workspace("acme", "acme-try", &copy(false), &quiet).await.unwrap();
    let s = Scope::Workspace(id_of(&a.k, "acme-try").await);
    assert_eq!(shown(a.crm.reveal(&a.k, s, "kept").await.unwrap()), "Kp-5151-kept-in-sandbox", "kept");
    let public = |scope, name| {
        let crm = a.crm.clone();
        let k = a.k.clone();
        async move { crm.describe(&k, scope, name).await.unwrap().unwrap().public_key }
    };
    assert_eq!(public(s, "pair").await, public(ws, "pair").await, "a kept key pair is the same pair");
    assert_ne!(public(s, "made-pair").await, public(ws, "made-pair").await, "a regenerated pair is its own");
    let signed = |scope| {
        let crm = a.crm.clone();
        let k = a.k.clone();
        async move { crm.sign(&k, scope, "made", b"m").await.unwrap() }
    };
    assert_ne!(signed(s).await, signed(ws).await, "a regenerated value is its own");
    assert!(a.crm.describe(&a.k, s, "blank").await.unwrap().is_none(), "blank: not there");
    assert_ne!(a.crm.derive(&a.k, s, "links").await.unwrap().bytes(), a.crm.derive(&a.k, ws, "links").await.unwrap().bytes(), "keys of its own");
    a.k.copy_workspace("acme", "acme-try", &copy(true), &quiet).await.unwrap();
    assert_eq!(a.crm.describe(&a.k, s, "kept").await.unwrap().unwrap().version, 1, "resumed: nothing made twice");
    let Scope::Workspace(sandbox) = s else { unreachable!() };
    assert!(rows_of(&a.k.db, sandbox).await.1 >= 4);
    a.k.delete_sandbox("acme-try").await.unwrap();
    assert_eq!(rows_of(&a.k.db, sandbox).await, (0, 0, 0), "a deleted sandbox takes its vault with it");
    assert_eq!(shown(a.crm.reveal(&a.k, ws, "kept").await.unwrap()), "Kp-5151-kept-in-sandbox", "the live workspace's untouched");
}

/// The source's address, keys and files, for a move from `from`.
fn source(from: &Installation, keys: Vec<SecretKey>, stop: Option<Stop>) -> MoveSource {
    MoveSource {
        url: from.url.clone(),
        keys,
        files: Some(Arc::new(DirFiles(from.files.clone())) as Arc<dyn ferrum_kernel::files::FileStore>),
        regions: None,
        base_domain: None,
        wait: Duration::ZERO,
        not_sealed: vec![],
        stop_after: stop,
    }
}

async fn moved(a: &Installation, b: &Installation, needles: &mut Vec<String>) {
    let t = workspace(&a.k, "mover").await;
    let ws = Scope::Workspace(t);
    a.crm.set(&a.k, ws, "api", value("Mv-7070-carried-over"), Rule { reveal: true, ..Default::default() }).await.unwrap();
    a.crm.manage(&a.k, ws, "api", true).await.unwrap();
    a.crm.set(&a.k, ws, "hooks", New::Random(32), Rule { overlap_secs: 600, ..Default::default() }).await.unwrap();
    let signed = a.crm.sign(&a.k, ws, "hooks", b"payload").await.unwrap();
    needles.push("Mv-7070-carried-over".into());

    // Without a transfer, or with a wrong key: refused before anything is frozen.
    says(b.k.move_workspace("mover", &source(a, vec![a.key.clone()], None), Run::Start, &quiet).await.unwrap_err(), "make a transfer of them there");
    let transfer = vault::transfer_out(&a.k, t).await.unwrap();
    needles.push(transfer.to_string());
    let transfer = SecretKey::from_base64(&transfer).unwrap();
    needles.push(hex::encode(Zeroizing::new(base64::Engine::decode(&base64::engine::general_purpose::STANDARD, transfer.to_base64()).unwrap())));
    says(
        b.k.move_workspace("mover", &source(a, vec![a.key.clone(), SecretKey::new([9; 32])], None), Run::Start, &quiet).await.unwrap_err(),
        "is the key of this workspace's transfer",
    );
    // A key that changed after the transfer was made: made again.
    a.k.rotate_workspace_key(ws).await.unwrap();
    says(
        b.k.move_workspace("mover", &source(a, vec![a.key.clone(), transfer.clone()], None), Run::Start, &quiet).await.unwrap_err(),
        "make the transfer again",
    );
    let mut tx = a.k.db.system_tx().await.unwrap();
    let marks: i64 = sqlx::query_scalar("select count(*) from workspace_move where workspace = $1").bind(t).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(marks, 0, "nothing was frozen");
    let transfer = SecretKey::from_base64(&vault::transfer_out(&a.k, t).await.unwrap()).unwrap();
    needles.push(transfer.to_base64());
    let keys = vec![a.key.clone(), transfer.clone()];

    // Copied, then a change at the source: the copy is dropped, and copied afresh.
    b.k.move_workspace("mover", &source(a, keys.clone(), Some(Stop::Copied)), Run::Start, &quiet).await.unwrap();
    assert_eq!(rows_of(&b.k.db, t).await.0, 2, "its keys arrived with the copy: the workspace's and crm's");
    a.crm.set(&a.k, ws, "late", value("Lt-8080-after-the-copy"), Rule { reveal: true, ..Default::default() }).await.unwrap();
    needles.push("Lt-8080-after-the-copy".into());
    says(b.k.move_workspace("mover", &source(a, keys.clone(), None), Run::Resume, &quiet).await.unwrap_err(), "its vault");
    assert_eq!(rows_of(&b.k.db, t).await, (0, 0, 0), "the stale copy's vault went with it");
    // Undone before the point of no return: nothing of it stays here, the source keeps its own.
    b.k.move_workspace("mover", &source(a, keys.clone(), Some(Stop::Copied)), Run::Resume, &quiet).await.unwrap();
    b.k.move_workspace("mover", &source(a, keys.clone(), None), Run::Abort, &quiet).await.unwrap();
    assert_eq!(rows_of(&b.k.db, t).await, (0, 0, 0));
    assert_eq!(shown(a.crm.reveal(&a.k, ws, "late").await.unwrap()), "Lt-8080-after-the-copy");
    // Again, stopped after the copy as a crash would, and resumed: here it is, its secrets open with this root.
    b.k.move_workspace("mover", &source(a, keys.clone(), Some(Stop::Copied)), Run::Start, &quiet).await.unwrap();
    b.k.move_workspace("mover", &source(a, keys.clone(), None), Run::Resume, &quiet).await.unwrap();
    assert_eq!(shown(b.crm.reveal(&b.k, ws, "api").await.unwrap()), "Mv-7070-carried-over");
    assert_eq!(b.crm.describe(&b.k, ws, "api").await.unwrap().unwrap().managed_by.as_deref(), Some("crm"), "still its app's to manage");
    assert_eq!(shown(b.crm.reveal(&b.k, ws, "late").await.unwrap()), "Lt-8080-after-the-copy");
    assert!(b.crm.verify(&b.k, ws, "hooks", b"payload", &signed).await.unwrap(), "what it signed there verifies here");
    let arrived = vault::events(&b.k.db, None, 100).await.unwrap().into_iter().find(|e| e.action == "workspace_arrived").expect("recorded");
    assert_eq!(arrived.details["workspace"], json!(t));

    // Finished at the source: its copy there opens nothing, the transfer with it.
    assert!(rows_of(&a.k.db, t).await.0 > 0, "kept at the source until the move is finished");
    a.k.finish_move("mover", "checked at the target").await.unwrap();
    assert_eq!(rows_of(&a.k.db, t).await, (0, 0, 0));
    assert!(matches!(a.crm.reveal(&a.k, ws, "api").await, Err(Error::NotFound(_))));

    // And back again, with a transfer made where it is now.
    let back = SecretKey::from_base64(&vault::transfer_out(&b.k, t).await.unwrap()).unwrap();
    a.k.move_workspace("mover", &source(b, vec![b.key.clone(), back], None), Run::Start, &quiet).await.unwrap();
    assert_eq!(shown(a.crm.reveal(&a.k, ws, "api").await.unwrap()), "Mv-7070-carried-over", "back where it started");
    assert_eq!(a.crm.describe(&a.k, ws, "api").await.unwrap().unwrap().managed_by.as_deref(), Some("crm"), "and back again");
    b.k.finish_move("mover", "checked back home").await.unwrap();
    assert_eq!(rows_of(&b.k.db, t).await, (0, 0, 0));
}

/// The tables of `schema` that hold any of `needles` (each looked for as it is written in a row's text).
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
