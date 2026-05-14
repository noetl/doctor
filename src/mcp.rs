//! MCP HTTP surface.
//!
//! Exposes the doctor CLI subcommands as JSON-over-HTTP tools so that
//! monitoring agents and other MCP clients can invoke runtime-reaper
//! diagnostics + safe repair without each caller embedding the detection
//! logic. The transport is intentionally simple — one POST endpoint per
//! tool — so the same surface works as a small Kubernetes Deployment,
//! a sidecar, or a long-running CronJob webhook target.
//!
//! Routes:
//!
//!   GET  /healthz                                  -> {"status":"ok"}
//!   GET  /tools                                    -> tool manifest
//!   POST /tools/detect/invoke                      -> detection outcome
//!   POST /tools/reachability/invoke                -> reachability outcome
//!   POST /tools/repair_trigger_reaper/invoke       -> repair outcome
//!
//! Each invoke body is `{}` (no arguments are accepted today; values are
//! taken from the server's startup config). Returning `{"result": Outcome}`
//! keeps the wire format stable for additional tools.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json as AxumJson},
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::signal;

use crate::embed;
use crate::report::Outcome;
use crate::runner::{PlaybookRunOptions, PlaybookRunner};

#[derive(Clone)]
struct AppState {
    runner: Arc<PlaybookRunner>,
    noetl_url: String,
    pg_dsn: Option<String>,
}

pub async fn serve(host: String, port: u16, runner: PlaybookRunner, noetl_url: String, pg_dsn: Option<String>) -> Result<()> {
    let state = AppState { runner: Arc::new(runner), noetl_url, pg_dsn };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/tools", get(list_tools))
        .route("/tools/detect/invoke", post(invoke_detect))
        .route("/tools/reachability/invoke", post(invoke_reachability))
        .route("/tools/repair_trigger_reaper/invoke", post(invoke_trigger_reaper))
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}").parse().with_context(|| format!("invalid bind address {host}:{port}"))?;
    let listener = TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    tracing::warn!(target: "noetl_doctor::mcp", %addr, "noetl-doctor MCP listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve failed")?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut term) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            term.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    tracing::warn!(target: "noetl_doctor::mcp", "shutdown signal received");
}

async fn healthz() -> impl IntoResponse {
    AxumJson(json!({"status": "ok"}))
}

async fn list_tools() -> impl IntoResponse {
    AxumJson(json!({
        "tools": [
            {
                "name": "detect",
                "description": "Run the stuck-execution + stale-command detection playbook.",
                "input_schema": {"type": "object", "properties": {}, "additionalProperties": false}
            },
            {
                "name": "reachability",
                "description": "Probe NoETL server, Postgres and runtime pool reachability.",
                "input_schema": {"type": "object", "properties": {}, "additionalProperties": false}
            },
            {
                "name": "repair_trigger_reaper",
                "description": "Ask the NoETL server to perform a single command-reaper sweep (404-tolerant).",
                "input_schema": {"type": "object", "properties": {}, "additionalProperties": false}
            }
        ]
    }))
}

async fn invoke_detect(State(state): State<AppState>) -> Result<AxumJson<Value>, ApiError> {
    let playbook = embed::materialize_playbook(embed::DETECT_STUCK_EXECUTIONS).map_err(ApiError::from)?;
    let mut set = vec![format!("noetl_server_url={}", state.noetl_url)];
    if let Some(dsn) = &state.pg_dsn {
        set.push(format!("pg_dsn={}", dsn));
    }
    let value = state
        .runner
        .run(PlaybookRunOptions { playbook: playbook.path().to_path_buf(), set, runtime: "local" })
        .await
        .map_err(ApiError::from)?;
    let status = value.get("status").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
    let outcome = Outcome::from_status("detect", &status, value);
    Ok(AxumJson(json!({"result": outcome})))
}

async fn invoke_reachability(State(state): State<AppState>) -> Result<AxumJson<Value>, ApiError> {
    let playbook = embed::materialize_playbook(embed::REACHABILITY_SMOKE).map_err(ApiError::from)?;
    let mut set = vec![format!("noetl_server_url={}", state.noetl_url)];
    if let Some(dsn) = &state.pg_dsn {
        set.push(format!("pg_dsn={}", dsn));
    }
    let value = state
        .runner
        .run(PlaybookRunOptions { playbook: playbook.path().to_path_buf(), set, runtime: "local" })
        .await
        .map_err(ApiError::from)?;
    let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let outcome = Outcome::for_bool("reachability", ok, value);
    Ok(AxumJson(json!({"result": outcome})))
}

async fn invoke_trigger_reaper(State(state): State<AppState>) -> Result<AxumJson<Value>, ApiError> {
    let playbook = embed::materialize_playbook(embed::TRIGGER_COMMAND_REAPER).map_err(ApiError::from)?;
    let set = vec![format!("noetl_server_url={}", state.noetl_url)];
    let value = state
        .runner
        .run(PlaybookRunOptions { playbook: playbook.path().to_path_buf(), set, runtime: "local" })
        .await
        .map_err(ApiError::from)?;
    let status = value.get("status").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
    let outcome = Outcome::from_status("repair.trigger_reaper", &status, value);
    Ok(AxumJson(json!({"result": outcome})))
}

struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = json!({"error": format!("{:#}", self.0)});
        (StatusCode::INTERNAL_SERVER_ERROR, AxumJson(body)).into_response()
    }
}
