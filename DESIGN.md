# Workflow — Design Document

## Overview

An issues-based workflow orchestration system for AI agents. Jobs are Forgejo issues.
Agents are workers that claim and execute jobs. A sidecar service is the single
coordination point — workers never touch NATS or IndraDB directly.

The backend is intentionally swappable (Forgejo-first, with progressive feature
activation for GitHub/GitLab). All job-management intelligence lives in the sidecar;
the issue tracker is a display and persistence layer for human visibility.

---

## Components

```
┌──────────────────────────────────────────────────────────────┐
│ Forgejo                                                      │
│  Issues = jobs   Labels = state   Assignees = audit trail    │
└──────────────────────────────────────────────────────────────┘
         │ PostgreSQL (CDC polls for changes)
┌────────▼─────────┐
│ CDC process       │  polls Forgejo's PostgreSQL, publishes IssueSnapshot
└────────┬─────────┘
         │ NATS JetStream "workflow-changes"
┌────────▼─────────────────────────────────────────────────────┐
│ Sidecar (axum HTTP service)                                   │
│                                                               │
│  Consumer        ─► Graph (IndraDB/RocksDB)                  │
│    ↓ publishes       Coordinator (NATS KV)                   │
│  JobTransition       Forgejo API client                      │
│    ↓                                                          │
│  Dispatcher      ─► assigns jobs to workers via NATS         │
│                  ─► handles worker outcomes (NATS)            │
│                  ─► manages preemption                        │
│  API handler     ─► HTTP endpoints for admin/CLI              │
│  Timeout monitor ─► scans stale claims                        │
└──────────────────────────────────────────────────────────────┘
         ▲ NATS pub/sub only              ▲ sidecar API (HTTP)
         │                                │
┌────────┴──────────┐        ┌────────────┴────────┐
│ Workers            │        │ Work Factories       │
│ (AI agents)        │        │ (job generators)     │
│                    │        │                      │
│ NATS: register,    │        │ sidecar API          │
│   idle, heartbeat, │        │   + Forgejo API      │
│   outcome          │        │     (issue creation) │
│ Forgejo API        │        └────────────────────  │
│   (content ops)    │
└────────────────────┘
```

### Forgejo
Source of truth for job content, history, and human visibility. The CDC process
polls Forgejo's PostgreSQL database for changes. Workers interact with Forgejo directly
for content operations (reading issue body, posting comments, creating PRs,
updating branches) once they hold an exclusive claim — contention-free by design.

### Sidecar
The single coordination gateway. Uses two Forgejo identities for audit trail clarity:
- **`workflow-sync`** — sync identity: label mutations, dependency resolution, CDC consumption
- **`workflow-dispatcher`** — dispatcher identity: assignee changes, failure comments

Owns:
- All state transitions (label mutations via sync identity)
- Exclusive claim issuance (via NATS KV CAS)
- Task graph (IndraDB/RocksDB): dependency edges, reverse-dep index
- Dispatcher: centralized worker assignment via NATS pub/sub
- Heartbeat monitoring and timeout enforcement
- Dependency resolution: when a job closes, walks reverse deps and unblocks ready jobs
- Failure recording: writes structured failure comments via dispatcher identity
- Transition event stream: publishes `JobTransition` events for reactive subscribers

### Dispatcher (sidecar module)
Centralized worker assignment. Subscribes to:
- `workflow.jobs.transition` — reacts to on-deck jobs (assign or preempt)
- `workflow.dispatch.register` — worker registration with capabilities
- `workflow.dispatch.idle` — worker ready for work
- `workflow.dispatch.heartbeat` — forwards to coordinator for claim keepalive
- `workflow.dispatch.outcome` — handles complete/fail/abandon/yield (release claim, update graph, sync Forgejo). On yield, releases the worker without publishing a transition — the CDC consumer owns the InReview transition via `has_open_pr`.

Publishes to per-worker subjects:
- `workflow.dispatch.assign.{worker_id}` — job assignment
- `workflow.dispatch.preempt.{worker_id}` — preemption notice

### NATS KV (sidecar-internal)
Two buckets, never accessed by workers directly (except `workflow-sessions` by interactive workers):
- `workflow-claims`: `{owner}/{repo}/{number}` → `ClaimState` JSON
  - Used for: atomic claim CAS, heartbeat updates, timeout/lease detection
- `workflow-sessions`: `{nats_job_key}` → `SessionInfo` JSON
  - Used for: tracking active interactive agent sessions (ttyd URLs, tmux names)

### IndraDB / RocksDB (sidecar-internal)
Persistent task graph. Vertices = jobs, edges = dependencies.

**Vertex properties:**
- `job_key`: `{owner}/{repo}/{number}` (string)
- `state`: `JobState` (serialized)
- `timeout_secs`: optional u64

**Edge type:** `depends_on` (outbound = dependent, inbound = dependency)

Vertex UUIDs are derived deterministically via UUID v5 from the job key —
no separate index required, no rebuild on restart.

**Key graph queries:**
- Reverse deps of job X → inbound `depends_on` edges to X's vertex
- All deps of job X → outbound `depends_on` edges from X's vertex
- All deps done? → check `state == Done` on all inbound vertices

### Reviewer (standalone process — `crates/reviewer`)
Automated PR reviewer. Runs as a separate container (`workflow-reviewer`
binary). Subscribes to `workflow.jobs.transition` via NATS and reacts when a
job enters InReview. Finds the linked PR (`Closes #N`), then dispatches a
Claude-based review action (`review-work.yml`) to evaluate the changes.

Review behavior is controlled by `review:X` labels on the issue:
- `review:human` → always escalates to a human reviewer (skips the action)
- `review:low` / `review:medium` / `review:high` → dispatches the Claude
  review action with the corresponding confidence threshold (default: high)

The review action submits a PR review with the appropriate state
(APPROVED/REQUEST_CHANGES/COMMENT). The reviewer reads the state from the
Forgejo API — no comment markers are parsed. The reviewer then acts:
- **approve** → merge the PR (CDC detects closed_by_merge → Done)
- **request_changes** → publishes a `JobTransition{ChangesRequested}` directly
  to NATS. The dispatcher sets the graph state + Forgejo label and routes the
  job back to the original worker for rework.
- **escalate** → add human reviewer to the PR

Communicates with the sidecar via NATS: fire-and-forget messages for activities
and journal, plus direct `JobTransition` publishing for ChangesRequested.
Uses a `DashSet` to deduplicate concurrent InReview transitions. Uses the
`workflow-reviewer` Forgejo identity for audit trail clarity.

### Workers
AI agents. Register with the dispatcher via NATS with capability tags.
Receive job assignments pushed by the dispatcher. Execute work using Forgejo
for content ops. Report outcomes (complete/fail/abandon/yield) back via NATS.
Workers never make HTTP calls to the sidecar — the dispatcher manages all
lifecycle state on their behalf.

Three worker modes:
- **SimWorker** — creates a branch, commits a file, opens a PR, sleeps, then
  yields. Used for testing without Forgejo Actions.
- **ActionWorker** — creates a `work/action/{N}` branch, dispatches a Forgejo
  Actions workflow on that branch, polls until complete, then yields. The action
  run is tied to the branch so `prettyref` matches the PR's head ref.
- **InteractiveWorker** — spawns a Claude Code CLI in a tmux session fronted by
  ttyd (web terminal). Can pause for human help via `NeedsHelp` state. Supports
  headless (peek) and interactive modes, switchable via NATS attach/detach signals.
  Supports flavored images (`Dockerfile.agent-rust` etc.) with pre-installed
  toolchains, and Claude Code plugins (auto-discovered from `/opt/plugins/`) that
  enforce quality gates (e.g. `cargo fmt` + `cargo clippy` before `git commit`).

Both modes return `Outcome::Yield` after opening a PR. The CDC consumer detects
`has_open_pr` and transitions the job to InReview.

### Work Factories
Separate processes that generate work. They inspect current job state via the sidecar
and create new Forgejo issues when appropriate (which the CDC picks up and the sidecar
queues). They are not workers — they don't claim or execute jobs.

Examples:
- "Keep at least N `on-deck` jobs of type X"
- "When job Y completes, create follow-up jobs Z1..Zn"
- "Create a daily audit job every morning"

Factories implement a `WorkFactory` trait: `async fn poll(sidecar, forgejo) -> Result<()>`.

---

## Job State Machine

```
                    ┌─────────┐
         manual /   │  On Ice │  intentionally withheld;
         factory    └────┬────┘  set by creator or requeue
                         │
                    ┌────▼────┐
       sidecar sets │ Blocked │  has unresolved deps (sidecar sets on open)
       on open if   └────┬────┘
       deps unresolved   │  all deps Done (sidecar, triggered by CDC)
                         │
                    ┌────▼────┐
                    │ On Deck │  claimable; label stays on-deck while unclaimed
                    └────┬────┘
                         │  dispatcher assigns (NATS KV CAS)
                    ┌────▼──────────┐
                    │ On The Stack  │  exclusively held by one worker
                    └──┬────┬───┬──┘
                       │    │   │
            worker asks│    │   │  fail / heartbeat timeout / lease expiry
            for help   │    │   │
              ┌────────▼┐   │ ┌─▼──────┐
              │ Needs   │   │ │ Failed │  DLQ; error + logs stored
              │ Help    │   │ │ (DLQ)  │  as Forgejo issue comment
              └────┬────┘   │ └──┬─────┘
   claim held;     │        │    │
   heartbeats      │        │    │
   continue.       │        │    │
   human responds  │        │    │
   → OnTheStack    │        │    │
                   │   yield│    │
                   │ + PR   │    │
                  ┌▼────────▼┐   │
                  │  In      │   │
                  │  Review  │   │
                  └──┬─┬─────┘   │
                     │ │        │  manual / factory requeue
    REQUEST_CHANGES  │ │        └──────────────┐
    review on PR     │ │                       │
    (CDC detects)  ┌─▼─┘               On Deck or On Ice
               │    │
          ┌────▼──────────┐│ merged PR closes issue
          │ Changes       ││ (CDC detects closed_by_merge)
          │ Requested     ││
          └────┬──────────┘│
               │    │       closed without merged PR
     dispatcher│  ┌─▼────┐  (no status:done label)
     routes to │  │ Done │  ┌─────────┐
     original  │  └──────┘  │ Revoked │  dependents stay blocked
     worker    │       │    └─────────┘
               └───────┘
            On The Stack     triggers dep resolution:
            (is_rework=true) walk reverse deps in IndraDB,
                             transition unblocked jobs → On Deck
```

### Done vs Revoked detection

When the CDC sees `is_closed=true`, the consumer checks:
1. **`status:done` label present** → Done (human marked it)
2. **`closed_by_merge=true`** (CDC detected merged PR with `Closes #N`) → Done (sidecar adds `status:done` label)
3. **Neither** → Revoked (closed without completion; dependents stay blocked)

Revoked is terminal. `all_deps_done` checks for `state == Done` specifically — Revoked deps do not satisfy it. Since a job was never Done before being Revoked, its dependents were never unblocked.

### Changes-requested routing

When the reviewer decides REQUEST_CHANGES:
1. Reviewer publishes `JobTransition{ChangesRequested}` directly to NATS
2. Dispatcher receives the transition, sets graph state + `status:changes-requested` label
3. Dispatcher reads the assignee (preserved from the original work cycle)
4. If the original worker is idle → assign immediately with `is_rework: true`
5. If busy → queue in `workflow-pending-reworks` NATS KV bucket; assigned when the worker next idles
6. Worker receives assignment, addresses review feedback, pushes changes → back to InReview

### Sidecar behavior on CDC snapshots

The consumer processes each `IssueSnapshot` idempotently. It reads the previous
state from the graph, computes the resolved state, and publishes a `JobTransition`
event if they differ.

| Snapshot condition | Action |
|---|---|
| Closed + (`status:done` label OR `closed_by_merge`) | Set `Done` in graph. Add `status:done` label if absent. Walk reverse deps — unblock any whose deps are all done. Publish transition. |
| Closed + no done label + no merged PR | Set `Revoked` in graph. Publish transition. Dependents stay blocked. |
| `on-ice` label, or no status label at all | Set `OnIce` in graph. (New issues without a status label default to on-ice.) |
| `on-the-stack` + `has_open_pr` | Transition to `InReview`. Worker yielded after PR creation; CDC detected the open PR. |
| No deps or all deps done + `has_open_pr` | Set `InReview`. PR exists for a ready job. |
| Has unresolved deps | Set `Blocked`. Sync label to Forgejo if it doesn't match. |
| No deps or all deps done | Set `OnDeck`. Sync label. Publish transition → dispatcher reacts. |
| `on-the-stack`/`in-review`/`failed`/`changes-requested` label | Respect the explicit status label. |

Note: `ChangesRequested` is NOT triggered by CDC. The reviewer publishes this transition directly via NATS.

All state changes publish `JobTransition` events. The dispatcher uses `OnDeck`
transitions to assign idle workers or preempt lower-priority workers.

---

## Claim Lifecycle (Dispatched Mode)

```
Worker                          Dispatcher (sidecar)              NATS KV / Forgejo
  │                                │                                │
  │── pub WorkerRegistration ─────►│  (capabilities)                │
  │── pub IdleEvent ──────────────►│                                │
  │                                │── get on-deck jobs from graph  │
  │                                │── try_claim (CAS) ────────────►│
  │                                │◄─ ok (revision) ───────────────│
  │                                │── PATCH label: on-the-stack ──►│ Forgejo
  │◄── pub Assignment ─────────────│  (job + claim)                 │
  │                                │                                │
  │── pub WorkerHeartbeat ────────►│                                │
  │                (periodic)      │── update last_heartbeat (CAS) ►│
  │                                │                                │
  │  [work happens in Forgejo directly — branch, PR, comments]      │
  │                                │                                │
  │── pub WorkerOutcome ──────────►│  (yield)                       │
  │                                │── release worker (no transition│
  │                                │   published — CDC owns it)     │
  │── pub IdleEvent ──────────────►│  (ready for next job)          │
  │                                │                                │
  │         [CDC detects has_open_pr → consumer transitions to InReview]
  │         [Reviewer reacts: approve+merge or escalate to human]
```

### HTTP Claim Lifecycle (Legacy/Admin)

The HTTP endpoints (`/claim`, `/complete`, `/abandon`, `/fail`, `/requeue`) still
exist for the CLI and admin tools. They perform the same state transitions and
publish `JobTransition` events. The dispatched worker loop does not use them.

### Timeout monitoring (sidecar background task)
- Runs every N seconds (configurable)
- Scans all NATS KV claim entries
- For each: if `now - last_heartbeat > job.timeout_secs` → trigger failure
- Failure path: delete claim, set `Failed` in IndraDB, PATCH Forgejo label,
  POST structured failure comment to Forgejo issue

### Per-job timeout
- Global default in sidecar config: `default_task_timeout_secs`
- Override via label: `timeout:3600` on the Forgejo issue
- Sidecar reads timeout label on claim, stores in NATS KV claim entry

---

## Failure Record

Posted as a Forgejo issue comment (machine-readable block + human-readable summary):

```
<!-- workflow:failure
{
  "worker_id": "agent-42",
  "kind": "heartbeat_timeout" | "worker_reported" | "lease_expired",
  "reason": "...",
  "logs": "...",
  "failed_at": "2026-03-17T..."
}
-->

⚠️ **Job failed** — `heartbeat_timeout` by worker `agent-42` at 2026-03-17T...

**Reason:** ...
```

Failed jobs are automatically retried up to 3 times by the dispatcher. After
exhausting retries, a human or work factory must call `POST /requeue` with
`{ "target": "on-deck" | "on-ice" }`.

---

## Sidecar API Reference

All responses are JSON. Errors use standard HTTP status codes with a
`{ "error": "..." }` body.

### Job discovery
```
GET /jobs?state=on-deck|on-the-stack|blocked|...
  → { jobs: [Job] }
  All states visible; workers use this for context before choosing a task.

GET /jobs/:owner/:repo/:number
  → { job: Job, claim: ClaimState | null, failure: FailureRecord | null }

GET /jobs/:owner/:repo/:number/deps
  → { dependencies: [Job], all_done: bool }
```

### Job lifecycle
```
POST /jobs/:owner/:repo/:number/claim
  Body: { worker_id: string }
  → 200 ClaimResponse | 409 Conflict | 400 Bad Request

POST /jobs/:owner/:repo/:number/heartbeat
  Body: { worker_id: string }
  → 200 | 403 Forbidden (not the current holder)

POST /jobs/:owner/:repo/:number/complete
  Body: { worker_id: string }
  → 200 | 403 Forbidden

POST /jobs/:owner/:repo/:number/abandon
  Body: { worker_id: string }
  → 200 (job returns to on-deck) | 403 Forbidden

POST /jobs/:owner/:repo/:number/fail
  Body: { worker_id: string, reason: string, logs?: string }
  → 200 | 403 Forbidden

POST /jobs/:owner/:repo/:number/requeue
  Body: { target: "on-deck" | "on-ice" }
  → 200 (no auth — admin/factory operation)
```

### Repo resources
```
GET /repos/:owner/:repo/users
  → { users: [UserInfo] }

GET /repos/:owner/:repo/labels
  → { labels: [ForgejoLabel] }

POST /repos/:owner/:repo/issues
  Body: { title: string, body?: string, labels?: [u64] }
  → { number: u64 }
```

---

## Crate Structure

```
workflow/
  Cargo.toml                  # workspace

  crates/
    types/                    # workflow-types
      src/lib.rs              # Job, JobState, ClaimState, FailureRecord,
                              # JobTransition, ReviewRequirement, dispatch types
                              # (WorkerRegistration, Assignment, PreemptNotice,
                              # WorkerHeartbeat, WorkerOutcome, OutcomeReport),
                              # API req/resp types, IssueSnapshot, label parsing

    sidecar/                  # workflow-sidecar (binary)
      src/
        main.rs               # axum server setup, dispatcher + consumer startup
        config.rs             # env-based config
        error.rs              # AppError, IntoResponse impl
        graph.rs              # TaskGraph: IndraDB/RocksDB wrapper
        coord.rs              # Coordinator: NATS KV claims + publish_transition()
        forgejo.rs            # ForgejoClient: labels, assignees, comments
        consumer.rs           # CDC stream consumer, publishes JobTransition events
        dispatcher.rs         # Centralized worker assignment, handles worker
                              #   lifecycle (heartbeat, outcome) via NATS.
                              #   Also handles activity.append + journal.append
                              #   from external processes (reviewer).
        api.rs                # HTTP handlers, publish transitions after mutations
        monitor.rs            # Background timeout scan, publishes transitions

    reviewer/                 # workflow-reviewer (binary, separate container)
      src/main.rs             # Standalone PR reviewer: subscribes to InReview
                              #   transitions, dispatches review-work.yml,
                              #   parses decision, merges / escalates.
                              #   Communicates with sidecar via NATS fire-and-forget.

    cdc/                      # workflow-cdc (binary)
      src/main.rs             # Polls Forgejo's PostgreSQL, publishes IssueSnapshot

    worker/                   # workflow-worker (library)
      src/
        lib.rs
        client.rs             # SidecarClient: HTTP client (legacy/admin/CLI)
        forgejo.rs            # ForgejoClient: content ops only
        worker.rs             # Worker trait: async fn execute(job, forgejo)
        dispatch.rs           # DispatchedWorkerLoop: pure NATS worker loop
        factory.rs            # WorkFactory trait: async fn poll(sidecar, forgejo)

    forgejo-api/              # forgejo-api (library, generated)
                              # OpenAPI-derived Forgejo REST API client
                              # Used by worker crate and reviewer

    cli/                      # worker-cli (binary)
      src/
        main.rs               # CLI args, subcommands (jobs/show/work/claim/
                              #   sim/action/agent/interact/seed/requeue)
        workers/
          mod.rs              # Worker type re-exports
          sim.rs              # SimWorker: branch → commit → PR → yield
          action.rs           # ActionWorker: branch → dispatch Actions → poll → yield
          interactive.rs      # InteractiveWorker: Claude + tmux + ttyd, NeedsHelp support
          cli.rs              # CliWorker: legacy interactive terminal prompts
```

---

## Key Design Principles

1. **Sidecar is the only coordinator.** The dispatcher (part of the sidecar) owns
   all state transitions, claim management, and worker assignment. Workers never
   touch NATS KV or IndraDB directly.
2. **Workers are pure executors.** They receive assignments via NATS, do content ops
   via Forgejo, and report outcomes via NATS. No HTTP calls to the sidecar.
3. **Forgejo is human-visible truth.** All state is mirrored to labels/comments so
   humans can inspect and intervene without tooling.
4. **Event-driven, not polling.** The CDC stream drives state reconciliation. The
   consumer publishes `JobTransition` events that the dispatcher and future reactors
   subscribe to. The transition stream is derived — not a source of truth.
5. **Exclusive ownership guaranteed before content ops.** Workers get a claim before
   touching branches/PRs. The sidecar's CAS guarantee means no two workers race on
   the same job.
6. **Default-deny.** No job moves to `on-deck` without explicit sidecar action.
   `on-ice` is respected as a hold. Failed jobs auto-retry up to `max_retries` (default 3, configurable via `retry:N` label); after exhausting retries, they stay in Failed until manually requeued.
7. **Work factories are not special.** They are ordinary clients of the sidecar +
   Forgejo APIs. Job generation logic lives in the factory, not in the job system.

---

## Open Questions / Future Work

- **Multi-repo queues**: currently one repo = one queue. Cross-repo deps would require
  qualifying all dep references with `owner/repo/number`. Deferred.
- **Backend swappability**: Forgejo-specific details are isolated in `sidecar/forgejo.rs`
  and `worker/forgejo.rs`. A `ForgejoBackend` trait can be extracted when adding
  GitHub/GitLab support.
