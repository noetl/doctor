# noetl/doctor

Out-of-process runtime reaper and self-healing surface for NoETL.

`doctor` is a thin **Rust wrapper** around the existing
[`noetl`](https://github.com/noetl/cli) Rust CLI. It does **not** ship
its own execution engine: every healing operation resolves to one of
the bundled YAML playbooks under [`playbooks/`](playbooks) and is
executed via:

```
noetl run <playbook> --runtime local --set k=v ...
```

Each playbook's action step prints a JSON object before the terminal
no-op step. Doctor extracts the last balanced JSON object from stdout
or stderr, re-emits it as a stable `Outcome` shape on stdout, and the
process exit code (0/2/3) lets monitoring branch:

| exit | meaning                          |
|------|----------------------------------|
| 0    | OK / repaired                    |
| 2    | anomaly detected                 |
| 3    | doctor itself failed (CLI error) |

The durable runtime fix lives in
[`repos/noetl/noetl/server/command_reaper.py`](../noetl/noetl/server/command_reaper.py).
Doctor is the monitoring-callable surface around it: it inspects NoETL
through public APIs / read-only SQL via playbook tools, and delegates
every state change to NoETL (admin endpoints or playbooks).

In the NoETL docs and operations runbooks, "doctor" is shorthand for
this runtime reaper surface. It is not a generic troubleshooting
toolkit, and it is not the component that directly reclaims commands.
That responsibility stays in the NoETL server.

## Current recovery model

NoETL self-healing has two layers:

1. **In-process command reaper** (`repos/noetl`) — runs inside
   `deploy/noetl-server` under a `RuntimeLease`, scans `noetl.command`
   for recoverable non-terminal commands, and republishes NATS command
   notifications. Workers still go through the normal claim endpoint,
   so `claim_policy` remains the authority.
2. **Doctor** (`repos/doctor`) — exposes monitoring and MCP entry
   points that run local-runtime playbooks. It detects stale command
   rows, inspects one execution, probes reachability, and can trigger a
   best-effort server reaper sweep when the NoETL admin endpoint exists.

The PFT v2 validation run on 2026-05-15 proved the first layer. Execution
`627209422065893596` completed all 10 facilities and the NoETL server
logs showed two automatic command-reaper recoveries:

```
[COMMAND-REAPER] Found 20 orphaned active command(s); re-publishing
[COMMAND-REAPER] Re-published ... step=fetch_mds_details:task_sequence
[COMMAND-REAPER] Re-published 20/20 recovered commands
```

That happened twice for `fetch_mds_details:task_sequence`. Doctor was
not required to directly fix those rows; it is the surface a monitoring
system can call to detect or nudge the same recovery path.

## Why Rust

- Single static binary, easy to drop into a Kubernetes Job or sidecar.
- Reuses the canonical `noetl` Rust CLI so doctor never re-implements
  catalog resolution, playbook parsing, or local-runtime execution.
- Mirrors the layout of `repos/cli` (clap derive + tokio + axum), so
  contributors familiar with the NoETL CLI find their way around
  immediately.

## Layout

```
doctor/
  Cargo.toml
  rust-toolchain.toml
  Dockerfile             # single multi-stage build, bundles noetl + noetl-doctor
  Makefile
  README.md
  src/
    main.rs              # clap CLI, dispatch table
    runner.rs            # spawn `noetl run`, parse last JSON object from stdout/stderr
    report.rs            # stable Outcome JSON shape + exit-code mapping
    embed.rs             # compile-time embed of playbooks/*.yaml
    mcp.rs               # axum HTTP MCP surface (POST /tools/<name>/invoke)
  playbooks/
    detect_stuck_executions.yaml       # action: detect | help
    inspect_stale_commands.yaml        # action: inspect | help
    reachability_smoke.yaml            # action: probe | help
    trigger_command_reaper.yaml        # action: trigger | help
    provision_doctor_mcp.yaml          # action: deploy | redeploy | status | destroy | logs | help
  tests/
    cli.rs               # `assert_cmd` integration tests for clap parsing
```

### Playbook authoring convention

All bundled playbooks follow the conventions used in `repos/ops`
(see `automation/development/mcp_kubernetes.yaml` and
`automation/infrastructure/*.yaml`):

- A `workload.action` field with `help` (or the playbook's primary
  verb) as the safe default — bare invocation always lands on `help`
  for lifecycle playbooks, or on the canonical read-only verb for
  detection playbooks.
- A `start` step that dispatches to action-specific branches via
  `next.arcs[].when:` guards.
- A `show_help` step printing usage when no known action matches.
- All bundled runtime reaper playbooks use the Rust CLI local-runtime
  subset. In practice that means `kind: shell` with `curl`, `psql`, and
  `jq`; do not use server-only tool kinds such as `postgres`, `python`,
  or `noop` here.
- Provisioning playbooks use `kind: shell` with inline `kubectl` and an
  `ensure_kube_context` guard so they refuse to run against the wrong
  cluster.

Doctor's Rust CLI subcommands set the right `--set action=...` for
the corresponding playbook so the caller never has to remember the
action name.

## Quick start

```bash
# Build the binary
cargo build --release

# Detection with Postgres DSN (preferred: gives full noetl.command visibility)
target/release/noetl-doctor detect \
  --noetl-url http://localhost:8082 \
  --pg-dsn   postgresql://noetl@localhost:54321/noetl \
  --stale-seconds 300

# Reachability smoke
target/release/noetl-doctor reachability \
  --noetl-url http://localhost:8082 \
  --pg-dsn   postgresql://noetl@localhost:54321/noetl

# Nudge the in-process command reaper (404-tolerant)
target/release/noetl-doctor repair trigger-reaper \
  --noetl-url http://localhost:8082

# Escape hatch: run any local-runtime playbook
target/release/noetl-doctor repair run-playbook \
  ./playbooks/inspect_stale_commands.yaml \
  --set execution_id=626611573817082718 \
  --set pg_dsn=postgresql://noetl@localhost:54321/noetl

# Long-running MCP server
target/release/noetl-doctor mcp serve --host 0.0.0.0 --port 8765
```

## Provisioning the doctor MCP server (ops-style)

Doctor ships its own provisioning playbook — same shape as
`repos/ops/automation/development/mcp_kubernetes.yaml`. Two equivalent
entry points:

```bash
# Via the Rust CLI (convenient defaults: namespace=noetl-doctor,
# context=kind-noetl, action=help when no arg given)
target/release/noetl-doctor provision deploy \
  --image ghcr.io/noetl/doctor:0.1.0

target/release/noetl-doctor provision status
target/release/noetl-doctor provision logs
target/release/noetl-doctor provision destroy

# Or directly via the NoETL Rust CLI, exactly the same pattern
# repos/ops uses for every other resource:
noetl run playbooks/provision_doctor_mcp.yaml --runtime local \
  --set action=deploy \
  --set image=ghcr.io/noetl/doctor:0.1.0 \
  --set namespace=noetl-doctor \
  --set noetl_url=http://noetl-server.noetl.svc.cluster.local:80

noetl run playbooks/provision_doctor_mcp.yaml --runtime local \
  --set action=status
```

The provisioning playbook applies a `ServiceAccount`, `Deployment`,
and `Service` via inline `kubectl apply -f -` heredocs (no Helm chart
required), refuses to run against the wrong `kubectl` context, and
optionally accepts a Postgres DSN that lands in a separate opaque
`Secret` rather than in the Deployment's `env`.

The other healing playbooks accept `action=<verb>` too, so the
canonical local-runtime invocation matches the rest of the NoETL
ecosystem:

```bash
noetl run playbooks/detect_stuck_executions.yaml --runtime local \
  --set action=detect \
  --set pg_dsn=postgresql://noetl@localhost:54321/noetl \
  --set stale_seconds=300

noetl run playbooks/reachability_smoke.yaml --runtime local \
  --set action=probe \
  --set noetl_server_url=http://localhost:8082 \
  --set pg_dsn=postgresql://noetl@localhost:54321/noetl

noetl run playbooks/trigger_command_reaper.yaml --runtime local \
  --set action=trigger \
  --set noetl_server_url=http://localhost:8082
```

## Configuration

Global flags / env vars (apply to all subcommands):

| flag                          | env                                   | default                  |
|-------------------------------|---------------------------------------|--------------------------|
| `--noetl-bin`                 | `NOETL_DOCTOR_NOETL_BIN`              | `which noetl`            |
| `--noetl-url`                 | `NOETL_DOCTOR_NOETL_URL`              | `http://localhost:8082`  |
| `--pg-dsn`                    | `NOETL_DOCTOR_PG_DSN`                 | _unset_                  |
| `--stale-seconds`             | `NOETL_DOCTOR_STALE_SECONDS`          | `300`                    |
| `--pending-retry-seconds`     | `NOETL_DOCTOR_PENDING_RETRY_SECONDS`  | `60`                     |
| `--max-rows`                  | `NOETL_DOCTOR_MAX_ROWS`               | `200`                    |

Values that match a `workload.*` field on a bundled playbook are forwarded
as `--set key=value` automatically.

## Docker

```bash
docker build -t noetl/doctor:dev .

# One-shot detection (default CMD)
docker run --rm noetl/doctor:dev detect \
  --noetl-url http://noetl-server.noetl.svc.cluster.local:80

# MCP server
docker run --rm -p 8765:8765 noetl/doctor:dev mcp serve
```

The image bundles the upstream `noetl` CLI release pinned by the
`NOETL_CLI_VERSION` build arg. The default is currently `2.14.2`.
The Dockerfile maps Docker/Podman `TARGETARCH` to the matching
`noetl-v<version>-linux-<arch>.tar.gz` release asset. To target a
different release, pass `--build-arg NOETL_CLI_VERSION=<version>`.

## MCP surface

When run as `noetl-doctor mcp serve`, doctor exposes:

```
GET  /healthz
GET  /tools                                    # tool manifest
POST /tools/detect/invoke                      # detection outcome
POST /tools/reachability/invoke                # reachability outcome
POST /tools/repair_trigger_reaper/invoke       # repair outcome
```

Each `invoke` returns `{"result": Outcome}` matching the stdout JSON
shape, so the CLI and the MCP wire format share one contract.

## Scope boundaries

`doctor` **must**:

- treat NoETL as a black box served over HTTP plus a Postgres database.
- delegate any state change to a NoETL playbook or NoETL API.
- run as a single static Rust binary that calls the `noetl` Rust CLI.

`doctor` **must not**:

- re-implement `noetl.claim_policy.decide_reclaim_for_existing_claim`.
- write to `noetl.command` directly.
- force `loop.done` or fabricate completion events.
- ship its own playbook execution engine.

## Related docs

- NoETL operations guide:
  `repos/docs/docs/operations/runtime-reaper-doctor.md`
- Runtime correctness implementation:
  `repos/noetl/noetl/server/command_reaper.py`
- Claim arbitration:
  `repos/noetl/noetl/server/api/core/claim_policy.py`
