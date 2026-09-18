use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use cua_driver_core::{
    protocol::{Content, ToolResult},
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use super::{ToolState, ZoomContext};

pub struct ZoomTool {
    pub state: Arc<ToolState>,
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "zoom".into(),
        description: "Capture a cropped JPEG of a window region (x1,y1)–(x2,y2) in screenshot \
            pixel coordinates, with 20% padding added on each side. The output image is at most \
            500 px wide.\n\n\
            After a zoom, pass `from_zoom=true` to click/type_text to auto-translate coordinates \
            back to full-window space.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["window_id", "x1", "y1", "x2", "y2"],
            "properties": {
                "window_id": { "type": "integer", "description": "CGWindowID from list_windows." },
                "pid":       { "type": "integer", "description": "Target pid — required for from_zoom click/type translation." },
                "x1": { "type": "number", "description": "Left edge of region in screenshot pixels." },
                "y1": { "type": "number", "description": "Top edge of region in screenshot pixels." },
                "x2": { "type": "number", "description": "Right edge of region in screenshot pixels." },
                "y2": { "type": "number", "description": "Bottom edge of region in screenshot pixels." }
            },
            "additionalProperties": false
        }),
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for ZoomTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let window_id = match args.require_u32("window_id") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let pid = args.opt_i64("pid").map(|v| v as i32);
        let delegation_route = crate::ax::app_context::delegation_route_from_args(&args);
        let x1 = match args.require_f64("x1") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let y1 = match args.require_f64("y1") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let x2 = match args.require_f64("x2") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let y2 = match args.require_f64("y2") {
            Ok(v) => v,
            Err(e) => return e,
        };

        if x2 <= x1 || y2 <= y1 {
            return ToolResult::error("x2 must be > x1 and y2 must be > y1");
        }

        let window =
            match tokio::task::spawn_blocking(move || crate::windows::window_info_by_id(window_id))
                .await
            {
                Ok(Some(window)) => window,
                _ => return ToolResult::error(
                    "The requested zoom window is no longer available. Re-observe the application.",
                ),
            };
        if pid.is_some_and(|pid| pid != window.pid) {
            return super::explicit_window_owner_mismatch_refusal();
        }
        let pre_capture_pid = window.pid;
        let pre_capture_is_helper = crate::ax::app_context::hide_open_save_panel_from_inventory(
            window.pid,
            &window.app_name,
        );
        if pre_capture_is_helper {
            let Some(route) = delegation_route.as_ref() else {
                return super::app_context_delegation_direct_target_refusal();
            };
            if !delegation_authorizes_window(route, window.pid, window_id)
                || !crate::ax::app_context::delegation_route_is_live(route)
            {
                return super::app_context_delegation_stale_refusal();
            }
        }

        let state = self.state.clone();
        let result = tokio::task::spawn_blocking(move || {
            let png_bytes = crate::capture::screenshot_window_bytes(window_id)?;
            cursor_overlay::capture_utils::crop_png_to_jpeg(&png_bytes, x1, y1, x2, y2, 500)
        })
        .await;

        match result {
            Ok(Ok(crop)) => {
                // A CGWindowID can be recycled while capture is in flight.
                // Re-check exact ownership and delegated host association
                // before publishing pixels or a zoom coordinate transform.
                let post_window = tokio::task::spawn_blocking(move || {
                    crate::windows::window_info_by_id(window_id)
                })
                .await;
                let post_window = match post_window {
                    Ok(Some(window)) if window.pid == pre_capture_pid => window,
                    _ => return super::explicit_window_owner_mismatch_refusal(),
                };
                let post_is_helper = crate::ax::app_context::hide_open_save_panel_from_inventory(
                    post_window.pid,
                    &post_window.app_name,
                );
                if !zoom_post_capture_target_matches(
                    pre_capture_pid,
                    pre_capture_is_helper,
                    post_window.pid,
                    post_is_helper,
                ) {
                    return super::explicit_window_owner_mismatch_refusal();
                }
                if pre_capture_is_helper
                    && !delegation_route.as_ref().is_some_and(|route| {
                        delegation_authorizes_window(route, post_window.pid, window_id)
                            && crate::ax::app_context::delegation_route_is_live(route)
                    })
                {
                    return super::app_context_delegation_stale_refusal();
                }
                // Store zoom context so from_zoom clicks can translate back.
                if let Some(p) = pid {
                    state.zoom_registry.set(
                        p,
                        ZoomContext {
                            origin_x: crop.origin_x,
                            origin_y: crop.origin_y,
                            scale_inv: crop.scale_inv,
                        },
                    );
                }
                let (w, h) = (crop.out_w, crop.out_h);
                let b64 = BASE64.encode(&crop.jpeg_bytes);
                ToolResult {
                    content: vec![
                        Content::image_jpeg(b64),
                        Content::text(format!(
                            "Zoom region ({x1:.0},{y1:.0})–({x2:.0},{y2:.0}) → {w}×{h} px JPEG."
                        )),
                    ],
                    is_error: None,
                    structured_content: Some(serde_json::json!({
                        // `format` stays for back-compat. `mime_type` is the
                        // Surface-7 addition that mirrors the MCP image part's
                        // `mimeType` onto the structured payload, so consumers
                        // don't have to translate "jpeg" → "image/jpeg" or
                        // sniff base64 magic bytes.
                        "width": w, "height": h, "format": "jpeg",
                        "mime_type": "image/jpeg"
                    })),
                    action_record: None,
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Zoom failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

fn delegation_authorizes_window(
    route: &crate::ax::app_context::AppContextDelegationRoute,
    pid: i32,
    window_id: u32,
) -> bool {
    route.delegation.target == crate::ax::app_context::AppContextTarget { pid, window_id }
}

fn zoom_post_capture_target_matches(
    pre_pid: i32,
    pre_is_helper: bool,
    post_pid: i32,
    post_is_helper: bool,
) -> bool {
    pre_pid == post_pid && pre_is_helper == post_is_helper
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegated_zoom_is_bound_to_exact_helper_target() {
        let route = crate::ax::app_context::AppContextDelegationRoute {
            session: crate::transient_ui::TransientSessionKey::Anonymous,
            expected_host_identity: crate::ax::app_context::ExpectedAppIdentity {
                bundle_id: Some("com.example.host".into()),
                app_name: Some("Host".into()),
            },
            delegation: crate::ax::app_context::AppContextDelegation {
                host_pid: 42,
                target: crate::ax::app_context::AppContextTarget {
                    pid: 900,
                    window_id: 77,
                },
                panel_kind: crate::ax::app_context::OpenSavePanelKind::Open,
            },
            generation: 1,
        };
        assert!(delegation_authorizes_window(&route, 900, 77));
        assert!(!delegation_authorizes_window(&route, 901, 77));
        assert!(!delegation_authorizes_window(&route, 900, 78));
    }

    #[test]
    fn zoom_rejects_window_id_reuse_or_helper_classification_change() {
        assert!(zoom_post_capture_target_matches(900, true, 900, true));
        assert!(!zoom_post_capture_target_matches(900, true, 901, true));
        assert!(!zoom_post_capture_target_matches(900, false, 900, true));
    }
}
