# CLAUDE.md — workflow-sidecar

The central coordination service. An Axum HTTP server that owns all state transitions, claim management, dependency resolution, and worker dispatch.

## Module responsibilities

| Module | Owns | Depends on |
|---|---|---|
| `config.rs` | Env-based configuration | — |
| `error.rs` | `AppError` enum → Axum `IntoResponse` | — |
| `graph.rs` | `TaskGraph` — IndraDB/RocksDB wrapper | indradb-lib |
| `coord.rs` | `Coordinator` — NATS JetStream KV for claims, transition event publishing | async-nats |
| `forgejo.rs` | `ForgejoClient` — label/assignee/comment mutations, collaborator queries | reqwest |
| `consumer.rs` | CDC stream consumer — processes `IssueSnapshot`, detects state changes, publishes `JobTransition` events. Handles Done vs Revoked detection (via `closed_by_merge` from CDC) and `propagate_unblock`. InReview transitions are dispatcher-owned (on yield). ChangesRequested transitions are published by the reviewer, not the consumer. | graph, coord, forgejo |
| `dispatcher.rs` | Centralized worker assignment — subscribes to transitions, worker events (register/idle/heartbeat/outcome), monitor advisory events (lease-expired/timeout/orphan), interactive help subjects, activity appends, and journal appends. Manages claims, preemption, changes-requested routing (sets graph state + Forgejo label on ChangesRequested), and NeedsHelp transitions. Handles monitor events with re-verification before acting. Pending reworks are persisted in NATS KV (`workflow-pending-reworks`). Abandon blacklist persisted in NATS KV (`workflow-abandon-blacklist`, 1h TTL). On `Yield` outcome, transitions to InReview. Job-to-worker matching checks capabilities, worker type (`worker:X` labels), and platform (`platform:X` labels). Also handles `workflow.journal.append` from external processes (reviewer). Activity recording rate-limited to 50 entries per job. | graph, coord, forgejo |
| `api.rs` | HTTP handlers for `/jobs/*`, `/factories/*`, `/repos/*/users`, `/repos/*/labels`, `/repos/*/issues`; publishes `JobTransition` after state mutations | graph, coord, forgejo |
| `monitor.rs` | Advisory-only background scan loop; detects lease expiry, job timeouts, and orphaned jobs. Publishes advisory events to `workflow.monitor.*` subjects for the dispatcher to handle. Never mutates state, releases claims, or sets labels directly. | coord |
| `events.rs` | `SseEvent` enum, broadcast channel for SSE. Emitted from `AppState::publish_transition`, `AppState::journal`, `AppState::emit_worker`, and monitor cleanup. | tokio::sync::broadcast |
| `registry.rs` | In-process factory registry | — |

## Event-driven architecture

State changes flow through a derived `JobTransition` event stream on NATS subject `workflow.jobs.transition`. Events are published by:
- **Consumer** — when CDC snapshots cause state changes (blocked→on-deck, dep unblocking, on-the-stack+has_open_pr→in-review, etc.)
- **Reviewer** — publishes ChangesRequested transitions directly via NATS (not via CDC)
- **API handlers** — after claim/complete/abandon/fail/requeue mutations
- **Dispatcher** — after handling worker outcomes and monitor advisory events (lease-expired, timeout, orphan)

The dispatcher subscribes to this stream plus worker-specific NATS subjects:
- `workflow.dispatch.register` — worker registration with capabilities, worker type, and platform
- `workflow.dispatch.idle` — worker ready for new assignment
- `workflow.dispatch.heartbeat` — worker heartbeats (forwarded to coordinator)
- `workflow.dispatch.outcome` — worker reports job result (dispatcher handles claim release, state transition, Forgejo sync)
- `workflow.interact.help` — worker requests human help (dispatcher transitions to NeedsHelp, posts Forgejo comment)
- `workflow.interact.respond.*` — human responds to help request (dispatcher delivers to worker)
- `workflow.activity.append` — workers/reviewer publish activity entries for graph tracking
- `workflow.journal.append` — reviewer (external process) publishes journal entries
- `workflow.monitor.lease-expired` — monitor advisory: worker lease expired (dispatcher re-verifies before acting)
- `workflow.monitor.timeout` — monitor advisory: job exceeded deadline (dispatcher re-verifies before acting)
- `workflow.monitor.orphan` — monitor advisory: job on_the_stack/needs_help with no claim (dispatcher re-verifies before acting)

Workers never make HTTP calls to the sidecar. All worker↔dispatcher communication is NATS pub/sub.

## Invariants

- **Two Forgejo identities in the sidecar.** `state.forgejo` uses the `workflow-sync` user (labels, deps). `state.dispatcher_forgejo` uses the `workflow-dispatcher` user (assignees, comments). The reviewer runs as a separate process with its own `workflow-reviewer` identity (see `crates/reviewer`).
- **Pending reworks in NATS KV** (`workflow-pending-reworks` bucket) maps job_key → preferred worker_id. Populated when a ChangesRequested transition fires but the original worker is busy. Consumed on next idle from that worker. Persisted across sidecar restarts.
- **Retry attempts on Job** (`job.retry_attempt` field) tracks the number of failed attempts. Persisted in the graph (RocksDB). The dispatcher auto-retries failed jobs up to `job.max_retries` (default 3, from `retry:N` label) by requeueing to on-deck. After exhausting retries, the job stays in Failed. Survives sidecar restarts.
- **Only this crate mutates Forgejo labels and assignees.** Workers do content ops only.
- **NATS KV buckets**: `workflow-claims` (claim state, CAS), `workflow-sessions` (interactive agent session metadata), `workflow-pending-reworks` (deferred rework routing), and `workflow-abandon-blacklist` (1-hour TTL, prevents assign→abandon loops). All are sidecar-internal — no external client should access them directly.
- **NATS stream** `workflow-changes` is the CDC event source. The sidecar consumes with a durable pull consumer named `sidecar`.
- **`JobTransition` is derived, not authoritative.** The graph and Forgejo labels are the sources of truth. Transition events are fire-and-forget notifications.
- **Cycle detection** runs on every `sync_deps` call in `graph.rs`. If a cycle is detected, the offending edge is rejected and a warning comment is posted to the Forgejo issue.
- **CAS on claims**: `coord.rs` uses NATS KV compare-and-swap for atomic claim acquisition.
- **Reviewer is a separate process**: The reviewer was extracted to `crates/reviewer` and runs as its own container. It communicates with the sidecar via NATS: fire-and-forget messages (`workflow.activity.append`, `workflow.journal.append`) and direct transition publishing (`workflow.jobs.transition` for ChangesRequested).
- **Yield transitions to InReview**: The dispatcher's yield handler transitions the job to InReview (graph + label) and publishes a `JobTransition`. This is dispatcher-owned to avoid race conditions with claim checks.
- **Monitor is advisory only**: The monitor publishes advisory events to `workflow.monitor.*` — it never mutates state, releases claims, or sets labels. The dispatcher receives these events, re-verifies the claim/state (worker may have heartbeated since detection), and only then acts. This eliminates the race where the monitor releases a claim while the dispatcher is mid-assignment.

## Adding a new API endpoint

1. Add the handler in `api.rs`
2. Add the route in `main.rs` router setup
3. Add request/response types in `crates/types/src/lib.rs` if needed
4. If the handler mutates job state, publish a `JobTransition` via `coord.publish_transition()`

## HTTP API endpoints

| Method | Path | Handler | Purpose |
|---|---|---|---|
| GET | `/jobs` | `list_jobs` | All jobs across all repos |
| GET | `/jobs/available` | `available_jobs` | On-deck jobs sorted by priority (for seed/tests) |
| GET | `/jobs/:owner/:repo/:number` | `get_job` | Single job detail + claim + failure |
| GET | `/jobs/:owner/:repo/:number/deps` | `get_deps` | Dependencies + all_done flag |
| GET | `/factories` | `list_factories` | Factory observability |
| GET | `/repos/:owner/:repo/users` | `list_users` | Collaborator list (for UI roster) |
| GET | `/repos/:owner/:repo/labels` | `list_labels` | Label list (for UI issue creation) |
| POST | `/repos/:owner/:repo/issues` | `create_issue` | Create issue via sidecar proxy |
| GET | `/sessions/:owner/:repo/:number/:session_id/output` | `get_session_recording` | Session recording |
| POST | `/interact/attach` | `attach_session` | Switch agent to interactive mode |
| POST | `/interact/detach` | `detach_session` | Switch agent to headless mode |
| GET | `/events` | `sse_events` | SSE stream: snapshot + incremental updates |
| GET | `/graph` | (static file) | Graph viewer web UI |

## NATS admin endpoints (request-reply)

| Subject | Payload | Purpose |
|---|---|---|
| `workflow.admin.requeue` | `RequeueRequest` | Requeue a job to on-deck or on-ice |
| `workflow.admin.factory-poll` | `{ "name": "..." }` | Trigger a factory poll |

## Static assets

`static/` contains the graph viewer HTML served at `GET /graph`. See `static/CLAUDE.md` for comprehensive UI documentation (layout, components, interactions, data sources, caching, state colors).

Summary: single-page Cytoscape.js DAG viewer with HTML overlay cards for nodes, worker roster strip, dispatcher journal, detail panel, new issue form, and interactive session controls. Uses SSE (`GET /events`) for real-time updates — no polling.
