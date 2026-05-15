//! Integration smoke tests for the `noetl-doctor` binary.
//!
//! These do not require a running NoETL server; they verify the CLI
//! parses, prints help / version, and surfaces a sane error when the
//! `noetl` CLI binary is not on PATH.

use assert_cmd::Command;
use predicates::str::contains;

fn doctor() -> Command {
    Command::cargo_bin("noetl-doctor").expect("noetl-doctor binary built")
}

#[test]
fn prints_help() {
    doctor()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("Out-of-process runtime reaper"))
        .stdout(contains("detect"))
        .stdout(contains("reachability"))
        .stdout(contains("repair"))
        .stdout(contains("mcp"));
}

#[test]
fn prints_version() {
    doctor().arg("--version").assert().success().stdout(contains("noetl-doctor"));
}

#[test]
fn playbooks_subcommand_lists_bundled_names_without_requiring_noetl_cli() {
    // `playbooks` is a pure metadata listing of the embedded YAML; it
    // must succeed even when the `noetl` CLI is not installed.
    doctor()
        .arg("playbooks")
        .env("PATH", "/nonexistent")
        .env_remove("NOETL_DOCTOR_NOETL_BIN")
        .assert()
        .success()
        .stdout(contains("doctor/detect_stuck_executions"))
        .stdout(contains("doctor/reachability_smoke"))
        .stdout(contains("doctor/trigger_command_reaper"));
}

#[test]
fn missing_noetl_cli_surfaces_clear_error() {
    doctor()
        .args(["detect", "--noetl-url", "http://x"])
        .env("PATH", "/nonexistent")
        .env_remove("NOETL_DOCTOR_NOETL_BIN")
        .assert()
        .failure()
        .stderr(contains("noetl CLI not found"));
}
