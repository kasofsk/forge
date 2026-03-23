# CLAUDE.md — workflow

## What this is

Issues-based workflow orchestration for AI agents. Forgejo issues are jobs, a sidecar service coordinates, workers claim and execute. See `DESIGN.md` for full architecture.

## Quick start

```bash
./scripts/init.sh          # full setup: infra, users, build, CDC, sidecar, seed fixture
```

This single command tears everything down and rebuilds from scratch. It starts all services and seeds a default fixture. When it finishes, the graph viewer is at `http://localhost:8080/graph` and Forgejo at `http://localhost:3000`.

To start workers after init: `./scripts/workers.sh` (action workers + runners by default)

To stop: `pkill -f workflow-sidecar; pkill -f workflow-cdc` and `./scripts/workers.sh --down`

## Build & test

```bash
cargo build                                                # build all crates
cargo test -p workflow-types                               # unit tests (fast, no infra needed)
cargo test -p workflow-integration-tests -- --include-ignored  # integration tests (requires init.sh)
```

## Workspace layout

| Crate | Kind | Purpose |
|---|---|---|
| `types` | lib | Shared types: Job, JobState, ClaimState, JobTransition, dispatch types, API request/response types |
| `sidecar` | bin | Coordination gateway: CDC consumer, dispatcher, graph, claims, HTTP API, timeout monitor |
| `reviewer` | bin | Standalone PR review process: subscribes to InReview transitions, dispatches Claude review actions, merges/escalates |
| `cdc` | bin | Change data capture: polls Forgejo's PostgreSQL DB, publishes IssueSnapshot to NATS |
| `worker` | lib | SDK for building workers (NATS-based dispatched loop) and work factories |
| `cli` | bin | Interactive CLI worker for manual testing, fixture seeding, admin commands |
| `forgejo-api` | lib | Generated Forgejo REST API client (OpenAPI-derived), used by worker and reviewer |
| `integration` | test | Integration tests against live Forgejo + sidecar |

## Data flow

```
Forgejo (backed by PostgreSQL)
    ↓ CDC polls PostgreSQL for changed issues
CDC process (workflow-cdc)
    ↓ publishes IssueSnapshot to NATS JetStream
Stream "workflow-changes"
    ↓ durable pull consumer
Sidecar consumer
    ↓ reconciles graph, detects state changes
    ↓ publishes JobTransition to "workflow.jobs.transition"
    ↓ sets labels back on Forgejo
    ↓
Dispatcher (subscribes to transitions + worker events)
    → assigns jobs to idle workers via NATS
    → handles worker outcomes (complete/fail/abandon/yield)
    → manages preemption for high-priority jobs
```

The CDC process polls Forgejo's PostgreSQL database for changed issues and publishes full issue snapshots to a NATS JetStream stream. The sidecar consumes the stream with a durable consumer that tracks its position automatically. On restart, both resume from where they left off. No webhooks required.

The consumer publishes `JobTransition` events whenever a job changes state. The dispatcher and future reactors subscribe to this derived event stream. Workers communicate with the dispatcher exclusively via NATS pub/sub — they never make HTTP calls to the sidecar.

## NATS subjects

| Subject | Publisher | Subscriber | Payload |
|---|---|---|---|
| `workflow.changes` | CDC | Consumer | `IssueSnapshot` |
| `workflow.jobs.transition` | Consumer, Dispatcher | Dispatcher, Reviewer (separate process) | `JobTransition` |
| `workflow.monitor.lease-expired` | Monitor | Dispatcher | `LeaseExpiredEvent` |
| `workflow.monitor.timeout` | Monitor | Dispatcher | `JobTimeoutEvent` |
| `workflow.monitor.orphan` | Monitor | Dispatcher | `OrphanDetectedEvent` |
| `workflow.dispatch.register` | Worker | Dispatcher | `WorkerRegistration` |
| `workflow.dispatch.unregister` | Worker | Dispatcher | `IdleEvent` |
| `workflow.dispatch.idle` | Worker | Dispatcher | `IdleEvent` |
| `workflow.dispatch.heartbeat` | Worker | Dispatcher | `WorkerHeartbeat` |
| `workflow.dispatch.outcome` | Worker | Dispatcher | `WorkerOutcome` |
| `workflow.dispatch.assign.{id}` | Dispatcher | Worker | `Assignment` |
| `workflow.dispatch.preempt.{id}` | Dispatcher | Worker | `PreemptNotice` |
| `workflow.interact.help` | Worker | Dispatcher | `HelpRequest` |
| `workflow.interact.respond.{nats_key}` | CLI/Human | Dispatcher | `HelpResponse` |
| `workflow.interact.deliver.{worker_id}` | Dispatcher | Worker | `HelpResponse` |
| `workflow.interact.attach.{worker_id}` | CLI | Worker | (empty) |
| `workflow.interact.detach.{worker_id}` | CLI | Worker | (empty) |
| `workflow.activity.append` | Worker, Reviewer | Dispatcher | `ActivityAppend` |
| `workflow.journal.append` | Reviewer | Dispatcher | `JournalAppend` |
| `workflow.admin.requeue` | CLI | Dispatcher | `RequeueRequest` |
| `workflow.admin.factory-poll` | CLI | Dispatcher | `{ "name": "..." }` |

## Key conventions

- **Three Forgejo identities.** The sidecar uses `workflow-sync` for label/dep operations (sync role) and `workflow-dispatcher` for assignee/comment operations (dispatcher role). The reviewer process (separate container) uses `workflow-reviewer` for PR reviews, merges, and escalation comments. This gives a clear audit trail in Forgejo.
- **Sidecar owns all state transitions.** Workers never mutate labels, assignees, or NATS KV directly. Content ops (comments, branches, PRs) are fine once a claim is held.
- **Dispatcher owns the worker lifecycle.** Workers communicate purely via NATS — register, idle, heartbeat, outcome. The dispatcher handles claims, state transitions, and Forgejo sync on their behalf.
- **Forgejo is the human-visible truth.** Every state change must be mirrored to Forgejo labels/comments so a human can inspect the system without tooling.
- **CDC is the source of events.** The sidecar learns about issue changes via the NATS stream, not webhooks. The CDC process polls Forgejo's PostgreSQL database for changes.
- **Transitions are derived events.** `JobTransition` events are published when state changes, but the graph and Forgejo labels remain the sources of truth.
- **Default-deny.** Jobs don't move to `on-deck` without explicit sidecar action. Failed jobs don't auto-retry. `on-ice` is a hold.
- **Deterministic UUIDs.** IndraDB vertex IDs are UUID v5 from `{owner}/{repo}/{number}` — no separate index, no rebuild on restart.
- **Dependency metadata lives in issue bodies** as `<!-- workflow:deps:1,2,3 -->` HTML comments, parsed by the sidecar from CDC snapshots.
- **Review lifecycle.** Workers create PRs with `Closes #N`. After the worker yields, the dispatcher transitions to InReview. The automated reviewer dispatches a Claude-based review action (`review-work.yml`) to evaluate the PR. Based on the job's `review` label (low/medium/high/human), the reviewer either merges, requests changes, or escalates to a human reviewer. On REQUEST_CHANGES, the reviewer publishes a `ChangesRequested` transition directly to NATS (not via CDC). The dispatcher sets the label and routes the job back to the original worker. After rework, the worker yields again → InReview for re-review. Merging the PR auto-closes the issue → Done. Closing without a merged PR → Revoked (dependents stay blocked).
- **Per-repo merge queue.** The reviewer serializes merges per repo using a NATS KV-backed lock (`workflow-merge-queue` bucket). When a PR is approved, the reviewer acquires a lock on `{owner}.{repo}.lock` (CAS). If held, the PR is queued in `{owner}.{repo}.queue`. After each merge, the lock holder processes the next queued entry. Merges use the `rebase` strategy so Forgejo rebases onto main before fast-forwarding, catching conflicts at merge time. On failure, the PR is sent back for rework.
- **Yield and Complete both transition to InReview.** Workers report `Outcome::Yield` after opening a PR. The dispatcher transitions the job to InReview on both Yield and Complete outcomes (graph + Forgejo label) and publishes the transition event so the reviewer picks it up. In practice, workers use Yield (not Complete) to signal "PR opened, ready for review."
- **Worker re-announcement.** Idle workers periodically re-register with the dispatcher (every 15s). This ensures recovery after sidecar restarts without manual intervention.
- **Action workers dispatch on work branches.** Action workers create a `work/action/{N}` branch before dispatching the Forgejo Actions workflow on it. This ties the action run to the branch (visible via `prettyref`), making it easy to correlate runs with issues in the graph viewer.
- **Job routing via labels.** Jobs can be restricted to specific worker types via `worker:X` labels (e.g. `worker:interactive`), specific platforms via `platform:X` labels (e.g. `platform:macos`), and specific capabilities via `capability:X` labels. The dispatcher checks all three constraints when matching jobs to workers.
- **Review requirements via labels.** The `review:human` label forces human review. `review:low`, `review:medium`, `review:high` set the confidence threshold for the automated Claude reviewer (default: high). The reviewer dispatches a `review-work.yml` Forgejo Action to evaluate the PR.
- **Changes requested via NATS.** The reviewer publishes a `JobTransition{ChangesRequested}` directly to NATS after the review action returns a REQUEST_CHANGES decision. The dispatcher handles this transition: sets the graph state, syncs the Forgejo label, and routes the job back to the original worker. Pending reworks are persisted in NATS KV (`workflow-pending-reworks`) so they survive sidecar restarts.
- **Interactive workers can pause for help.** An interactive (agent) worker can publish a `HelpRequest` to transition the job to `NeedsHelp`. The dispatcher posts a comment to Forgejo and waits for a human to respond. The worker holds its claim and continues heartbeating while paused.
- **Monitor is advisory only.** The monitor detects lease expiry, job timeouts, and orphaned jobs, but does NOT mutate state. It publishes advisory events (`workflow.monitor.*`) that the dispatcher handles. The dispatcher re-verifies claims before acting, eliminating races where the monitor revokes a claim while the dispatcher is mid-assignment.
- **Lease vs timeout.** Each claim has two deadlines: a short-lived *lease* (default 60s) that must be renewed by heartbeats, and a long-lived *job timeout* (default 3600s) for the total execution time. Lease expiry requeues; timeout expiry fails.
- **Abandon blacklist in NATS KV.** The `workflow-abandon-blacklist` bucket (1-hour TTL) prevents tight assign→abandon loops. Persisted across sidecar restarts.
- **Git credential helper.** Agent images include a credential helper (`infra/agent/git-credential-forgejo.sh`) that returns `FORGEJO_TOKEN` for HTTP clone auth. Combined with global git config (`user.name`, `user.email`), agents can `git clone` and push without manual token handling.
- **Agent plugins as quality gates.** Claude Code plugins (configured in `agent-plugins.conf`, downloaded from GitHub marketplace repos) enforce pre-commit checks via `PreToolUse` hooks. The `rust-lint` plugin runs `cargo fmt --check` + `cargo clippy` before any `git commit`, blocking the commit if checks fail. The `check-duplication` plugin runs `jscpd` to detect copy-pasted code. Plugins are auto-discovered from `/opt/plugins/` at launch; override with `AGENT_PLUGIN_DIRS`.

## Environment variables

### Sidecar

| Var | Default | Notes |
|---|---|---|
| `FORGEJO_URL` | (required) | |
| `FORGEJO_TOKEN` | (required) | Sync identity token (labels, deps) |
| `DISPATCHER_FORGEJO_URL` | (same as FORGEJO_URL) | Dispatcher identity URL |
| `DISPATCHER_FORGEJO_TOKEN` | (required) | Dispatcher identity token (assignees, comments) |
| `NATS_URL` | `nats://localhost:4222` | Internal port; Docker maps external 4223→4222 |
| `DB_PATH` | `./workflow.db` | RocksDB path |
| `LISTEN_ADDR` | `0.0.0.0:3000` | Code default; deployed at 8080 via `.sidecar.env` |
| `RECORDING_TTL_SECS` | `0` | TTL for session recordings in object store (0 = no TTL) |
| `DEFAULT_TIMEOUT_SECS` | `3600` | Job-level timeout |
| `LEASE_SECS` | `60` | Heartbeat lease window |
| `HEARTBEAT_INTERVAL_SECS` | `10` | Expected heartbeat cadence |
| `MONITOR_INTERVAL_SECS` | `15` | Timeout/lease scan interval |
| `WORKER_PRUNE_SECS` | `90` | Prune workers unseen for this long |

### Reviewer (separate process)

| Var | Default | Notes |
|---|---|---|
| `FORGEJO_URL` | (required) | Forgejo base URL |
| `REVIEWER_FORGEJO_TOKEN` | (required) | Reviewer identity token (PR reviews, merge) |
| `NATS_URL` | `nats://localhost:4222` | NATS connection |
| `REVIEWER_HUMAN_LOGIN` | `you` | Human reviewer login for escalation |
| `REVIEWER_DELAY_SECS` | `3` | Delay before reviewing (avoids PR creation race) |
| `REVIEWER_WORKFLOW` | `review-work.yml` | Workflow file for Claude-based review action |
| `REVIEWER_RUNNER` | `ubuntu-latest` | Runner label for review action |
| `REVIEWER_ACTION_TIMEOUT_SECS` | `600` | Timeout for review action completion |
| `REVIEWER_POLL_SECS` | `10` | Poll interval for review action status |
| `SIDECAR_URL` | `http://localhost:8080` | Sidecar HTTP API URL (for startup reconciliation) |

### CDC

| Var | Default | Notes |
|---|---|---|
| `DATABASE_URL` | `postgres://forgejo:forgejo@localhost:5432/forgejo` | PostgreSQL connection string |
| `NATS_URL` | `nats://localhost:4223` | External port; maps to internal 4222 in Docker |
| `CDC_POLL_MS` | `250` | Poll interval for change detection |

## Infrastructure

- **Docker Compose**: `docker-compose.infra.yml` defines PostgreSQL (internal), Forgejo (port 3000, bind-mounted to `.data/forgejo/`), and NATS with JetStream (port 4223→4222). `docker-compose.yml` includes infra and adds CDC + sidecar (port 8080) + reviewer + shared runner. Worker containers defined in `docker-compose.workers.yml`.
- **Shared runner**: A shared `act_runner` container starts with `docker compose up` (alongside CDC/sidecar/reviewer). Handles system-level actions like review workflows. Scalable via `--scale runner=N`. Action workers also have paired dedicated runners in `docker-compose.workers.yml`.
- **Forgejo config**: `infra/forgejo/app.ini` is copied into `.data/` before first boot. Includes CORS for the graph viewer's cross-origin API calls.
- **Terraform** (`infra/`): Provisions repos, users, labels, collaborator permissions. Uses Lerentis/gitea provider.
- **`scripts/init.sh`**: One-command setup. Teardown → infra → Terraform → build runner-env + sidecar images → build flavored agent + runner images → get runner registration token → CDC + sidecar + reviewer + runner → seed → verify.
- **`scripts/workers.sh`**: Creates Forgejo API tokens and launches worker containers. Defaults to action workers + paired runners. Supports `--sim` (sim workers only), `--agent` (interactive Claude agent workers), `--flavor` (use a flavored agent image, e.g. `--flavor rust`), `--delay`, `--count`, `--build`, `--down`. Runner registration token is loaded from `.sidecar.env` (created by init.sh).
- **Flavored agent images**: `flavors.conf` maps flavor names to Dockerfiles and capability labels (e.g. `rust:Dockerfile.agent-rust:rust`). `init.sh` builds base + flavored agent images and matching runner-env images. `workers.sh --agent --flavor rust` starts agents with the Rust toolchain and `capability:rust` routing.
- **Agent plugins**: `agent-plugins.conf` declares which Claude Code plugins to install from GitHub marketplace repos (e.g. `kasofsk/claude-plugins:rust-lint,check-duplication`). Plugins are downloaded at image build time into `/opt/plugins/` and auto-discovered by the worker at launch. Plugins use `PreToolUse` hooks to enforce quality gates (e.g. `cargo fmt` + `cargo clippy` before `git commit`).
- **CI workflows**: `action/ci-rust.yml` runs Rust CI on PRs using composite actions from `github.com/kasofsk/opinionated`. The shared runner gets flavor-specific labels (e.g. `rust:docker://workflow-runner-env-rust:latest`) so CI jobs route to the right runner image.

## Gitignored runtime artifacts

- `.sidecar.env` — generated credentials
- `.workers.env` — generated worker tokens
- `.data/` — RocksDB, Forgejo bind-mount
- `target/` — build output

## Agent configuration files

| File | Purpose |
|------|---------|
| `flavors.conf` | Flavor→Dockerfile→capability mapping (e.g. `rust:Dockerfile.agent-rust:rust`) |
| `agent-plugins.conf` | Claude Code plugins to install from GitHub marketplaces |
| `Dockerfile.agent` | Base agent image (curl, git, tmux, jq, nodejs, Claude Code, plugins) |
| `Dockerfile.agent-rust` | Rust flavor (extends base with rustup, clippy, rustfmt, build-essential) |
| `Dockerfile.runner-env-rust` | Runner image for Rust CI jobs |
| `infra/agent/git-credential-forgejo.sh` | Git credential helper using `FORGEJO_TOKEN` env var |
| `infra/agent/download-plugins.sh` | Fetches plugins from GitHub marketplace repos based on `agent-plugins.conf` |
