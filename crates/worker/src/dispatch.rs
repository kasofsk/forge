//! NATS-based dispatched worker loop.
//!
//! Instead of polling the sidecar for available jobs, a dispatched worker
//! registers with the dispatcher, signals when idle, and waits for assignment
//! messages pushed over NATS. All lifecycle reporting (heartbeat, outcome)
//! goes through NATS — the worker never makes HTTP calls to the sidecar.

use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use workflow_types::{
    Assignment, IdleEvent, OutcomeReport, PreemptNotice, WorkerHeartbeat, WorkerOutcome,
    WorkerRegistration,
};

use crate::forgejo::ForgejoClient;
use crate::worker::{Outcome, Worker};

/// Runs the dispatched worker lifecycle: register → idle → receive assignment → execute → repeat.
pub struct DispatchedWorkerLoop<W> {
    worker: W,
    forgejo: ForgejoClient,
    nats: async_nats::Client,
    heartbeat_interval: Duration,
}

impl<W: Worker> DispatchedWorkerLoop<W> {
    pub async fn new(
        worker: W,
        forgejo_url: &str,
        forgejo_token: &str,
        nats_url: &str,
        heartbeat_interval: Duration,
    ) -> Result<Self> {
        let nats = async_nats::connect(nats_url)
            .await
            .context("connect to NATS for dispatch")?;
        Ok(Self {
            worker,
            forgejo: ForgejoClient::new(forgejo_url, forgejo_token),
            nats,
            heartbeat_interval,
        })
    }

    /// Publish an unregister message so the dispatcher cleans up immediately.
    async fn unregister(&self) {
        let idle = IdleEvent {
            worker_id: self.worker.worker_id().to_string(),
        };
        if let Ok(payload) = serde_json::to_vec(&idle) {
            let _ = self
                .nats
                .publish("workflow.dispatch.unregister", Bytes::from(payload))
                .await;
            let _ = self.nats.flush().await;
        }
    }

    /// Run the dispatched worker loop. Does not return under normal operation.
    pub async fn run(&self) -> Result<()> {
        let worker_id = self.worker.worker_id().to_string();

        let reg = WorkerRegistration {
            worker_id: worker_id.clone(),
            capabilities: self.worker.capabilities(),
            worker_type: self.worker.worker_type().to_string(),
            platform: self.worker.platform(),
        };

        // Register with the dispatcher and wait for acknowledgement
        // before sending idle, so registration is fully processed first.
        self.nats
            .request(
                "workflow.dispatch.register",
                Bytes::from(serde_json::to_vec(&reg)?),
            )
            .await
            .context("register with dispatcher")?;

        // Subscribe to assignment and preempt subjects.
        let assign_subject = format!("workflow.dispatch.assign.{worker_id}");
        let preempt_subject = format!("workflow.dispatch.preempt.{worker_id}");
        let mut assign_sub = self
            .nats
            .subscribe(assign_subject)
            .await
            .context("subscribe to assignments")?;
        let mut preempt_sub = self
            .nats
            .subscribe(preempt_subject)
            .await
            .context("subscribe to preempts")?;

        // Listen for OS shutdown signals so we can unregister gracefully.
        let shutdown = CancellationToken::new();
        {
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                let mut sigterm =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("register SIGTERM handler");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = sigterm.recv() => {},
                }
                shutdown.cancel();
            });
        }

        loop {
            // Signal idle.
            let idle = IdleEvent {
                worker_id: worker_id.clone(),
            };
            self.nats
                .publish(
                    "workflow.dispatch.idle",
                    Bytes::from(serde_json::to_vec(&idle)?),
                )
                .await
                .context("publish idle")?;

            tracing::info!(worker_id, "waiting for assignment");

            // Wait for an assignment, but periodically re-register + re-idle
            // in case the sidecar restarted and lost its in-memory registry.
            let mut reannounce = tokio::time::interval(Duration::from_secs(15));
            reannounce.tick().await; // consume the immediate first tick

            let assignment: Assignment = loop {
                tokio::select! {
                    msg = assign_sub.next() => {
                        match msg {
                            Some(msg) => match serde_json::from_slice(&msg.payload) {
                                Ok(a) => break a,
                                Err(e) => {
                                    tracing::warn!("bad assignment payload: {e:#}");
                                    continue;
                                }
                            },
                            None => anyhow::bail!("assignment subscription closed"),
                        }
                    }
                    _ = reannounce.tick() => {
                        // Re-register (request/reply — confirms dispatcher is alive).
                        match self.nats
                            .request(
                                "workflow.dispatch.register",
                                Bytes::from(serde_json::to_vec(&reg)?),
                            )
                            .await
                        {
                            Ok(_) => {
                                // Re-send idle so the dispatcher knows we're available.
                                let idle = IdleEvent { worker_id: worker_id.clone() };
                                let _ = self.nats
                                    .publish(
                                        "workflow.dispatch.idle",
                                        Bytes::from(serde_json::to_vec(&idle)?),
                                    )
                                    .await;
                                tracing::debug!(worker_id, "re-announced to dispatcher");
                            }
                            Err(e) => {
                                tracing::debug!(worker_id, error = %e, "re-register failed (dispatcher may be down)");
                            }
                        }
                    }
                    _ = shutdown.cancelled() => {
                        tracing::info!(worker_id, "shutdown signal received (idle)");
                        self.unregister().await;
                        return Ok(());
                    }
                }
            };

            let job = {
                let mut j = assignment.job.clone();
                j.is_rework = assignment.is_rework;
                j
            };
            let job_key = job.key();

            tracing::info!(
                worker_id,
                key = %job_key,
                "received assignment"
            );

            // Local validation — worker can still reject.
            if !self.worker.accepts(&job) {
                tracing::info!(worker_id, key = %job_key, "rejecting assignment (accepts=false)");
                self.publish_outcome(&worker_id, &job_key, OutcomeReport::Abandon)
                    .await;
                continue;
            }

            // Spawn heartbeat via NATS. If the heartbeat is rejected (claim
            // revoked), cancel execution via the shared CancellationToken.
            let cancellation = CancellationToken::new();
            let hb_cancel = cancellation.clone();
            {
                let nats = self.nats.clone();
                let worker_c = worker_id.clone();
                let key_c = job_key.clone();
                let interval = self.heartbeat_interval;
                let cancel_on_reject = cancellation.clone();
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(interval);
                    let mut consecutive_failures: u32 = 0;
                    loop {
                        tokio::select! {
                            _ = ticker.tick() => {
                                let hb = WorkerHeartbeat {
                                    worker_id: worker_c.clone(),
                                    job_key: key_c.clone(),
                                };
                                if let Ok(payload) = serde_json::to_vec(&hb) {
                                    if let Err(e) = nats
                                        .publish("workflow.dispatch.heartbeat", Bytes::from(payload))
                                        .await
                                    {
                                        tracing::warn!("heartbeat publish failed: {e:#}");
                                        consecutive_failures += 1;
                                        // After 3 consecutive publish failures, the NATS
                                        // connection is likely broken. Cancel execution so
                                        // the worker can re-idle and reconnect.
                                        if consecutive_failures >= 3 {
                                            tracing::error!(
                                                worker_id = %worker_c,
                                                job_key = %key_c,
                                                "heartbeat publish failed {consecutive_failures} times — cancelling execution"
                                            );
                                            cancel_on_reject.cancel();
                                            break;
                                        }
                                    } else {
                                        consecutive_failures = 0;
                                    }
                                }
                            }
                            _ = hb_cancel.cancelled() => break,
                        }
                    }
                });
            }

            // Execute with preemption, claim revocation, and shutdown listening.
            let outcome = tokio::select! {
                result = self.worker.execute(&job, &self.forgejo) => result,
                preempt_msg = preempt_sub.next() => {
                    // Preemption arrived — cancel the worker and treat as abandon.
                    cancellation.cancel();
                    if let Some(msg) = preempt_msg {
                        if let Ok(notice) = serde_json::from_slice::<PreemptNotice>(&msg.payload) {
                            tracing::info!(
                                worker_id,
                                reason = %notice.reason,
                                new_job = %notice.new_job.key(),
                                "preempted"
                            );
                        }
                    }
                    Ok(Outcome::Abandon)
                }
                _ = cancellation.cancelled() => {
                    // Heartbeat task detected claim revocation or NATS failure.
                    tracing::warn!(worker_id, key = %job_key, "execution cancelled — claim likely revoked");
                    Ok(Outcome::Abandon)
                }
                _ = shutdown.cancelled() => {
                    tracing::info!(worker_id, "shutdown signal received (executing)");
                    cancellation.cancel();
                    Ok(Outcome::Abandon)
                }
            };

            // If shutdown arrived during execution, report abandon and unregister.
            if shutdown.is_cancelled() {
                self.publish_outcome(&worker_id, &job_key, OutcomeReport::Abandon).await;
                self.unregister().await;
                return Ok(());
            }

            cancellation.cancel(); // stop heartbeat

            // Report outcome via NATS — the dispatcher handles the rest.
            let report = match outcome {
                Ok(Outcome::Complete) => {
                    tracing::info!(worker_id, key = %job_key, "job complete");
                    OutcomeReport::Complete
                }
                Ok(Outcome::Fail { reason, logs }) => {
                    tracing::warn!(worker_id, key = %job_key, "job failed: {reason}");
                    OutcomeReport::Fail { reason, logs }
                }
                Ok(Outcome::Abandon) => {
                    tracing::info!(worker_id, key = %job_key, "job abandoned");
                    OutcomeReport::Abandon
                }
                Ok(Outcome::Yield) => {
                    tracing::info!(worker_id, key = %job_key, "job yielded (external transition)");
                    OutcomeReport::Yield
                }
                Err(e) => {
                    tracing::error!(worker_id, key = %job_key, "execute error: {e:#}");
                    OutcomeReport::Fail {
                        reason: format!("unexpected error: {e:#}"),
                        logs: None,
                    }
                }
            };

            self.publish_outcome(&worker_id, &job_key, report).await;
        }
    }

    /// Fire-and-forget publish of a worker outcome to NATS.
    async fn publish_outcome(&self, worker_id: &str, job_key: &str, outcome: OutcomeReport) {
        let msg = WorkerOutcome {
            worker_id: worker_id.to_string(),
            job_key: job_key.to_string(),
            outcome,
        };
        match serde_json::to_vec(&msg) {
            Ok(payload) => {
                if let Err(e) = self
                    .nats
                    .publish("workflow.dispatch.outcome", Bytes::from(payload))
                    .await
                {
                    tracing::error!(worker_id, job_key, error = %e, "failed to publish outcome");
                }
            }
            Err(e) => {
                tracing::error!(worker_id, job_key, error = %e, "failed to serialize outcome");
            }
        }
    }
}
