use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;

pub struct ListWindowsTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "list_windows".into(),
        description: "List public layer-0 top-level windows currently known to WindowServer. \
            Includes off-screen windows (minimized, on another Space, hidden-launched). \
            Private trusted system helpers such as the AppKit Open/Save panel service are omitted; \
            observe those only through the original host application's app_context. \
            Use this to find a window_id before calling get_window_state.\n\n\
            Per-record fields: window_id, pid, app_name, title, bounds \
            (x/y/width/height, top-left origin), z_index (integer or null; higher values are \
            closer to the front; null means stacking order is unavailable and callers must not \
            infer one), is_on_screen, space_ids, current_space_id (the active Space on that \
            window's display), and on_current_space. Set include_lifecycle_evidence:true with \
            an exact pid to add a fresh per-window AX membership/minimized probe. This can \
            distinguish a live minimized or off-Space AX window from a WindowServer-only row. \
            WindowServer-only is evidence, never generic proof that a window closed. The top-level current_space_id is \
            WindowServer's main/global active Space and can differ from a record's \
            current_space_id when displays use independent Spaces. To select a frontmost candidate, take the \
            maximum integer z_index; if every value is null, use an explicit fallback instead of \
            relying on array order.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "pid": {
                    "type": "integer",
                    "description": "Optional pid filter. When set, only this pid's windows are returned."
                },
                "on_screen_only": {
                    "type": "boolean",
                    "description": "When true, drop windows not on the current Space. Default false."
                },
                "include_lifecycle_evidence": {
                    "type": "boolean",
                    "description": "When true, requires pid and adds lifecycle_evidence from one fresh AXWindows query for that process. Default false."
                }
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
impl Tool for ListWindowsTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid_filter: Option<i32> = args.opt_i64("pid").map(|v| v as i32);
        let on_screen_only = args.bool_or("on_screen_only", false);
        let include_lifecycle_evidence = args.bool_or("include_lifecycle_evidence", false);
        if include_lifecycle_evidence && pid_filter.is_none() {
            return ToolResult::error(
                "include_lifecycle_evidence requires an exact pid so the bounded AX probe cannot fan out across every application.",
            )
            .with_structured(serde_json::json!({
                "code": "lifecycle_evidence_requires_pid",
                "effect": "refused",
                "retryable": true,
                "suggestion": "Call list_windows again with pid and include_lifecycle_evidence:true."
            }));
        }

        let enumeration = if on_screen_only {
            crate::windows::visible_automation_windows_with_space_snapshot()
        } else {
            crate::windows::all_automation_windows_with_space_snapshot()
        };
        let current_space_id = enumeration.current_space_id;
        let mut windows = enumeration.windows;

        // The AppKit Open/Save XPC service is an implementation detail, not a
        // public application/window target. Use the cheap bundle/path/name
        // predicate here; full signature and AX validation belongs only to an
        // app-context observation that can establish the host relationship.
        let mut helper_pids = std::collections::HashMap::new();
        windows = filter_private_helper_windows(windows, |window| {
            *helper_pids.entry(window.pid).or_insert_with(|| {
                crate::ax::app_context::hide_open_save_panel_from_inventory(
                    window.pid,
                    &window.app_name,
                )
            })
        });

        if let Some(pid) = pid_filter {
            windows.retain(|w| w.pid == pid);
        }

        let lifecycle_evidence = if let Some(pid) =
            pid_filter.filter(|_| include_lifecycle_evidence)
        {
            let window_ids = windows
                .iter()
                .map(|window| window.window_id)
                .collect::<Vec<_>>();
            let fallback_ids = window_ids.clone();
            Some(
                match tokio::task::spawn_blocking(move || {
                    crate::ax::exact_target::gather_ax_window_lifecycle_evidence(pid, window_ids)
                })
                .await
                {
                    Ok(evidence) => evidence,
                    Err(error) => {
                        tracing::warn!(
                            "list_windows lifecycle evidence task failed for pid {pid}: {error}"
                        );
                        fallback_ids
                            .into_iter()
                            .map(|window_id| {
                                (
                                    window_id,
                                    crate::ax::exact_target::AxWindowLifecycleEvidence::AxUnavailable {
                                        app_hidden: None,
                                        query_succeeded: false,
                                    },
                                )
                            })
                            .collect()
                    }
                },
            )
        } else {
            None
        };
        let windows_json: Vec<Value> = windows
            .iter()
            .map(|window| {
                let evidence = lifecycle_evidence
                    .as_ref()
                    .and_then(|evidence| evidence.get(&window.window_id))
                    .copied();
                window_record_with_lifecycle_evidence(window, evidence)
            })
            .collect();

        ToolResult::text(format!("Found {} window(s).", windows_json.len())).with_structured(
            serde_json::json!({
                "windows": windows_json,
                "current_space_id": current_space_id
            }),
        )
    }
}

pub(super) fn filter_private_helper_windows(
    windows: Vec<crate::windows::WindowInfo>,
    mut is_private_helper: impl FnMut(&crate::windows::WindowInfo) -> bool,
) -> Vec<crate::windows::WindowInfo> {
    windows
        .into_iter()
        .filter(|window| !is_private_helper(window))
        .collect()
}

fn lifecycle_evidence_json(evidence: crate::ax::exact_target::AxWindowLifecycleEvidence) -> Value {
    use crate::ax::exact_target::AxWindowLifecycleEvidence;

    match evidence {
        AxWindowLifecycleEvidence::AxPresent {
            minimized,
            app_hidden,
            snapshot_complete,
        } => serde_json::json!({
            "state": "ax_window_live",
            "window_server_present": true,
            "ax_query_succeeded": true,
            "ax_snapshot_complete": snapshot_complete,
            "ax_window_present": true,
            "minimized": minimized,
            "app_hidden": app_hidden,
            "source": "fresh_ax_windows"
        }),
        AxWindowLifecycleEvidence::WindowServerOnly { app_hidden } => serde_json::json!({
            "state": "window_server_only",
            "window_server_present": true,
            "ax_query_succeeded": true,
            "ax_snapshot_complete": true,
            "ax_window_present": false,
            "minimized": null,
            "app_hidden": app_hidden,
            "source": "fresh_ax_windows"
        }),
        AxWindowLifecycleEvidence::AxUnavailable {
            app_hidden,
            query_succeeded,
        } => serde_json::json!({
            "state": "unknown",
            "window_server_present": true,
            "ax_query_succeeded": query_succeeded,
            "ax_snapshot_complete": false,
            "ax_window_present": null,
            "minimized": null,
            "app_hidden": app_hidden,
            "source": "fresh_ax_windows"
        }),
    }
}

fn window_record_with_lifecycle_evidence(
    window: &crate::windows::WindowInfo,
    evidence: Option<crate::ax::exact_target::AxWindowLifecycleEvidence>,
) -> Value {
    let mut record = window_record_json(window);
    if let Some(evidence) = evidence {
        record["lifecycle_evidence"] = lifecycle_evidence_json(evidence);
    }
    record
}

pub(super) fn window_record_json(w: &crate::windows::WindowInfo) -> Value {
    serde_json::json!({
        "window_id": w.window_id,
        "pid": w.pid,
        "app_name": w.app_name,
        "title": w.title,
        "bounds": {
            "x": w.bounds.x,
            "y": w.bounds.y,
            "width": w.bounds.width,
            "height": w.bounds.height
        },
        "layer": w.layer,
        "z_index": w.z_index,
        "is_on_screen": w.is_on_screen,
        "current_space_id": w.current_space_id,
        "on_current_space": w.on_current_space,
        "space_ids": w.space_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::exact_target::AxWindowLifecycleEvidence;

    fn window(window_id: u32, pid: i32, app_name: &str) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id,
            pid,
            app_name: app_name.into(),
            title: "Document".into(),
            bounds: crate::windows::WindowBounds {
                x: 1.0,
                y: 2.0,
                width: 300.0,
                height: 200.0,
            },
            layer: 0,
            z_index: 7,
            is_on_screen: true,
            current_space_id: Some(1),
            on_current_space: Some(true),
            space_ids: Some(vec![1]),
        }
    }

    #[test]
    fn private_helper_windows_are_removed_from_public_inventory() {
        let windows = vec![window(1, 10, "Editor"), window(2, 20, "Private Helper")];
        let public = filter_private_helper_windows(windows, |window| window.pid == 20);
        assert_eq!(public.len(), 1);
        assert_eq!(public[0].window_id, 1);
    }

    #[test]
    fn window_record_includes_observed_z_index() {
        let window = crate::windows::WindowInfo {
            window_id: 42,
            pid: 123,
            app_name: "Example".into(),
            title: "Document".into(),
            bounds: crate::windows::WindowBounds {
                x: 1.0,
                y: 2.0,
                width: 300.0,
                height: 200.0,
            },
            layer: 0,
            z_index: 7,
            is_on_screen: true,
            current_space_id: Some(1),
            on_current_space: Some(true),
            space_ids: Some(vec![1]),
        };

        assert_eq!(window_record_json(&window)["z_index"], serde_json::json!(7));
        assert_eq!(
            window_record_json(&window)["current_space_id"],
            serde_json::json!(1)
        );
        assert_eq!(
            window_record_json(&window)["on_current_space"],
            serde_json::json!(true)
        );
        assert!(window_record_json(&window)
            .get("lifecycle_evidence")
            .is_none());
    }

    #[test]
    fn lifecycle_evidence_is_explicit_without_overclaiming_closure() {
        let live = lifecycle_evidence_json(AxWindowLifecycleEvidence::AxPresent {
            minimized: Some(true),
            app_hidden: Some(false),
            snapshot_complete: true,
        });
        assert_eq!(live["state"], "ax_window_live");
        assert_eq!(live["ax_query_succeeded"], true);
        assert_eq!(live["ax_snapshot_complete"], true);
        assert_eq!(live["ax_window_present"], true);
        assert_eq!(live["minimized"], true);
        assert_eq!(live["app_hidden"], false);

        let server_only = lifecycle_evidence_json(AxWindowLifecycleEvidence::WindowServerOnly {
            app_hidden: Some(false),
        });
        assert_eq!(server_only["state"], "window_server_only");
        assert_eq!(server_only["ax_query_succeeded"], true);
        assert_eq!(server_only["ax_window_present"], false);
        assert!(server_only.get("closed").is_none());

        let unknown = lifecycle_evidence_json(AxWindowLifecycleEvidence::AxUnavailable {
            app_hidden: None,
            query_succeeded: false,
        });
        assert_eq!(unknown["state"], "unknown");
        assert_eq!(unknown["ax_query_succeeded"], false);
        assert_eq!(unknown["ax_snapshot_complete"], false);
        assert!(unknown["ax_window_present"].is_null());
        assert!(unknown["app_hidden"].is_null());
    }

    #[test]
    fn offspace_and_minimized_windows_remain_explicitly_live_when_ax_present() {
        let mut window = crate::windows::WindowInfo {
            window_id: 42,
            pid: 123,
            app_name: "Terminal".into(),
            title: "shell".into(),
            bounds: crate::windows::WindowBounds {
                x: 1.0,
                y: 2.0,
                width: 300.0,
                height: 200.0,
            },
            layer: 0,
            z_index: 7,
            is_on_screen: false,
            current_space_id: Some(1),
            on_current_space: Some(false),
            space_ids: Some(vec![2]),
        };
        let offspace = window_record_with_lifecycle_evidence(
            &window,
            Some(AxWindowLifecycleEvidence::AxPresent {
                minimized: Some(false),
                app_hidden: Some(false),
                snapshot_complete: true,
            }),
        );
        assert_eq!(offspace["is_on_screen"], false);
        assert_eq!(offspace["on_current_space"], false);
        assert_eq!(offspace["lifecycle_evidence"]["state"], "ax_window_live");

        window.on_current_space = Some(true);
        window.space_ids = Some(vec![1]);
        let minimized = window_record_with_lifecycle_evidence(
            &window,
            Some(AxWindowLifecycleEvidence::AxPresent {
                minimized: Some(true),
                app_hidden: Some(false),
                snapshot_complete: true,
            }),
        );
        assert_eq!(minimized["is_on_screen"], false);
        assert_eq!(minimized["on_current_space"], true);
        assert_eq!(minimized["lifecycle_evidence"]["state"], "ax_window_live");
        assert_eq!(minimized["lifecycle_evidence"]["minimized"], true);
    }
}
