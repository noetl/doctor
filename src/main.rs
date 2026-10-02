//! `noetl-doctor` — out-of-process runtime reaper for NoETL.
//!
//! This binary is a thin Rust wrapper over the existing `noetl` Rust CLI.
//! It does **not** re-implement NoETL internals; instead, every subcommand
//! resolves to one of the bundled healing playbooks under `playbooks/`
//! and shells out to:
//!
//!     noetl run <playbook> --runtime local --set key=value ...
//!
//! Output of the playbook (`status` step or `result.data`) is parsed and
//! re-emitted as a stable JSON report so monitoring rules can branch on
//! `exit_code` and machine-readable fields. The durable runtime fix lives
//! in `repos/noetl/noetl/server/command_reaper.py`; doctor is the
//! monitoring-callable surface around it.
//!
//! Commands:
//!
//! * `noetl-doctor detect` — run `playbooks/detect_stuck_executions.yaml`
//! * `noetl-doctor reachability` — run `playbooks/reachability_smoke.yaml`
//! * `noetl-doctor repair trigger-reaper` — run `playbooks/trigger_command_reaper.yaml`
//! * `noetl-doctor repair run-playbook <path>` — escape hatch: run any local-runtime playbook
//! * `noetl-doctor provision <action>` — ops-style lifecycle for the MCP server
//!   (deploy / redeploy / status / destroy / logs / help), mirrors `repos/ops`
//! * `noetl-doctor mcp serve` — expose detect/reachability/repair as MCP tools

mod embed;
mod mcp;
mod report;
mod runner;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::Level;
use tracing_subscriber::EnvFilter;

use crate::report::{exit_code_for_outcome, Outcome};
use crate::runner::PlaybookRunner;

#[derive(Parser, Debug)]
#[command(
    name = "noetl-doctor",
    version,
    about = "Out-of-process runtime reaper for NoETL (Rust wrapper over the noetl CLI).",
    long_about = None,
)]
struct Cli {
    /// Override the `noetl` Rust CLI binary path. Defaults to `which noetl`.
    #[arg(long, env = "NOETL_DOCTOR_NOETL_BIN", global = true)]
    noetl_bin: Option<PathBuf>,

    /// Base URL of the NoETL server. Used as `--set noetl_server_url=...`.
    #[arg(long, env = "NOETL_DOCTOR_NOETL_URL", global = true, default_value = "http://localhost:8082")]
    noetl_url: String,

    /// Postgres DSN to the NoETL database (read-only). Used as `--set pg_dsn=...`.
    #[arg(long, env = "NOETL_DOCTOR_PG_DSN", global = true)]
    pg_dsn: Option<String>,

    /// Stale threshold (seconds) for CLAIMED/RUNNING commands.
    #[arg(long, env = "NOETL_DOCTOR_STALE_SECONDS", global = true, default_value_t = 300.0)]
    stale_seconds: f64,

    /// Stale threshold (seconds) for PENDING commands.
    #[arg(long, env = "NOETL_DOCTOR_PENDING_RETRY_SECONDS", global = true, default_value_t = 60.0)]
    pending_retry_seconds: f64,

    /// Maximum rows surfaced per detection query.
    #[arg(long, env = "NOETL_DOCTOR_MAX_ROWS", global = true, default_value_t = 200)]
    max_rows: u32,

    /// Always emit JSON on stdout.
    #[arg(long, global = true, default_value_t = true)]
    json: bool,

    /// Increase log verbosity (`-v`, `-vv`).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the stuck-execution + stale-command detection playbook.
    Detect,

    /// Run the server / Postgres / worker-pool reachability smoke playbook.
    Reachability,

    /// Safe repair actions. Each action delegates to a NoETL playbook or API.
    Repair {
        #[command(subcommand)]
        action: RepairAction,
    },

    /// Provision the noetl-doctor MCP server itself.
    ///
    /// Drives the ops-style `playbooks/provision_doctor_mcp.yaml`
    /// (lifecycle verbs: deploy / redeploy / status / destroy / logs /
    /// help). Mirrors the conventions used by `repos/ops` playbooks
    /// under `automation/development/` and `automation/infrastructure/`.
    Provision {
        /// Lifecycle verb forwarded to the provisioning playbook.
        #[arg(value_enum, default_value_t = ProvisionAction::Help)]
        action: ProvisionAction,

        /// Override the doctor MCP image (e.g. ghcr.io/noetl/doctor:0.1.0).
        #[arg(long)]
        image: Option<String>,

        /// Kubernetes namespace to provision into.
        #[arg(long, default_value = "noetl-doctor")]
        namespace: String,

        /// Kubernetes context to refuse to run against any other.
        #[arg(long, default_value = "kind-noetl")]
        expected_kube_context: String,

        /// Additional `--set key=value` overrides forwarded verbatim.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
    },

    /// MCP HTTP server exposing detect / reachability / repair as tools.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },

    /// List the bundled healing playbook paths (useful for `noetl run`).
    Playbooks,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum ProvisionAction {
    Deploy,
    Redeploy,
    Status,
    Destroy,
    Logs,
    Help,
}

impl ProvisionAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Deploy => "deploy",
            Self::Redeploy => "redeploy",
            Self::Status => "status",
            Self::Destroy => "destroy",
            Self::Logs => "logs",
            Self::Help => "help",
        }
    }
}

#[derive(Subcommand, Debug)]
enum RepairAction {
    /// Ask the running NoETL server to perform one command-reaper sweep.
    ///
    /// Tolerates HTTP 404 — older NoETL versions do not yet expose the
    /// admin sweep endpoint, in which case the in-process reaper still
    /// runs on its own interval.
    TriggerReaper,

    /// Escape hatch: run any local-runtime NoETL playbook.
    RunPlaybook {
        /// Path to a `.yaml` playbook.
        playbook: PathBuf,

        /// Forwarded verbatim as `--set key=value` arguments.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum McpAction {
    /// Serve the MCP HTTP surface (POST /tools/<name>/invoke).
    Serve {
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
        #[arg(long, default_value_t = 8765)]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let result: Result<Outcome> = dispatch(cli).await;
    match result {
        Ok(outcome) => {
            // Always emit on stdout, regardless of --json flag, because monitoring
            // pipelines pin a stable JSON shape (see `report::Outcome`).
            match serde_json::to_string_pretty(&outcome) {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("serialization error: {e}"),
            }
            exit_code_for_outcome(&outcome)
        }
        Err(err) => {
            eprintln!("noetl-doctor error: {err:#}");
            ExitCode::from(3)
        }
    }
}

fn init_logging(verbose: u8) {
    let default_level = match verbose {
        0 => Level::WARN,
        1 => Level::INFO,
        _ => Level::DEBUG,
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level.to_string()));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}

async fn dispatch(cli: Cli) -> Result<Outcome> {
    // `playbooks` is a pure metadata listing of the embedded YAML and
    // does not need the noetl CLI on PATH. All other subcommands shell
    // out to `noetl run` so resolve the binary up-front.
    if matches!(cli.command, Command::Playbooks) {
        return commands::list_playbooks();
    }

    let noetl_bin = runner::resolve_noetl_binary(cli.noetl_bin.as_deref()).context("locating noetl CLI binary")?;
    let runner = PlaybookRunner::new(noetl_bin);

    match &cli.command {
        Command::Detect => commands::detect(&runner, &cli).await,
        Command::Reachability => commands::reachability(&runner, &cli).await,
        Command::Repair { action } => match action {
            RepairAction::TriggerReaper => commands::trigger_reaper(&runner, &cli).await,
            RepairAction::RunPlaybook { playbook, set } => {
                commands::run_playbook(&runner, playbook.clone(), set).await
            }
        },
        Command::Provision { action, image, namespace, expected_kube_context, set } => {
            commands::provision(&runner, *action, image.as_deref(), namespace, expected_kube_context, set).await
        }
        Command::Mcp { action } => match action {
            McpAction::Serve { host, port } => {
                let host = host.clone();
                let port = *port;
                mcp::serve(host, port, runner, cli.noetl_url.clone(), cli.pg_dsn.clone()).await?;
                Ok(Outcome::ok("mcp.serve", serde_json::json!({"shutdown": "clean"})))
            }
        },
        Command::Playbooks => unreachable!("handled above"),
    }
}

mod commands {
    use std::path::PathBuf;

    use anyhow::Result;

    use crate::embed;
    use crate::report::Outcome;
    use crate::runner::{PlaybookRunOptions, PlaybookRunner};
    use crate::Cli;

    pub(super) async fn detect(runner: &PlaybookRunner, cli: &Cli) -> Result<Outcome> {
        let playbook = embed::materialize_playbook(embed::DETECT_STUCK_EXECUTIONS)?;
        let mut set = vec![
            format!("noetl_server_url={}", cli.noetl_url),
            format!("stale_seconds={}", cli.stale_seconds),
            format!("pending_retry_seconds={}", cli.pending_retry_seconds),
            format!("max_rows={}", cli.max_rows),
        ];
        if let Some(dsn) = &cli.pg_dsn {
            set.push(format!("pg_dsn={}", dsn));
        }
        let value =
            runner.run(PlaybookRunOptions { playbook: playbook.path().to_path_buf(), set, runtime: "local" }).await?;

        let status = extract_status(&value);
        Ok(Outcome::from_status("detect", &status, value))
    }

    pub(super) async fn reachability(runner: &PlaybookRunner, cli: &Cli) -> Result<Outcome> {
        let playbook = embed::materialize_playbook(embed::REACHABILITY_SMOKE)?;
        let mut set = vec![format!("noetl_server_url={}", cli.noetl_url)];
        if let Some(dsn) = &cli.pg_dsn {
            set.push(format!("pg_dsn={}", dsn));
        }
        let value =
            runner.run(PlaybookRunOptions { playbook: playbook.path().to_path_buf(), set, runtime: "local" }).await?;

        let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        Ok(Outcome::for_bool("reachability", ok, value))
    }

    pub(super) async fn trigger_reaper(runner: &PlaybookRunner, cli: &Cli) -> Result<Outcome> {
        let playbook = embed::materialize_playbook(embed::TRIGGER_COMMAND_REAPER)?;
        let set = vec![format!("noetl_server_url={}", cli.noetl_url)];
        let value =
            runner.run(PlaybookRunOptions { playbook: playbook.path().to_path_buf(), set, runtime: "local" }).await?;
        let status = extract_status(&value);
        Ok(Outcome::from_status("repair.trigger_reaper", &status, value))
    }

    fn extract_status(value: &serde_json::Value) -> String {
        value.get("status").and_then(|v| v.as_str()).unwrap_or("unknown").to_string()
    }

    pub(super) async fn run_playbook(runner: &PlaybookRunner, playbook: PathBuf, set: &[String]) -> Result<Outcome> {
        let value =
            runner.run(PlaybookRunOptions { playbook, set: set.to_vec(), runtime: "local" }).await?;
        Ok(Outcome::ok("repair.run_playbook", value))
    }

    pub(super) async fn provision(
        runner: &PlaybookRunner,
        action: super::ProvisionAction,
        image: Option<&str>,
        namespace: &str,
        expected_kube_context: &str,
        extra: &[String],
    ) -> Result<Outcome> {
        let playbook = embed::materialize_playbook(embed::PROVISION_DOCTOR_MCP)?;
        let mut set = vec![
            format!("action={}", action.as_str()),
            format!("namespace={}", namespace),
            format!("expected_kube_context={}", expected_kube_context),
        ];
        if let Some(img) = image {
            set.push(format!("image={}", img));
        }
        // Caller-supplied --set pairs win over our defaults; the NoETL
        // CLI applies later --set values on top of earlier ones, so
        // append rather than prepend.
        for kv in extra {
            set.push(kv.clone());
        }
        let value =
            runner.run(PlaybookRunOptions { playbook: playbook.path().to_path_buf(), set, runtime: "local" }).await?;
        // The provisioning playbook has no JSON `status` field — it
        // succeeds (kubectl exit 0) or fails. We map that to Outcome::ok
        // and let the caller inspect `data` for the kubectl output that
        // the runner's stdout-capture surfaces.
        Ok(Outcome::ok(format!("provision.{}", action.as_str()), value))
    }

    pub(super) fn list_playbooks() -> Result<Outcome> {
        let value = serde_json::json!({
            "detect_stuck_executions": embed::DETECT_STUCK_EXECUTIONS_NAME,
            "inspect_stale_commands": embed::INSPECT_STALE_COMMANDS_NAME,
            "reachability_smoke": embed::REACHABILITY_SMOKE_NAME,
            "trigger_command_reaper": embed::TRIGGER_COMMAND_REAPER_NAME,
            "provision_doctor_mcp": embed::PROVISION_DOCTOR_MCP_NAME,
        });
        Ok(Outcome::ok("playbooks", value))
    }
}

