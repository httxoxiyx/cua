use cua_driver_core::protocol::ToolResult;
use serde_json::json;

pub(crate) fn exact_window_ownership_result(
    pid: u32,
    window_id: u64,
    owner_pid: Option<u32>,
) -> Result<(), ToolResult> {
    match owner_pid {
        Some(owner_pid) if owner_pid == pid => Ok(()),
        Some(owner_pid) => Err(ToolResult::error(format!(
            "window_id {window_id} belongs to pid {owner_pid}, not pid {pid}."
        ))
        .with_structured(json!({
            "code": "window_target_mismatch",
            "effect": "refused",
            "pid": pid,
            "window_id": window_id,
            "owner_pid": owner_pid,
        }))),
        None => Err(ToolResult::error(format!(
            "window_id {window_id} is closed, stale, invalid, or has no provable owner."
        ))
        .with_structured(json!({
            "code": "window_target_not_found",
            "effect": "refused",
            "pid": pid,
            "window_id": window_id,
        }))),
    }
}

pub(crate) fn window_target_resolution_failed(
    pid: u32,
    window_id: u64,
    reason: impl Into<String>,
) -> ToolResult {
    let reason = reason.into();
    ToolResult::error(format!(
        "could not resolve owner for window_id {window_id}: {reason}"
    ))
    .with_structured(json!({
        "code": "window_target_resolution_failed",
        "effect": "refused",
        "pid": pid,
        "window_id": window_id,
        "reason": reason,
    }))
}

pub(crate) fn publish_after_exact_window_ownership<T>(
    pid: u32,
    window_id: u64,
    owner_pid: Option<u32>,
    publish: impl FnOnce() -> T,
) -> Result<T, ToolResult> {
    exact_window_ownership_result(pid, window_id, owner_pid)?;
    Ok(publish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_codes_distinguish_mismatch_from_stale() {
        assert!(exact_window_ownership_result(42, 7, Some(42)).is_ok());

        let mismatch = exact_window_ownership_result(42, 7, Some(99)).unwrap_err();
        assert_eq!(
            mismatch.structured_content.as_ref().unwrap()["code"],
            "window_target_mismatch"
        );
        assert_eq!(
            mismatch.structured_content.as_ref().unwrap()["owner_pid"],
            99
        );

        let stale = exact_window_ownership_result(42, 7, None).unwrap_err();
        assert_eq!(
            stale.structured_content.as_ref().unwrap()["code"],
            "window_target_not_found"
        );
    }

    #[test]
    fn post_capture_gate_does_not_publish_a_stale_or_foreign_frame() {
        let mut published = false;
        let stale =
            publish_after_exact_window_ownership(42, 7, None, || published = true).unwrap_err();
        assert!(!published);
        assert_eq!(
            stale.structured_content.unwrap()["code"],
            "window_target_not_found"
        );

        let mut published = false;
        let mismatch =
            publish_after_exact_window_ownership(42, 7, Some(99), || published = true).unwrap_err();
        assert!(!published);
        assert_eq!(
            mismatch.structured_content.unwrap()["code"],
            "window_target_mismatch"
        );

        publish_after_exact_window_ownership(42, 7, Some(42), || published = true).unwrap();
        assert!(published);
    }

    #[test]
    fn lookup_failure_is_not_misreported_as_not_found() {
        let failure = window_target_resolution_failed(42, 7, "probe task panicked");
        assert_eq!(
            failure.structured_content.unwrap()["code"],
            "window_target_resolution_failed"
        );
    }
}
