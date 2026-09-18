//! Installed-product identity.
//!
//! Release and source builds deliberately use different executable names and
//! bundle identifiers. App folder and display names may change without changing
//! the runtime namespace (MCP proxy, daemon, status, autostart).

#[cfg(target_os = "macos")]
use platform_macos::app_identity::{
    driver_app_for_executable as app_bundle_for_executable, is_app_directory,
};
use std::path::Path;
#[cfg(all(test, target_os = "macos"))]
use std::path::PathBuf;

pub const RELEASE_CLI_NAME: &str = "cua-driver";
pub const LOCAL_CLI_NAME: &str = "cua-driver-local";

pub const RELEASE_APP_NAME: &str = "CuaDriver";
pub const LOCAL_APP_NAME: &str = "CuaDriverLocal";
pub const RELEASE_BUNDLE_ID: &str = "com.trycua.driver";
pub const LOCAL_BUNDLE_ID: &str = "com.trycua.driver.local";

pub(crate) fn path_is_local(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    let canonical_path = path.canonicalize().ok();
    #[cfg(target_os = "macos")]
    let path = canonical_path.as_deref().unwrap_or(path);

    // Only bare CLI binaries use the filename fallback. An unrelated or invalid
    // app must not acquire our local namespace by naming its executable after us.
    #[cfg(target_os = "macos")]
    if path.ancestors().skip(1).any(is_app_directory) {
        return app_bundle_for_executable(path).is_some_and(|bundle| bundle.is_local);
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    file_name == LOCAL_CLI_NAME || file_name == format!("{LOCAL_CLI_NAME}.exe")
}

/// Whether this process is the explicitly-installed source-build product.
pub fn is_local_installation() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .is_some_and(|path| path_is_local(&path))
}

pub fn cli_name() -> &'static str {
    if is_local_installation() {
        LOCAL_CLI_NAME
    } else {
        RELEASE_CLI_NAME
    }
}

pub fn state_namespace() -> &'static str {
    if is_local_installation() {
        "cua-driver-local"
    } else {
        "cua-driver"
    }
}

pub fn user_home_subdirectory() -> &'static str {
    if is_local_installation() {
        ".cua-driver-local"
    } else {
        ".cua-driver"
    }
}

#[cfg(target_os = "windows")]
pub fn uia_executable_name() -> &'static str {
    if is_local_installation() {
        "cua-driver-uia-local.exe"
    } else {
        "cua-driver-uia.exe"
    }
}

#[cfg(target_os = "windows")]
pub fn autostart_task_name() -> &'static str {
    if is_local_installation() {
        "cua-driver-local-serve"
    } else {
        "cua-driver-serve"
    }
}

pub fn app_name() -> &'static str {
    if is_local_installation() {
        LOCAL_APP_NAME
    } else {
        RELEASE_APP_NAME
    }
}

pub fn app_bundle_path() -> String {
    #[cfg(target_os = "macos")]
    if let Some(bundle) = std::env::current_exe()
        .ok()
        .and_then(|path| app_bundle_for_executable(&path))
    {
        return bundle.app_path.to_string_lossy().into_owned();
    }
    format!("/Applications/{}.app", app_name())
}

pub fn bundle_id() -> &'static str {
    if is_local_installation() {
        LOCAL_BUNDLE_ID
    } else {
        RELEASE_BUNDLE_ID
    }
}

/// A bundled daemon already has its stable TCC responsibility identity and
/// must not disclaim it during startup.
#[cfg(target_os = "macos")]
pub fn is_executable_inside_cuadriver_app() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|path| app_bundle_for_executable(&path))
        .is_some()
}

/// Returns `true` when the env var is one of `1|true|yes|on`
/// (case-insensitive). Anything else, including unset, is falsy.
#[cfg(target_os = "windows")]
pub fn is_env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_identity_requires_the_explicit_local_product_name() {
        assert!(path_is_local(Path::new("/dev/cua-driver-local")));
        assert!(path_is_local(Path::new(
            "/dev/.cua-driver-local/cua-driver-local.exe"
        )));
        assert!(!path_is_local(Path::new(
            "/Applications/CuaDriver.app/Contents/MacOS/cua-driver"
        )));
        assert!(!path_is_local(Path::new("/tmp/cua-driver-local-test")));
        assert!(!path_is_local(Path::new(
            "/tmp/CuaDriverLocal.app/Contents/MacOS/unrelated"
        )));
    }

    #[cfg(target_os = "macos")]
    mod macos {
        use super::*;
        use core_foundation::{
            base::TCFType,
            dictionary::CFDictionary,
            propertylist::{create_data, kCFPropertyListBinaryFormat_v1_0},
            string::CFString,
        };

        fn app_fixture(
            root: &Path,
            name: &str,
            bundle_id: &str,
            declared_executable: &str,
            actual_executable: &str,
            package_type: &str,
        ) -> PathBuf {
            let contents = root.join(name).join("Contents");
            let macos = contents.join("MacOS");
            std::fs::create_dir_all(&macos).unwrap();
            let dictionary = CFDictionary::from_CFType_pairs(&[
                (
                    CFString::new("CFBundleIdentifier"),
                    CFString::new(bundle_id),
                ),
                (
                    CFString::new("CFBundleExecutable"),
                    CFString::new(declared_executable),
                ),
                (
                    CFString::new("CFBundlePackageType"),
                    CFString::new(package_type),
                ),
                // A display name resembling the other channel must not matter.
                (
                    CFString::new("CFBundleDisplayName"),
                    CFString::new(LOCAL_APP_NAME),
                ),
            ]);
            let data =
                create_data(dictionary.as_CFTypeRef(), kCFPropertyListBinaryFormat_v1_0).unwrap();
            std::fs::write(contents.join("Info.plist"), data.bytes()).unwrap();
            let executable = macos.join(actual_executable);
            std::fs::write(&executable, b"fixture").unwrap();
            executable
        }

        #[test]
        fn renamed_apps_keep_their_plist_identity_and_actual_path() {
            let root = tempfile::tempdir().unwrap();
            for (name, id, executable, is_local) in [
                ("cua.app", LOCAL_BUNDLE_ID, LOCAL_CLI_NAME, true),
                ("Renamed.app", RELEASE_BUNDLE_ID, RELEASE_CLI_NAME, false),
                (
                    "CuaDriverLocal.app",
                    RELEASE_BUNDLE_ID,
                    RELEASE_CLI_NAME,
                    false,
                ),
                ("CuaDriver.app", LOCAL_BUNDLE_ID, LOCAL_CLI_NAME, true),
            ] {
                let path = app_fixture(root.path(), name, id, executable, executable, "APPL");
                let bundle = app_bundle_for_executable(&path).unwrap();
                assert_eq!(
                    bundle.app_path,
                    root.path().join(name).canonicalize().unwrap()
                );
                assert_eq!(bundle.is_local, is_local);
                assert_eq!(path_is_local(&path), is_local);
            }
        }

        #[test]
        fn invalid_bundle_identities_do_not_qualify() {
            let root = tempfile::tempdir().unwrap();
            for (index, (id, declared, actual, package)) in [
                ("com.example.other", LOCAL_CLI_NAME, LOCAL_CLI_NAME, "APPL"),
                (LOCAL_BUNDLE_ID, RELEASE_CLI_NAME, LOCAL_CLI_NAME, "APPL"),
                (RELEASE_BUNDLE_ID, LOCAL_CLI_NAME, LOCAL_CLI_NAME, "APPL"),
                (LOCAL_BUNDLE_ID, LOCAL_CLI_NAME, "helper", "APPL"),
                (LOCAL_BUNDLE_ID, LOCAL_CLI_NAME, LOCAL_CLI_NAME, "BNDL"),
            ]
            .into_iter()
            .enumerate()
            {
                let path = app_fixture(
                    root.path(),
                    &format!("invalid-{index}.app"),
                    id,
                    declared,
                    actual,
                    package,
                );
                assert!(app_bundle_for_executable(&path).is_none());
                assert!(!path_is_local(&path));
            }
        }

        #[test]
        fn missing_malformed_and_wrongly_typed_plists_do_not_qualify() {
            let root = tempfile::tempdir().unwrap();
            let path = app_fixture(
                root.path(),
                "cua.app",
                LOCAL_BUNDLE_ID,
                LOCAL_CLI_NAME,
                LOCAL_CLI_NAME,
                "APPL",
            );
            let plist = root.path().join("cua.app/Contents/Info.plist");
            for data in [
                b"not a plist".as_slice(),
                b"<plist version=\"1.0\"><array/></plist>".as_slice(),
                b"<plist version=\"1.0\"><dict><key>CFBundlePackageType</key><integer>1</integer></dict></plist>".as_slice(),
            ] {
                std::fs::write(&plist, data).unwrap();
                assert!(app_bundle_for_executable(&path).is_none());
                assert!(!path_is_local(&path));
            }
            std::fs::remove_file(plist).unwrap();
            assert!(app_bundle_for_executable(&path).is_none());
            assert!(!path_is_local(&path));
        }

        #[test]
        fn only_the_app_executable_layout_qualifies() {
            let root = tempfile::tempdir().unwrap();
            let bare = root.path().join(LOCAL_CLI_NAME);
            std::fs::write(&bare, b"fixture").unwrap();
            assert!(app_bundle_for_executable(&bare).is_none());
            assert!(path_is_local(&bare));
            let non_app = app_fixture(
                root.path(),
                "ordinary-directory",
                LOCAL_BUNDLE_ID,
                LOCAL_CLI_NAME,
                LOCAL_CLI_NAME,
                "APPL",
            );
            assert!(app_bundle_for_executable(&non_app).is_none());
            let misplaced = root
                .path()
                .join("cua.app/Contents/Resources")
                .join(LOCAL_CLI_NAME);
            std::fs::create_dir_all(misplaced.parent().unwrap()).unwrap();
            std::fs::write(&misplaced, b"fixture").unwrap();
            assert!(app_bundle_for_executable(&misplaced).is_none());
            assert!(!path_is_local(&misplaced));
        }

        #[test]
        fn symlinks_use_the_target_bundle_identity() {
            use std::os::unix::fs::symlink;

            let root = tempfile::tempdir().unwrap();
            let executable = app_fixture(
                root.path(),
                "cua.app",
                LOCAL_BUNDLE_ID,
                LOCAL_CLI_NAME,
                LOCAL_CLI_NAME,
                "APPL",
            );
            let alias = root.path().join(RELEASE_CLI_NAME);
            symlink(&executable, &alias).unwrap();
            assert!(path_is_local(&alias));
            let bundle_alias = root.path().join("Renamed.app");
            symlink(root.path().join("cua.app"), &bundle_alias).unwrap();
            for path in [
                alias,
                bundle_alias.join("Contents/MacOS").join(LOCAL_CLI_NAME),
            ] {
                let bundle = app_bundle_for_executable(&path).unwrap();
                assert_eq!(
                    bundle.app_path,
                    root.path().join("cua.app").canonicalize().unwrap()
                );
                assert!(bundle.is_local);
            }

            let bare = root.path().join(LOCAL_CLI_NAME);
            std::fs::write(&bare, b"fixture").unwrap();
            std::fs::remove_file(&executable).unwrap();
            symlink(&bare, &executable).unwrap();
            assert!(app_bundle_for_executable(&executable).is_none());
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn env_truthiness_is_strict() {
        let name = "CUA_DRIVER_RS_TEST_TRUTHY";
        for value in ["1", "true", "TRUE", "Yes", "on", " 1 "] {
            std::env::set_var(name, value);
            assert!(is_env_truthy(name), "expected truthy for {value:?}");
        }
        for value in ["0", "false", "no", "off", ""] {
            std::env::set_var(name, value);
            assert!(!is_env_truthy(name), "expected falsy for {value:?}");
        }
        std::env::remove_var(name);
    }
}
