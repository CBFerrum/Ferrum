//! This build's server with a stand-in for the Cloud app beside it, for scripts/oidc_rehearsal.sh: the stand-in registers
//! the forge's companion as Cloud would (the right that lets a person in, the forge's account name as a claim), and
//! declares its client for a workspace from its own code with its own vault handle, as Cloud's controller would.
//!
//!   oidc_rehearsal start | task …                         the server, as `ferrum` runs it, with the stand-in registered
//!   oidc_rehearsal setup <workspace> <redirect_uri>       declares the forge's client there, and adds a person the
//!                                                         forge lets in and one it does not; prints the client, its
//!                                                         secret and the forge's account names, as JSON
//!   oidc_rehearsal deactivate <workspace> <email>         deactivates a person, as an administrator does
//!
//! Run from the server's directory, with `LOCO_ENV` and `DATABASE_URL` set, as the binary is.

use async_trait::async_trait;
use ferrum_kernel::oidc::Companion;
use ferrum_kernel::vault::{Scope, Vault};
use ferrum_kernel::{Action, HookRegistry, Kernel};
use ferrum_server::app::{App, EXTRA_APPS};
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};
use uuid::Uuid;

static FORGE: OnceLock<Vault> = OnceLock::new();

/// The forge's account name for a person of a workspace, as the Cloud app keeps it (its `forge_login`).
fn forge_login(tenant: Uuid, user: Uuid) -> String {
    format!("u-{}", &hex::encode(Sha256::digest(format!("{tenant}|{user}")))[..20])
}

struct Forge;

#[async_trait]
impl Companion for Forge {
    fn right(&self) -> (&str, Action) {
        ("LetterHead", Action::Read)
    }

    async fn claims(&self, _k: &Kernel, tenant: Uuid, user: Uuid) -> ferrum_kernel::Result<Map<String, Value>> {
        let login = forge_login(tenant, user);
        // An address only this account has: the forge links a new subject by name, and else by address (docs/api.md).
        Ok(json!({"preferred_username": login, "email": format!("{login}@people.ferrum.invalid")}).as_object().cloned().unwrap_or_default())
    }
}

fn stand_in(h: &mut HookRegistry) {
    h.app("rehearsal", &[], |h| {
        let _ = FORGE.set(h.vault());
        h.companion("forge", Arc::new(Forge));
    });
}

async fn kernel() -> Result<Arc<Kernel>, String> {
    let env = Environment::from(loco_rs::environment::resolve_from_env());
    let config = env.load_from_folder(std::path::Path::new("config")).map_err(|e| e.to_string())?;
    let ctx = <App as Hooks>::after_context(AppContext::builder(env, Default::default(), config).build()).await.map_err(|e| e.to_string())?;
    Ok(ctx.shared_store.get::<AppState>().ok_or("no state")?.kernel.clone())
}

async fn setup(slug: &str, redirect: &str) -> Result<Value, String> {
    let k = kernel().await?;
    let (tenant, _) = k.tenant_by_slug(slug).await.map_err(|e| e.to_string())?;
    let forge = FORGE.get().ok_or("the stand-in did not register")?;
    let client = forge.companion(&k, tenant, "forge", &[redirect.to_string()]).await.map_err(|e| e.to_string())?;
    let secret = forge.reveal(&k, Scope::Workspace(tenant), &client.secret).await.map_err(|e| e.to_string())?;
    let admin_email = format!("admin@{slug}.test");
    let s = k.login(slug, &admin_email, "Correct-Horse-9").await.map_err(|e| e.to_string())?;
    let admin = k.load_ctx(tenant, s.user, None, "rehearsal".into()).await.map_err(|e| e.to_string())?;
    // Someone the app lets in (a role that reads letter heads) with no account at the forge yet, and someone it does not.
    let rule = serde_json::from_value(json!({"entity": "LetterHead", "read": true})).map_err(|e| e.to_string())?;
    k.set_role_permission(&admin, "Developer", &rule).await.map_err(|e| e.to_string())?;
    let dev =
        k.create_user(&admin, &format!("dev@{slug}.test"), "Dev", Some("Correct-Horse-9"), &["Developer".into()], false).await.map_err(|e| e.to_string())?;
    let member = k.create_user(&admin, &format!("member@{slug}.test"), "Member", Some("Correct-Horse-9"), &[], false).await.map_err(|e| e.to_string())?;
    Ok(json!({
        "client_id": client.client_id,
        "secret": String::from_utf8(secret.to_vec()).map_err(|e| e.to_string())?,
        "admin_login": forge_login(tenant, s.user),
        "dev_login": forge_login(tenant, dev),
        "member_login": forge_login(tenant, member),
    }))
}

async fn deactivate(slug: &str, email: &str) -> Result<Value, String> {
    let k = kernel().await?;
    let (tenant, _) = k.tenant_by_slug(slug).await.map_err(|e| e.to_string())?;
    let s = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.map_err(|e| e.to_string())?;
    let admin = k.load_ctx(tenant, s.user, None, "rehearsal".into()).await.map_err(|e| e.to_string())?;
    let mut tx = k.db.system_tx().await.map_err(|e| e.to_string())?;
    let user: Uuid = sqlx::query_scalar("select id from app_user where tenant_id = $1 and email = $2")
        .bind(tenant)
        .bind(email)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    k.set_user_active(&admin, user, false).await.map_err(|e| e.to_string())?;
    Ok(json!({"deactivated": email}))
}

fn main() -> loco_rs::Result<()> {
    let _ = EXTRA_APPS.set(stand_in);
    let args: Vec<String> = std::env::args().collect();
    let (Some(verb), Some(a), Some(b)) = (args.get(1), args.get(2), args.get(3)) else { return ferrum_server::run() };
    let work = match verb.as_str() {
        "setup" => setup(a, b),
        "deactivate" => return run(deactivate(a, b)),
        _ => return ferrum_server::run(),
    };
    run(work)
}

fn run(work: impl std::future::Future<Output = Result<Value, String>>) -> loco_rs::Result<()> {
    let rt =
        tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().map_err(|e| loco_rs::Error::Message(e.to_string()))?;
    match rt.block_on(work) {
        Ok(v) => {
            println!("{v}");
            Ok(())
        }
        Err(e) => Err(loco_rs::Error::Message(e)),
    }
}
