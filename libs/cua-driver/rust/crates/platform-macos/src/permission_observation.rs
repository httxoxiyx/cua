use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::app_identity::driver_app_for_executable;

const SCHEMA_VERSION: u8 = 1;
const SOURCE: &str = "permissions_grant";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DirectCaptureVerification {
    pub source: String,
    pub verified_at: String,
    pub bundle_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedVerification {
    schema_version: u8,
    source: String,
    verified_at_unix_seconds: i64,
    bundle_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct DirectCaptureEvidenceStore {
    path: PathBuf,
    bundle_id: String,
}

impl DirectCaptureEvidenceStore {
    pub(crate) fn new(path: PathBuf, bundle_id: impl Into<String>) -> Self {
        Self {
            path,
            bundle_id: bundle_id.into(),
        }
    }

    pub(crate) fn record_now(&self) -> Result<DirectCaptureVerification, String> {
        self.record_at(time::OffsetDateTime::now_utc().unix_timestamp())?;
        self.load()
            .ok_or_else(|| "recorded direct-capture verification did not validate".to_owned())
    }

    /// Replace the previous successful probe with fresh evidence. If the
    /// replacement cannot be written, remove the old record so a caller can
    /// never mistake stale evidence for the probe that just succeeded.
    pub(crate) fn refresh_now(&self) -> Result<DirectCaptureVerification, String> {
        match self.record_now() {
            Ok(verification) => Ok(verification),
            Err(error) => {
                let message = match self.clear() {
                    Ok(()) => error,
                    Err(clear_error) => {
                        format!("{error}; clear prior direct-capture verification: {clear_error}")
                    }
                };
                Err(message)
            }
        }
    }

    pub(crate) fn load(&self) -> Option<DirectCaptureVerification> {
        let record = self.load_record()?;
        let verified_at =
            time::OffsetDateTime::from_unix_timestamp(record.verified_at_unix_seconds)
                .ok()?
                .format(&time::format_description::well_known::Rfc3339)
                .ok()?;
        Some(DirectCaptureVerification {
            source: record.source,
            verified_at,
            bundle_id: record.bundle_id,
        })
    }

    pub(crate) fn clear(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove {}: {error}", self.path.display())),
        }
    }

    fn record_at(&self, observed_at_unix_seconds: i64) -> Result<(), String> {
        if observed_at_unix_seconds <= 0 {
            return Err("verification timestamp is unavailable".to_owned());
        }
        // Package A and package B intentionally share a TCC identity and
        // therefore the same evidence file. Ensure every successful probe is
        // observable as a strict refresh even when both complete during the
        // same wall-clock second.
        let verified_at_unix_seconds = self
            .load_record()
            .map(|record| {
                if record.verified_at_unix_seconds >= observed_at_unix_seconds {
                    record
                        .verified_at_unix_seconds
                        .checked_add(1)
                        .ok_or_else(|| "verification timestamp overflowed".to_owned())
                } else {
                    Ok(observed_at_unix_seconds)
                }
            })
            .transpose()?
            .unwrap_or(observed_at_unix_seconds);
        let parent = self
            .path
            .parent()
            .ok_or_else(|| "direct-capture verification path has no parent".to_owned())?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
        let record = PersistedVerification {
            schema_version: SCHEMA_VERSION,
            source: SOURCE.to_owned(),
            verified_at_unix_seconds,
            bundle_id: self.bundle_id.clone(),
        };
        write_json_atomic(&self.path, &record)
    }

    fn load_record(&self) -> Option<PersistedVerification> {
        let bytes = std::fs::read(&self.path).ok()?;
        let record: PersistedVerification = serde_json::from_slice(&bytes).ok()?;
        if record.schema_version != SCHEMA_VERSION
            || record.source != SOURCE
            || record.verified_at_unix_seconds <= 0
            || record.bundle_id != self.bundle_id
        {
            return None;
        }
        Some(record)
    }
}

/// Resolve the evidence namespace from the canonical executable's validated
/// app metadata. Folder names and caller-provided bundle labels are never
/// trusted for this decision.
pub(crate) fn direct_capture_evidence_store_for_driver_executable(
    executable: &Path,
    home: &Path,
) -> Option<DirectCaptureEvidenceStore> {
    let identity = driver_app_for_executable(executable)?;
    direct_capture_evidence_store_for_bundle(identity.bundle_id, home)
}

pub(crate) fn direct_capture_evidence_store_for_bundle(
    bundle_id: &str,
    home: &Path,
) -> Option<DirectCaptureEvidenceStore> {
    let state_directory = match bundle_id {
        "com.meta.musecode.cua.driver.local" => ".cua-driver-local",
        "com.meta.musecode.cua.driver" => ".cua-driver",
        _ => return None,
    };
    Some(DirectCaptureEvidenceStore::new(
        home.join(state_directory)
            .join("direct-capture-verification.json"),
        bundle_id,
    ))
}

pub(crate) fn current_driver_direct_capture_evidence_store(
) -> Result<DirectCaptureEvidenceStore, String> {
    let executable = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| format!("resolve current driver executable: {error}"))?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "current user home directory is unavailable".to_owned())?;
    direct_capture_evidence_store_for_driver_executable(&executable, &home).ok_or_else(|| {
        "current executable is not a recognized installed CuaDriver app identity".to_owned()
    })
}

fn write_json_atomic(path: &Path, value: &PersistedVerification) -> Result<(), String> {
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("create {}: {error}", temporary.display()))?;
        let mut bytes = serde_json::to_vec_pretty(value)
            .map_err(|error| format!("serialize direct-capture verification: {error}"))?;
        bytes.push(b'\n');
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("write {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("replace {}: {error}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE_BUNDLE_ID: &str = "com.meta.musecode.cua.driver";
    const VERIFIED_AT: i64 = 1_754_352_000;

    fn store(path: PathBuf) -> DirectCaptureEvidenceStore {
        DirectCaptureEvidenceStore::new(path, RELEASE_BUNDLE_ID)
    }

    #[test]
    fn verification_round_trips_with_honest_consent_provenance() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = store(temp.path().join("verification.json"));
        store.record_at(VERIFIED_AT).expect("record verification");

        assert_eq!(
            store.load(),
            Some(DirectCaptureVerification {
                source: SOURCE.to_owned(),
                verified_at: "2025-08-05T00:00:00Z".to_owned(),
                bundle_id: RELEASE_BUNDLE_ID.to_owned(),
            })
        );
    }

    #[test]
    fn same_identity_update_strictly_refreshes_evidence_timestamp() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("verification.json");
        let package_a = store(path.clone());
        package_a
            .record_at(VERIFIED_AT)
            .expect("record package A verification");

        // Package B has a different binary/CDHash but the same designated TCC
        // identity and evidence namespace. A second-granularity wall clock may
        // not advance during a fast update, so the writer must do so itself.
        let package_b = store(path.clone());
        package_b
            .record_at(VERIFIED_AT)
            .expect("record package B verification");
        let refreshed: PersistedVerification =
            serde_json::from_slice(&std::fs::read(&path).expect("read refreshed verification"))
                .expect("parse refreshed verification");
        assert_eq!(refreshed.verified_at_unix_seconds, VERIFIED_AT + 1);

        // A backwards wall-clock adjustment must not make fresh evidence look
        // older than the evidence emitted by the previous package.
        package_b
            .record_at(VERIFIED_AT - 60)
            .expect("refresh after backwards clock adjustment");
        let refreshed_again: PersistedVerification = serde_json::from_slice(
            &std::fs::read(path).expect("read second refreshed verification"),
        )
        .expect("parse second refreshed verification");
        assert_eq!(refreshed_again.verified_at_unix_seconds, VERIFIED_AT + 2);
    }

    #[test]
    fn verification_rejects_wrong_identity_schema_source_and_timestamp() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("verification.json");
        let store = store(path.clone());
        let valid = PersistedVerification {
            schema_version: SCHEMA_VERSION,
            source: SOURCE.to_owned(),
            verified_at_unix_seconds: VERIFIED_AT,
            bundle_id: RELEASE_BUNDLE_ID.to_owned(),
        };
        let invalid_records = [
            PersistedVerification {
                schema_version: SCHEMA_VERSION + 1,
                ..valid.clone()
            },
            PersistedVerification {
                source: "unknown".to_owned(),
                ..valid.clone()
            },
            PersistedVerification {
                verified_at_unix_seconds: 0,
                ..valid.clone()
            },
            PersistedVerification {
                bundle_id: "com.meta.musecode.cua.driver.local".to_owned(),
                ..valid
            },
        ];

        for record in invalid_records {
            std::fs::write(
                &path,
                serde_json::to_vec(&record).expect("serialize record"),
            )
            .expect("write invalid record");
            assert_eq!(store.load(), None);
        }
    }

    #[test]
    fn clear_removes_existing_evidence_and_tolerates_absence() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = store(temp.path().join("verification.json"));
        store.record_at(VERIFIED_AT).expect("record verification");

        store.clear().expect("clear verification");
        store.clear().expect("clear absent verification");
        assert_eq!(store.load(), None);
    }
}
