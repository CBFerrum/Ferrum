//! The App Manager over MCP: AI agents list, plan, install, enable, disable and remove apps
//! under the same rules as people — administrators only, and an install needs the digest of the
//! plan that was reviewed. Changes run detached, so a client that disconnects cannot cut one off.

use crate::appmgr::{AppManager, Refusal};
use ferrum_kernel::{Ctx, Error, Kernel, Result};
use ferrum_mcp::{ToolProvider, tool_definition};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct AppTools(pub Arc<AppManager>);

async fn detached<T: Send + 'static>(work: impl Future<Output = Result<T>> + Send + 'static) -> Result<T> {
    tokio::spawn(work).await.map_err(|e| Error::internal(format!("app change: {e}")))?
}

fn to_value(v: impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(v).map_err(Error::internal)
}

#[async_trait::async_trait]
impl ToolProvider for AppTools {
    fn tools(&self, ctx: &Ctx) -> Vec<Value> {
        if !ctx.is_admin {
            return vec![];
        }
        let app = json!({"type": "string", "description": "App id, e.g. crm"});
        let version = json!({"type": "string", "description": "An exact version; the newest one that fits when omitted"});
        vec![
            tool_definition(
                "list_apps",
                "List apps",
                "Apps installed in this workspace (enabled or not, with any update available) and apps that can be installed from this server, uploads and app stores.",
                json!({}),
                &[],
                true,
                false,
                true,
            ),
            tool_definition(
                "plan_app",
                "Plan an app install",
                "What installing or upgrading an app would do: the apps it brings in dependency order, the permissions it asks for (and which are new), and the consent digest install_app needs. Show the permissions to the user and get their agreement before installing.",
                json!({"app": app, "version": version}),
                &["app"],
                true,
                false,
                true,
            ),
            tool_definition(
                "install_app",
                "Install or upgrade an app",
                "Carries out the plan from plan_app (upgrade: true for an installed app). consent must be that plan's digest; if the plan changed since, nothing is installed and the new plan comes back to review.",
                json!({"app": app, "version": version, "consent": {"type": "string", "description": "The digest of the plan the user agreed to"}, "upgrade": {"type": "boolean"}}),
                &["app", "consent"],
                false,
                false,
                false,
            ),
            tool_definition(
                "set_app_enabled",
                "Enable or disable an app",
                "Disabling takes an app's records and screens out of the workspace and turns its plugin off, keeping its data and who had which role; enabling brings everything back.",
                json!({"app": app, "enabled": {"type": "boolean"}}),
                &["app", "enabled"],
                false,
                false,
                true,
            ),
            tool_definition(
                "uninstall_app",
                "Remove an app",
                "Removes an app from this workspace. With delete_data: true its records are deleted for good — ask the user first.",
                json!({"app": app, "delete_data": {"type": "boolean"}}),
                &["app"],
                false,
                true,
                false,
            ),
        ]
    }

    async fn call(&self, _k: &Kernel, ctx: &Ctx, name: &str, args: &Value) -> Option<Result<Value>> {
        let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
        let flag = |key: &str| args.get(key).and_then(Value::as_bool).unwrap_or(false);
        let app = || text("app").ok_or_else(|| Error::field("app", "is required"));
        let (apps, ctx) = (self.0.clone(), ctx.clone());
        Some(match name {
            "list_apps" => apps.overview(&ctx).await,
            "plan_app" => match app() {
                Ok(id) => apps.plan(&ctx, &id, text("version").as_deref()).await.and_then(to_value),
                Err(e) => Err(e),
            },
            "install_app" => {
                let (id, consent) = match (app(), text("consent").ok_or_else(|| Error::field("consent", "is required (the digest from plan_app)"))) {
                    (Ok(id), Ok(c)) => (id, c),
                    (Err(e), _) | (_, Err(e)) => return Some(Err(e)),
                };
                let (version, upgrade) = (text("version"), flag("upgrade"));
                detached(async move {
                    match apps.install(&ctx, &id, version.as_deref(), &consent, upgrade).await {
                        Ok(plan) => Ok(json!({"installed": true, "plan": to_value(plan)?})),
                        Err(Refusal::Stale(plan)) => Ok(json!({"installed": false, "reason": plan.changed_message(), "plan": to_value(plan)?})),
                        Err(Refusal::Failed(e)) => Err(e),
                    }
                })
                .await
            }
            "set_app_enabled" => match (app(), args.get("enabled").and_then(Value::as_bool)) {
                (Ok(id), Some(enabled)) => {
                    detached(async move { apps.set_enabled(&ctx, &id, enabled).await.map(|()| json!({"app": id, "enabled": enabled})) }).await
                }
                (Err(e), _) => Err(e),
                (_, None) => Err(Error::field("enabled", "is required")),
            },
            "uninstall_app" => match app() {
                Ok(id) => {
                    let delete_data = flag("delete_data");
                    detached(async move { apps.uninstall(&ctx, &id, delete_data).await.map(|deleted| json!({"removed": id, "deleted_data": deleted})) }).await
                }
                Err(e) => Err(e),
            },
            _ => return None,
        })
    }
}
