//! Driver app identity from the real executable and its enclosing bundle.

use std::path::{Path, PathBuf};

use core_foundation::{
    base::{CFType, TCFType},
    data::CFData,
    dictionary::CFDictionary,
    propertylist::{create_with_data, kCFPropertyListImmutable, CFPropertyList},
    string::CFString,
};

/// Validated installed-product identity; app folder and display names do not
/// select the release/local channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverAppIdentity {
    pub app_path: PathBuf,
    pub bundle_id: &'static str,
    pub executable_name: &'static str,
    pub is_local: bool,
}

/// Whether a path component has the app-bundle extension, independent of its
/// basename. This structural check alone does not establish product identity.
pub fn is_app_directory(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
}

/// Recognize the real executable's immediate app bundle, including when invoked
/// through a CLI or app-folder symlink. CoreFoundation handles XML and binary
/// plists; directory names alone never establish a product identity.
pub fn driver_app_for_executable(executable: &Path) -> Option<DriverAppIdentity> {
    let executable = executable.canonicalize().ok()?;
    if !executable.is_file() {
        return None;
    }
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    if macos.file_name()? != "MacOS"
        || contents.file_name()? != "Contents"
        || !is_app_directory(app)
    {
        return None;
    }

    let data = std::fs::read(contents.join("Info.plist")).ok()?;
    let (raw, _) = create_with_data(CFData::from_buffer(&data), kCFPropertyListImmutable).ok()?;
    // `create_with_data` returns a retained property list on success.
    let plist = unsafe { CFPropertyList::wrap_under_create_rule(raw) };
    let dictionary = plist.downcast::<CFDictionary>()?;
    let string_value = |key: &str| -> Option<String> {
        let key = CFString::new(key);
        let value = dictionary.find(key.as_CFTypeRef())?;
        // Values in a parsed property-list dictionary are valid CF objects.
        let value = unsafe { CFType::wrap_under_get_rule(*value) };
        Some(value.downcast::<CFString>()?.to_string())
    };
    if string_value("CFBundlePackageType")?.as_str() != "APPL" {
        return None;
    }
    let bundle_id = string_value("CFBundleIdentifier")?;
    let bundle_executable = string_value("CFBundleExecutable")?;
    let (bundle_id, executable_name, is_local) =
        match (bundle_id.as_str(), bundle_executable.as_str()) {
            ("com.trycua.driver.local", "cua-driver-local") => {
                ("com.trycua.driver.local", "cua-driver-local", true)
            }
            ("com.trycua.driver", "cua-driver") => ("com.trycua.driver", "cua-driver", false),
            _ => return None,
        };
    if executable.file_name()? != executable_name {
        return None;
    }
    Some(DriverAppIdentity {
        app_path: app.to_path_buf(),
        bundle_id,
        executable_name,
        is_local,
    })
}
