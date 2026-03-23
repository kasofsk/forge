//! Standalone reviewer process: automated PR review triggered by InReview transitions.
//!
//! Subscribes to `workflow.jobs.transition` and reacts when a job moves to
//! InReview. Finds the linked PR, then dispatches a Claude-based review action
//! (Forgejo Actions workflow) to evaluate the changes.
//!
//! Communication with the sidecar is fire-and-forget via NATS:
//! - `workflow.activity.append` — record review activities in the graph
//! - `workflow.journal.append` — record journal entries
//!
//! This is NOT a worker — it doesn't claim jobs. It acts in a supervisory
//! capacity, similar to the dispatcher.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_nats::jetstream::{self as jetstream, kv};
use dashmap::DashSet;
use futures::StreamExt;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use workflow_types::{
    ActivityAppend, ActivityEntry, ActivityStatus, Job, JobState, JobTransition, JournalAppend,
    ReviewRequirement,
};
use workflow_worker::forgejo::{ForgejoClient, PullRequest};
use workflow_worker::SidecarClient;

const SUBJECT_TRANSITION: &str = "workflow.jobs.transition";
const SUBJECT_ACTIVITY_APPEND: &str = "workflow.activity.append";
const SUBJECT_JOURNAL_APPEND: &str = "workflow.journal.append";
const MAX_POLL_ERRORS: u32 = 5;
const MERGE_QUEUE_BUCKET: &str = "workflow-merge-queue";

/// An entry in the per-repo merge queue.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MergeQueueEntry {
    owner: String,
    repo: String,
    issue_number: u64,
    pr_number: u64,
    assignees: Vec<String>,
}

/// Decision returned by the review action, derived from PR review state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReviewDecision {
    Approve,
    RequestChanges,
    Escalate,
}

const REWORK_COUNT_BUCKET: &str = "workflow-rework-counts";
const MAX_REWORK_CYCLES: u32 = 3;

/// Configuration parsed from environment variables.
struct ReviewerConfig {
    nats_url: String,
    forgejo_url: String,
    forgejo_token: String,
    human_login: String,
    delay_secs: u64,
    review_workflow: String,
    review_runner: String,
    review_timeout_secs: u64,
    review_poll_secs: u64,
}

impl ReviewerConfig {
    fn from_env() -> Result<Self> {
        Ok(Self {
            nats_url: std::env::var("NATS_URL")
                .unwrap_or_else(|_| "nats://localhost:4222".to_string()),
            forgejo_url: std::env::var("FORGEJO_URL").context("FORGEJO_URL required")?,
            forgejo_token: std::env::var("REVIEWER_FORGEJO_TOKEN")
                .context("REVIEWER_FORGEJO_TOKEN required")?,
            human_login: std::env::var("REVIEWER_HUMAN_LOGIN")
                .unwrap_or_else(|_| "you".to_string()),
            delay_secs: env_u64("REVIEWER_DELAY_SECS", 3),
            review_workflow: std::env::var("REVIEWER_WORKFLOW")
                .unwrap_or_else(|_| "review-work.yml".to_string()),
            review_runner: std::env::var("REVIEWER_RUNNER")
                .unwrap_or_else(|_| "ubuntu-latest".to_string()),
            review_timeout_secs: env_u64("REVIEWER_ACTION_TIMEOUT_SECS", 600),
            review_poll_secs: env_u64("REVIEWER_POLL_SECS", 10),
        })
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Helper to extract PR number from the generated model.
fn pr_num(pr: &PullRequest) -> u64 {
    pr.number.unwrap_or(0) as u64
}

struct Reviewer {
    nats: async_nats::Client,
    forgejo: ForgejoClient,
    human_login: String,
    delay_secs: u64,
    review_workflow: String,
    review_runner: String,
    review_timeout_secs: u64,
    review_poll_secs: u64,
    /// Job keys currently being reviewed — prevents double-handling from
    /// duplicate InReview transitions (CDC path + dispatcher path).
    in_flight: DashSet<String>,
    /// Per-repo merge queue: serializes merges to avoid conflicts.
    kv_merge_queue: kv::Store,
    /// Per-job rework counter: tracks how many times a job has been sent back
    /// for changes (both conflict-rework and review-rework). Escalates to
    /// human after MAX_REWORK_CYCLES to prevent infinite loops.
    kv_rework_counts: kv::Store,
}

impl Reviewer {
    async fn new(nats: async_nats::Client, forgejo: ForgejoClient, config: &ReviewerConfig) -> Result<Self> {
        let js = jetstream::new(nats.clone());
        let kv_merge_queue = js
            .create_key_value(kv::Config {
                bucket: MERGE_QUEUE_BUCKET.to_string(),
                history: 1,
                ..Default::default()
            })
            .await
            .context("create merge-queue KV bucket")?;

        let kv_rework_counts = js
            .create_key_value(kv::Config {
                bucket: REWORK_COUNT_BUCKET.to_string(),
                history: 1,
                ..Default::default()
            })
            .await
            .context("create rework-counts KV bucket")?;

        Ok(Self {
            nats,
            forgejo,
            human_login: config.human_login.clone(),
            delay_secs: config.delay_secs,
            review_workflow: config.review_workflow.clone(),
            review_runner: config.review_runner.clone(),
            review_timeout_secs: config.review_timeout_secs,
            review_poll_secs: config.review_poll_secs,
            in_flight: DashSet::new(),
            kv_merge_queue,
            kv_rework_counts,
        })
    }

    /// Reconcile InReview jobs on startup by querying the sidecar API.
    async fn reconcile_on_startup(self: &Arc<Self>, sidecar_url: &str) {
        let sidecar = SidecarClient::new(sidecar_url);
        let jobs = match sidecar.list_jobs(Some("status:in-review")).await {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("reviewer: startup reconciliation failed: {e:#}");
                return;
            }
        };

        if jobs.is_empty() {
            tracing::info!("reviewer: no InReview jobs found on startup");
            return;
        }

        tracing::info!(
            count = jobs.len(),
            "reviewer: reconciling InReview jobs from startup"
        );

        for job in &jobs {
            let job_key = job.key();
            if !self.in_flight.insert(job_key.clone()) {
                continue;
            }

            let transition = JobTransition {
                job: job.clone(),
                previous_state: None,
                new_state: JobState::InReview,
            };

            let reviewer = Arc::clone(self);
            let key = job_key.clone();
            tokio::spawn(async move {
                if let Err(e) = reviewer.handle_in_review(&transition).await {
                    tracing::error!(
                        job = %key,
                        error = %e,
                        "reviewer: failed to reconcile in-review job"
                    );
                }
                reviewer.in_flight.remove(&key);
            });
        }
    }

    /// Start the background NATS subscription.
    async fn start(self: Arc<Self>) -> Result<()> {
        let mut sub = self
            .nats
            .subscribe(String::from(SUBJECT_TRANSITION))
            .await
            .context("reviewer: subscribe to jobs.transition")?;

        let reviewer = Arc::clone(&self);
        tokio::spawn(async move {
            tracing::info!("reviewer started");
            while let Some(msg) = sub.next().await {
                let transition: JobTransition = match serde_json::from_slice(&msg.payload) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("reviewer: bad transition payload: {e:#}");
                        continue;
                    }
                };

                if transition.new_state != JobState::InReview {
                    continue;
                }

                let job_key = transition.job.key();
                if !reviewer.in_flight.insert(job_key.clone()) {
                    tracing::debug!(job_key = %job_key, "reviewer: already handling, skipping duplicate");
                    continue;
                }

                let reviewer = Arc::clone(&reviewer);
                tokio::spawn(async move {
                    let key = transition.job.key();
                    if let Err(e) = reviewer.handle_in_review(&transition).await {
                        tracing::error!(
                            job = %key,
                            error = %e,
                            "reviewer: failed to handle in-review"
                        );
                    }
                    reviewer.in_flight.remove(&key);
                });
            }
        });

        Ok(())
    }

    async fn handle_in_review(&self, transition: &JobTransition) -> Result<()> {
        let job = &transition.job;
        let owner = &job.repo_owner;
        let repo = &job.repo_name;
        let job_key = job.key();

        // Delay to avoid racing with PR creation.
        if self.delay_secs > 0 {
            tokio::time::sleep(Duration::from_secs(self.delay_secs)).await;
        }

        // Find the linked PR: search open PRs for "Closes #N".
        let pr = self.find_linked_pr(owner, repo, job.number).await?;
        let pr = match pr {
            Some(pr) => pr,
            None => {
                tracing::debug!(
                    job_key = %job_key,
                    "reviewer: no linked PR found, skipping"
                );
                return Ok(());
            }
        };


        tracing::info!(
            job_key = %job_key,
            pr_number = pr_num(&pr),
            review = ?job.review,
            "reviewer: reviewing PR #{}", pr_num(&pr)
        );

        // If the PR was already approved (previous review cycle approved but
        // merge failed due to conflicts), skip the review action entirely and
        // go straight to the merge queue. Skip the mergeability check too —
        // Forgejo's `mergeable` field can be stale after a force-push.
        let pr_number = pr_num(&pr);
        let already_approved = self.pr_has_approval(owner, repo, pr_number).await;

        if already_approved {
            tracing::info!(
                job_key = %job_key,
                pr_number,
                "reviewer: PR already approved, sending to merge queue"
            );
            self.finalize_review_activity(&job_key, "approve", ActivityStatus::Success).await;
            return self.approve_and_merge(owner, repo, job.number, &pr, &job.assignees).await;
        }

        // Preemptive mergeability check for PRs that haven't been approved yet.
        if let Ok(fresh_pr) = self.forgejo.get_pr(owner, repo, pr_number).await {
            if fresh_pr.mergeable == Some(false) {
                let rework_count = self.get_rework_count(&job_key).await;

                if rework_count >= MAX_REWORK_CYCLES {
                    tracing::info!(
                        job_key = %job_key,
                        pr_number,
                        rework_count,
                        "reviewer: rework cycle limit reached, escalating to human"
                    );
                    self.finalize_review_activity(&job_key, "escalate", ActivityStatus::Escalated).await;
                    return self.escalate_to_human(owner, repo, job.number, &pr, job.review).await;
                }

                tracing::info!(
                    job_key = %job_key,
                    pr_number,
                    rework_count,
                    "reviewer: PR has merge conflicts, requesting rebase"
                );
                self.forgejo
                    .post_comment(
                        owner,
                        repo,
                        pr_number,
                        "## Merge Conflict\n\n\
                         This PR has conflicts with the base branch and cannot be reviewed until they are resolved.\n\n\
                         Please rebase your branch on `main` and resolve any conflicts:\n\
                         ```\ngit fetch origin\ngit rebase origin/main\n# resolve conflicts\ngit push --force\n```",
                    )
                    .await?;
                self.finalize_review_activity(
                    &job_key,
                    "request_changes",
                    ActivityStatus::ChangesRequested,
                )
                .await;
                self.increment_rework_count(&job_key).await;
                return self
                    .request_rework(owner, repo, job.number, &pr, &job.assignees)
                    .await;
            }
        }

        match job.review {
            ReviewRequirement::Human => {
                tracing::info!(job_key = %job_key, "reviewer: human review required by label");
                self.escalate_to_human(owner, repo, job.number, &pr, job.review)
                    .await?;
            }
            ReviewRequirement::Low | ReviewRequirement::Medium | ReviewRequirement::High => {
                let decision = self
                    .review_via_action(owner, repo, job.number, &pr, job.review)
                    .await;

                // Check rework counter to prevent infinite loops.
                let rework_count = self.get_rework_count(&job_key).await;
                let decision = match decision {
                    Ok(ReviewDecision::RequestChanges) if rework_count >= MAX_REWORK_CYCLES => {
                        tracing::info!(
                            job_key = %job_key,
                            rework_count,
                            "reviewer: rework cycle limit reached, escalating to human"
                        );
                        Ok(ReviewDecision::Escalate)
                    }
                    other => other,
                };

                match decision {
                    Ok(ReviewDecision::Approve) => {
                        self.finalize_review_activity(
                            &job_key,
                            "approve",
                            ActivityStatus::Success,
                        )
                        .await;
                        self.reset_rework_count(&job_key).await;
                        self.approve_and_merge(owner, repo, job.number, &pr, &job.assignees)
                            .await?;
                    }
                    Ok(ReviewDecision::RequestChanges) => {
                        self.finalize_review_activity(
                            &job_key,
                            "request_changes",
                            ActivityStatus::ChangesRequested,
                        )
                        .await;
                        self.increment_rework_count(&job_key).await;
                        self.request_rework(owner, repo, job.number, &pr, &job.assignees).await?;
                    }
                    Ok(ReviewDecision::Escalate) => {
                        self.finalize_review_activity(
                            &job_key,
                            "escalate",
                            ActivityStatus::Escalated,
                        )
                        .await;
                        self.escalate_to_human(owner, repo, job.number, &pr, job.review)
                            .await?;
                    }
                    Err(e) => {
                        tracing::warn!(
                            job_key = %job_key,
                            error = %e,
                            "reviewer: review action failed, escalating to human"
                        );
                        self.finalize_review_activity(
                            &job_key,
                            "escalate",
                            ActivityStatus::Failure,
                        )
                        .await;
                        self.escalate_to_human(owner, repo, job.number, &pr, job.review)
                            .await?;
                    }
                }
            }
        }

        Ok(())
    }

    // ── NATS fire-and-forget helpers ──────────────────────────────────────

    /// Publish an activity entry to the sidecar via NATS.
    async fn publish_activity(&self, job_key: &str, entry: ActivityEntry) {
        let append = ActivityAppend {
            job_key: job_key.to_string(),
            entry,
        };
        if let Ok(payload) = serde_json::to_vec(&append) {
            if let Err(e) = self
                .nats
                .publish(String::from(SUBJECT_ACTIVITY_APPEND), payload.into())
                .await
            {
                tracing::warn!(job_key = %job_key, error = %e, "failed to publish activity append");
            }
        }
    }

    /// Publish a journal entry to the sidecar via NATS.
    async fn journal(&self, action: &str, comment: &str, job_key: Option<&str>, worker_id: Option<&str>) {
        let append = JournalAppend {
            action: action.to_string(),
            comment: comment.to_string(),
            job_key: job_key.map(|s| s.to_string()),
            worker_id: worker_id.map(|s| s.to_string()),
        };
        if let Ok(payload) = serde_json::to_vec(&append) {
            if let Err(e) = self
                .nats
                .publish(String::from(SUBJECT_JOURNAL_APPEND), payload.into())
                .await
            {
                tracing::warn!(action, error = %e, "failed to publish journal append");
            }
        }
    }

    // ── Action-based review ──────────────────────────────────────────────

    /// Dispatch the review workflow, poll until complete, parse the decision.
    async fn review_via_action(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        pr: &PullRequest,
        requirement: ReviewRequirement,
    ) -> Result<ReviewDecision> {
        let job_key = format!("{owner}/{repo}/{issue_number}");
        let pr_number = pr_num(pr);

        let req_str = match requirement {
            ReviewRequirement::Low => "low",
            ReviewRequirement::Medium => "medium",
            ReviewRequirement::High => "high",
            ReviewRequirement::Human => "high", // unreachable, but safe default
        };

        let mut inputs = std::collections::HashMap::new();
        inputs.insert("issue_number".to_string(), issue_number.to_string());
        inputs.insert("pr_number".to_string(), pr_number.to_string());
        inputs.insert("review_requirement".to_string(), req_str.to_string());
        inputs.insert("runner_label".to_string(), self.review_runner.clone());
        inputs.insert(
            "forgejo_url".to_string(),
            std::env::var("FORGEJO_URL").unwrap_or_default(),
        );

        // Dispatch the review workflow on the PR's head branch.
        let head_ref = pr
            .head
            .as_ref()
            .and_then(|h| h.r#ref.clone())
            .unwrap_or_else(|| "main".to_string());

        let run_id = match self
            .forgejo
            .dispatch_workflow(owner, repo, &self.review_workflow, &head_ref, &inputs)
            .await?
        {
            Some(id) => id,
            None => bail!("dispatch_workflow returned no run ID for review of {job_key}"),
        };

        tracing::info!(
            job_key = %job_key,
            run_id,
            workflow = %self.review_workflow,
            "reviewer: dispatched review action"
        );

        self.journal(
            "review-dispatch",
            &format!("Dispatched review action (run {run_id}) for PR #{pr_number}"),
            Some(&job_key),
            Some("workflow-reviewer"),
        )
        .await;

        // Record review activity start.
        let review_id = format!("review-{run_id}");
        self.publish_activity(
            &job_key,
            ActivityEntry::Review {
                review_id: review_id.clone(),
                status: ActivityStatus::Running,
                started_at: chrono::Utc::now(),
                ended_at: None,
                decision: None,
                run_id: Some(run_id),
            },
        )
        .await;

        // Poll until the action run completes.
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(self.review_timeout_secs);
        let mut consecutive_errors: u32 = 0;

        loop {
            if tokio::time::Instant::now() > deadline {
                bail!(
                    "review action run {run_id} timed out after {}s",
                    self.review_timeout_secs
                );
            }

            tokio::time::sleep(Duration::from_secs(self.review_poll_secs)).await;

            let run = match self.forgejo.get_action_run(owner, repo, run_id).await {
                Ok(run) => {
                    consecutive_errors = 0;
                    run
                }
                Err(e) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_POLL_ERRORS {
                        bail!("review action run {run_id}: {MAX_POLL_ERRORS} consecutive poll errors, last: {e:#}");
                    }
                    tracing::warn!(
                        job_key = %job_key,
                        run_id,
                        error = %e,
                        attempt = consecutive_errors,
                        "reviewer: transient poll error, retrying"
                    );
                    continue;
                }
            };

            if run.is_completed() {
                if !run.is_success() {
                    bail!(
                        "review action run {run_id} failed with status={}",
                        run.status
                    );
                }

                tracing::info!(
                    job_key = %job_key,
                    run_id,
                    "reviewer: review action completed successfully"
                );
                break;
            }

            tracing::debug!(
                job_key = %job_key,
                run_id,
                status = %run.status,
                "reviewer: review action still running"
            );
        }

        // Read the decision from the PR review state (APPROVED/REQUEST_CHANGES/COMMENT).
        // The review action submits a proper PR review — no need to parse comment markers.
        self.read_latest_review_state(owner, repo, pr_number).await
    }

    /// Read the most recent PR review state to determine the decision.
    /// Uses the Forgejo API `state` field directly instead of parsing
    /// comment markers, which avoids stale-marker pollution.
    async fn read_latest_review_state(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
    ) -> Result<ReviewDecision> {
        let url = format!(
            "{}/api/v1/repos/{owner}/{repo}/pulls/{pr_number}/reviews",
            self.forgejo.base_url()
        );
        let reviews: Vec<serde_json::Value> = self.forgejo.get_json(&url).await
            .context("failed to fetch PR reviews")?;

        // Take the most recent non-dismissed review.
        for review in reviews.iter().rev() {
            let dismissed = review.get("dismissed").and_then(|d| d.as_bool()).unwrap_or(false);
            if dismissed {
                continue;
            }
            match review.get("state").and_then(|s| s.as_str()) {
                Some("APPROVED") => return Ok(ReviewDecision::Approve),
                Some("REQUEST_CHANGES") => return Ok(ReviewDecision::RequestChanges),
                Some("COMMENT") => return Ok(ReviewDecision::Escalate),
                _ => continue,
            }
        }

        tracing::warn!(
            "reviewer: no PR review found for PR #{pr_number}, defaulting to escalate"
        );
        Ok(ReviewDecision::Escalate)
    }

    // ── Rework counter (NATS KV) ─────────────────────────────────────────

    /// Get the current rework count for a job. Returns 0 if no entry exists.
    async fn get_rework_count(&self, job_key: &str) -> u32 {
        // NATS KV keys can't contain '/', use '.' as separator.
        let key = job_key.replace('/', ".");
        match self.kv_rework_counts.get(&key).await {
            Ok(Some(bytes)) => {
                std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0)
            }
            _ => 0,
        }
    }

    /// Increment the rework count for a job.
    async fn increment_rework_count(&self, job_key: &str) {
        let key = job_key.replace('/', ".");
        let count = self.get_rework_count(job_key).await + 1;
        if let Err(e) = self.kv_rework_counts.put(&key, count.to_string().into()).await {
            tracing::warn!(job_key, error = %e, "failed to increment rework count");
        }
    }

    /// Reset the rework count (e.g. on approval).
    async fn reset_rework_count(&self, job_key: &str) {
        let key = job_key.replace('/', ".");
        if let Err(e) = self.kv_rework_counts.delete(&key).await {
            tracing::warn!(job_key, error = %e, "failed to reset rework count");
        }
    }

    /// Check if the PR has a non-dismissed APPROVED review.
    async fn pr_has_approval(&self, owner: &str, repo: &str, pr_number: u64) -> bool {
        let url = format!(
            "{}/api/v1/repos/{owner}/{repo}/pulls/{pr_number}/reviews",
            self.forgejo.base_url()
        );
        let reviews: Vec<serde_json::Value> = match self.forgejo.get_json(&url).await {
            Ok(r) => r,
            Err(_) => return false,
        };
        reviews.iter().any(|r| {
            r.get("state").and_then(|s| s.as_str()) == Some("APPROVED")
                && r.get("dismissed").and_then(|d| d.as_bool()) != Some(true)
        })
    }

    // ── Outcome actions ──────────────────────────────────────────────────

    /// Find an open PR whose body contains "Closes #N".
    async fn find_linked_pr(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
    ) -> Result<Option<PullRequest>> {
        let prs = self.forgejo.list_prs(owner, repo, "open").await?;
        let pattern = format!("Closes #{issue_number}");

        Ok(prs.into_iter().find(|pr| {
            pr.body
                .as_deref()
                .map(|b| b.contains(&pattern))
                .unwrap_or(false)
        }))
    }

    /// Enqueue a PR for merge via the per-repo merge queue.
    /// Acquires a lock so only one PR merges at a time per repo.
    async fn approve_and_merge(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        pr: &PullRequest,
        assignees: &[String],
    ) -> Result<()> {
        let entry = MergeQueueEntry {
            owner: owner.to_string(),
            repo: repo.to_string(),
            issue_number,
            pr_number: pr_num(pr),
            assignees: assignees.to_vec(),
        };

        let lock_key = format!("{}.{}.lock", owner, repo);

        if self.try_acquire_merge_lock(&lock_key).await {
            tracing::info!(
                owner, repo, pr_number = pr_num(pr),
                "merge queue: lock acquired, merging"
            );
            self.do_merge(&entry).await;
            self.release_merge_lock(&lock_key).await;
            self.process_merge_queue(owner, repo).await;
        } else {
            self.push_merge_queue(owner, repo, &entry).await;
            tracing::info!(
                owner, repo, pr_number = pr_num(pr),
                "merge queue: lock held, queued for later"
            );
            self.journal(
                "merge-queued",
                &format!("PR #{} queued for merge (another merge in progress)", pr_num(pr)),
                Some(&format!("{owner}/{repo}/{issue_number}")),
                Some("workflow-reviewer"),
            )
            .await;
        }

        Ok(())
    }

    // ── Merge queue internals ────────────────────────────────────────────

    async fn try_acquire_merge_lock(&self, lock_key: &str) -> bool {
        match self.kv_merge_queue.entry(lock_key).await {
            Ok(None) => self.kv_merge_queue.create(lock_key, "1".into()).await.is_ok(),
            Ok(Some(entry))
                if entry.operation == kv::Operation::Delete
                    || entry.operation == kv::Operation::Purge =>
            {
                self.kv_merge_queue.update(lock_key, "1".into(), entry.revision).await.is_ok()
            }
            _ => false,
        }
    }

    async fn release_merge_lock(&self, lock_key: &str) {
        if let Err(e) = self.kv_merge_queue.delete(lock_key).await {
            tracing::error!(lock_key, error = %e, "failed to release merge lock");
        }
    }

    async fn push_merge_queue(&self, owner: &str, repo: &str, entry: &MergeQueueEntry) {
        let queue_key = format!("{owner}.{repo}.queue");
        let mut queue = self.load_merge_queue(&queue_key).await;
        if !queue.iter().any(|e| e.pr_number == entry.pr_number) {
            queue.push(entry.clone());
        }
        if let Ok(payload) = serde_json::to_vec(&queue) {
            let _ = self.kv_merge_queue.put(&queue_key, payload.into()).await;
        }
    }

    async fn pop_merge_queue(&self, owner: &str, repo: &str) -> Option<MergeQueueEntry> {
        let queue_key = format!("{owner}.{repo}.queue");
        let mut queue = self.load_merge_queue(&queue_key).await;
        if queue.is_empty() {
            return None;
        }
        let entry = queue.remove(0);
        if let Ok(payload) = serde_json::to_vec(&queue) {
            let _ = self.kv_merge_queue.put(&queue_key, payload.into()).await;
        }
        Some(entry)
    }

    async fn load_merge_queue(&self, queue_key: &str) -> Vec<MergeQueueEntry> {
        match self.kv_merge_queue.get(queue_key).await {
            Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
            _ => vec![],
        }
    }

    async fn process_merge_queue(&self, owner: &str, repo: &str) {
        let lock_key = format!("{owner}.{repo}.lock");
        while let Some(entry) = self.pop_merge_queue(owner, repo).await {
            if !self.try_acquire_merge_lock(&lock_key).await {
                self.push_merge_queue(owner, repo, &entry).await;
                return;
            }
            tracing::info!(owner, repo, pr_number = entry.pr_number, "merge queue: processing queued entry");
            self.do_merge(&entry).await;
            self.release_merge_lock(&lock_key).await;
        }
    }

    /// Execute a single merge: rebase onto main then fast-forward.
    async fn do_merge(&self, entry: &MergeQueueEntry) {
        let owner = &entry.owner;
        let repo = &entry.repo;
        let pr_number = entry.pr_number;
        let job_key = format!("{owner}/{repo}/{}", entry.issue_number);

        let mut backoff = Duration::from_millis(500);
        let max_attempts = 5;
        let mut merged = false;

        for attempt in 1..=max_attempts {
            match self.forgejo.merge_pr(owner, repo, pr_number, "rebase").await {
                Ok(()) => {
                    merged = true;
                    break;
                }
                Err(e) if attempt < max_attempts => {
                    tracing::debug!(
                        pr_number, attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        error = %e, "merge queue: not ready, retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                }
                Err(e) => {
                    tracing::warn!(pr_number, error = %e, "merge queue: failed after {max_attempts} attempts");
                    break;
                }
            }
        }

        if !merged {
            let job_key_for_count = format!("{owner}/{repo}/{}", entry.issue_number);
            self.increment_rework_count(&job_key_for_count).await;
            self.forgejo
                .post_comment(
                    owner, repo, pr_number,
                    "## Merge Conflict\n\n\
                     This PR cannot be merged due to conflicts with the base branch.\n\n\
                     Please rebase your branch on `main` and resolve any conflicts:\n\
                     ```\ngit fetch origin\ngit rebase origin/main\n# resolve conflicts\ngit push --force\n```",
                )
                .await
                .ok();
            if let Ok(Some(pr)) = self.find_linked_pr(owner, repo, entry.issue_number).await {
                let _ = self.request_rework(owner, repo, entry.issue_number, &pr, &entry.assignees).await;
            }
            return;
        }

        self.journal(
            "merge",
            &format!("Merged PR #{pr_number} (via merge queue)"),
            Some(&job_key),
            Some("workflow-reviewer"),
        )
        .await;

        tracing::info!(job_key = %job_key, pr_number, "merge queue: merged PR");
    }

    /// Request changes: publish a ChangesRequested transition directly via NATS.
    /// This replaces the old CDC-based detection of `has_changes_requested` from
    /// the database, which was brittle and version-dependent.
    async fn request_rework(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        pr: &PullRequest,
        assignees: &[String],
    ) -> Result<()> {
        let job_key = format!("{owner}/{repo}/{issue_number}");

        // Publish ChangesRequested transition directly.
        let transition = JobTransition {
            job: Job {
                repo_owner: owner.to_string(),
                repo_name: repo.to_string(),
                number: issue_number,
                title: String::new(), // dispatcher reads from graph
                state: JobState::InReview,
                assignees: assignees.to_vec(),
                dependency_numbers: vec![],
                priority: 50,
                timeout_secs: None,
                capabilities: vec![],
                max_retries: 3,
                retry_attempt: 0,
                worker_type: None,
                platform: vec![],
                review: Default::default(),
                activities: vec![],
                is_rework: false,
            },
            previous_state: Some(JobState::InReview),
            new_state: JobState::ChangesRequested,
        };
        if let Ok(payload) = serde_json::to_vec(&transition) {
            if let Err(e) = self
                .nats
                .publish(SUBJECT_TRANSITION, payload.into())
                .await
            {
                tracing::error!(job_key = %job_key, error = %e, "failed to publish ChangesRequested transition");
            }
        }

        self.journal(
            "changes-requested",
            &format!(
                "Review requested changes on PR #{} → ChangesRequested",
                pr_num(pr)
            ),
            Some(&job_key),
            Some("workflow-reviewer"),
        )
        .await;

        tracing::info!(
            job_key = %job_key,
            pr_number = pr_num(pr),
            "reviewer: requested rework (transition published)"
        );

        Ok(())
    }

    /// Escalate to the human reviewer.
    async fn escalate_to_human(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        pr: &PullRequest,
        review: ReviewRequirement,
    ) -> Result<()> {
        let job_key = format!("{owner}/{repo}/{issue_number}");

        self.forgejo
            .add_pr_reviewer(owner, repo, pr_num(pr), &self.human_login)
            .await?;

        let reason = match review {
            ReviewRequirement::Human => {
                "The `review:human` label requires manual review.".to_string()
            }
            ReviewRequirement::Low | ReviewRequirement::Medium | ReviewRequirement::High => {
                format!(
                    "The automated reviewer could not confidently assess this PR at the `review:{:?}` level.",
                    review
                )
                .to_lowercase()
            }
        };
        let comment = format!(
            "🔍 **Escalated for human review** — @{}\n\n{reason}",
            self.human_login
        );
        self.forgejo
            .post_comment(owner, repo, pr_num(pr), &comment)
            .await?;

        self.journal(
            "escalate",
            &format!(
                "Escalated PR #{} to human reviewer @{}",
                pr_num(pr),
                self.human_login
            ),
            Some(&job_key),
            Some("workflow-reviewer"),
        )
        .await;

        tracing::info!(
            job_key = %job_key,
            pr_number = pr_num(pr),
            human = %self.human_login,
            "reviewer: escalated to human reviewer"
        );

        Ok(())
    }

    /// Finalize the most recent running review activity by publishing an
    /// activity update via NATS. The sidecar's activity append handler will
    /// find the running Review entry and update it.
    async fn finalize_review_activity(
        &self,
        job_key: &str,
        decision: &str,
        _status: ActivityStatus,
    ) {
        // Record the review decision as a journal entry. The sidecar will
        // surface this in the dispatcher journal and SSE events.
        self.journal(
            &format!("review-{decision}"),
            &format!("Review decision: {decision}"),
            Some(job_key),
            Some("workflow-reviewer"),
        )
        .await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let config = ReviewerConfig::from_env()?;

    let nats = async_nats::connect(&config.nats_url)
        .await
        .context("connect to NATS")?;

    let forgejo = ForgejoClient::new(&config.forgejo_url, &config.forgejo_token);

    let reviewer = Arc::new(Reviewer::new(nats, forgejo, &config).await?);

    // Reconcile any InReview jobs that may have been missed during downtime.
    let sidecar_url = std::env::var("SIDECAR_URL")
        .unwrap_or_else(|_| "http://localhost:8080".to_string());
    reviewer.reconcile_on_startup(&sidecar_url).await;

    Arc::clone(&reviewer).start().await?;

    tracing::info!("reviewer process running — waiting for InReview transitions");

    // Block forever — the subscription runs in a spawned task.
    tokio::signal::ctrl_c().await?;
    tracing::info!("reviewer shutting down");

    Ok(())
}
