//! Update checks are disabled in this build.
//!
//! Upstream builds asked GitHub's releases API whether a newer Cua Driver
//! existed: automatically on the interactive entry points (`mcp`, `serve`,
//! `doctor`) and on demand from `check-update`, `update`, and the
//! `check_for_update` MCP tool, caching the answer under the user's home
//! directory. This build removes the request, the cache and its refresh, the
//! startup banner, and the HTTP client. Every update surface returns the same
//! static answer below without any network or filesystem access.

/// The answer every update surface gives in this build.
pub const DISABLED_MESSAGE: &str =
    "Update checks are disabled in this build; update through your distribution channel.";

/// Payload of `check-update --json`, `update --json`, and the
/// `check_for_update` MCP tool. Constant apart from the compiled-in version.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
pub struct UpdateState {
    pub current_version: &'static str,
    /// Always `false`.
    pub update_checks_enabled: bool,
    /// Always `false`: nothing was checked.
    pub update_available: bool,
    /// Always `None`: nothing was fetched.
    pub latest_version: Option<&'static str>,
    /// Always `"disabled"`.
    pub source: &'static str,
    pub message: &'static str,
}

/// The static update state. Performs no I/O.
pub const fn update_state() -> UpdateState {
    UpdateState {
        current_version: env!("CARGO_PKG_VERSION"),
        update_checks_enabled: false,
        update_available: false,
        latest_version: None,
        source: "disabled",
        message: DISABLED_MESSAGE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_state_is_static_and_never_reports_an_update() {
        let state = update_state();
        assert_eq!(state.current_version, env!("CARGO_PKG_VERSION"));
        assert!(!state.update_checks_enabled);
        assert!(!state.update_available);
        assert_eq!(state.latest_version, None);
        assert_eq!(state.source, "disabled");
        assert_eq!(state.message, DISABLED_MESSAGE);
        assert_eq!(update_state(), state);

        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["update_available"], false);
        assert_eq!(json["update_checks_enabled"], false);
        assert!(json["latest_version"].is_null());
    }

    #[test]
    fn disabled_message_points_at_the_distribution_channel() {
        assert!(DISABLED_MESSAGE.contains("Update checks are disabled in this build"));
        assert!(DISABLED_MESSAGE.contains("distribution channel"));
    }
}
