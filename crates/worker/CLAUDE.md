# CLAUDE.md — workflow-worker

SDK library for building workers and work factories. This crate is a dependency of any agent that participates in the workflow system.

## Key abstractions

- **`Worker` trait** (`worker.rs`): Implement `execute(job, forgejo) → Result<Outcome>` to define what a worker does. Workers receive jobs from the dispatcher and return an `Outcome` (Complete, Fail, Abandon, or Yield) — they never make lifecycle HTTP calls. Both sim and action workers return `Outcome::Yield` after opening a PR. The dispatcher transitions the job to InReview on yield.
- **`DispatchedWorkerLoop`** (`dispatch.rs`): NATS-based worker loop. Registers with the dispatcher, signals idle, receives assignments, runs execute, and reports outcomes — all via NATS pub/sub. No HTTP calls to the sidecar. While idle, periodically re-registers + re-idles every 15s to survive sidecar restarts.
- **`WorkFactory` trait** (`factory.rs`): Implement `poll(sidecar, forgejo)` to generate new jobs. Factories inspect state and create Forgejo issues; they never claim or execute.
- **`SidecarClient`** (`client.rs`): Read-only HTTP client for the sidecar API. Used by CLI read commands (`jobs`, `show`), factories, and integration tests. Admin ops (requeue, factory poll) use NATS request-reply.
- **`ForgejoClient`** (`forgejo.rs`): Wraps the generated `forgejo-api` crate with ergonomic methods. Covers content ops (issues, comments, branches, PRs, file creation), review ops (submit_review, merge_pr, add_pr_reviewer, list_pr_files, list_comments), Forgejo Actions (dispatch_workflow, list_action_runs, get_action_run), generic HTTP helpers (base_url, get_json, post_json), and admin ops (admin_create_user, add_collaborator). Label/assignee mutation is sidecar-only. The reviewer module uses this client for PR reviews, merges, and review action dispatch.

## Dispatched worker lifecycle (NATS-only)

```
Worker                          Dispatcher (sidecar)
  │                                │
  │── publish WorkerRegistration ─►│  (capabilities, worker_id)
  │── publish IdleEvent ──────────►│
  │                                │── finds matching on-deck job
  │                                │── claims via coord (NATS KV CAS)
  │◄── publish Assignment ────────│  (job + claim on personal subject)
  │                                │
  │  [execute: content ops via Forgejo]
  │── publish WorkerHeartbeat ────►│  (periodic, dispatcher forwards to coord)
  │                                │
  │── publish WorkerOutcome ──────►│  (complete/fail/abandon)
  │                                │── releases claim, updates graph + Forgejo
  │── publish IdleEvent ──────────►│  (ready for next job)
```

Workers communicate exclusively via NATS. The dispatcher handles all claim management, state transitions, and Forgejo label/assignee mutations on the worker's behalf.

**Heartbeat cancellation**: The heartbeat task tracks consecutive NATS publish failures. After 3 consecutive failures (indicating a broken NATS connection or revoked claim), it cancels execution via the shared `CancellationToken`. The worker then reports `Outcome::Abandon` and re-idles. This also covers preemption — the `cancellation.cancelled()` branch in the execute `select!` treats any cancellation as abandon.

## Permission boundary

- **Forgejo API** via `ForgejoClient` — wraps the generated `forgejo-api` crate (workspace member at `crates/forgejo-api`)
- Content ops: create/edit issues, post comments, create branches, create/get PRs, create files (base64 via contents API)
- Review ops: submit_review (APPROVED/REQUEST_CHANGES), merge_pr (merge/squash/rebase), add_pr_reviewer, list_pr_files
- Actions ops: dispatch_workflow, list_action_runs, get_action_run
- Admin ops: admin_create_user, add_collaborator (used by integration tests and init scripts)
- Workers intentionally lack label/assignee mutation methods. State transitions are sidecar-only.
- In dispatched mode, workers don't even need HTTP access to the sidecar.
