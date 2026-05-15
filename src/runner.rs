//! Shells out to the existing `noetl` Rust CLI to execute healing playbooks.
//!
//! Doctor never re-implements playbook execution: the canonical NoETL CLI
//! is the single execution engine. The runner here is responsible for:
//!
//! * locating the `noetl` binary (CLI flag → env → `which`),
//! * assembling the `noetl run <playbook> --runtime local --set k=v ...` command,
//! * capturing stdout/stderr,
//! * parsing the final structured value emitted by the playbook's terminal
//!   step. The playbooks under `playbooks/*.yaml` write a JSON object as the
//!   tool result on the report step; we look for the last JSON object in
//!   stdout (the `noetl` CLI prints structured output on success).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use tokio::process::Command;

/// Locate the `noetl` Rust CLI binary.
///
/// Precedence:
/// 1. `--noetl-bin <path>` (or `NOETL_DOCTOR_NOETL_BIN`)
/// 2. `which noetl`
pub fn resolve_noetl_binary(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        if !p.exists() {
            return Err(anyhow!("--noetl-bin path does not exist: {}", p.display()));
        }
        return Ok(p.to_path_buf());
    }
    which::which("noetl").context(
        "noetl CLI not found on PATH; install noetl 2.13.0+ (https://github.com/noetl/cli) \
         or pass --noetl-bin /path/to/noetl",
    )
}

#[derive(Debug, Clone)]
pub struct PlaybookRunOptions {
    pub playbook: PathBuf,
    pub set: Vec<String>,
    pub runtime: &'static str,
}

pub struct PlaybookRunner {
    noetl_bin: PathBuf,
}

impl PlaybookRunner {
    pub fn new(noetl_bin: PathBuf) -> Self {
        Self { noetl_bin }
    }

    #[allow(dead_code)] // exposed for downstream embedders + debug logging
    pub fn binary(&self) -> &Path {
        &self.noetl_bin
    }

    /// Execute `noetl run <playbook> --runtime <runtime> --set k=v ...` and
    /// return the playbook's final structured result.
    ///
    /// The healing playbooks all end with a `report` step whose tool emits
    /// a JSON object via Python `return {...}`; the NoETL CLI prints that
    /// object on stdout. We tolerate non-JSON noise around it by scanning
    /// for the last balanced JSON object in the output.
    pub async fn run(&self, opts: PlaybookRunOptions) -> Result<Value> {
        let mut cmd = Command::new(&self.noetl_bin);
        cmd.arg("run").arg(&opts.playbook).arg("--runtime").arg(opts.runtime);
        for kv in &opts.set {
            cmd.arg("--set").arg(kv);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);

        tracing::info!(
            target: "noetl_doctor::runner",
            cli = %self.noetl_bin.display(),
            playbook = %opts.playbook.display(),
            runtime = opts.runtime,
            "executing noetl run"
        );

        let output = cmd.output().await.with_context(|| {
            format!("failed to spawn `noetl run {}` (binary at {})", opts.playbook.display(), self.noetl_bin.display())
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if !output.status.success() {
            tracing::warn!(
                target: "noetl_doctor::runner",
                code = output.status.code(),
                "noetl run exited non-zero"
            );
            return Err(anyhow!(
                "noetl run failed (exit {:?})\nstderr: {}\nstdout: {}",
                output.status.code(),
                tail(&stderr, 1500),
                tail(&stdout, 1500),
            ));
        }

        Ok(extract_last_json_object(&stdout).unwrap_or_else(|| {
            // Healing playbooks should always emit a JSON object on the
            // terminal report step; if for any reason they don't, surface
            // the raw stdout so the caller can debug rather than swallow it.
            serde_json::json!({
                "raw_stdout": tail(&stdout, 4000),
                "raw_stderr": tail(&stderr, 1000),
                "note": "noetl run succeeded but no JSON object was found on stdout"
            })
        }))
    }
}

/// Heuristic: find the last balanced JSON object/array in `s`. The healing
/// playbooks finish with `return {...}` from a Python step; the NoETL CLI
/// prints that structure as the last thing on stdout.
fn extract_last_json_object(s: &str) -> Option<Value> {
    let bytes = s.as_bytes();
    // Walk backwards looking for a closing brace, then locate the matching open.
    for end in (0..bytes.len()).rev() {
        let ch = bytes[end];
        if ch != b'}' && ch != b']' {
            continue;
        }
        let open = if ch == b'}' { b'{' } else { b'[' };
        let mut depth: i64 = 0;
        let mut start: Option<usize> = None;
        for (i, b) in bytes.iter().enumerate().take(end + 1).rev() {
            if *b == ch {
                depth += 1;
            } else if *b == open {
                depth -= 1;
                if depth == 0 {
                    start = Some(i);
                    break;
                }
            }
        }
        if let Some(s_idx) = start {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes[s_idx..=end]) {
                return Some(v);
            }
        }
    }
    None
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let start = s.len() - max;
        let mut adj = start;
        // Don't break UTF-8.
        while adj < s.len() && !s.is_char_boundary(adj) {
            adj += 1;
        }
        format!("...{}", &s[adj..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_trailing_json_object() {
        let stdout = "some noisy log\nINFO 2026-05-14 hello\n{\"status\":\"stuck\",\"count\":3}\n";
        let v = extract_last_json_object(stdout).expect("json should be found");
        assert_eq!(v["status"], "stuck");
        assert_eq!(v["count"], 3);
    }

    #[test]
    fn picks_last_object_when_multiple_present() {
        let stdout = "{\"prev\":true}\n{\"final\":true,\"n\":7}\n";
        let v = extract_last_json_object(stdout).unwrap();
        assert_eq!(v["final"], true);
        assert_eq!(v["n"], 7);
    }

    #[test]
    fn returns_none_when_no_json() {
        assert!(extract_last_json_object("no objects here\n").is_none());
    }

    #[test]
    fn extracts_nested_object() {
        let stdout = "log\n{\"a\":{\"b\":1,\"c\":[1,2,3]}}";
        let v = extract_last_json_object(stdout).unwrap();
        assert_eq!(v["a"]["b"], 1);
        assert_eq!(v["a"]["c"][2], 3);
    }
}
