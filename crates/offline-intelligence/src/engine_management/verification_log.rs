//! Engine Verification Log
//!
//! Records which *acceleration types* have been proven unusable on THIS machine
//! by a real backend-load check, so neither the installer nor the app retries
//! them forever.
//!
//! ## Why this exists
//!
//! The decision table (`EngineRegistry::select_correct_engine`) is a pure
//! function of detected hardware capability. Capability probes are registry
//! lookups: "is a Vulkan ICD registered?", "does nvidia-smi report a new enough
//! driver?". Those answer *what is worth trying*. They cannot answer *what
//! actually works*, and on a large class of real machines the two differ:
//!
//!   - `ggml-vulkan.dll` and `ggml-cuda.dll` import `VCRUNTIME140_1.dll`, which
//!     `llama-server.exe` and the CPU/RPC backends do NOT (verified from the
//!     b8037 PE import directories). With an older 14.0-14.16 VC++ redist the
//!     engine starts, CPU and RPC load, and `LoadLibrary("ggml-vulkan.dll")`
//!     fails *silently* — ggml simply never registers the backend.
//!   - A registered Vulkan ICD can belong to a driver too old for what
//!     ggml-vulkan needs, so the ICD probe passes and the load still fails.
//!
//! Without a memory of that failure, the app re-selects the same engine on
//! every launch and every user-triggered reinstall, and never becomes usable
//! even though the CPU engine would work perfectly. This log is that memory.
//!
//! ## Keying: (acceleration, engine_version)
//!
//! An entry suppresses one acceleration for ONE llama.cpp version. Bumping
//! `ENGINE_VERSION` makes every existing entry irrelevant, so a demotion is
//! never permanent by accident — a new upstream release gets a fresh chance on
//! the same hardware. Entries for other versions are kept on disk (they cost
//! nothing and describe real history) but never consulted.
//!
//! Exclusion is by acceleration rather than by engine id on purpose: the two
//! CUDA engines (12.4 and 13.1) share their entire failure surface
//! (`ggml-cuda.dll`, `nvcuda.dll`, `VCRUNTIME140_1.dll`), so proving one
//! unusable proves the other unusable. Excluding by id would download 370+ MB
//! to fail identically.
//!
//! ## Shared with the NSIS installer
//!
//! `apps/desktop/src-tauri/installer-hooks.nsi` (`oci_WriteVerificationLog`)
//! writes this exact file with this exact schema, so a demotion decided during
//! installation is honoured by the app and vice versa. The two decision tables
//! stay in sync, which is the standing contract for this subsystem. The
//! installer writes only `acceleration`, `engine_version`, `reason` and
//! `source`; the remaining fields are `#[serde(default)]` so its output parses
//! unchanged.
//!
//! ## Failure policy
//!
//! Reading is infallible by construction. A missing file is "nothing has
//! failed yet". A corrupt or unparseable file is logged loudly and treated as
//! empty — the cost of that is one retry of an engine that may fail again
//! (recoverable, self-correcting), whereas refusing to start because a cache
//! file is malformed would be a far worse outcome.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use super::registry::AccelerationType;

/// Current on-disk schema version. Bump only for a breaking layout change;
/// the NSIS writer must be updated in the same commit.
pub const SCHEMA_VERSION: u32 = 1;

/// File name inside the engines directory.
pub const VERIFICATION_LOG_FILENAME: &str = "verification.json";

/// One proven-unusable acceleration on this machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationFailure {
    /// `AccelerationType`'s Display form: "CPU", "CUDA", "Vulkan", "Metal",
    /// "OpenCL", "DirectML". Compared case-insensitively when matching.
    pub acceleration: String,
    /// llama.cpp version the failure was observed at, e.g. "b8037".
    pub engine_version: String,
    /// Human-readable cause, surfaced to the user.
    pub reason: String,
    /// "installer" or "app" — diagnostics only, never affects matching.
    #[serde(default)]
    pub source: String,
    /// Present when the app recorded it; the installer omits it.
    #[serde(default)]
    pub engine_id: Option<String>,
    /// Unix seconds; the installer omits it (NSIS has no clock primitive that
    /// is worth the complexity here).
    #[serde(default)]
    pub recorded_at: Option<u64>,
}

/// The full on-disk document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationLog {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub failures: Vec<VerificationFailure>,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Default for VerificationLog {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            failures: Vec::new(),
        }
    }
}

impl VerificationLog {
    /// Path of the log inside an engines storage directory.
    pub fn path_in(engines_dir: &Path) -> PathBuf {
        engines_dir.join(VERIFICATION_LOG_FILENAME)
    }

    /// Load the log. Never fails — see the module doc's failure policy.
    pub fn load(engines_dir: &Path) -> Self {
        let path = Self::path_in(engines_dir);
        if !path.exists() {
            return Self::default();
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "Could not read engine verification log at {:?}: {}. Treating as empty — \
                     a previously-demoted acceleration may be retried once.",
                    path, e
                );
                return Self::default();
            }
        };

        match serde_json::from_str::<Self>(&content) {
            Ok(log) => {
                if log.schema_version > SCHEMA_VERSION {
                    warn!(
                        "Engine verification log at {:?} has schema version {} (this build \
                         understands {}). Ignoring it rather than acting on fields it may not \
                         describe correctly.",
                        path, log.schema_version, SCHEMA_VERSION
                    );
                    return Self::default();
                }
                log
            }
            Err(e) => {
                warn!(
                    "Engine verification log at {:?} is unparseable ({}). Treating as empty — \
                     a previously-demoted acceleration may be retried once.",
                    path, e
                );
                Self::default()
            }
        }
    }

    /// Write the log. Errors are returned so callers can log them, but a write
    /// failure is never fatal: the worst outcome is that the demotion is not
    /// remembered across restarts.
    pub fn save(&self, engines_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(engines_dir)?;
        let path = Self::path_in(engines_dir);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Is this acceleration proven unusable at this engine version?
    ///
    /// CPU is never excludable. It is the terminal row of the decision table on
    /// every platform, so honouring an exclusion for it could only ever produce
    /// "no engine at all" — and if the CPU backend genuinely cannot load, no
    /// other engine in the catalog could either (they all ship the same
    /// `ggml-cpu-*` set). That case is reported as an explicit engine failure,
    /// not routed around.
    pub fn is_excluded(&self, acceleration: &AccelerationType, engine_version: &str) -> bool {
        if *acceleration == AccelerationType::CPU {
            return false;
        }
        let wanted = acceleration.to_string();
        self.failures.iter().any(|f| {
            f.acceleration.eq_ignore_ascii_case(&wanted)
                && f.engine_version.eq_ignore_ascii_case(engine_version)
        })
    }

    /// Reason recorded for an excluded acceleration, for user-facing messages.
    pub fn reason_for(
        &self,
        acceleration: &AccelerationType,
        engine_version: &str,
    ) -> Option<&str> {
        let wanted = acceleration.to_string();
        self.failures
            .iter()
            .find(|f| {
                f.acceleration.eq_ignore_ascii_case(&wanted)
                    && f.engine_version.eq_ignore_ascii_case(engine_version)
            })
            .map(|f| f.reason.as_str())
    }

    /// Record a proven backend-load failure. Idempotent: re-recording the same
    /// (acceleration, version) refreshes the reason rather than appending a
    /// duplicate, so the file cannot grow without bound across restarts.
    ///
    /// Recording a CPU failure is allowed and useful for diagnostics, but
    /// `is_excluded` will still never suppress CPU.
    pub fn record_failure(
        &mut self,
        acceleration: &AccelerationType,
        engine_id: &str,
        engine_version: &str,
        reason: impl Into<String>,
    ) {
        let acceleration_str = acceleration.to_string();
        let reason = reason.into();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();

        if let Some(existing) = self.failures.iter_mut().find(|f| {
            f.acceleration.eq_ignore_ascii_case(&acceleration_str)
                && f.engine_version.eq_ignore_ascii_case(engine_version)
        }) {
            existing.reason = reason;
            existing.engine_id = Some(engine_id.to_string());
            existing.recorded_at = now;
            existing.source = "app".to_string();
            return;
        }

        info!(
            "Recording '{}' acceleration as unusable on this machine at engine version {} \
             (engine '{}'): {}",
            acceleration_str, engine_version, engine_id, reason
        );

        self.failures.push(VerificationFailure {
            acceleration: acceleration_str,
            engine_version: engine_version.to_string(),
            reason,
            source: "app".to_string(),
            engine_id: Some(engine_id.to_string()),
            recorded_at: now,
        });
    }

    /// Clear the exclusion for an acceleration that has now verified
    /// successfully — e.g. after the user installed the VC++ redistributable or
    /// updated their GPU driver. Returns true when something was removed.
    ///
    /// This is what keeps a demotion from outliving the problem that caused it.
    pub fn clear_acceleration(
        &mut self,
        acceleration: &AccelerationType,
        engine_version: &str,
    ) -> bool {
        let wanted = acceleration.to_string();
        let before = self.failures.len();
        self.failures.retain(|f| {
            !(f.acceleration.eq_ignore_ascii_case(&wanted)
                && f.engine_version.eq_ignore_ascii_case(engine_version))
        });
        let removed = self.failures.len() != before;
        if removed {
            info!(
                "'{}' acceleration verified successfully at engine version {} — clearing its \
                 previous failure record",
                wanted, engine_version
            );
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_with(entries: &[(&str, &str)]) -> VerificationLog {
        VerificationLog {
            schema_version: SCHEMA_VERSION,
            failures: entries
                .iter()
                .map(|(accel, ver)| VerificationFailure {
                    acceleration: accel.to_string(),
                    engine_version: ver.to_string(),
                    reason: "test".into(),
                    source: "test".into(),
                    engine_id: None,
                    recorded_at: None,
                })
                .collect(),
        }
    }

    #[test]
    fn excludes_matching_acceleration_and_version() {
        let log = log_with(&[("Vulkan", "b8037")]);
        assert!(log.is_excluded(&AccelerationType::Vulkan, "b8037"));
    }

    #[test]
    fn version_bump_clears_the_exclusion() {
        // The central guarantee: a demotion is scoped to the release that failed.
        let log = log_with(&[("Vulkan", "b8037")]);
        assert!(!log.is_excluded(&AccelerationType::Vulkan, "b9000"));
    }

    #[test]
    fn other_accelerations_are_unaffected() {
        let log = log_with(&[("Vulkan", "b8037")]);
        assert!(!log.is_excluded(&AccelerationType::CUDA, "b8037"));
    }

    #[test]
    fn cpu_is_never_excludable() {
        // Even with an explicit CPU entry, CPU must remain selectable — it is
        // the terminal row of the decision table.
        let log = log_with(&[("CPU", "b8037")]);
        assert!(!log.is_excluded(&AccelerationType::CPU, "b8037"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let log = log_with(&[("vulkan", "B8037")]);
        assert!(log.is_excluded(&AccelerationType::Vulkan, "b8037"));
    }

    #[test]
    fn recording_twice_updates_rather_than_duplicates() {
        let mut log = VerificationLog::default();
        log.record_failure(&AccelerationType::Vulkan, "e1", "b8037", "first");
        log.record_failure(&AccelerationType::Vulkan, "e1", "b8037", "second");
        assert_eq!(log.failures.len(), 1);
        assert_eq!(log.failures[0].reason, "second");
    }

    #[test]
    fn recording_different_versions_keeps_both() {
        let mut log = VerificationLog::default();
        log.record_failure(&AccelerationType::Vulkan, "e1", "b8037", "old");
        log.record_failure(&AccelerationType::Vulkan, "e2", "b9000", "new");
        assert_eq!(log.failures.len(), 2);
    }

    #[test]
    fn clearing_removes_only_the_matching_entry() {
        let mut log = log_with(&[("Vulkan", "b8037"), ("CUDA", "b8037")]);
        assert!(log.clear_acceleration(&AccelerationType::Vulkan, "b8037"));
        assert!(!log.is_excluded(&AccelerationType::Vulkan, "b8037"));
        assert!(log.is_excluded(&AccelerationType::CUDA, "b8037"));
    }

    #[test]
    fn clearing_a_missing_entry_reports_false() {
        let mut log = VerificationLog::default();
        assert!(!log.clear_acceleration(&AccelerationType::Vulkan, "b8037"));
    }

    #[test]
    fn parses_the_exact_document_the_nsis_installer_writes() {
        // Byte-for-byte the shape emitted by oci_WriteVerificationLog. If this
        // test fails, the installer and the app have drifted apart.
        let installer_json = r#"{
  "schema_version": 1,
  "failures": [
    {
      "acceleration": "Vulkan",
      "engine_version": "b8037",
      "reason": "Engine never self-reported loading its Vulkan backend when launched with --version during installation",
      "source": "installer"
    }
  ]
}
"#;
        let log: VerificationLog = serde_json::from_str(installer_json).expect("installer JSON must parse");
        assert!(log.is_excluded(&AccelerationType::Vulkan, "b8037"));
        assert_eq!(log.failures[0].engine_id, None);
        assert_eq!(log.failures[0].recorded_at, None);
    }

    #[test]
    fn parses_an_installer_document_with_two_failures() {
        let installer_json = r#"{
  "schema_version": 1,
  "failures": [
    { "acceleration": "CUDA", "engine_version": "b8037", "reason": "r", "source": "installer" },
    { "acceleration": "Vulkan", "engine_version": "b8037", "reason": "r", "source": "installer" }
  ]
}"#;
        let log: VerificationLog = serde_json::from_str(installer_json).unwrap();
        assert!(log.is_excluded(&AccelerationType::CUDA, "b8037"));
        assert!(log.is_excluded(&AccelerationType::Vulkan, "b8037"));
    }

    #[test]
    fn empty_failures_array_from_installer_parses() {
        let log: VerificationLog =
            serde_json::from_str(r#"{"schema_version":1,"failures":[]}"#).unwrap();
        assert!(!log.is_excluded(&AccelerationType::Vulkan, "b8037"));
    }

    #[test]
    fn missing_file_is_empty_not_an_error() {
        let dir = std::env::temp_dir().join("oca-verif-missing-test");
        let _ = std::fs::remove_dir_all(&dir);
        let log = VerificationLog::load(&dir);
        assert!(log.failures.is_empty());
    }

    #[test]
    fn corrupt_file_is_treated_as_empty() {
        let dir = std::env::temp_dir().join("oca-verif-corrupt-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(VerificationLog::path_in(&dir), "{not json at all").unwrap();
        let log = VerificationLog::load(&dir);
        assert!(log.failures.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn future_schema_version_is_ignored_rather_than_misread() {
        let dir = std::env::temp_dir().join("oca-verif-future-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            VerificationLog::path_in(&dir),
            r#"{"schema_version":999,"failures":[{"acceleration":"Vulkan","engine_version":"b8037","reason":"r","source":"x"}]}"#,
        )
        .unwrap();
        let log = VerificationLog::load(&dir);
        assert!(!log.is_excluded(&AccelerationType::Vulkan, "b8037"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join("oca-verif-roundtrip-test");
        let _ = std::fs::remove_dir_all(&dir);
        let mut log = VerificationLog::default();
        log.record_failure(&AccelerationType::Vulkan, "eid", "b8037", "because");
        log.save(&dir).unwrap();

        let loaded = VerificationLog::load(&dir);
        assert!(loaded.is_excluded(&AccelerationType::Vulkan, "b8037"));
        assert_eq!(loaded.reason_for(&AccelerationType::Vulkan, "b8037"), Some("because"));
        assert_eq!(loaded.failures[0].engine_id.as_deref(), Some("eid"));
        assert!(loaded.failures[0].recorded_at.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
