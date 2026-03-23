//! Consumes issue snapshots from the NATS JetStream stream published by the
//! CDC process and reconciles sidecar state.
//!
//! Each message is a full [`IssueSnapshot`] — a denormalized view of one issue
//! at a point in time. The consumer is idempotent: processing the same snapshot
//! twice produces the same result.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::PullConsumer, stream};
use futures::StreamExt;
use workflow_types::{
    parse_capabilities, parse_deps, parse_platform, parse_priority, parse_retries,
    parse_review_requirement, parse_timeout, parse_worker_type, ForgejoLabel, IssueSnapshot, Job,
    JobState, JobTransition,
};

use crate::AppState;

const STREAM_NAME: &str = "workflow-changes";
const SUBJECT: &str = "workflow.changes";
const CONSUMER_NAME: &str = "sidecar";

/// Start the background consumer task. Call once at startup.
pub async fn start(state: Arc<AppState>) -> Result<()> {
    let js = jetstream::new(state.coord.nats_client().clone());

    // Ensure the stream exists (CDC creates it too, but be defensive).
    let stream = js
        .get_or_create_stream(stream::Config {
            name: STREAM_NAME.into(),
            subjects: vec![SUBJECT.into()],
            retention: stream::RetentionPolicy::Limits,
            max_age: Duration::from_secs(7 * 24 * 3600),
            storage: stream::StorageType::File,
            ..Default::default()
        })
        .await
        .context("create/get CDC stream")?;

    // Durable pull consumer — survives sidecar restarts.
    let consumer: PullConsumer = stream
        .get_or_create_consumer(
            CONSUMER_NAME,
            jetstream::consumer::pull::Config {
                durable_name: Some(CONSUMER_NAME.into()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ..Default::default()
            },
        )
        .await
        .context("create/get CDC consumer")?;

    tokio::spawn(async move {
        tracing::info!("CDC consumer started");
        loop {
            if let Err(e) = consume_batch(&consumer, &state).await {
                tracing::error!(error = %e, "CDC consumer error, retrying in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    });

    Ok(())
}

async fn consume_batch(consumer: &PullConsumer, state: &Arc<AppState>) -> Result<()> {
    let mut messages = consumer.messages().await.context("pull messages")?;

    while let Some(msg) = messages.next().await {
        let msg = msg.context("receive message")?;
        let snap: IssueSnapshot =
            serde_json::from_slice(&msg.payload).context("parse IssueSnapshot")?;

        if let Err(e) = process_snapshot(state, &snap).await {
            tracing::error!(
                error = %e,
                repo = %snap.repo_owner,
                number = snap.number,
                "failed to process snapshot, nacking"
            );
            // Don't ack — will be redelivered.
            continue;
        }

        msg.ack().await.map_err(|e| anyhow::anyhow!("ack: {e}"))?;
    }

    Ok(())
}

/// Reconcile sidecar state from a full issue snapshot.
///
/// This replaces the webhook-based approach. Every snapshot carries the full
/// issue state, so we don't need to differentiate between opened/edited/closed
/// events — we just converge to the correct state.
async fn process_snapshot(state: &Arc<AppState>, snap: &IssueSnapshot) -> Result<()> {
    let owner = &snap.repo_owner;
    let repo = &snap.repo_name;
    let job_key = format!("{owner}/{repo}/{}", snap.number);

    tracing::debug!(
        key = %job_key,
        is_closed = snap.is_closed,
        labels = ?snap.labels,
        "processing snapshot"
    );

    // Build pseudo-labels for the parsing helpers.
    let labels: Vec<ForgejoLabel> = snap
        .labels
        .iter()
        .enumerate()
        .map(|(i, name)| ForgejoLabel {
            id: i as u64,
            name: name.clone(),
            color: String::new(),
        })
        .collect();

    let dep_numbers = parse_deps(&snap.body);
    let priority = parse_priority(&labels);
    let timeout_secs = parse_timeout(&labels);
    let capabilities = parse_capabilities(&labels);
    let max_retries = parse_retries(&labels);
    // worker_type, platform, review parsed below alongside Job construction

    let has_status_label = snap
        .labels
        .iter()
        .any(|l| JobState::from_label(l).is_some());
    let on_ice =
        snap.labels.iter().any(|l| l == "status:on-ice") || (!snap.is_closed && !has_status_label);

    // Determine what state this issue should be in.
    let has_done_label = snap.labels.iter().any(|l| l == "status:done");
    let target_state = if snap.is_closed {
        if has_done_label || snap.closed_by_merge {
            JobState::Done
        } else {
            JobState::Revoked
        }
    } else if on_ice {
        JobState::OnIce
    } else {
        // Check for an explicit status label from Forgejo.
        let explicit = snap.labels.iter().find_map(|l| JobState::from_label(l));

        // If claimed (on-the-stack) or terminal-ish, respect the label.
        // Otherwise compute from deps.
        match explicit {
            Some(
                s @ (JobState::OnTheStack
                | JobState::InReview
                | JobState::Failed
                | JobState::ChangesRequested),
            ) => s,
            _ => JobState::OnDeck, // corrected below after dep sync
        }
    };

    // Read the previous job BEFORE any writes so we can detect changes
    // and preserve accumulated activities.
    let previous_job = state.graph.get_job(&job_key)?;
    let previous_state = previous_job.as_ref().map(|j| j.state.clone());
    let existing_activities = previous_job
        .as_ref()
        .map(|j| j.activities.clone())
        .unwrap_or_default();

    let worker_type = parse_worker_type(&labels);
    let platform = parse_platform(&labels);
    let review = parse_review_requirement(&labels);

    let job = Job {
        repo_owner: owner.clone(),
        repo_name: repo.clone(),
        number: snap.number,
        title: snap.title.clone(),
        state: target_state.clone(),
        assignees: snap.assignees.clone(),
        dependency_numbers: dep_numbers.clone(),
        priority,
        timeout_secs,
        capabilities,
        max_retries,
        worker_type,
        platform,
        review,
        activities: existing_activities,
        retry_attempt: previous_job.as_ref().map(|j| j.retry_attempt).unwrap_or(0),
        is_rework: false,
    };

    // Upsert the job and sync dependency edges.
    state.graph.upsert_job(&job)?;

    let dep_keys: Vec<String> = dep_numbers
        .iter()
        .map(|n| format!("{owner}/{repo}/{n}"))
        .collect();
    let rejected = state.graph.sync_deps(&job_key, &dep_keys)?;

    if !rejected.is_empty() {
        let msg = format!(
            "⚠️ **Cycle detected** — deps ignored: {}",
            rejected.join(", ")
        );
        let _ = state
            .forgejo
            .post_comment(owner, repo, snap.number, &msg)
            .await;
    }

    // For closed issues: Done vs Revoked.
    if snap.is_closed {
        if target_state == JobState::Done {
            // Legitimate completion — add status:done label if not present
            if !has_done_label {
                state
                    .forgejo
                    .set_job_state(owner, repo, snap.number, &JobState::Done)
                    .await?;
            }
            state.graph.set_state(&job_key, &JobState::Done)?;
            if previous_state.as_ref() != Some(&JobState::Done) {
                let mut done_job = job.clone();
                done_job.state = JobState::Done;
                state
                    .publish_transition(&JobTransition {
                        job: done_job,
                        previous_state: previous_state.clone(),
                        new_state: JobState::Done,
                    })
                    .await;
            }
            propagate_unblock(state, &job_key).await?;
        } else {
            // Revoked — closed without merged PR or done label
            state.graph.set_state(&job_key, &JobState::Revoked)?;
            if previous_state.as_ref() != Some(&JobState::Revoked) {
                state
                    .forgejo
                    .set_job_state(owner, repo, snap.number, &JobState::Revoked)
                    .await?;
                let mut revoked_job = job.clone();
                revoked_job.state = JobState::Revoked;
                state
                    .publish_transition(&JobTransition {
                        job: revoked_job,
                        previous_state: previous_state.clone(),
                        new_state: JobState::Revoked,
                    })
                    .await;
            }
            // Dependents stay blocked — the dep was never Done so they
            // were never unblocked in the first place.
        }
        return Ok(());
    }

    // For non-claimed, non-terminal issues: re-evaluate blocked vs on-deck.
    // Skip if there's an active claim — the dispatcher owns the state once a
    // worker has claimed the job.  Without this, a CDC snapshot captured between
    // the dispatcher setting on-the-stack in the graph and the label update
    // landing on Forgejo causes the consumer to force the job back to on-deck,
    // triggering a thrashing loop.
    let has_active_claim = state.coord.get_claim(&job_key).await?.is_some();
    if !on_ice
        && !has_active_claim
        && !matches!(
            target_state,
            JobState::OnTheStack | JobState::InReview | JobState::Failed | JobState::ChangesRequested
        )
    {
        let resolved = if !dep_numbers.is_empty()
            && !state
                .graph
                .all_declared_deps_done(owner, repo, &dep_numbers)?
        {
            JobState::Blocked
        } else {
            JobState::OnDeck
        };

        state.graph.set_state(&job_key, &resolved)?;

        // Sync the label back to Forgejo if it doesn't match.
        let forgejo_state = snap.labels.iter().find_map(|l| JobState::from_label(l));
        if forgejo_state.as_ref() != Some(&resolved) {
            state
                .forgejo
                .set_job_state(owner, repo, snap.number, &resolved)
                .await?;
        }

        // Publish transition when state changed.
        if previous_state.as_ref() != Some(&resolved) {
            let mut resolved_job = job.clone();
            resolved_job.state = resolved.clone();
            state
                    .publish_transition(&JobTransition {
                    job: resolved_job,
                    previous_state,
                    new_state: resolved,
                })
                .await;
        }
    } else {
        // Claimed / terminal / on-ice — sync label if missing (e.g. issue created with no labels).
        let forgejo_state = snap.labels.iter().find_map(|l| JobState::from_label(l));
        if forgejo_state.as_ref() != Some(&target_state) {
            state
                .forgejo
                .set_job_state(owner, repo, snap.number, &target_state)
                .await?;
        }

        if previous_state.as_ref() != Some(&target_state) {
            state
                    .publish_transition(&JobTransition {
                    job: job.clone(),
                    previous_state,
                    new_state: target_state,
                })
                .await;
        }
    }

    Ok(())
}

/// Walk reverse deps and unblock any that are now fully resolved.
async fn propagate_unblock(state: &Arc<AppState>, closed_key: &str) -> Result<()> {
    let dependents = state.graph.get_dependents(closed_key)?;
    for dep_key in dependents {
        if let Some(dep_job) = state.graph.get_job(&dep_key)? {
            let all_done = state.graph.all_declared_deps_done(
                &dep_job.repo_owner,
                &dep_job.repo_name,
                &dep_job.dependency_numbers,
            )?;
            if all_done {
                if dep_job.state == JobState::Blocked {
                    state.graph.set_state(&dep_key, &JobState::OnDeck)?;
                    state
                        .forgejo
                        .set_job_state(
                            &dep_job.repo_owner,
                            &dep_job.repo_name,
                            dep_job.number,
                            &JobState::OnDeck,
                        )
                        .await?;

                    let mut unblocked = dep_job.clone();
                    unblocked.state = JobState::OnDeck;
                    state
                    .publish_transition(&JobTransition {
                            job: unblocked,
                            previous_state: Some(JobState::Blocked),
                            new_state: JobState::OnDeck,
                        })
                        .await;
                }
            }
        }
    }
    Ok(())
}
