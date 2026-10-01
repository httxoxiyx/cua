//! Parity check for `cua-driver update`.
//!
//! Update checks are disabled in this build: `update` must print the static
//! notice without contacting GitHub, and `update --apply` must refuse without
//! downloading or installing anything.

use std::process::Command;

#[cfg(target_os = "windows")]
fn main() {
    let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/debug/cua-driver.exe");

    let out = Command::new(&exe)
        .arg("update")
        .output()
        .expect("run update");
    let stdout = String::from_utf8_lossy(&out.stdout);
    println!("update stdout:");
    println!("{stdout}");

    assert!(out.status.success(), "update exited with {:?}", out.status);
    assert!(
        stdout.contains("Current version:"),
        "missing 'Current version:' header"
    );
    assert!(
        stdout.contains(
            "Update checks are disabled in this build; update through your distribution channel."
        ),
        "missing the static disabled notice"
    );
    // The removed network check printed these.
    for removed in [
        "Checking for updates",
        "Already up to date",
        "New version available:",
        "Could not reach GitHub",
    ] {
        assert!(
            !stdout.contains(removed),
            "update printed removed output {removed:?}:\n{stdout}"
        );
    }

    let apply = Command::new(&exe)
        .args(["update", "--apply"])
        .output()
        .expect("run update --apply");
    assert_eq!(
        apply.status.code(),
        Some(1),
        "update --apply must refuse without installing anything"
    );

    println!("\n✅ PASS: update CLI prints the static disabled notice and --apply refuses");
}

#[cfg(not(target_os = "windows"))]
fn main() {}
