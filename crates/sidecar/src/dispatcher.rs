//! Dispatcher: centralized worker assignment via NATS pub/sub.
//!
//! Workers register with capability tags and signal when idle. The dispatcher
//! picks the best matching on-deck job, claims it through the coordinator, and
//! pushes an assignment. When a high-priority job arrives and all capable
//! workers are busy on lower-priority work, the dispatcher preempts the
//! lowest-priority worker.

use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use chrono::Utc;
use futures::StreamExt;
use workflow_types::{
    ActivityAppend, ActivityEntry, ActivityStatus, Assignment, FailureKind, FailureRecord,
    HelpRequest, HelpResponse, IdleEvent, Job, JobState, JobTimeoutEvent, JobTransition,
    JournalAppend, LeaseExpiredEvent, OrphanDetectedEvent, OutcomeReport, PreemptNotice,
    RequeueRequest, RequeueTarget, WorkerHeartbeat, WorkerInfo, WorkerOutcome,
    WorkerRegistration, WorkerState,
};

use crate::AppState;

// ── NATS subjects ────────────────────────────────────────────────────────────

const SUBJECT_REGISTER: &str = "workflow.dispatch.register";
const SUBJECT_IDLE: &str = "workflow.dispatch.idle";
const SUBJECT_TRANSITION: &str = "workflow.jobs.transition";
const SUBJECT_HEARTBEAT: &str = "workflow.dispatch.heartbeat";
const SUBJECT_OUTCOME: &str = "workflow.dispatch.outcome";
const SUBJECT_HELP: &str = "workflow.interact.help";
const SUBJECT_RESPOND_WILDCARD: &str = "workflow.interact.respond.*";
const SUBJECT_ACTIVITY_APPEND: &str = "workflow.activity.append";
const SUBJECT_JOURNAL_APPEND: &str = "workflow.journal.append";
const SUBJECT_UNREGISTER: &str = "workflow.dispatch.unregister";
const SUBJECT_ADMIN_REQUEUE: &str = "workflow.admin.requeue";
const SUBJECT_ADMIN_FACTORY_POLL: &str = "workflow.admin.factory-poll";
const SUBJECT_MONITOR_LEASE_EXPIRED: &str = "workflow.monitor.lease-expired";
const SUBJECT_MONITOR_TIMEOUT: &str = "workflow.monitor.timeout";
const SUBJECT_MONITOR_ORPHAN: &str = "workflow.monitor.orphan";

fn subject_assign(worker_id: &str) -> String {
    format!("workflow.dispatch.assign.{worker_id}")
}

fn subject_preempt(worker_id: &str) -> String {
    format!("workflow.dispatch.preempt.{worker_id}")
}

/// Deliver a help response to a specific worker.
fn subject_deliver(worker_id: &str) -> String {
    format!("workflow.interact.deliver.{worker_id}")
}

/// Convert a job key (`owner/repo/123`) to a NATS-safe token (`owner.repo.123`).
pub fn job_key_to_nats(job_key: &str) -> String {
    job_key.replace('/', ".")
}

/// Request payload for `workflow.admin.factory-poll`.
#[derive(Debug, serde::Deserialize)]
struct FactoryPollRequest {
    name: String,
}

pub struct Dispatcher {
    state: Arc<AppState>,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("worker_count", &self.state.dispatch_registry.len())
            .finish()
    }
}

impl Dispatcher {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    /// Start background NATS subscriptions for register, idle, and transition events.
    pub async fn start(self: Arc<Self>) -> Result<()> {
        let nats = self.state.coord.nats_client().clone();

        let mut reg_sub = nats
            .subscribe(String::from(SUBJECT_REGISTER))
            .await
            .context("subscribe to dispatch.register")?;

        let mut idle_sub = nats
            .subscribe(String::from(SUBJECT_IDLE))
            .await
            .context("subscribe to dispatch.idle")?;

        let mut transition_sub = nats
            .subscribe(String::from(SUBJECT_TRANSITION))
            .await
            .context("subscribe to jobs.transition")?;

        // Registration handler — uses request/reply so workers wait for ack.
        let dispatcher = Arc::clone(&self);
        let nats_reg = nats.clone();
        tokio::spawn(async move {
            while let Some(msg) = reg_sub.next().await {
                let reg: WorkerRegistration = match serde_json::from_slice(&msg.payload) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("bad registration payload: {e:#}");
                        continue;
                    }
                };
                let already_registered = dispatcher
                    .state
                    .dispatch_registry
                    .contains_key(&reg.worker_id);
                if !already_registered {
                    let caps_str = if reg.capabilities.is_empty() {
                        "none".to_string()
                    } else {
                        reg.capabilities.join(", ")
                    };
                    dispatcher
                        .state
                        .journal(
                            "register",
                            &format!("Worker registered with capabilities: [{caps_str}]"),
                            None,
                            Some(&reg.worker_id),
                        )
                        .await;
                    tracing::info!(
                        worker_id = %reg.worker_id,
                        capabilities = ?reg.capabilities,
                        "worker registered"
                    );
                }
                let now = Utc::now();
                let info = WorkerInfo {
                    worker_id: reg.worker_id,
                    capabilities: reg.capabilities,
                    worker_type: reg.worker_type,
                    platform: reg.platform,
                    state: WorkerState::Idle,
                    current_job_key: None,
                    current_job_priority: None,
                    last_seen: now,
                };
                dispatcher.state.dispatch_registry.insert(info.worker_id.clone(), info.clone());
                dispatcher.state.emit_worker(&info);
                // Reply so the worker knows registration is complete
                // before it sends its first idle event.
                if let Some(reply) = msg.reply {
                    let _ = nats_reg.publish(reply, Bytes::new()).await;
                }
            }
        });

        // Unregister handler — worker is shutting down gracefully.
        let mut unreg_sub = nats
            .subscribe(String::from(SUBJECT_UNREGISTER))
            .await
            .context("subscribe to dispatch.unregister")?;

        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = unreg_sub.next().await {
                let unreg: IdleEvent = match serde_json::from_slice(&msg.payload) {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::warn!("bad unregister payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_unregister(&unreg.worker_id).await {
                    tracing::error!(
                        worker_id = %unreg.worker_id,
                        error = %e,
                        "failed to handle unregister"
                    );
                }
            }
        });

        // Idle handler
        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = idle_sub.next().await {
                let idle: IdleEvent = match serde_json::from_slice(&msg.payload) {
                    Ok(i) => i,
                    Err(e) => {
                        tracing::warn!("bad idle payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_idle(&idle.worker_id).await {
                    tracing::error!(
                        worker_id = %idle.worker_id,
                        error = %e,
                        "failed to handle idle event"
                    );
                }
            }
        });

        // Transition handler — reacts to job state changes
        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = transition_sub.next().await {
                let transition: JobTransition = match serde_json::from_slice(&msg.payload) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("bad transition payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_transition(&transition).await {
                    tracing::error!(
                        job = %transition.job.key(),
                        error = %e,
                        "failed to handle transition"
                    );
                }
            }
        });

        // Heartbeat handler — workers publish heartbeats via NATS
        let mut hb_sub = nats
            .subscribe(String::from(SUBJECT_HEARTBEAT))
            .await
            .context("subscribe to dispatch.heartbeat")?;

        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = hb_sub.next().await {
                let hb: WorkerHeartbeat = match serde_json::from_slice(&msg.payload) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::warn!("bad heartbeat payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_heartbeat(&hb).await {
                    tracing::warn!(
                        worker_id = %hb.worker_id,
                        job_key = %hb.job_key,
                        error = %e,
                        "failed to forward heartbeat"
                    );
                }
            }
        });

        // Outcome handler — workers report job results via NATS
        let mut outcome_sub = nats
            .subscribe(String::from(SUBJECT_OUTCOME))
            .await
            .context("subscribe to dispatch.outcome")?;

        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = outcome_sub.next().await {
                let outcome: WorkerOutcome = match serde_json::from_slice(&msg.payload) {
                    Ok(o) => o,
                    Err(e) => {
                        tracing::warn!("bad outcome payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_outcome(&outcome).await {
                    tracing::error!(
                        worker_id = %outcome.worker_id,
                        job_key = %outcome.job_key,
                        error = %e,
                        "failed to handle worker outcome"
                    );
                }
            }
        });

        // Help-request handler — worker needs human input.
        let mut help_sub = nats
            .subscribe(String::from(SUBJECT_HELP))
            .await
            .context("subscribe to interact.help")?;

        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = help_sub.next().await {
                let req: HelpRequest = match serde_json::from_slice(&msg.payload) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("bad help request payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_help_request(&req).await {
                    tracing::error!(
                        job_key = %req.job_key,
                        worker_id = %req.worker_id,
                        error = %e,
                        "failed to handle help request"
                    );
                }
            }
        });

        // Human response handler — routes response back to the waiting worker.
        let mut respond_sub = nats
            .subscribe(String::from(SUBJECT_RESPOND_WILDCARD))
            .await
            .context("subscribe to interact.respond.*")?;

        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = respond_sub.next().await {
                // Extract job key from the subject suffix (replace '.' back to '/').
                let nats_key = msg
                    .subject
                    .strip_prefix("workflow.interact.respond.")
                    .unwrap_or("")
                    .replace('.', "/");
                let resp: HelpResponse = match serde_json::from_slice(&msg.payload) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("bad help response payload for {nats_key}: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_help_response(&nats_key, &resp).await {
                    tracing::error!(
                        job_key = %nats_key,
                        error = %e,
                        "failed to handle help response"
                    );
                }
            }
        });

        // Activity append handler — workers publish activity entries.
        let mut activity_sub = nats
            .subscribe(String::from(SUBJECT_ACTIVITY_APPEND))
            .await
            .context("subscribe to activity.append")?;

        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = activity_sub.next().await {
                let append: ActivityAppend = match serde_json::from_slice(&msg.payload) {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!("bad activity append payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher
                    .state
                    .graph
                    .append_activity(&append.job_key, append.entry)
                {
                    tracing::error!(
                        job_key = %append.job_key,
                        error = %e,
                        "failed to append activity"
                    );
                    continue;
                }
                // Emit updated job via SSE so the graph viewer gets the new activity.
                if let Ok(Some(job)) = dispatcher.state.graph.get_job(&append.job_key) {
                    dispatcher.state.emit_job(&job);
                }
            }
        });

        // Journal append handler — external processes (reviewer) publish journal entries.
        let mut journal_sub = nats
            .subscribe(String::from(SUBJECT_JOURNAL_APPEND))
            .await
            .context("subscribe to journal.append")?;

        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = journal_sub.next().await {
                let append: JournalAppend = match serde_json::from_slice(&msg.payload) {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!("bad journal append payload: {e:#}");
                        continue;
                    }
                };
                dispatcher
                    .state
                    .journal(
                        &append.action,
                        &append.comment,
                        append.job_key.as_deref(),
                        append.worker_id.as_deref(),
                    )
                    .await;
            }
        });

        // Admin: requeue handler (request-reply)
        let mut requeue_sub = nats
            .subscribe(String::from(SUBJECT_ADMIN_REQUEUE))
            .await
            .context("subscribe to admin.requeue")?;

        let dispatcher = Arc::clone(&self);
        let nats_requeue = nats.clone();
        tokio::spawn(async move {
            while let Some(msg) = requeue_sub.next().await {
                let req: RequeueRequest = match serde_json::from_slice(&msg.payload) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("bad admin requeue payload: {e:#}");
                        if let Some(reply) = msg.reply {
                            let _ = nats_requeue
                                .publish(reply, Bytes::from(format!("{{\"error\":\"{e}\"}}")))
                                .await;
                        }
                        continue;
                    }
                };
                let result = dispatcher.handle_admin_requeue(&req).await;
                if let Some(reply) = msg.reply {
                    let body = match &result {
                        Ok(()) => Bytes::from_static(b"{\"ok\":true}"),
                        Err(e) => Bytes::from(format!("{{\"error\":\"{e:#}\"}}")),
                    };
                    let _ = nats_requeue.publish(reply, body).await;
                }
                if let Err(e) = result {
                    tracing::error!(error = %e, "failed to handle admin requeue");
                }
            }
        });

        // Monitor: lease-expired advisory events
        let mut lease_sub = nats
            .subscribe(String::from(SUBJECT_MONITOR_LEASE_EXPIRED))
            .await
            .context("subscribe to monitor.lease-expired")?;

        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = lease_sub.next().await {
                let event: LeaseExpiredEvent = match serde_json::from_slice(&msg.payload) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("bad lease-expired payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_lease_expired(&event).await {
                    tracing::error!(
                        job_key = %event.job_key,
                        error = %e,
                        "failed to handle lease-expired event"
                    );
                }
            }
        });

        // Monitor: timeout advisory events
        let mut timeout_sub = nats
            .subscribe(String::from(SUBJECT_MONITOR_TIMEOUT))
            .await
            .context("subscribe to monitor.timeout")?;

        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = timeout_sub.next().await {
                let event: JobTimeoutEvent = match serde_json::from_slice(&msg.payload) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("bad timeout payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_job_timeout(&event).await {
                    tracing::error!(
                        job_key = %event.job_key,
                        error = %e,
                        "failed to handle timeout event"
                    );
                }
            }
        });

        // Monitor: orphan advisory events
        let mut orphan_sub = nats
            .subscribe(String::from(SUBJECT_MONITOR_ORPHAN))
            .await
            .context("subscribe to monitor.orphan")?;

        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = orphan_sub.next().await {
                let event: OrphanDetectedEvent = match serde_json::from_slice(&msg.payload) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("bad orphan payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_orphan_detected(&event).await {
                    tracing::error!(
                        job_key = %event.job_key,
                        error = %e,
                        "failed to handle orphan event"
                    );
                }
            }
        });

        // Admin: factory poll handler (request-reply)
        let mut factory_poll_sub = nats
            .subscribe(String::from(SUBJECT_ADMIN_FACTORY_POLL))
            .await
            .context("subscribe to admin.factory-poll")?;

        let dispatcher = Arc::clone(&self);
        let nats_factory = nats.clone();
        tokio::spawn(async move {
            while let Some(msg) = factory_poll_sub.next().await {
                let req: FactoryPollRequest = match serde_json::from_slice(&msg.payload) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("bad admin factory-poll payload: {e:#}");
                        if let Some(reply) = msg.reply {
                            let _ = nats_factory
                                .publish(reply, Bytes::from(format!("{{\"error\":\"{e}\"}}")))
                                .await;
                        }
                        continue;
                    }
                };
                let result = dispatcher
                    .state
                    .registry
                    .poll_factory(&req.name, Arc::clone(&dispatcher.state))
                    .await;
                if let Some(reply) = msg.reply {
                    let body = match &result {
                        Ok(()) => Bytes::from_static(b"{\"ok\":true}"),
                        Err(e) => Bytes::from(format!("{{\"error\":\"{e:#}\"}}")),
                    };
                    let _ = nats_factory.publish(reply, body).await;
                }
                if let Err(e) = result {
                    tracing::error!(error = %e, "failed to handle admin factory poll");
                }
            }
        });

        Ok(())
    }

    /// Handle an admin requeue request: move a job to on-deck or on-ice.
    async fn handle_admin_requeue(&self, req: &RequeueRequest) -> Result<()> {
        let key = format!("{}/{}/{}", req.owner, req.repo, req.number);
        let job = self
            .state
            .graph
            .get_job(&key)?
            .ok_or_else(|| anyhow::anyhow!("job not found: {key}"))?;
        let previous_state = job.state.clone();

        let new_state = match req.target {
            RequeueTarget::OnDeck => JobState::OnDeck,
            RequeueTarget::OnIce => JobState::OnIce,
        };

        self.state.graph.set_state(&key, &new_state)?;
        self.state
            .forgejo
            .set_job_state(&req.owner, &req.repo, req.number, &new_state)
            .await?;

        if previous_state != new_state {
            let mut updated_job = job;
            updated_job.state = new_state.clone();
            self.state
                .publish_transition(&JobTransition {
                    job: updated_job,
                    previous_state: Some(previous_state),
                    new_state,
                })
                .await;
        }

        Ok(())
    }

    /// Called when a worker is shutting down. Release any held claim, requeue the job,
    /// and remove the worker from the registry.
    async fn handle_unregister(&self, worker_id: &str) -> Result<()> {
        // If the worker is busy, abandon its current job.
        let current_job_key = self
            .state
            .dispatch_registry
            .get(worker_id)
            .and_then(|e| e.current_job_key.clone());

        if let Some(ref job_key) = current_job_key {
            // Release the claim and requeue.
            let _ = self.state.coord.release(job_key).await;

            let parts: Vec<&str> = job_key.splitn(3, '/').collect();
            if parts.len() == 3 {
                let (owner, repo) = (parts[0], parts[1]);
                let number: u64 = parts[2].parse().unwrap_or(0);
                self.state.graph.set_state(job_key, &JobState::OnDeck)?;
                self.state
                    .forgejo
                    .set_job_state(owner, repo, number, &JobState::OnDeck)
                    .await?;

                if let Ok(Some(job)) = self.state.graph.get_job(job_key) {
                    self.state
                        .publish_transition(&JobTransition {
                            job,
                            previous_state: Some(JobState::OnTheStack),
                            new_state: JobState::OnDeck,
                        })
                        .await;
                }
            }

            self.state
                .journal(
                    "unregister",
                    &format!("Worker shutting down — job requeued"),
                    Some(job_key),
                    Some(worker_id),
                )
                .await;
        } else {
            self.state
                .journal(
                    "unregister",
                    "Worker shutting down",
                    None,
                    Some(worker_id),
                )
                .await;
        }

        // Note: pending reworks in NATS KV don't need cleanup on unregister —
        // they'll be ignored if the worker never comes back.

        // Remove from registry and emit SSE event.
        self.state.dispatch_registry.remove(worker_id);
        self.state.emit_worker_removed(worker_id);

        tracing::info!(worker_id, "worker unregistered (graceful shutdown)");
        Ok(())
    }

    /// Called when a worker signals idle. Check pending reworks first, then normal assign.
    async fn handle_idle(&self, worker_id: &str) -> Result<()> {
        // Update status to idle.
        if let Some(mut entry) = self.state.dispatch_registry.get_mut(worker_id) {
            entry.state = WorkerState::Idle;
            entry.current_job_key = None;
            entry.current_job_priority = None;
            entry.last_seen = Utc::now();
            self.state.emit_worker(&entry);
        } else {
            tracing::warn!(worker_id, "idle from unregistered worker, ignoring");
            return Ok(());
        }

        // Check if this worker has a pending rework assignment.
        // First check NATS KV (explicit pending_reworks from ChangesRequested transitions).
        // Then check the graph for any ChangesRequested jobs assigned to this worker
        // (handles restart recovery when pending_reworks wasn't persisted).
        if let Some(job_key) = self.find_pending_rework(worker_id).await {
            if let Ok(Some(job)) = self.state.graph.get_job(&job_key) {
                if job.state == JobState::ChangesRequested {
                    return self.try_assign_rework(worker_id, &job).await;
                }
            }
            // Job no longer in ChangesRequested — clean up stale entry.
            let _ = self.state.coord.remove_pending_rework(&job_key).await;
        }

        // Restart recovery: check graph for ChangesRequested jobs assigned to this worker.
        if let Ok(cr_jobs) = self.state.graph.get_all_jobs(Some(&JobState::ChangesRequested)) {
            for job in &cr_jobs {
                if job.assignees.iter().any(|a| a == worker_id) {
                    return self.try_assign_rework(worker_id, job).await;
                }
            }
        }

        self.try_assign(worker_id).await
    }

    /// Try to assign an on-deck job to a specific idle worker.
    async fn try_assign(&self, worker_id: &str) -> Result<()> {
        let (w_caps, w_type, w_platform) = match self.state.dispatch_registry.get(worker_id) {
            Some(entry) => (
                entry.capabilities.clone(),
                entry.worker_type.clone(),
                entry.platform.clone(),
            ),
            None => return Ok(()),
        };

        let mut jobs = self.state.graph.get_all_jobs(Some(&JobState::OnDeck))?;
        jobs.sort_by(|a, b| b.priority.cmp(&a.priority));

        for job in &jobs {
            if !job_matches_worker(job, &w_caps, &w_type, &w_platform) {
                continue;
            }

            // Skip jobs this worker previously abandoned (circuit breaker).
            if self.state.coord.is_blacklisted(&job.key(), worker_id).await {
                continue;
            }

            // Guard: verify all declared deps are actually Done. The graph may
            // mark a job on-deck before CDC has synced all dependency edges.
            if !job.dependency_numbers.is_empty() {
                let deps_ok = self
                    .state
                    .graph
                    .all_declared_deps_done(
                        &job.repo_owner,
                        &job.repo_name,
                        &job.dependency_numbers,
                    )
                    .unwrap_or(false);
                if !deps_ok {
                    tracing::debug!(
                        job_key = %job.key(),
                        "skipping — deps not all done"
                    );
                    continue;
                }
            }

            // Try to claim.
            let timeout = job
                .timeout_secs
                .unwrap_or(self.state.config.default_timeout_secs);
            let lease_secs = self.state.config.lease_secs;
            let claim = self
                .state
                .coord
                .try_claim(&job.key(), worker_id.to_string(), timeout, lease_secs)
                .await?;

            let claim = match claim {
                Some(c) => c,
                None => continue, // already claimed by someone else
            };

            // Transition state and set assignee to the worker's Forgejo user.
            self.state
                .graph
                .set_state(&job.key(), &JobState::OnTheStack)?;
            self.state
                .forgejo
                .set_job_state(
                    &job.repo_owner,
                    &job.repo_name,
                    job.number,
                    &JobState::OnTheStack,
                )
                .await?;
            self.state
                .dispatcher_forgejo
                .set_assignees(
                    &job.repo_owner,
                    &job.repo_name,
                    job.number,
                    vec![worker_id.to_string()],
                )
                .await?;

            // Mark worker as busy.
            if let Some(mut entry) = self.state.dispatch_registry.get_mut(worker_id) {
                entry.state = WorkerState::Busy;
                entry.current_job_key = Some(job.key());
                entry.current_job_priority = Some(job.priority);
                entry.last_seen = Utc::now();
                self.state.emit_worker(&entry);
            }

            // Publish assignment.
            let assignment = Assignment {
                job: job.clone(),
                claim,
                is_rework: false,
            };
            self.state
                .coord
                .nats_client()
                .publish(
                    subject_assign(worker_id),
                    Bytes::from(serde_json::to_vec(&assignment)?),
                )
                .await
                .context("publish assignment")?;

            self.state
                .journal(
                    "assign",
                    &format!(
                        "Assigned {} (priority {}) to worker",
                        job.key(),
                        job.priority
                    ),
                    Some(&job.key()),
                    Some(worker_id),
                )
                .await;

            // Record activity start.
            let w_type = self
                .state
                .dispatch_registry
                .get(worker_id)
                .map(|e| e.worker_type.clone())
                .unwrap_or_default();
            self.record_activity_start(&job.key(), worker_id, &w_type);

            tracing::info!(
                worker_id,
                job_key = %job.key(),
                priority = job.priority,
                "assigned job to worker"
            );

            return Ok(());
        }

        tracing::debug!(worker_id, "no matching on-deck jobs for idle worker");
        Ok(())
    }

    /// Forward a worker heartbeat to the coordinator.
    async fn handle_heartbeat(&self, hb: &WorkerHeartbeat) -> Result<()> {
        if let Some(mut entry) = self.state.dispatch_registry.get_mut(&hb.worker_id) {
            entry.last_seen = Utc::now();
        } else {
            // Worker is heartbeating but not in registry (sidecar restarted while
            // worker was busy). Auto-register as Busy so the UI shows it.
            let now = Utc::now();
            let info = WorkerInfo {
                worker_id: hb.worker_id.clone(),
                state: WorkerState::Busy,
                current_job_key: Some(hb.job_key.clone()),
                current_job_priority: None,
                capabilities: vec![],
                worker_type: String::new(),
                platform: vec![],
                last_seen: now,
            };
            self.state.emit_worker(&info);
            self.state.dispatch_registry.insert(hb.worker_id.clone(), info);
            tracing::info!(worker_id = %hb.worker_id, job_key = %hb.job_key, "auto-registered busy worker from heartbeat");
        }
        let ok = self
            .state
            .coord
            .heartbeat(&hb.job_key, &hb.worker_id)
            .await?;
        if !ok {
            tracing::warn!(
                worker_id = %hb.worker_id,
                job_key = %hb.job_key,
                "heartbeat rejected — worker is not the claim holder"
            );
        }
        Ok(())
    }

    /// Handle a worker's outcome report: release claim, update state, sync Forgejo.
    async fn handle_outcome(&self, wo: &WorkerOutcome) -> Result<()> {
        let key = &wo.job_key;

        // Parse owner/repo/number from the job key.
        let parts: Vec<&str> = key.splitn(3, '/').collect();
        if parts.len() != 3 {
            anyhow::bail!("malformed job key: {key}");
        }
        let (owner, repo) = (parts[0], parts[1]);
        let number: u64 = parts[2].parse().context("parse issue number")?;

        // Release the claim.
        self.state.coord.release(key).await?;

        // Finalize the current activity.
        let activity_status = match &wo.outcome {
            OutcomeReport::Complete | OutcomeReport::Yield => ActivityStatus::Success,
            OutcomeReport::Fail { .. } => ActivityStatus::Failure,
            OutcomeReport::Abandon => ActivityStatus::Failure,
        };
        self.record_activity_end(key, activity_status);

        let new_state = match &wo.outcome {
            OutcomeReport::Complete => {
                self.state.graph.set_state(key, &JobState::InReview)?;
                self.state
                    .forgejo
                    .set_job_state(owner, repo, number, &JobState::InReview)
                    .await?;
                self.state
                    .journal(
                        "complete",
                        &format!("Worker completed job → in-review"),
                        Some(key),
                        Some(&wo.worker_id),
                    )
                    .await;
                tracing::info!(
                    worker_id = %wo.worker_id,
                    job_key = key,
                    "worker completed job"
                );
                JobState::InReview
            }
            OutcomeReport::Fail { reason, logs } => {
                // Increment and persist retry_attempt on the job in the graph.
                let (attempt, max_retries) = {
                    let job = self.state.graph.get_job(key)?;
                    let mut attempt = job.as_ref().map(|j| j.retry_attempt).unwrap_or(0);
                    attempt += 1;
                    let max = job.as_ref().map(|j| j.max_retries).unwrap_or(3);
                    // Persist the incremented attempt count.
                    if let Some(mut j) = job {
                        j.retry_attempt = attempt;
                        self.state.graph.upsert_job(&j)?;
                    }
                    (attempt, max)
                };

                let failure = FailureRecord {
                    worker_id: wo.worker_id.clone(),
                    kind: FailureKind::WorkerReported,
                    reason: reason.clone(),
                    logs: logs.clone(),
                    failed_at: Utc::now(),
                };

                // Post the failure comment regardless of retry.
                self.state
                    .dispatcher_forgejo
                    .post_failure_comment(owner, repo, number, &failure)
                    .await?;

                if attempt <= max_retries {
                    // Retry: return to on-deck for re-assignment.
                    self.state.graph.set_state(key, &JobState::OnDeck)?;
                    self.state
                        .forgejo
                        .set_job_state(owner, repo, number, &JobState::OnDeck)
                        .await?;
                    self.state
                        .journal(
                            "retry",
                            &format!("Attempt {attempt}/{max_retries} failed: {reason} — retrying"),
                            Some(key),
                            Some(&wo.worker_id),
                        )
                        .await;
                    tracing::warn!(
                        worker_id = %wo.worker_id,
                        job_key = key,
                        attempt,
                        max_retries,
                        reason,
                        "job failed, retrying"
                    );
                    JobState::OnDeck
                } else {
                    // Exhausted retries — mark as permanently failed.
                    self.state.graph.set_state(key, &JobState::Failed)?;
                    self.state
                        .forgejo
                        .set_job_state(owner, repo, number, &JobState::Failed)
                        .await?;
                    self.state
                        .journal(
                            "fail",
                            &format!("All {max_retries} retries exhausted: {reason}"),
                            Some(key),
                            Some(&wo.worker_id),
                        )
                        .await;
                    tracing::error!(
                        worker_id = %wo.worker_id,
                        job_key = key,
                        attempt,
                        max_retries,
                        reason,
                        "job failed permanently — retries exhausted"
                    );
                    JobState::Failed
                }
            }
            OutcomeReport::Abandon => {
                self.state.graph.set_state(key, &JobState::OnDeck)?;
                self.state
                    .forgejo
                    .set_job_state(owner, repo, number, &JobState::OnDeck)
                    .await?;

                // Blacklist this (job, worker) pair to prevent tight re-assign loops.
                // Persisted in NATS KV with 1-hour TTL.
                let _ = self.state.coord.blacklist_abandon(key, &wo.worker_id).await;

                self.state
                    .journal(
                        "abandon",
                        &format!("Worker abandoned job → on-deck"),
                        Some(key),
                        Some(&wo.worker_id),
                    )
                    .await;
                tracing::info!(
                    worker_id = %wo.worker_id,
                    job_key = key,
                    "worker abandoned job (blacklisted from re-assignment)"
                );
                JobState::OnDeck
            }
            OutcomeReport::Yield => {
                // Worker yielded after opening a PR. Transition to InReview
                // so the reviewer picks it up. This is dispatcher-driven (not
                // CDC-driven) to avoid race conditions with claim checks.
                self.state.graph.set_state(key, &JobState::InReview)?;
                self.state
                    .forgejo
                    .set_job_state(owner, repo, number, &JobState::InReview)
                    .await?;

                // Clear any pending rework for this job — the worker just completed it.
                let _ = self.state.coord.remove_pending_rework(key).await;

                self.state
                    .journal(
                        "yield",
                        &format!("Worker yielded → in-review"),
                        Some(key),
                        Some(&wo.worker_id),
                    )
                    .await;
                tracing::info!(
                    worker_id = %wo.worker_id,
                    job_key = key,
                    "worker yielded job → in-review"
                );
                JobState::InReview
            }
        };

        // Mark worker as transitioning — it will send IdleEvent when ready.
        if let Some(mut entry) = self.state.dispatch_registry.get_mut(&wo.worker_id) {
            entry.state = WorkerState::Transitioning;
            entry.current_job_key = None;
            entry.current_job_priority = None;
            entry.last_seen = Utc::now();
            self.state.emit_worker(&entry);
        }

        // Publish transition event for other reactors.
        if let Ok(Some(job)) = self.state.graph.get_job(key) {
            self.state
                .publish_transition(&JobTransition {
                    job,
                    previous_state: Some(JobState::OnTheStack),
                    new_state,
                })
                .await;
        }

        Ok(())
    }

    /// React to a state-transition event from the NATS stream.
    async fn handle_transition(&self, t: &JobTransition) -> Result<()> {
        match &t.new_state {
            JobState::OnDeck => {
                // A job became on-deck — try idle workers, then preemption.
                if let Some(worker_id) = self.find_idle_worker(&t.job).await {
                    return self.try_assign(&worker_id).await;
                }

                if let Some((victim_id, victim_priority)) =
                    self.find_preemption_candidate(&t.job, t.job.priority)
                {
                    self.state.journal(
                        "preempt",
                        &format!(
                            "Preempting {} (priority {victim_priority}) for higher-priority job {} (priority {})",
                            victim_id, t.job.key(), t.job.priority
                        ),
                        Some(&t.job.key()),
                        Some(&victim_id),
                    ).await;
                    tracing::info!(
                        job_key = %t.job.key(),
                        job_priority = t.job.priority,
                        victim_worker = %victim_id,
                        victim_priority,
                        "preempting worker for higher-priority job"
                    );

                    let notice = PreemptNotice {
                        reason: format!(
                            "higher-priority job {} (priority {}) arrived",
                            t.job.key(),
                            t.job.priority
                        ),
                        new_job: t.job.clone(),
                    };

                    self.state
                        .coord
                        .nats_client()
                        .publish(
                            subject_preempt(&victim_id),
                            Bytes::from(serde_json::to_vec(&notice)?),
                        )
                        .await
                        .context("publish preempt notice")?;
                }
            }
            JobState::OnTheStack => {
                // A worker claimed this job — check if already tracked.
                let job_key = t.job.key();
                for entry in self.state.dispatch_registry.iter() {
                    if entry.current_job_key.as_deref() == Some(&job_key) {
                        return Ok(());
                    }
                }
            }
            JobState::ChangesRequested => {
                // A reviewer requested changes. Set state + label, then route
                // back to the original worker. (Previously done by the consumer
                // via CDC; now the reviewer publishes this transition directly.)
                let job_key = t.job.key();

                // Skip if rework is already pending or in progress for this job.
                if self.state.coord.has_pending_rework(&job_key).await {
                    tracing::debug!(job_key = %job_key, "rework already pending, skipping duplicate");
                    return Ok(());
                }
                if let Ok(Some(current)) = self.state.graph.get_job(&job_key) {
                    if current.state == JobState::OnTheStack {
                        tracing::debug!(job_key = %job_key, "job already on-the-stack for rework, skipping");
                        return Ok(());
                    }
                }

                // Set graph state and Forgejo label (idempotent).
                self.state
                    .graph
                    .set_state(&job_key, &JobState::ChangesRequested)?;
                self.state
                    .forgejo
                    .set_job_state(
                        &t.job.repo_owner,
                        &t.job.repo_name,
                        t.job.number,
                        &JobState::ChangesRequested,
                    )
                    .await?;

                // Get the original worker from the transition's assignees, falling
                // back to the graph if the transition has empty assignees.
                let original_worker = t.job.assignees.first().cloned().or_else(|| {
                    self.state.graph.get_job(&job_key).ok().flatten()
                        .and_then(|j| j.assignees.first().cloned())
                });

                if let Some(ref worker_id) = original_worker {
                    // Check if the original worker is currently idle.
                    let is_idle = self
                        .state
                        .dispatch_registry
                        .get(worker_id)
                        .map(|e| matches!(e.state, WorkerState::Idle))
                        .unwrap_or(false);

                    if is_idle {
                        // Assign immediately.
                        self.try_assign_rework(worker_id, &t.job).await?;
                    } else {
                        // Worker is busy — queue for when it becomes idle (persisted in NATS KV).
                        let _ = self.state.coord
                            .put_pending_rework(&job_key, worker_id)
                            .await;
                        self.state
                            .journal(
                                "changes-requested",
                                &format!("Rework queued for worker {worker_id} (currently busy)"),
                                Some(&job_key),
                                Some(worker_id),
                            )
                            .await;
                        tracing::info!(
                            job_key = %job_key,
                            worker_id = %worker_id,
                            "rework queued — original worker is busy"
                        );
                    }
                } else {
                    tracing::warn!(
                        job_key = %job_key,
                        "rework requested but no assignee to route back to"
                    );
                }
            }
            JobState::Done | JobState::Failed | JobState::InReview | JobState::Revoked => {
                // Terminal or review states: sync graph + label (idempotent), then
                // clear the worker's busy tracking.
                let job_key = t.job.key();
                if matches!(t.new_state, JobState::InReview) {
                    // InReview may come from the yield handler (already set) or from
                    // an external source. Ensure graph + label are in sync.
                    let _ = self.state.graph.set_state(&job_key, &JobState::InReview);
                    let _ = self.state.forgejo.set_job_state(
                        &t.job.repo_owner, &t.job.repo_name, t.job.number, &JobState::InReview
                    ).await;
                }
                let _ = self.state.coord.remove_pending_rework(&job_key).await;
                for mut entry in self.state.dispatch_registry.iter_mut() {
                    if entry.current_job_key.as_deref() == Some(&job_key) {
                        entry.state = WorkerState::Transitioning;
                        entry.current_job_key = None;
                        entry.current_job_priority = None;
                        entry.last_seen = Utc::now();
                        self.state.emit_worker(&entry);
                        break;
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// Assign a rework job to the original worker. Claim, transition Rework → OnTheStack,
    /// and publish an Assignment with `is_rework: true`.
    async fn try_assign_rework(&self, worker_id: &str, job: &workflow_types::Job) -> Result<()> {
        let job_key = job.key();

        let timeout = job
            .timeout_secs
            .unwrap_or(self.state.config.default_timeout_secs);
        let lease_secs = self.state.config.lease_secs;
        let claim = self
            .state
            .coord
            .try_claim(&job_key, worker_id.to_string(), timeout, lease_secs)
            .await?;
        let claim = match claim {
            Some(c) => c,
            None => {
                tracing::warn!(job_key = %job_key, "rework claim failed — already claimed");
                return Ok(());
            }
        };

        self.state
            .graph
            .set_state(&job_key, &JobState::OnTheStack)?;
        self.state
            .forgejo
            .set_job_state(
                &job.repo_owner,
                &job.repo_name,
                job.number,
                &JobState::OnTheStack,
            )
            .await?;
        // Assignee is already set from the original work cycle — no need to re-set.
        // Note: stale REQUEST_CHANGES reviews are dismissed by the reviewer when it
        // handles the next InReview transition (reviewer has the right permissions).

        if let Some(mut entry) = self.state.dispatch_registry.get_mut(worker_id) {
            entry.state = WorkerState::Busy;
            entry.current_job_key = Some(job_key.clone());
            entry.current_job_priority = Some(job.priority);
            entry.last_seen = Utc::now();
            self.state.emit_worker(&entry);
        }

        let assignment = Assignment {
            job: job.clone(),
            claim,
            is_rework: true,
        };
        self.state
            .coord
            .nats_client()
            .publish(
                subject_assign(worker_id),
                Bytes::from(serde_json::to_vec(&assignment)?),
            )
            .await
            .context("publish rework assignment")?;

        let _ = self.state.coord.remove_pending_rework(&job_key).await;

        self.state
            .journal(
                "changes-requested",
                &format!("Re-assigned {} to original worker for rework", job_key),
                Some(&job_key),
                Some(worker_id),
            )
            .await;

        // Record activity start.
        let w_type = self
            .state
            .dispatch_registry
            .get(worker_id)
            .map(|e| e.worker_type.clone())
            .unwrap_or_default();
        self.record_activity_start(&job_key, worker_id, &w_type);

        tracing::info!(
            worker_id,
            job_key = %job_key,
            "assigned rework to original worker"
        );

        Ok(())
    }

    /// Worker published a help request: transition NeedsHelp, post Forgejo comment,
    /// store pending_helps so a human response can be routed back.
    async fn handle_help_request(&self, req: &HelpRequest) -> Result<()> {
        let key = &req.job_key;
        let parts: Vec<&str> = key.splitn(3, '/').collect();
        if parts.len() != 3 {
            anyhow::bail!("malformed job key: {key}");
        }
        let (owner, repo) = (parts[0], parts[1]);
        let number: u64 = parts[2].parse().context("parse issue number")?;

        self.state.graph.set_state(key, &JobState::NeedsHelp)?;
        self.state
            .forgejo
            .set_job_state(owner, repo, number, &JobState::NeedsHelp)
            .await?;

        // Post a comment explaining the situation and how to respond.
        let nats_key = job_key_to_nats(key);
        let mut comment = format!(
            "<!-- workflow:needs-help -->\n\n\
             🤚 **Agent needs your input** (job `{key}`)\n\n\
             **Reason:** {reason}",
            reason = req.reason,
        );
        if let Some(hint) = &req.session_hint {
            comment.push_str(&format!(
                "\n\n**Observe the session:**\n```\n{hint}\n```"
            ));
        }
        comment.push_str(&format!(
            "\n\n**To respond**, run:\n```\nworkflow-cli interact respond {key} \"your response here\"\n```\
             \nor publish directly to NATS subject `workflow.interact.respond.{nats_key}`."
        ));

        self.state
            .dispatcher_forgejo
            .post_comment(owner, repo, number, &comment)
            .await?;

        // Store the mapping so we can route the response.
        self.state
            .pending_helps
            .insert(key.to_string(), req.worker_id.clone());

        self.state
            .journal(
                "needs-help",
                &format!("Worker requested human input: {}", req.reason),
                Some(key),
                Some(&req.worker_id),
            )
            .await;

        tracing::info!(
            job_key = %key,
            worker_id = %req.worker_id,
            reason = %req.reason,
            "worker requested human input — job paused"
        );

        // Publish transition so the graph viewer reflects the new state.
        if let Ok(Some(job)) = self.state.graph.get_job(key) {
            self.state
                .publish_transition(&JobTransition {
                    job,
                    previous_state: Some(JobState::OnTheStack),
                    new_state: JobState::NeedsHelp,
                })
                .await;
        }

        Ok(())
    }

    /// Human sent a response: route it to the waiting worker, restore OnTheStack.
    async fn handle_help_response(&self, job_key: &str, resp: &HelpResponse) -> Result<()> {
        let worker_id = match self.state.pending_helps.remove(job_key) {
            Some((_, wid)) => wid,
            None => {
                tracing::warn!(job_key, "received help response but no pending help request");
                return Ok(());
            }
        };

        let parts: Vec<&str> = job_key.splitn(3, '/').collect();
        if parts.len() != 3 {
            anyhow::bail!("malformed job key: {job_key}");
        }
        let (owner, repo) = (parts[0], parts[1]);
        let number: u64 = parts[2].parse().context("parse issue number")?;

        // Restore OnTheStack — worker is back in control.
        self.state
            .graph
            .set_state(job_key, &JobState::OnTheStack)?;
        self.state
            .forgejo
            .set_job_state(owner, repo, number, &JobState::OnTheStack)
            .await?;

        // Deliver the response directly to the worker via NATS.
        self.state
            .coord
            .nats_client()
            .publish(
                subject_deliver(&worker_id),
                Bytes::from(serde_json::to_vec(resp)?),
            )
            .await
            .context("deliver help response to worker")?;

        self.state
            .journal(
                "help-response",
                "Human responded — worker resumed",
                Some(job_key),
                Some(&worker_id),
            )
            .await;

        tracing::info!(
            job_key,
            worker_id = %worker_id,
            "help response delivered — job resumed"
        );

        if let Ok(Some(job)) = self.state.graph.get_job(job_key) {
            self.state
                .publish_transition(&JobTransition {
                    job,
                    previous_state: Some(JobState::NeedsHelp),
                    new_state: JobState::OnTheStack,
                })
                .await;
        }

        Ok(())
    }

    // ── Monitor event handlers ────────────────────────────────────────────

    /// Handle a lease-expired advisory from the monitor.
    /// Re-verifies the claim is still stale before acting.
    async fn handle_lease_expired(&self, event: &LeaseExpiredEvent) -> Result<()> {
        let key = &event.job_key;

        // Re-check the claim — worker may have heartbeated since monitor detected expiry.
        let claim = self.state.coord.get_claim(key).await?;
        let claim = match claim {
            Some(c) if c.is_lease_expired() && c.worker_id == event.worker_id => c,
            Some(_) => {
                tracing::debug!(
                    job_key = key,
                    "lease-expired event stale — claim is now valid, ignoring"
                );
                return Ok(());
            }
            None => {
                tracing::debug!(
                    job_key = key,
                    "lease-expired event stale — no claim exists, ignoring"
                );
                return Ok(());
            }
        };

        let parts: Vec<&str> = key.splitn(3, '/').collect();
        if parts.len() != 3 {
            anyhow::bail!("malformed job key: {key}");
        }
        let (owner, repo) = (parts[0], parts[1]);
        let number: u64 = parts[2].parse().context("parse issue number")?;

        tracing::warn!(
            job_key = key,
            worker_id = event.worker_id,
            elapsed_secs = event.elapsed_secs,
            "confirmed lease expired — releasing claim and requeueing"
        );

        // Release the claim.
        if let Err(e) = self.state.coord.release(key).await {
            tracing::error!(key, "failed to release expired lease: {e:#}");
        }

        // Check retry budget.
        let (attempt, max_retries) = {
            let job = self.state.graph.get_job(key).ok().flatten();
            let mut attempt = job.as_ref().map(|j| j.retry_attempt).unwrap_or(0);
            attempt += 1;
            let max = job.as_ref().map(|j| j.max_retries).unwrap_or(3);
            if let Some(mut j) = job {
                j.retry_attempt = attempt;
                let _ = self.state.graph.upsert_job(&j);
            }
            (attempt, max)
        };

        let failure = FailureRecord {
            worker_id: claim.worker_id.clone(),
            kind: FailureKind::LeaseExpired,
            reason: format!(
                "No heartbeat for {}s (lease: {}s). Attempt {attempt}/{max_retries}.",
                event.elapsed_secs, claim.lease_secs
            ),
            logs: None,
            failed_at: Utc::now(),
        };

        if let Err(e) = self
            .state
            .dispatcher_forgejo
            .post_failure_comment(owner, repo, number, &failure)
            .await
        {
            tracing::error!(key, "failed to post lease-expiry comment: {e:#}");
        }

        if attempt <= max_retries {
            self.state.graph.set_state(key, &JobState::OnDeck)?;
            self.state
                .forgejo
                .set_job_state(owner, repo, number, &JobState::OnDeck)
                .await?;
            if let Err(e) = self
                .state
                .dispatcher_forgejo
                .clear_assignees(owner, repo, number)
                .await
            {
                tracing::error!(key, "failed to clear assignees on lease expiry: {e:#}");
            }

            self.state
                .journal(
                    "lease-expired",
                    &format!(
                        "Worker {} lease expired (attempt {attempt}/{max_retries}) — requeueing",
                        claim.worker_id
                    ),
                    Some(key),
                    Some(&claim.worker_id),
                )
                .await;

            if let Ok(Some(job)) = self.state.graph.get_job(key) {
                self.state
                    .publish_transition(&JobTransition {
                        job,
                        previous_state: Some(JobState::OnTheStack),
                        new_state: JobState::OnDeck,
                    })
                    .await;
            }
        } else {
            self.state.graph.set_state(key, &JobState::Failed)?;
            self.state
                .forgejo
                .set_job_state(owner, repo, number, &JobState::Failed)
                .await?;

            self.state
                .journal(
                    "fail",
                    &format!(
                        "Worker {} lease expired — all {max_retries} retries exhausted",
                        claim.worker_id
                    ),
                    Some(key),
                    Some(&claim.worker_id),
                )
                .await;

            if let Ok(Some(job)) = self.state.graph.get_job(key) {
                self.state
                    .publish_transition(&JobTransition {
                        job,
                        previous_state: Some(JobState::OnTheStack),
                        new_state: JobState::Failed,
                    })
                    .await;
            }
        }

        // Mark worker as transitioning in registry.
        if let Some(mut entry) = self.state.dispatch_registry.get_mut(&event.worker_id) {
            entry.state = WorkerState::Transitioning;
            entry.current_job_key = None;
            entry.current_job_priority = None;
            entry.last_seen = Utc::now();
            self.state.emit_worker(&entry);
        }

        Ok(())
    }

    /// Handle a job-timeout advisory from the monitor.
    /// Re-verifies the claim before failing the job.
    async fn handle_job_timeout(&self, event: &JobTimeoutEvent) -> Result<()> {
        let key = &event.job_key;

        // Re-check the claim — may have been released already.
        let claim = self.state.coord.get_claim(key).await?;
        let claim = match claim {
            Some(c) if c.is_timed_out() && c.worker_id == event.worker_id => c,
            _ => {
                tracing::debug!(
                    job_key = key,
                    "timeout event stale — claim changed, ignoring"
                );
                return Ok(());
            }
        };

        let parts: Vec<&str> = key.splitn(3, '/').collect();
        if parts.len() != 3 {
            anyhow::bail!("malformed job key: {key}");
        }
        let (owner, repo) = (parts[0], parts[1]);
        let number: u64 = parts[2].parse().context("parse issue number")?;

        let elapsed = chrono::Utc::now()
            .signed_duration_since(claim.claimed_at)
            .num_seconds();

        tracing::info!(
            job_key = key,
            worker_id = event.worker_id,
            elapsed_secs = elapsed,
            timeout_secs = claim.timeout_secs,
            "confirmed job timeout — failing"
        );

        let failure = FailureRecord {
            worker_id: claim.worker_id.clone(),
            kind: FailureKind::HeartbeatTimeout,
            reason: format!(
                "Job exceeded deadline: {elapsed}s elapsed (limit: {}s)",
                claim.timeout_secs
            ),
            logs: None,
            failed_at: Utc::now(),
        };

        if let Err(e) = self.state.coord.release(key).await {
            tracing::error!(key, "failed to release timed-out claim: {e:#}");
        }

        self.state.graph.set_state(key, &JobState::Failed)?;
        self.state
            .forgejo
            .set_job_state(owner, repo, number, &JobState::Failed)
            .await?;

        if let Err(e) = self
            .state
            .dispatcher_forgejo
            .post_failure_comment(owner, repo, number, &failure)
            .await
        {
            tracing::error!(key, "failed to post timeout comment: {e:#}");
        }

        if let Err(e) = self
            .state
            .dispatcher_forgejo
            .clear_assignees(owner, repo, number)
            .await
        {
            tracing::error!(key, "failed to clear assignees on timeout: {e:#}");
        }

        self.state
            .journal(
                "timeout",
                &format!(
                    "Job deadline exceeded ({}s) — failed permanently",
                    claim.timeout_secs
                ),
                Some(key),
                Some(&claim.worker_id),
            )
            .await;

        if let Ok(Some(job)) = self.state.graph.get_job(key) {
            self.state
                .publish_transition(&JobTransition {
                    job,
                    previous_state: Some(JobState::OnTheStack),
                    new_state: JobState::Failed,
                })
                .await;
        }

        // Mark worker as transitioning in registry.
        if let Some(mut entry) = self.state.dispatch_registry.get_mut(&event.worker_id) {
            entry.state = WorkerState::Transitioning;
            entry.current_job_key = None;
            entry.current_job_priority = None;
            entry.last_seen = Utc::now();
            self.state.emit_worker(&entry);
        }

        Ok(())
    }

    /// Handle an orphan-detected advisory from the monitor.
    /// Re-verifies the orphan state before requeueing.
    async fn handle_orphan_detected(&self, event: &OrphanDetectedEvent) -> Result<()> {
        let key = &event.job_key;

        // Re-check: job might have been claimed between monitor detection and this handler.
        let has_claim = self.state.coord.get_claim(key).await?.is_some();
        if has_claim {
            tracing::debug!(
                job_key = key,
                "orphan event stale — claim now exists, ignoring"
            );
            return Ok(());
        }

        // Re-check state: job might have transitioned since the monitor detected it.
        let job = match self.state.graph.get_job(key)? {
            Some(j) if matches!(j.state, JobState::OnTheStack | JobState::NeedsHelp) => j,
            _ => {
                tracing::debug!(
                    job_key = key,
                    "orphan event stale — job state changed, ignoring"
                );
                return Ok(());
            }
        };

        let previous_state = job.state.clone();

        tracing::warn!(
            job_key = key,
            state = ?previous_state,
            "confirmed orphaned job — requeueing"
        );

        self.state.graph.set_state(key, &JobState::OnDeck)?;
        self.state
            .forgejo
            .set_job_state(&job.repo_owner, &job.repo_name, job.number, &JobState::OnDeck)
            .await?;

        self.state
            .journal(
                "orphan-recovery",
                &format!("Job was {:?} with no claim — requeued", previous_state),
                Some(key),
                None,
            )
            .await;

        if let Ok(Some(updated)) = self.state.graph.get_job(key) {
            self.state
                .publish_transition(&JobTransition {
                    job: updated,
                    previous_state: Some(previous_state),
                    new_state: JobState::OnDeck,
                })
                .await;
        }

        Ok(())
    }

    /// Find a pending rework for this worker by checking NATS KV.
    /// Since KV is keyed by job_key, we check all ChangesRequested jobs
    /// assigned to this worker for a pending rework entry.
    async fn find_pending_rework(&self, worker_id: &str) -> Option<String> {
        // Get all jobs from the graph that are ChangesRequested and assigned to this worker.
        let jobs = self.state.graph.get_all_jobs(Some(&JobState::ChangesRequested)).ok()?;
        for job in jobs {
            if job.state == JobState::ChangesRequested
                && job.assignees.iter().any(|a| a == worker_id)
            {
                let key = job.key();
                if self.state.coord.has_pending_rework(&key).await {
                    return Some(key);
                }
            }
        }
        None
    }

    /// Find the first idle worker eligible to work the given job.
    async fn find_idle_worker(&self, job: &Job) -> Option<String> {
        let job_key = job.key();
        for entry in self.state.dispatch_registry.iter() {
            if matches!(entry.state, WorkerState::Idle)
                && job_matches_worker(
                    job,
                    &entry.capabilities,
                    &entry.worker_type,
                    &entry.platform,
                )
                && !self.state.coord.is_blacklisted(&job_key, &entry.worker_id).await
            {
                return Some(entry.worker_id.clone());
            }
        }
        None
    }

    /// Find the busy worker on the lowest-priority job that is lower than `new_priority`
    /// and is eligible to work the given job.
    fn find_preemption_candidate(
        &self,
        job: &Job,
        new_priority: u32,
    ) -> Option<(String, u32)> {
        let mut best: Option<(String, u32)> = None;

        for entry in self.state.dispatch_registry.iter() {
            if matches!(entry.state, WorkerState::Busy) {
                if let Some(priority) = entry.current_job_priority {
                    if priority < new_priority
                        && job_matches_worker(
                            job,
                            &entry.capabilities,
                            &entry.worker_type,
                            &entry.platform,
                        )
                    {
                        match &best {
                            None => best = Some((entry.worker_id.clone(), priority)),
                            Some((_, best_prio)) if &priority < best_prio => {
                                best = Some((entry.worker_id.clone(), priority));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        best
    }

    // ── Activity tracking ────────────────────────────────────────────────────

    /// Record an activity start when a worker is assigned a job.
    /// Also finalizes any stale running activities from previous cycles
    /// (belt-and-suspenders for races between activity append and update).
    fn record_activity_start(&self, job_key: &str, worker_id: &str, worker_type: &str) {
        const MAX_ACTIVITIES: usize = 50;

        // Guard against activity bloat from loops.
        if let Ok(Some(job)) = self.state.graph.get_job(job_key) {
            if job.activities.len() >= MAX_ACTIVITIES {
                tracing::warn!(
                    job_key,
                    count = job.activities.len(),
                    "activity limit reached ({MAX_ACTIVITIES}), skipping new activity record"
                );
                return;
            }
        }

        let now = Utc::now();

        // Finalize any stale running activities before starting a new one.
        let _ = self.state.graph.update_activity(job_key, |activities| {
            let mut changed = false;
            for entry in activities.iter_mut() {
                if *entry.status() == ActivityStatus::Running {
                    entry.set_status(ActivityStatus::Success);
                    entry.set_ended_at(now);
                    changed = true;
                }
            }
            changed
        });

        let entry = match worker_type {
            "interactive" => ActivityEntry::InteractiveSession {
                session_id: format!("{}-{}", worker_id, now.timestamp_millis()),
                worker_id: worker_id.to_string(),
                status: ActivityStatus::Running,
                started_at: now,
                ended_at: None,
                recording_key: None,
            },
            _ => ActivityEntry::ActionRun {
                run_id: 0, // updated later if the worker reports it
                workflow: String::new(),
                worker_id: worker_id.to_string(),
                status: ActivityStatus::Running,
                started_at: now,
                ended_at: None,
            },
        };

        if let Err(e) = self.state.graph.append_activity(job_key, entry) {
            tracing::error!(job_key, error = %e, "failed to record activity start");
        }
        if let Ok(Some(job)) = self.state.graph.get_job(job_key) {
            self.state.emit_job(&job);
        }
    }

    /// Update the most recent running activity on a job with a final status.
    fn record_activity_end(&self, job_key: &str, status: ActivityStatus) {
        let now = Utc::now();
        let final_status = status;
        if let Err(e) = self.state.graph.update_activity(job_key, |activities| {
            // Find the last running activity and finalize it.
            for entry in activities.iter_mut().rev() {
                if *entry.status() == ActivityStatus::Running {
                    entry.set_status(final_status.clone());
                    entry.set_ended_at(now);
                    return true;
                }
            }
            false
        }) {
            tracing::error!(job_key, error = %e, "failed to record activity end");
        }
        if let Ok(Some(job)) = self.state.graph.get_job(job_key) {
            self.state.emit_job(&job);
        }
    }
}

/// Check whether a worker is eligible to execute a job based on all routing
/// constraints: capabilities, worker type, and platform.
fn job_matches_worker(
    job: &Job,
    worker_caps: &[String],
    worker_type: &str,
    worker_platform: &[String],
) -> bool {
    // Capabilities: worker must have ALL required capabilities.
    if !job.capabilities.iter().all(|r| worker_caps.contains(r)) {
        return false;
    }

    // Worker type: if the job specifies a worker type, the worker must match.
    if let Some(ref required_type) = job.worker_type {
        if required_type != worker_type {
            return false;
        }
    }

    // Platform: worker must have ALL required platforms.
    if !job.platform.iter().all(|r| worker_platform.contains(r)) {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use workflow_types::ReviewRequirement;

    fn stub_job(
        caps: Vec<String>,
        worker_type: Option<String>,
        platform: Vec<String>,
    ) -> Job {
        Job {
            repo_owner: "o".into(),
            repo_name: "r".into(),
            number: 1,
            title: "test".into(),
            state: JobState::OnDeck,
            assignees: vec![],
            dependency_numbers: vec![],
            priority: 50,
            timeout_secs: None,
            capabilities: caps,
            max_retries: 3,
            retry_attempt: 0,
            worker_type,
            platform,
            review: ReviewRequirement::High,
            activities: vec![],
            is_rework: false,
        }
    }

    #[test]
    fn test_match_empty_requirements() {
        let job = stub_job(vec![], None, vec![]);
        assert!(job_matches_worker(&job, &["rust".into()], "action", &["linux".into()]));
        assert!(job_matches_worker(&job, &[], "sim", &[]));
    }

    #[test]
    fn test_match_capabilities_subset() {
        let job = stub_job(vec!["rust".into()], None, vec![]);
        assert!(job_matches_worker(&job, &["rust".into(), "frontend".into()], "action", &[]));
    }

    #[test]
    fn test_match_capabilities_missing() {
        let job = stub_job(vec!["rust".into(), "gpu".into()], None, vec![]);
        assert!(!job_matches_worker(&job, &["rust".into(), "frontend".into()], "action", &[]));
    }

    #[test]
    fn test_match_worker_type_required() {
        let job = stub_job(vec![], Some("interactive".into()), vec![]);
        assert!(!job_matches_worker(&job, &[], "action", &[]));
        assert!(job_matches_worker(&job, &[], "interactive", &[]));
    }

    #[test]
    fn test_match_worker_type_not_required() {
        let job = stub_job(vec![], None, vec![]);
        // Any worker type matches when job doesn't require one.
        assert!(job_matches_worker(&job, &[], "action", &[]));
        assert!(job_matches_worker(&job, &[], "interactive", &[]));
    }

    #[test]
    fn test_match_platform_required() {
        let job = stub_job(vec![], None, vec!["macos".into()]);
        assert!(!job_matches_worker(&job, &[], "action", &["linux".into()]));
        assert!(job_matches_worker(&job, &[], "action", &["macos".into(), "arm64".into()]));
    }

    #[test]
    fn test_match_platform_not_required() {
        let job = stub_job(vec![], None, vec![]);
        assert!(job_matches_worker(&job, &[], "action", &[]));
        assert!(job_matches_worker(&job, &[], "action", &["linux".into()]));
    }

    #[test]
    fn test_match_combined_constraints() {
        let job = stub_job(
            vec!["rust".into()],
            Some("interactive".into()),
            vec!["macos".into()],
        );
        // All match
        assert!(job_matches_worker(
            &job,
            &["rust".into(), "go".into()],
            "interactive",
            &["macos".into()],
        ));
        // Wrong worker type
        assert!(!job_matches_worker(
            &job,
            &["rust".into()],
            "action",
            &["macos".into()],
        ));
        // Missing capability
        assert!(!job_matches_worker(
            &job,
            &["go".into()],
            "interactive",
            &["macos".into()],
        ));
        // Missing platform
        assert!(!job_matches_worker(
            &job,
            &["rust".into()],
            "interactive",
            &["linux".into()],
        ));
    }
}
