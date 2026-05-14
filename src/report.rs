//! Stable JSON shape emitted by `noetl-doctor`.
//!
//! Monitoring rules pin against `Outcome.action`, `Outcome.severity` and
//! `Outcome.exit_code`. The exact `data` payload is playbook-specific and
//! intentionally forwarded as a `serde_json::Value` so we don't have to
//! keep doctor's typed shape in lock-step with playbook changes.

use std::process::ExitCode;

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// All clear.
    Ok,
    /// Anomaly surfaced; monitoring should branch.
    Anomaly,
    /// Repair attempted; check `data` to see what happened.
    Repaired,
    /// Doctor could not complete the action.
    Error,
}

#[derive(Debug, Serialize)]
pub struct Outcome {
    pub action: String,
    pub severity: Severity,
    pub generated_at: String,
    pub data: Value,
}

impl Outcome {
    pub fn ok(action: impl Into<String>, data: Value) -> Self {
        Self { action: action.into(), severity: Severity::Ok, generated_at: now_iso(), data }
    }

    pub fn anomaly(action: impl Into<String>, data: Value) -> Self {
        Self { action: action.into(), severity: Severity::Anomaly, generated_at: now_iso(), data }
    }

    #[allow(dead_code)] // exposed for downstream embedders of the crate
    pub fn repaired(action: impl Into<String>, data: Value) -> Self {
        Self { action: action.into(), severity: Severity::Repaired, generated_at: now_iso(), data }
    }

    #[allow(dead_code)] // exposed for downstream embedders of the crate
    pub fn error(action: impl Into<String>, data: Value) -> Self {
        Self { action: action.into(), severity: Severity::Error, generated_at: now_iso(), data }
    }

    /// Map a playbook-reported `status` string to a severity.
    ///
    /// Conventions used by bundled playbooks:
    ///
    /// * `"ok"`     → `Severity::Ok`
    /// * `"stuck"`  → `Severity::Anomaly`
    /// * `"noop"`   → `Severity::Repaired` (best-effort no-op repair)
    /// * `"error"`  → `Severity::Error`
    /// * anything else → `Severity::Anomaly` (fail loud)
    pub fn from_status(action: impl Into<String>, status: &str, data: Value) -> Self {
        let sev = match status {
            "ok" => Severity::Ok,
            "stuck" => Severity::Anomaly,
            "noop" => Severity::Repaired,
            "error" => Severity::Error,
            _ => Severity::Anomaly,
        };
        Self { action: action.into(), severity: sev, generated_at: now_iso(), data }
    }

    pub fn for_bool(action: impl Into<String>, ok: bool, data: Value) -> Self {
        if ok {
            Self::ok(action, data)
        } else {
            Self::anomaly(action, data)
        }
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Translate a doctor `Outcome` to a process exit code that monitoring
/// pipelines can branch on:
///
/// * `0` — clean / OK / repaired
/// * `2` — anomaly detected
/// * `3` — runtime error
pub fn exit_code_for_outcome(outcome: &Outcome) -> ExitCode {
    match outcome.severity {
        Severity::Ok | Severity::Repaired => ExitCode::from(0),
        Severity::Anomaly => ExitCode::from(2),
        Severity::Error => ExitCode::from(3),
    }
}
