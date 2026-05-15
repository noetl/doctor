//! Compile-time-embedded healing playbooks.
//!
//! At build time we vendor the YAML files under `playbooks/` into the
//! `noetl-doctor` binary via `include_str!`. At run time the
//! `materialize_playbook` helper writes the YAML to a tempfile and returns
//! a `tempfile::NamedTempFile` that the runner passes to the `noetl` CLI.
//!
//! Why embed: doctor must be shippable as a single static binary that
//! monitoring systems can drop on any host without also distributing the
//! playbook tree. The bundled YAML is also the authoritative source if a
//! caller wants to run the playbook directly with `noetl run`, so
//! `noetl-doctor playbooks` prints the friendly names; operators can copy
//! the path from disk for direct invocation.

use std::io::Write;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

pub const DETECT_STUCK_EXECUTIONS_NAME: &str = "doctor/detect_stuck_executions";
pub const INSPECT_STALE_COMMANDS_NAME: &str = "doctor/inspect_stale_commands";
pub const REACHABILITY_SMOKE_NAME: &str = "doctor/reachability_smoke";
pub const TRIGGER_COMMAND_REAPER_NAME: &str = "doctor/trigger_command_reaper";
pub const PROVISION_DOCTOR_MCP_NAME: &str = "doctor/provision_doctor_mcp";

pub const DETECT_STUCK_EXECUTIONS: &str =
    include_str!("../playbooks/detect_stuck_executions.yaml");
#[allow(dead_code)] // accessible via `noetl-doctor repair run-playbook` after disk extraction
pub const INSPECT_STALE_COMMANDS: &str =
    include_str!("../playbooks/inspect_stale_commands.yaml");
pub const REACHABILITY_SMOKE: &str = include_str!("../playbooks/reachability_smoke.yaml");
pub const TRIGGER_COMMAND_REAPER: &str =
    include_str!("../playbooks/trigger_command_reaper.yaml");
pub const PROVISION_DOCTOR_MCP: &str = include_str!("../playbooks/provision_doctor_mcp.yaml");

/// Write the given embedded playbook YAML into a tempfile and return its
/// handle. The tempfile is deleted when the handle is dropped, which the
/// caller controls — keep it alive for the lifetime of the `noetl run`.
pub fn materialize_playbook(yaml: &str) -> Result<NamedTempFile> {
    let mut tmp = tempfile::Builder::new()
        .prefix("noetl-doctor-")
        .suffix(".yaml")
        .tempfile()
        .context("creating tempfile for embedded playbook")?;
    tmp.write_all(yaml.as_bytes()).context("writing embedded playbook to tempfile")?;
    tmp.as_file_mut().flush().ok();
    Ok(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_playbooks_are_non_empty_yaml() {
        for (name, body) in [
            ("detect_stuck_executions", DETECT_STUCK_EXECUTIONS),
            ("inspect_stale_commands", INSPECT_STALE_COMMANDS),
            ("reachability_smoke", REACHABILITY_SMOKE),
            ("trigger_command_reaper", TRIGGER_COMMAND_REAPER),
            ("provision_doctor_mcp", PROVISION_DOCTOR_MCP),
        ] {
            assert!(body.contains("apiVersion: noetl.io/v2"), "{name} missing apiVersion");
            assert!(body.contains("kind: Playbook"), "{name} missing kind");
        }
    }

    #[test]
    fn provision_playbook_exposes_ops_style_action_dispatch() {
        // Sanity-check that the provisioning playbook follows the
        // canonical repos/ops shape: `action: help` default, shell
        // dispatch, lifecycle verbs.
        let body = PROVISION_DOCTOR_MCP;
        assert!(body.contains("action: help"), "expected action: help default");
        for verb in ["deploy", "redeploy", "status", "destroy", "logs"] {
            assert!(body.contains(verb), "missing lifecycle verb: {verb}");
        }
        assert!(body.contains("ensure_kube_context"), "missing kube context guard");
    }

    #[test]
    fn materialize_writes_the_full_payload() {
        let tmp = materialize_playbook(DETECT_STUCK_EXECUTIONS).unwrap();
        let written = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(written, DETECT_STUCK_EXECUTIONS);
    }

    /// Regression test for the parse failure Codex hit on 2026-05-14:
    ///
    ///     workflow[1].tool[0]: invalid type: map, expected variant identifier
    ///     at line 58 column 9
    ///
    /// Root cause: the noetl Rust CLI's local-runtime parser
    /// (`repos/cli/src/playbook_runner.rs`) defines `Step.tool` as
    /// `Option<Tool>` (a single internally-tagged enum), AND the
    /// `Tool` variants are limited to:
    /// `shell / http / playbook / duckdb / auth / sink / rhai`.
    /// `postgres` and `python` are server-only tools.
    ///
    /// Every bundled doctor playbook MUST therefore:
    ///   1. use a YAML mapping under `tool:` (not a sequence), AND
    ///   2. set `kind:` to one of the local-runtime variants.
    ///
    /// This test walks each embedded playbook and asserts both.
    #[test]
    fn embedded_playbooks_parse_under_local_runtime_schema() {
        const LOCAL_RUNTIME_TOOL_KINDS: &[&str] =
            &["shell", "http", "playbook", "duckdb", "auth", "sink", "rhai"];

        let cases = [
            ("detect_stuck_executions", DETECT_STUCK_EXECUTIONS),
            ("inspect_stale_commands", INSPECT_STALE_COMMANDS),
            ("reachability_smoke", REACHABILITY_SMOKE),
            ("trigger_command_reaper", TRIGGER_COMMAND_REAPER),
            ("provision_doctor_mcp", PROVISION_DOCTOR_MCP),
        ];

        for (name, body) in cases {
            let yaml: serde_yaml::Value =
                serde_yaml::from_str(body).unwrap_or_else(|e| panic!("{name}: yaml parse failed: {e}"));
            let workflow = yaml
                .get("workflow")
                .and_then(serde_yaml::Value::as_sequence)
                .unwrap_or_else(|| panic!("{name}: missing or non-sequence workflow"));

            for (i, step) in workflow.iter().enumerate() {
                let step_name =
                    step.get("step").and_then(serde_yaml::Value::as_str).unwrap_or("<unnamed>");
                let Some(tool) = step.get("tool") else {
                    continue;
                };
                assert!(
                    tool.is_mapping(),
                    "{name}: workflow[{i}] ({step_name}).tool must be a mapping for the \
                     `noetl run --runtime local` parser. Found: {tool:?}"
                );
                let kind = tool
                    .get("kind")
                    .and_then(serde_yaml::Value::as_str)
                    .unwrap_or_else(|| panic!("{name}: workflow[{i}] ({step_name}).tool.kind missing"));
                assert!(
                    LOCAL_RUNTIME_TOOL_KINDS.contains(&kind),
                    "{name}: workflow[{i}] ({step_name}).tool.kind = '{kind}' is not \
                     supported by the Rust local-runtime parser. Allowed: {LOCAL_RUNTIME_TOOL_KINDS:?}"
                );
            }
        }
    }
}
