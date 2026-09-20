//! Native-only lifecycle endpoints. The public marketplace tool surface does
//! not expose these controls; its checked executor binds them to one batch.

use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::{json, Value};
use std::sync::OnceLock;

pub struct BeginForegroundSegmentTool;
pub struct EndForegroundSegmentTool;

fn definition(end: bool) -> &'static ToolDef {
    static BEGIN: OnceLock<ToolDef> = OnceLock::new();
    static END: OnceLock<ToolDef> = OnceLock::new();
    let slot = if end { &END } else { &BEGIN };
    slot.get_or_init(|| {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "pid": {"type": "integer", "minimum": 1, "maximum": i32::MAX},
                "window_id": {"type": "integer", "minimum": 1, "maximum": u32::MAX}
            },
            "required": ["pid", "window_id"],
            "additionalProperties": false
        });
        if end {
            schema["properties"]["foreground_segment_id"] = json!({
                "type": "string", "minLength": 1, "maxLength": 128
            });
            schema["properties"]["mode"] = json!({
                "type": "string", "enum": ["finish", "abort"]
            });
            schema["required"] = json!(["pid", "window_id", "foreground_segment_id", "mode"]);
        }
        ToolDef {
            name: if end { "end_foreground_segment" } else { "begin_foreground_segment" }.into(),
            description: if end {
                "Settle this transport's exact native foreground segment. Finish may restore the original window only without intervention; abort never reclaims focus. Never replay an uncertain end."
            } else {
                "Reserve one exact native foreground batch segment for the current trusted transport. Captures original focus without activation or input. Only the same owner may use the returned token; end explicitly after settlement."
            }.into(),
            input_schema: schema,
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: false,
        }
    })
}

#[async_trait]
impl Tool for BeginForegroundSegmentTool {
    fn def(&self) -> &ToolDef {
        definition(false)
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        crate::foreground_activity::begin_segment(args).await
    }
}

#[async_trait]
impl Tool for EndForegroundSegmentTool {
    fn def(&self) -> &ToolDef {
        definition(true)
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        crate::foreground_activity::end_segment(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_segment_controls_require_exact_target_and_explicit_end_mode() {
        for end in [false, true] {
            let def = definition(end);
            assert!(!def.read_only && !def.destructive && !def.idempotent && !def.open_world);
            assert_eq!(def.input_schema["additionalProperties"], false);
            assert_eq!(def.input_schema["required"][0], "pid");
            assert_eq!(def.input_schema["required"][1], "window_id");
            assert!(def.input_schema["properties"].get("session_id").is_none());
            assert!(def.input_schema["properties"]
                .get("runtime_scope")
                .is_none());
        }
        assert_eq!(
            definition(true).input_schema["properties"]["mode"]["enum"],
            json!(["finish", "abort"])
        );
        assert_eq!(
            definition(true).input_schema["properties"]["foreground_segment_id"]["maxLength"],
            128
        );
    }
}
