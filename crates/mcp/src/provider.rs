//! Tools a host adds beside the built-in ones.

use crate::tools::tool;
use ferrum_kernel::{Ctx, Error, Kernel};
use serde_json::Value;

/// Tools a host adds beside the built-in ones (the server's App Manager): listed in `tools/list`
/// and called by name. Failures reach the model as tool results, like the built-in tools'.
#[async_trait::async_trait]
pub trait ToolProvider: Send + Sync {
    /// This caller's tools, each shaped like [`tool_definition`]'s result.
    fn tools(&self, ctx: &Ctx) -> Vec<Value>;
    /// Runs `name` if it is one of this provider's tools (`None` otherwise).
    async fn call(&self, k: &Kernel, ctx: &Ctx, name: &str, args: &Value) -> Option<Result<Value, Error>>;
}

/// A tool definition with its JSON Schema input and behaviour hints (for [`ToolProvider`]s).
#[allow(clippy::too_many_arguments)]
pub fn tool_definition(
    name: &str,
    title: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
    destructive: bool,
    idempotent: bool,
) -> Value {
    tool(name, title, description, properties, required, read_only, destructive, idempotent)
}
