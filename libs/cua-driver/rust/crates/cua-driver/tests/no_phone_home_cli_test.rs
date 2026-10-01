//! The built binary must not phone home: telemetry is removed, update checks
//! answer statically, and the skill pack comes from the binary itself.
//!
//! Every command runs against an isolated, empty home directory. The upstream
//! opt-in switches are forced *on* to show they no longer do anything, and the
//! proxy variables point at a closed local port so that any regression fails
//! fast instead of reaching the network.

use std::path::Path;
use std::process::{Command, Output};

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cua-driver"))
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData").join("Roaming"))
        .env("LOCALAPPDATA", home.join("AppData").join("Local"))
        .env_remove("HERMES_HOME")
        .env_remove("CUA_DRIVER_RS_HOME")
        .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "1")
        .env("CUA_TELEMETRY_ENABLED", "1")
        .env("CUA_DRIVER_RS_UPDATE_CHECK", "1")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .output()
        .expect("run cua-driver")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status {:?}\nstdout: {}\nstderr: {}",
        output.status,
        stdout(output),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn entries(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn telemetry_and_update_commands_answer_statically_and_create_nothing() {
    let home = tempfile::tempdir().unwrap();

    let status = run(home.path(), &["telemetry", "status", "--json"]);
    assert_success(&status);
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["enabled"], false);
    assert_eq!(status["source"], "removed");
    assert_eq!(status["legacy_state_present"], false);

    for verb in ["disable", "install-event", "inspect"] {
        let output = run(home.path(), &["telemetry", verb]);
        assert_success(&output);
        assert!(
            stdout(&output).contains("Telemetry has been removed"),
            "{verb}"
        );
    }
    let enable = run(home.path(), &["telemetry", "enable"]);
    assert!(!enable.status.success());
    assert!(String::from_utf8_lossy(&enable.stderr).contains("cannot be enabled"));

    let check = run(home.path(), &["check-update", "--json", "--no-cache"]);
    assert_success(&check);
    let check: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(check["update_checks_enabled"], false);
    assert_eq!(check["update_available"], false);
    assert!(check["latest_version"].is_null());
    assert_eq!(check["source"], "disabled");

    let update = run(home.path(), &["update"]);
    assert_success(&update);
    assert!(stdout(&update).contains(
        "Update checks are disabled in this build; update through your distribution channel."
    ));
    let apply = run(home.path(), &["update", "--apply"]);
    assert_eq!(apply.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&apply.stderr).contains("nothing was downloaded or installed"));

    // No installation ID, lifecycle marker, update cache, or anything else.
    assert!(
        entries(home.path()).is_empty(),
        "commands created {:?}",
        entries(home.path())
    );
}

#[test]
fn telemetry_reset_id_deletes_only_legacy_telemetry_files() {
    let home = tempfile::tempdir().unwrap();
    let current = home.path().join(".cua-driver");
    let legacy = home.path().join(".cua-driver-rs");
    for directory in [&current, &legacy] {
        std::fs::create_dir_all(directory.join(".release_installed")).unwrap();
        std::fs::write(directory.join(".release_installed").join("0.23.1"), "1").unwrap();
        for name in [
            ".telemetry_id",
            ".installation_recorded",
            ".telemetry_lifecycle.lock",
        ] {
            std::fs::write(directory.join(name), "1").unwrap();
        }
    }
    std::fs::write(current.join("config.json"), "{}").unwrap();

    let status = run(home.path(), &["telemetry", "status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["legacy_state_present"], true);

    let reset = run(home.path(), &["telemetry", "reset-id"]);
    assert_success(&reset);
    assert!(stdout(&reset).contains("Removed"), "{}", stdout(&reset));

    assert_eq!(entries(&current), vec!["config.json".to_owned()]);
    assert!(!legacy.exists(), "emptied legacy home should be removed");
}

#[test]
fn skills_install_uses_the_bundled_pack_and_rejects_remote_sources() {
    let home = tempfile::tempdir().unwrap();

    let remote = run(home.path(), &["skills", "install", "--from", "main"]);
    assert_eq!(remote.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&remote.stderr).contains("remote skill sources were removed"),
        "{}",
        String::from_utf8_lossy(&remote.stderr)
    );
    assert!(entries(home.path()).is_empty());

    let install = run(home.path(), &["skills", "install"]);
    assert_success(&install);
    let pack = home
        .path()
        .join(".cua-driver")
        .join("skills")
        .join("cua-driver");
    let canonical = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Skills/cua-driver/SKILL.md");
    assert_eq!(
        std::fs::read_to_string(pack.join("SKILL.md")).unwrap(),
        std::fs::read_to_string(canonical).unwrap()
    );
}
