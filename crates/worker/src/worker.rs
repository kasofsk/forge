use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use workflow_types::Job;

use crate::forgejo::ForgejoClient;

// ── Outcome ───────────────────────────────────────────────────────────────────

/// The result of executing a job.
///
/// Workers return this from `execute` to indicate what should happen next.
/// The dispatched loop converts each variant into the appropriate lifecycle
/// action via NATS.
#[derive(Debug)]
pub enum Outcome {
    /// Job finished successfully; transition to `in-review`.
    Complete,
    /// Job failed; record a failure comment and transition to `failed`.
    Fail {
        reason: String,
        logs: Option<String>,
    },
    /// Voluntarily return the job to `on-deck` (e.g. worker is shutting down).
    Abandon,
    /// Worker is done but an external signal (e.g. CDC detecting a PR) will
    /// handle the state transition. Releases the claim without changing state.
    Yield,
}

// ── Execution context ────────────────────────────────────────────────────────

/// Context passed alongside [`Worker::execute`] in dispatched mode.
///
/// Bundles the forgejo client with a cancellation token that fires when
/// the dispatcher preempts this worker for a higher-priority job.
pub struct ExecutionContext {
    pub forgejo: ForgejoClient,
    pub cancellation: CancellationToken,
}

// ── Worker trait ──────────────────────────────────────────────────────────────

/// Core trait for agents that execute jobs.
///
/// The [`crate::dispatch::DispatchedWorkerLoop`] handles the claim–heartbeat–outcome
/// lifecycle; implementors only need to provide the execution logic.
#[async_trait]
pub trait Worker: Send + Sync {
    /// Unique identifier for this worker instance, used for claims and assignees.
    fn worker_id(&self) -> &str;

    /// Capability tags this worker supports (e.g. `["rust", "frontend"]`).
    ///
    /// Used by the dispatcher to match workers to jobs with `capability:X` labels.
    /// Default: empty (accepts any job regardless of capability requirements).
    fn capabilities(&self) -> Vec<String> {
        vec![]
    }

    /// Worker type identifier: "sim", "action", "interactive", etc.
    /// Used by the dispatcher to match jobs with `worker:X` labels and by the
    /// UI to determine available affordances (e.g. attach button).
    fn worker_type(&self) -> &str {
        "unknown"
    }

    /// Platform tags this worker supports (e.g. `["macos", "arm64"]`).
    ///
    /// Used by the dispatcher to match workers to jobs with `platform:X` labels.
    /// Default: empty (matches jobs with no platform requirements, but NOT jobs
    /// that require a specific platform).
    fn platform(&self) -> Vec<String> {
        vec![]
    }

    /// Return `false` to skip a job without claiming it.
    ///
    /// Use this to implement worker specialization — e.g. only accept jobs
    /// whose title contains a certain tag. The default accepts every job.
    fn accepts(&self, _job: &Job) -> bool {
        true
    }

    /// Execute a claimed job.
    ///
    /// - The claim is already held when this is called.
    /// - The loop maintains a background heartbeat task for the duration of
    ///   this call; do **not** send heartbeats manually.
    /// - Use `forgejo` for content operations (comments, branches, PRs).
    /// - Return [`Outcome::Complete`], [`Outcome::Fail`], or [`Outcome::Abandon`].
    async fn execute(&self, job: &Job, forgejo: &ForgejoClient) -> Result<Outcome>;
}
