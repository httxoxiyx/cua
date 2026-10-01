//! `check_for_update` MCP tool — programmatic mirror of
//! `cua-driver check-update`.
//!
//! Update checks are disabled in this build (see [`crate::version_check`]).
//! The tool stays registered so the tool roster, capability vocabulary, and
//! authorization tables keep the same names, but it only returns the static
//! "update checks are disabled" result: it makes no network request and reads
//! or writes no files.
//!
//! Registered by [`register_into`] immediately after the per-platform
//! `register_tools()` call, so the result is identical to the per-platform
//! tools registering it themselves.

use async_trait::async_trait;
use serde_json::Value;

use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef, ToolRegistry},
};

pub struct CheckForUpdateTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "check_for_update".into(),
        description: "Report this build's update-check policy. Update checks are disabled \
             in this build: the tool never contacts the network and always returns \
             `update_available: false` with a message to update through your distribution \
             channel. Read-only. Mirror of `cua-driver check-update --json`."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        read_only: true,
        destructive: false,
        idempotent: true,
        // The response is static and nothing outside the process is consulted.
        open_world: false,
    })
}

/// The static `check_for_update` result: no network, no filesystem access.
fn update_check_result() -> ToolResult {
    let state = crate::version_check::update_state();
    let structured = serde_json::to_value(&state).unwrap_or_else(|_| serde_json::json!({}));
    ToolResult::text(state.message).with_structured(structured)
}

#[async_trait]
impl Tool for CheckForUpdateTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        update_check_result()
    }
}

/// Register the `check_for_update` tool into a freshly-built platform
/// registry. Call site lives in `main.rs`; this keeps the per-platform
/// `register_tools()` signature untouched.
pub fn register_into(registry: &mut ToolRegistry) {
    registry.register(Box::new(CheckForUpdateTool));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_for_update_returns_the_static_disabled_result() {
        let result = serde_json::to_value(update_check_result()).unwrap();
        assert_ne!(result["isError"], true, "{result}");
        assert_eq!(
            result["content"][0]["text"],
            crate::version_check::DISABLED_MESSAGE
        );
        assert_eq!(result["structuredContent"]["update_available"], false);
        assert_eq!(result["structuredContent"]["source"], "disabled");
        assert!(!def().open_world);
        assert!(def().read_only);
    }
}
