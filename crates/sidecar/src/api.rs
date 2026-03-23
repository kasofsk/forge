use axum::{
    extract::{Path, Query, State},
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures::stream::Stream;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use workflow_types::{
    CreateIssueRequest, CreateIssueResponse, DepsResponse, FactoryListResponse, Job,
    JobListResponse, JobResponse, JobState, LabelListResponse, UserListResponse,
};

use crate::error::AppError;
use crate::events::SseEvent;

use crate::AppState;

type Result<T> = std::result::Result<T, AppError>;

// ── Job discovery ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct JobListQuery {
    pub state: Option<String>,
}

pub async fn list_jobs(
    State(state): State<Arc<AppState>>,
    Query(params): Query<JobListQuery>,
) -> Result<Json<JobListResponse>> {
    let state_filter = params
        .state
        .as_deref()
        .and_then(JobState::from_label)
        .or_else(|| {
            params.state.as_deref().and_then(|s| {
                // Also accept bare names like "on-deck"
                JobState::from_label(&format!("status:{s}"))
            })
        });

    let jobs = state.graph.get_all_jobs(state_filter.as_ref())?;
    Ok(Json(JobListResponse { jobs }))
}

pub async fn get_job(
    State(s): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
) -> Result<Json<JobResponse>> {
    let key = format!("{owner}/{repo}/{number}");
    let job = s.graph.get_job(&key)?.ok_or(AppError::NotFound)?;
    let claim = s.coord.get_claim(&key).await?;
    Ok(Json(JobResponse {
        job,
        claim,
        failure: None,
    }))
}

pub async fn get_deps(
    State(s): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
) -> Result<Json<DepsResponse>> {
    let key = format!("{owner}/{repo}/{number}");
    let job = s.graph.get_job(&key)?.ok_or(AppError::NotFound)?;

    let mut dependencies: Vec<Job> = Vec::new();
    for dep_num in &job.dependency_numbers {
        let dep_key = format!("{owner}/{repo}/{dep_num}");
        if let Some(dep) = s.graph.get_job(&dep_key)? {
            dependencies.push(dep);
        }
    }

    let all_done = dependencies.iter().all(|j| j.state.is_terminal());
    Ok(Json(DepsResponse {
        dependencies,
        all_done,
    }))
}

// ── Available jobs ────────────────────────────────────────────────────────────

/// Return all on-deck jobs sorted by priority descending. Used by the
/// integration tests to discover claimable jobs and by the CLI `jobs` command.
pub async fn available_jobs(State(s): State<Arc<AppState>>) -> Result<Json<JobListResponse>> {
    let mut jobs = s.graph.get_all_jobs(Some(&JobState::OnDeck))?;
    jobs.sort_by(|a, b| b.priority.cmp(&a.priority));
    Ok(Json(JobListResponse { jobs }))
}

// ── Factory endpoints ─────────────────────────────────────────────────────────

pub async fn list_factories(State(s): State<Arc<AppState>>) -> Result<Json<FactoryListResponse>> {
    let factories = s.registry.list_factories().await;
    Ok(Json(FactoryListResponse { factories }))
}

// ── Users ────────────────────────────────────────────────────────────────────

pub async fn list_users(
    State(s): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<UserListResponse>> {
    let users = s.forgejo.get_repo_collaborators(&owner, &repo).await?;
    Ok(Json(UserListResponse { users }))
}

// ── Labels ───────────────────────────────────────────────────────────────────

pub async fn list_labels(
    State(s): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<LabelListResponse>> {
    let labels = s.forgejo.list_labels(&owner, &repo).await?;
    Ok(Json(LabelListResponse { labels }))
}

// ── Issue creation ───────────────────────────────────────────────────────────

pub async fn create_issue(
    State(s): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<CreateIssueRequest>,
) -> Result<Json<CreateIssueResponse>> {
    let number = s
        .forgejo
        .create_issue(&owner, &repo, &req.title, &req.body, &req.labels)
        .await?;
    Ok(Json(CreateIssueResponse { number }))
}

// ── Interactive session control ───────────────────────────────────────────

#[derive(Deserialize)]
pub struct AttachRequest {
    pub worker_id: String,
}

pub async fn attach_session(
    State(s): State<Arc<AppState>>,
    Json(req): Json<AttachRequest>,
) -> Result<Json<serde_json::Value>> {
    publish_interact(&s, &format!("workflow.interact.attach.{}", req.worker_id)).await
}

pub async fn detach_session(
    State(s): State<Arc<AppState>>,
    Json(req): Json<AttachRequest>,
) -> Result<Json<serde_json::Value>> {
    publish_interact(&s, &format!("workflow.interact.detach.{}", req.worker_id)).await
}

async fn publish_interact(s: &Arc<AppState>, subject: &str) -> Result<Json<serde_json::Value>> {
    s.coord
        .nats_client()
        .publish(subject.to_string(), bytes::Bytes::new())
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("{e:#}")))?;
    s.coord
        .nats_client()
        .flush()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("{e:#}")))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── SSE event stream ─────────────────────────────────────────────────────────

/// Build a full state snapshot for the initial SSE event.
/// Caps activities per job to keep the payload manageable.
async fn build_snapshot(s: &Arc<AppState>) -> serde_json::Value {
    const MAX_ACTIVITIES_PER_JOB: usize = 20;
    let mut jobs = s.graph.get_all_jobs(None).unwrap_or_default();
    for job in &mut jobs {
        if job.activities.len() > MAX_ACTIVITIES_PER_JOB {
            let len = job.activities.len();
            job.activities = job.activities.split_off(len - MAX_ACTIVITIES_PER_JOB);
        }
    }
    let workers: Vec<_> = s
        .dispatch_registry
        .iter()
        .map(|e| e.value().clone())
        .collect();
    let journal = s.coord.list_journal(200).await;
    let sessions = s.coord.list_sessions().await.unwrap_or_default();

    serde_json::json!({
        "jobs": jobs,
        "workers": workers,
        "journal": journal,
        "sessions": sessions,
    })
}

/// SSE endpoint: sends a full snapshot, then streams incremental events.
/// On lag (receiver fell behind), resends a full snapshot.
pub async fn sse_events(
    State(s): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = std::result::Result<Event, std::convert::Infallible>>> {
    // Subscribe BEFORE reading snapshot to avoid missing events.
    let rx = s.event_tx.subscribe();
    let snapshot = build_snapshot(&s).await;

    let stream = futures::stream::unfold(
        (rx, s, Some(snapshot)),
        |(mut rx, state, initial_snapshot)| async move {
            // Send initial snapshot first.
            if let Some(snap) = initial_snapshot {
                let event = Event::default()
                    .event("snapshot")
                    .data(snap.to_string());
                return Some((Ok(event), (rx, state, None)));
            }

            // Then stream incremental events.
            loop {
                match rx.recv().await {
                    Ok(sse_event) => {
                        let event_type = match &sse_event {
                            SseEvent::JobUpdate { .. } => "job_update",
                            SseEvent::WorkerUpdate { .. } => "worker_update",
                            SseEvent::WorkerRemoved { .. } => "worker_removed",
                            SseEvent::JournalEntry { .. } => "journal_entry",
                            SseEvent::SessionUpdate { .. } => "session_update",
                            SseEvent::SessionRemoved { .. } => "session_removed",
                        };
                        if let Ok(json) = serde_json::to_string(&sse_event) {
                            let event = Event::default().event(event_type).data(json);
                            return Some((Ok(event), (rx, state, None)));
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        // Receiver fell behind — resend full snapshot.
                        let snap = build_snapshot(&state).await;
                        let event = Event::default()
                            .event("snapshot")
                            .data(snap.to_string());
                        return Some((Ok(event), (rx, state, None)));
                    }
                    Err(RecvError::Closed) => return None,
                }
            }
        },
    );

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── Session recordings ───────────────────────────────────────────────────────

pub async fn get_session_recording(
    State(s): State<Arc<AppState>>,
    Path((owner, repo, number, session_id)): Path<(String, String, u64, String)>,
) -> Result<axum::response::Response> {
    let key = format!("sessions/{owner}/{repo}/{number}/{session_id}");
    match s.coord.get_recording(&key).await? {
        Some(data) => Ok(axum::response::Response::builder()
            .header("content-type", "text/plain; charset=utf-8")
            .body(axum::body::Body::from(data))
            .unwrap()),
        None => Err(AppError::NotFound),
    }
}

