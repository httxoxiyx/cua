//! Telemetry has been removed from this build of Cua Driver.
//!
//! Upstream builds sent product telemetry to a hosted analytics service:
//! installation and release lifecycle events, daemon and MCP startup, MCP
//! session starts, CLI command completions, and per-tool usage, each keyed by a
//! pseudonymous installation ID stored under the user's home directory. This
//! build deletes that pipeline outright. It has no analytics endpoint or key,
//! no HTTP client, no background delivery worker, no installation ID, and no
//! lifecycle markers. Nothing in this module touches the network, and nothing
//! here creates a file or a directory.
//!
//! What is left is deliberately small:
//!
//! - [`is_enabled`] is a compile-time `false`. No environment variable or
//!   config value can turn telemetry on, because there is nothing to turn on.
//! - [`status`] and [`remove_legacy_state`] back `cua-driver telemetry status`
//!   and `cua-driver telemetry reset-id`. They find and delete the identity,
//!   marker, and update-check cache files that an earlier build may have left
//!   in any Cua Driver home directory (release, `cua-driver-local`, or the
//!   pre-rename release home), whichever binary runs them. Both are local
//!   filesystem operations only.

use std::path::{Path, PathBuf};

/// Printed by every `cua-driver telemetry` subcommand.
pub const REMOVED_NOTICE: &str =
    "Telemetry has been removed from this build of Cua Driver; nothing is collected or sent.";

/// Always `false`. There is no telemetry to enable in this build.
pub const fn is_enabled() -> bool {
    false
}

// A future edit that re-enables telemetry fails to compile.
const _: () = assert!(!is_enabled());

/// Every home directory, under `$HOME` (`%USERPROFILE%` on Windows), where
/// earlier builds kept telemetry or update-check state: release installs,
/// source-built `cua-driver-local` installs, and the pre-rename release home.
/// Telemetry is gone from every namespace, so whichever binary runs the cleanup
/// sweeps all of them.
const LEGACY_HOME_SUBDIRECTORIES: &[&str] = &[".cua-driver", ".cua-driver-local", ".cua-driver-rs"];

/// Pre-rename home of release installs. [`remove_legacy_state`] also removes
/// the directory itself once nothing else is left in it.
const PRE_RENAME_HOME_SUBDIRECTORY: &str = ".cua-driver-rs";

/// Files earlier builds wrote for the installation ID, the consent preference
/// marker, delivery bookkeeping and its locks, and the cached upstream release
/// lookup of the removed update check.
const LEGACY_FILES: &[&str] = &[
    ".telemetry_id",
    ".telemetry_enabled",
    ".telemetry_identity.lock",
    ".telemetry_lifecycle.lock",
    ".telemetry_retry_after",
    ".telemetry_install_channel",
    ".installation_recorded",
    "version_check.json",
];

/// Directory of per-release "installed" markers written by earlier builds.
const LEGACY_RELEASE_MARKER_DIRECTORY: &str = ".release_installed";

/// Temporary installation-ID files an interrupted write could have left.
const LEGACY_TEMPORARY_PREFIX: &str = ".telemetry-id-";
const LEGACY_TEMPORARY_SUFFIX: &str = ".tmp";

/// Payload of `cua-driver telemetry status --json`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TelemetryStatus {
    /// Always `false`.
    pub enabled: bool,
    /// Always `"removed"`.
    pub source: &'static str,
    /// Whether telemetry or update-check files from an earlier build are still
    /// on disk in any Cua Driver home directory.
    pub legacy_state_present: bool,
    pub message: &'static str,
}

/// Report that telemetry is removed, and whether legacy files remain. Read-only.
pub fn status() -> TelemetryStatus {
    TelemetryStatus {
        enabled: is_enabled(),
        source: "removed",
        legacy_state_present: home_root().is_some_and(|root| legacy_state_present_under(&root)),
        message: REMOVED_NOTICE,
    }
}

/// Delete telemetry and update-check files left by an earlier build from
/// `~/.cua-driver`, `~/.cua-driver-local`, and the pre-rename
/// `~/.cua-driver-rs`. Returns the paths that were removed. Never creates
/// anything.
pub fn remove_legacy_state() -> Result<Vec<PathBuf>, String> {
    match home_root() {
        Some(root) => remove_legacy_state_under(&root),
        None => Ok(Vec::new()),
    }
}

/// The user's home directory: `$HOME`, else `%USERPROFILE%`. Empty or relative
/// values are skipped, so cleanup never resolves paths against the working
/// directory.
fn home_root() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .find(|path| path.is_absolute())
}

fn legacy_state_present_under(root: &Path) -> bool {
    LEGACY_HOME_SUBDIRECTORIES
        .iter()
        .any(|name| !legacy_entries_in(&root.join(name)).is_empty())
}

fn remove_legacy_state_under(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut removed = Vec::new();
    for name in LEGACY_HOME_SUBDIRECTORIES {
        let directory = root.join(name);
        removed.extend(remove_legacy_state_in(&directory)?);
        if *name == PRE_RENAME_HOME_SUBDIRECTORY {
            // Succeeds only when nothing else lives there.
            let _ = std::fs::remove_dir(&directory);
        }
    }
    Ok(removed)
}

/// Legacy telemetry entries directly inside `directory`. Read-only; a missing
/// directory has none.
fn legacy_entries_in(directory: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = LEGACY_FILES
        .iter()
        .copied()
        .chain([LEGACY_RELEASE_MARKER_DIRECTORY])
        .map(|name| directory.join(name))
        .filter(|path| path.symlink_metadata().is_ok())
        .collect();
    if let Ok(listing) = std::fs::read_dir(directory) {
        for entry in listing.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(LEGACY_TEMPORARY_PREFIX) && name.ends_with(LEGACY_TEMPORARY_SUFFIX)
            {
                entries.push(entry.path());
            }
        }
    }
    entries
}

fn remove_legacy_state_in(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = legacy_entries_in(directory);
    for path in &entries {
        // `symlink_metadata` keeps a symlinked marker directory from being
        // followed: the link itself is removed, never its target.
        let is_directory = path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.is_dir());
        let result = if is_directory {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        match result {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("failed to remove {}: {error}", path.display())),
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "1").unwrap();
    }

    #[test]
    fn telemetry_is_disabled_whatever_the_environment_or_config_says() {
        // The upstream opt-in/opt-out switches no longer exist; `is_enabled`
        // does not read them.
        assert!(!is_enabled());
        let status = status();
        assert!(!status.enabled);
        assert_eq!(status.source, "removed");
        assert_eq!(status.message, REMOVED_NOTICE);
    }

    const UNRELATED: &[&str] = &[
        "config.json",
        "release-channel",
        ".tcc-signing-identity",
        "packages/current/cua-driver",
    ];

    #[test]
    fn reset_removes_every_legacy_telemetry_file_and_keeps_unrelated_state() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        for name in LEGACY_FILES {
            write(&home.join(name));
        }
        write(&home.join(LEGACY_RELEASE_MARKER_DIRECTORY).join("0.23.2"));
        write(&home.join(".telemetry-id-0f8b.tmp"));
        for unrelated in UNRELATED {
            write(&home.join(unrelated));
        }

        let removed = remove_legacy_state_in(home).unwrap();

        assert_eq!(removed.len(), LEGACY_FILES.len() + 2);
        assert!(legacy_entries_in(home).is_empty());
        assert!(!home.join("version_check.json").exists());
        for unrelated in UNRELATED {
            assert!(home.join(unrelated).is_file(), "{unrelated} must survive");
        }
        // A second run is a no-op.
        assert!(remove_legacy_state_in(home).unwrap().is_empty());
    }

    #[test]
    fn reset_sweeps_every_cua_driver_home_whichever_binary_runs_it() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for name in LEGACY_HOME_SUBDIRECTORIES {
            let home = root.join(name);
            write(&home.join(".telemetry_id"));
            write(&home.join(".installation_recorded"));
            write(&home.join("version_check.json"));
            write(&home.join(LEGACY_RELEASE_MARKER_DIRECTORY).join("0.23.2"));
        }
        write(&root.join(".cua-driver").join("config.json"));
        write(
            &root
                .join(".cua-driver-local")
                .join("packages/current/cua-driver-local"),
        );
        assert!(legacy_state_present_under(root));

        let removed = remove_legacy_state_under(root).unwrap();

        assert_eq!(removed.len(), 4 * LEGACY_HOME_SUBDIRECTORIES.len());
        assert!(!legacy_state_present_under(root));
        for name in LEGACY_HOME_SUBDIRECTORIES {
            assert!(legacy_entries_in(&root.join(name)).is_empty(), "{name}");
        }
        assert!(root.join(".cua-driver").join("config.json").is_file());
        assert!(root
            .join(".cua-driver-local")
            .join("packages/current/cua-driver-local")
            .is_file());
        // Only the pre-rename home is removed once it is empty.
        assert!(!root.join(PRE_RENAME_HOME_SUBDIRECTORY).exists());
    }

    #[test]
    fn reset_keeps_a_pre_rename_home_that_still_holds_other_files() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(PRE_RENAME_HOME_SUBDIRECTORY);
        write(&legacy.join(".telemetry_id"));
        write(&legacy.join("packages/current/cua-driver"));

        remove_legacy_state_under(root.path()).unwrap();

        assert!(!legacy.join(".telemetry_id").exists());
        assert!(legacy.join("packages/current/cua-driver").is_file());
    }

    #[test]
    fn the_running_products_home_is_among_the_swept_homes() {
        assert!(LEGACY_HOME_SUBDIRECTORIES.contains(&crate::bundle::user_home_subdirectory()));
        assert!(LEGACY_HOME_SUBDIRECTORIES.contains(&PRE_RENAME_HOME_SUBDIRECTORY));
    }

    #[cfg(unix)]
    #[test]
    fn reset_removes_a_symlinked_marker_directory_without_following_it() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        write(&outside.join("keep"));
        let home = root.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(&outside, home.join(LEGACY_RELEASE_MARKER_DIRECTORY)).unwrap();

        remove_legacy_state_in(&home).unwrap();

        assert!(home
            .join(LEGACY_RELEASE_MARKER_DIRECTORY)
            .symlink_metadata()
            .is_err());
        assert!(outside.join("keep").is_file());
    }

    #[test]
    fn inspecting_and_resetting_a_missing_home_creates_nothing() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("never-created");

        assert!(legacy_entries_in(&home).is_empty());
        assert!(remove_legacy_state_in(&home).unwrap().is_empty());
        assert!(!legacy_state_present_under(root.path()));
        assert!(remove_legacy_state_under(root.path()).unwrap().is_empty());

        assert!(!home.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
