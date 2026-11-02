//! What an app declares it needs, and who stands behind that. The platform only knows the declaration
//! and asks: a server without something to provide resources refuses apps that require them, instead of
//! running them short of what they depend on.
//!
//! A plugin reaches a resource through [`Resources::query`] and [`Resources::execute`] by the name it
//! declared. Which workspace and which app are asking comes from the host, never from the plugin, so a
//! handle means nothing outside the app that holds it.

use crate::{Ctx, Kernel, Result};
use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

#[async_trait]
pub trait Resources: Send + Sync {
    /// Says what an app needs. Saying it again changes nothing, and lifts a [`Resources::withdraw`].
    async fn declare(&self, k: &Kernel, ctx: &Ctx, app: &str, declared: &[Value]) -> Result<()>;

    /// Which of these required names cannot be used yet.
    async fn missing(&self, k: &Kernel, tenant: Uuid, app: &str, required: &[String]) -> Result<Vec<String>>;

    /// The app has stopped, or gone. New access ends at once. Data stays, whatever becomes of the app.
    async fn withdraw(&self, k: &Kernel, ctx: &Ctx, app: &str, uninstalled: bool) -> Result<()>;

    /// Rows, as JSON objects, within the limits of the resource's broker.
    async fn query(&self, k: &Kernel, tenant: Uuid, app: &str, handle: &str, sql: &str, params: &[Value]) -> Result<Value>;

    /// How many rows a statement changed.
    async fn execute(&self, k: &Kernel, tenant: Uuid, app: &str, handle: &str, sql: &str, params: &[Value]) -> Result<u64>;
}
