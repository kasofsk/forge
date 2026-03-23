use std::sync::Arc;
use workflow_types::{JobState, LeaseExpiredEvent, JobTimeoutEvent, OrphanDetectedEvent};

use crate::events;
use crate::AppState;

// ── NATS subjects for monitor advisory events ────────────────────────────────

const SUBJECT_LEASE_EXPIRED: &str = "workflow.monitor.lease-expired";
const SUBJECT_TIMEOUT: &str = "workflow.monitor.timeout";
const SUBJECT_ORPHAN: &str = "workflow.monitor.orphan";

/// Background task: scan active claims for lease expiry and job deadline
/// timeouts, and prune stale workers from the dispatch registry.
///
/// The monitor is **advisory only** — it detects problems and publishes
/// events for the dispatcher to act on. It never mutates state, releases
/// claims, or sets labels directly.
pub async fn run_monitor(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(
        state.config.monitor_interval_secs,
    ));

    loop {
        interval.tick().await;
        if let Err(e) = check_claims(&state).await {
            tracing::error!("monitor error: {e:#}");
        }
        detect_orphaned_jobs(&state).await;
        cleanup_stale_sessions(&state).await;
        prune_stale_workers(&state);
    }
}

async fn check_claims(state: &Arc<AppState>) -> anyhow::Result<()> {
    let claims = state.coord.all_claims().await?;
    let nats = state.coord.nats_client();

    for (key, claim) in claims {
        // Jobs in NeedsHelp are intentionally paused — skip all checks.
        if let Ok(Some(job)) = state.graph.get_job(&key) {
            if job.state == JobState::NeedsHelp {
                tracing::debug!(
                    key,
                    worker_id = claim.worker_id,
                    "skipping — job is waiting for human input"
                );
                continue;
            }
        }

        // Two-tier check: lease expiry first (fast, requeue), then job deadline (slow, fail).
        if claim.is_lease_expired() {
            let elapsed = chrono::Utc::now()
                .signed_duration_since(claim.last_heartbeat)
                .num_seconds();

            tracing::warn!(
                key,
                worker_id = claim.worker_id,
                elapsed_since_heartbeat = elapsed,
                lease_secs = claim.lease_secs,
                "lease expired — publishing advisory event"
            );

            let event = LeaseExpiredEvent {
                job_key: key.clone(),
                worker_id: claim.worker_id.clone(),
                elapsed_secs: elapsed,
            };
            if let Ok(payload) = serde_json::to_vec(&event) {
                if let Err(e) = nats
                    .publish(SUBJECT_LEASE_EXPIRED, payload.into())
                    .await
                {
                    tracing::error!(key, "failed to publish lease-expired event: {e:#}");
                }
            }
        } else if claim.is_timed_out() {
            tracing::info!(
                key,
                worker_id = claim.worker_id,
                timeout_secs = claim.timeout_secs,
                "job deadline exceeded — publishing advisory event"
            );

            let event = JobTimeoutEvent {
                job_key: key.clone(),
                worker_id: claim.worker_id.clone(),
            };
            if let Ok(payload) = serde_json::to_vec(&event) {
                if let Err(e) = nats
                    .publish(SUBJECT_TIMEOUT, payload.into())
                    .await
                {
                    tracing::error!(key, "failed to publish timeout event: {e:#}");
                }
            }
        }
    }

    Ok(())
}

/// Detect jobs stuck as `on_the_stack` or `needs_help` with no active claim.
/// Instead of fixing them directly, publish an advisory event for the dispatcher.
async fn detect_orphaned_jobs(state: &Arc<AppState>) {
    let on_stack = state
        .graph
        .get_all_jobs(Some(&JobState::OnTheStack))
        .unwrap_or_default();
    let needs_help = state
        .graph
        .get_all_jobs(Some(&JobState::NeedsHelp))
        .unwrap_or_default();

    let nats = state.coord.nats_client();

    for job in on_stack.iter().chain(needs_help.iter()) {
        let key = job.key();
        let has_claim = state.coord.get_claim(&key).await.ok().flatten().is_some();
        if has_claim {
            continue;
        }

        tracing::warn!(
            key,
            state = ?job.state,
            "orphaned job detected — no active claim, publishing advisory event"
        );

        let event = OrphanDetectedEvent {
            job_key: key.clone(),
            previous_state: job.state.clone(),
        };
        if let Ok(payload) = serde_json::to_vec(&event) {
            if let Err(e) = nats.publish(SUBJECT_ORPHAN, payload.into()).await {
                tracing::error!(key, "failed to publish orphan event: {e:#}");
            }
        }
    }
}

/// Clean up session entries for jobs that are no longer on_the_stack/needs_help.
async fn cleanup_stale_sessions(state: &Arc<AppState>) {
    let sessions = match state.coord.list_sessions().await {
        Ok(s) => s,
        Err(_) => return,
    };
    for session in sessions {
        let nats_key = session.job_key.replace('/', ".");
        let job = state.graph.get_job(&session.job_key).ok().flatten();
        let is_active = job
            .as_ref()
            .map(|j| matches!(j.state, JobState::OnTheStack | JobState::NeedsHelp))
            .unwrap_or(false);
        if !is_active {
            tracing::debug!(
                job_key = session.job_key,
                "cleaning stale session"
            );
            let _ = state.coord.delete_session(&nats_key).await;
            events::emit(
                &state.event_tx,
                events::SseEvent::SessionRemoved {
                    job_key: session.job_key.clone(),
                },
            );
        }
    }
}

/// Remove workers from the registry that haven't been seen recently.
fn prune_stale_workers(state: &Arc<AppState>) {
    let cutoff = chrono::Utc::now()
        - chrono::Duration::seconds(state.config.worker_prune_secs as i64);

    let mut pruned = Vec::new();
    state.dispatch_registry.retain(|worker_id, info| {
        if info.last_seen < cutoff {
            pruned.push(worker_id.clone());
            false
        } else {
            true
        }
    });

    for worker_id in pruned {
        state.emit_worker_removed(&worker_id);
        tracing::info!(
            worker_id,
            prune_secs = state.config.worker_prune_secs,
            "pruned stale worker from registry"
        );
    }
}
