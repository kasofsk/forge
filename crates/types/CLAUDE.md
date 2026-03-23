# CLAUDE.md — workflow-types

Shared type definitions used by all crates. No business logic beyond parsing and serialization.

## What lives here

- **`Job`**, **`JobState`** (OnIce, Blocked, OnDeck, OnTheStack, NeedsHelp, InReview, ChangesRequested, Done, Failed, Revoked), **`ClaimState`**, **`FailureRecord`** — core domain types
- **`ReviewRequirement`** (Low, Medium, High, Human) — controls automated review confidence threshold
- **`JobTransition`** — derived event published when job state changes (previous + new state)
- **Dispatch types**: `WorkerRegistration`, `IdleEvent`, `Assignment`, `PreemptNotice` — dispatcher↔worker messages
- **Worker lifecycle types**: `WorkerHeartbeat`, `WorkerOutcome`, `OutcomeReport` (Complete, Fail, Abandon, Yield) — workers report heartbeats and outcomes via NATS, not HTTP
- **API types**: `DepsResponse`, `RequeueRequest`, `RequeueTarget`, `LabelListResponse`, `CreateIssueRequest`, `CreateIssueResponse`, etc.
- **`IssueSnapshot`** — CDC-produced denormalized issue state, includes `closed_by_merge` and `has_open_pr` booleans
- **Interactive worker types**: `HelpRequest`, `HelpResponse` — worker pauses for human input; `SessionInfo` — ttyd terminal metadata stored in `workflow-sessions` NATS KV bucket
- **Dispatch observability**: `WorkerState` (Idle, Busy, Transitioning), `WorkerInfo` — worker registry snapshot; `JournalEntry`, `JournalResponse` — dispatcher action log
- **Cross-process messaging**: `ActivityAppend` — workers/reviewer publish activity entries via NATS; `JournalAppend` — reviewer publishes journal entries via NATS (fire-and-forget, no reply needed)
- **Label parsing**: `priority:N`, `timeout:N`, `capability:X`, `retry:N`, `worker:X`, `platform:X`, `review:X` from label names; `<!-- workflow:deps:1,2,3 -->` from issue body
- **`FactoryStatus`** — observability metadata for work factories

## Conventions

- All types derive `Serialize`/`Deserialize`. Most also derive `Debug`, `Clone`.
- `JobState` round-trips through label strings (`on-deck`, `on-the-stack`, `changes-requested`, `revoked`, etc.) — keep these stable as they're Forgejo label names managed by Terraform.
- `NeedsHelp` state: worker is paused waiting for human input. Claim is held; heartbeats continue. Transition is driven by the dispatcher when it receives a `HelpRequest`.
- `is_terminal()` returns true for `Done` and `Revoked`. However, `all_deps_done` in graph.rs checks for `Done` specifically — Revoked deps block dependents.
- `Assignment` has `is_rework: bool` to signal the worker that this is a changes-requested cycle (branch/PR may already exist).
- `IssueSnapshot` has `closed_by_merge: bool` and `has_open_pr: bool`, populated by CDC from PostgreSQL queries. `has_changes_requested` was removed — the reviewer publishes ChangesRequested transitions directly via NATS.
- `Job` has `max_retries: u32` (default 3) parsed from `retry:N` labels via `parse_retries()`. `retry_attempt: u32` tracks the current attempt count, persisted in the graph.
- `Job` has `is_rework: bool` set by the dispatch loop when a job is re-assigned after changes-requested. Signals the worker to address review feedback on the existing branch/PR.
- `Job` has `worker_type: Option<String>` from `worker:X` labels, `platform: Vec<String>` from `platform:X` labels, and `review: ReviewRequirement` from `review:X` labels.
- `ReviewRequirement` defaults to `High`. Parsed from `review:human`, `review:low`, `review:medium`, `review:high` labels.
- `OutcomeReport` has a `Yield` variant — the dispatcher releases the claim and transitions to InReview.
- `OutcomeReport` uses `#[serde(tag = "kind")]` for tagged enum serialization.
- `ClaimState` has `lease_secs` and `lease_deadline` fields alongside `timeout_secs`. Lease = short heartbeat window (default 60s); timeout = total job deadline. `FailureKind::LeaseExpired` is distinct from `HeartbeatTimeout`.
- **Monitor advisory event types**: `LeaseExpiredEvent`, `JobTimeoutEvent`, `OrphanDetectedEvent` — published by the monitor, handled by the dispatcher. The monitor never mutates state directly.
- `WorkerRegistration` has `worker_type` ("sim", "action", "interactive") and `platform` (e.g. ["linux", "arm64"]) fields for job routing.
- This crate has zero async dependencies. Keep it that way.
