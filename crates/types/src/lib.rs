use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── Job state ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    OnIce,
    Blocked,
    OnDeck,
    OnTheStack,
    /// Worker is paused waiting for human input. Claim is held; heartbeats continue.
    NeedsHelp,
    InReview,
    ChangesRequested,
    Done,
    Failed,
    Revoked,
}

impl JobState {
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "status:on-ice" => Some(Self::OnIce),
            "status:blocked" => Some(Self::Blocked),
            "status:on-deck" => Some(Self::OnDeck),
            "status:on-the-stack" => Some(Self::OnTheStack),
            "status:needs-help" => Some(Self::NeedsHelp),
            "status:in-review" => Some(Self::InReview),
            "status:changes-requested" => Some(Self::ChangesRequested),
            "status:done" => Some(Self::Done),
            "status:failed" => Some(Self::Failed),
            "status:revoked" => Some(Self::Revoked),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::OnIce => "status:on-ice",
            Self::Blocked => "status:blocked",
            Self::OnDeck => "status:on-deck",
            Self::OnTheStack => "status:on-the-stack",
            Self::NeedsHelp => "status:needs-help",
            Self::InReview => "status:in-review",
            Self::ChangesRequested => "status:changes-requested",
            Self::Done => "status:done",
            Self::Failed => "status:failed",
            Self::Revoked => "status:revoked",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Revoked)
    }
}

// ── Review requirement ───────────────────────────────────────────────────────

/// Required confidence level for automated review.
///
/// The reviewer compares its own confidence against this threshold. If its
/// confidence is below the requirement, it escalates to a human reviewer.
///
/// Parsed from `review:X` labels:
/// - absent → `High` (default — reviewer must be highly confident)
/// - `review:low` → `Low`
/// - `review:medium` → `Medium`
/// - `review:high` → `High`
/// - `review:human` → `Human` (always escalate, bypasses confidence check)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewRequirement {
    /// Reviewer needs only low confidence to auto-approve.
    Low,
    /// Reviewer needs medium confidence to auto-approve.
    Medium,
    /// Reviewer needs high confidence to auto-approve (default).
    High,
    /// Always escalate to a human reviewer.
    Human,
}

impl Default for ReviewRequirement {
    fn default() -> Self {
        Self::High
    }
}

// ── Activity tracking ────────────────────────────────────────────────────────

/// A single entry in a job's activity timeline. Tracks interactive sessions,
/// action runs, and review actions in a unified list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityEntry {
    /// An interactive Claude agent session (headless or interactive mode).
    InteractiveSession {
        session_id: String,
        worker_id: String,
        status: ActivityStatus,
        started_at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ended_at: Option<DateTime<Utc>>,
        /// NATS Object Store key for the session recording (set on completion).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recording_key: Option<String>,
    },
    /// A Forgejo Actions workflow run (work or review).
    ActionRun {
        run_id: u64,
        workflow: String,
        worker_id: String,
        status: ActivityStatus,
        started_at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ended_at: Option<DateTime<Utc>>,
    },
    /// An automated or human PR review.
    Review {
        review_id: String,
        status: ActivityStatus,
        started_at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ended_at: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision: Option<String>,
        /// Action run ID if this review was done via a dispatched action.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<u64>,
    },
}

impl ActivityEntry {
    pub fn status(&self) -> &ActivityStatus {
        match self {
            Self::InteractiveSession { status, .. } => status,
            Self::ActionRun { status, .. } => status,
            Self::Review { status, .. } => status,
        }
    }

    pub fn set_status(&mut self, new_status: ActivityStatus) {
        match self {
            Self::InteractiveSession { status, .. } => *status = new_status,
            Self::ActionRun { status, .. } => *status = new_status,
            Self::Review { status, .. } => *status = new_status,
        }
    }

    pub fn set_ended_at(&mut self, time: DateTime<Utc>) {
        match self {
            Self::InteractiveSession { ended_at, .. } => *ended_at = Some(time),
            Self::ActionRun { ended_at, .. } => *ended_at = Some(time),
            Self::Review { ended_at, .. } => *ended_at = Some(time),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityStatus {
    Running,
    Success,
    Failure,
    Escalated,
    ChangesRequested,
}

/// Message from a worker to the sidecar to append an activity entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityAppend {
    pub job_key: String,
    pub entry: ActivityEntry,
}

/// Fire-and-forget message to append a journal entry via NATS.
/// Used by the reviewer (and potentially other external processes) to record
/// actions in the sidecar's journal without direct AppState access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalAppend {
    pub action: String,
    pub comment: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
}

// ── Job ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub repo_owner: String,
    pub repo_name: String,
    /// Forgejo issue number (per-repo, used in API paths)
    pub number: u64,
    pub title: String,
    pub state: JobState,
    pub assignees: Vec<String>,
    /// Issue numbers (same repo) this job depends on
    pub dependency_numbers: Vec<u64>,
    /// 0–100, higher = more urgent. Sourced from `priority:N` label; default 50.
    pub priority: u32,
    /// Timeout override in seconds. None = use sidecar default.
    pub timeout_secs: Option<u64>,
    /// Required capabilities from `capability:X` labels. Empty = any worker.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Max retry attempts from `retry:N` label. Default 3.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Number of times this job has been retried after failure. Persisted in
    /// the graph so it survives sidecar restarts.
    #[serde(default)]
    pub retry_attempt: u32,
    /// Required worker type from `worker:X` label. None = any worker type.
    #[serde(default)]
    pub worker_type: Option<String>,
    /// Required platforms from `platform:X` labels. Empty = any platform.
    #[serde(default)]
    pub platform: Vec<String>,
    /// Review requirement from `review:human` or `review:confidence:N` labels.
    #[serde(default)]
    pub review: ReviewRequirement,
    /// Activity timeline: interactive sessions, action runs, reviews.
    #[serde(default)]
    pub activities: Vec<ActivityEntry>,
    /// Set by the dispatch loop when a job is re-assigned after changes-requested.
    /// Signals the worker that a PR/branch already exists and review feedback should be addressed.
    #[serde(default)]
    pub is_rework: bool,
}

fn default_max_retries() -> u32 {
    3
}

impl Job {
    pub fn key(&self) -> String {
        format!("{}/{}/{}", self.repo_owner, self.repo_name, self.number)
    }
}

// ── Claim state (NATS KV) ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimState {
    pub worker_id: String,
    pub claimed_at: DateTime<Utc>,
    pub last_heartbeat: DateTime<Utc>,
    /// Resolved timeout for this claim (job override or sidecar default).
    /// This is the **job deadline** — how long the job is allowed to run total.
    pub timeout_secs: u64,
    /// Lease duration in seconds. The worker must heartbeat within this window
    /// or the lease expires and the job is requeued. Defaults to 30s for
    /// backwards compatibility with claims created before this field existed.
    #[serde(default = "default_lease_secs")]
    pub lease_secs: u64,
    /// Absolute deadline after which the lease is considered expired unless
    /// renewed by a heartbeat. Computed as `last_heartbeat + lease_secs`.
    #[serde(default = "Utc::now")]
    pub lease_deadline: DateTime<Utc>,
}

fn default_lease_secs() -> u64 {
    60
}

impl ClaimState {
    pub fn new(worker_id: String, timeout_secs: u64, lease_secs: u64) -> Self {
        let now = Utc::now();
        Self {
            worker_id,
            claimed_at: now,
            last_heartbeat: now,
            timeout_secs,
            lease_secs,
            lease_deadline: now + chrono::Duration::seconds(lease_secs as i64),
        }
    }

    /// Returns true if the worker's lease has expired (no heartbeat within `lease_secs`).
    /// This indicates a crashed or unreachable worker — the job should be requeued.
    pub fn is_lease_expired(&self) -> bool {
        Utc::now() > self.lease_deadline
    }

    /// Returns true if the job has exceeded its total deadline (`timeout_secs` since claim).
    /// This indicates a stuck worker — the job should be failed.
    pub fn is_timed_out(&self) -> bool {
        let elapsed = Utc::now()
            .signed_duration_since(self.claimed_at)
            .num_seconds();
        elapsed > self.timeout_secs as i64
    }

    /// Extend the lease deadline by `lease_secs` from now.
    pub fn renew_lease(&mut self) {
        let now = Utc::now();
        self.last_heartbeat = now;
        self.lease_deadline = now + chrono::Duration::seconds(self.lease_secs as i64);
    }
}

// ── Failure record ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    WorkerReported,
    HeartbeatTimeout,
    LeaseExpired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureRecord {
    pub worker_id: String,
    pub kind: FailureKind,
    pub reason: String,
    pub logs: Option<String>,
    pub failed_at: DateTime<Utc>,
}

impl FailureRecord {
    /// Renders the structured comment body for posting to Forgejo.
    pub fn to_comment_body(&self) -> String {
        let json = serde_json::to_string_pretty(self).unwrap_or_default();
        let kind = match self.kind {
            FailureKind::HeartbeatTimeout => "heartbeat_timeout",
            FailureKind::WorkerReported => "worker_reported",
            FailureKind::LeaseExpired => "lease_expired",
        };
        format!(
            "<!-- workflow:failure\n{json}\n-->\n\n\
             ⚠️ **Job failed** — `{kind}` by worker `{worker}` at {at}\n\n\
             **Reason:** {reason}",
            worker = self.worker_id,
            at = self.failed_at.to_rfc3339(),
            reason = self.reason,
        )
    }
}

// ── Factory status ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactoryStatus {
    pub name: String,
    pub enabled: bool,
    pub poll_interval_secs: Option<u64>,
    pub last_poll: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FactoryListResponse {
    pub factories: Vec<FactoryStatus>,
}

// ── User info ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub login: String,
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub avatar_url: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserListResponse {
    pub users: Vec<UserInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LabelListResponse {
    pub labels: Vec<ForgejoLabel>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateIssueRequest {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub labels: Vec<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateIssueResponse {
    pub number: u64,
}

// ── API: request / response types ────────────────────────────────────────────


#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequeueTarget {
    OnDeck,
    OnIce,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RequeueRequest {
    /// Repository owner (used by NATS admin handler; ignored by HTTP).
    #[serde(default)]
    pub owner: String,
    /// Repository name (used by NATS admin handler; ignored by HTTP).
    #[serde(default)]
    pub repo: String,
    /// Issue number (used by NATS admin handler; ignored by HTTP).
    #[serde(default)]
    pub number: u64,
    pub target: RequeueTarget,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobListResponse {
    pub jobs: Vec<Job>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobResponse {
    pub job: Job,
    pub claim: Option<ClaimState>,
    pub failure: Option<FailureRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DepsResponse {
    pub dependencies: Vec<Job>,
    pub all_done: bool,
}

// ── Transition events ────────────────────────────────────────────────────

/// Published to "workflow.jobs.transition" when the sidecar detects or
/// causes a state change.  This is a derived notification stream — the
/// graph and Forgejo labels remain the sources of truth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobTransition {
    pub job: Job,
    /// `None` when the job is first seen (new vertex in graph).
    pub previous_state: Option<JobState>,
    pub new_state: JobState,
}

// ── Monitor advisory events ──────────────────────────────────────────────

/// Published by the monitor when a worker's lease expires. The dispatcher
/// re-verifies and decides whether to release and requeue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseExpiredEvent {
    pub job_key: String,
    pub worker_id: String,
    pub elapsed_secs: i64,
}

/// Published by the monitor when a job exceeds its total deadline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobTimeoutEvent {
    pub job_key: String,
    pub worker_id: String,
}

/// Published by the monitor when a job is on_the_stack/needs_help with no claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrphanDetectedEvent {
    pub job_key: String,
    pub previous_state: JobState,
}

// ── Dispatch types ───────────────────────────────────────────────────────

/// Worker announces itself to the dispatcher with its capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRegistration {
    pub worker_id: String,
    pub capabilities: Vec<String>,
    /// Worker type: "sim", "action", "interactive". Determines UI affordances
    /// and job routing via `worker:X` labels.
    #[serde(default)]
    pub worker_type: String,
    /// Platform tags: "linux", "macos", "arm64", etc. Used for job routing
    /// via `platform:X` labels.
    #[serde(default)]
    pub platform: Vec<String>,
}

/// Worker signals it is ready for a new assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdleEvent {
    pub worker_id: String,
}

/// Dispatcher pushes a job assignment to a specific worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub job: Job,
    pub claim: ClaimState,
    #[serde(default)]
    pub is_rework: bool,
}

/// Dispatcher tells a worker to yield its current job for a higher-priority one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreemptNotice {
    pub reason: String,
    pub new_job: Job,
}

/// Worker publishes a heartbeat to NATS so the dispatcher can forward it
/// to the coordinator without the worker needing an HTTP client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHeartbeat {
    pub worker_id: String,
    pub job_key: String,
}

/// The result a worker reports after executing a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OutcomeReport {
    Complete,
    Fail {
        reason: String,
        logs: Option<String>,
    },
    Abandon,
    /// Release claim without changing state; an external signal handles the transition.
    Yield,
}

/// Worker reports the outcome of an assigned job via NATS.
/// The dispatcher handles claim release, state transitions, and Forgejo sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerOutcome {
    pub worker_id: String,
    pub job_key: String,
    pub outcome: OutcomeReport,
}

// ── Interactive worker types ─────────────────────────────────────────────────

/// Worker publishes this when it needs human input mid-task.
/// Published to `workflow.interact.help`.
/// The sidecar transitions the job to NeedsHelp and posts a Forgejo comment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelpRequest {
    pub job_key: String,
    pub worker_id: String,
    pub reason: String,
    /// Human-readable hint for joining the session (e.g. a tmux attach command).
    pub session_hint: Option<String>,
}

/// Human (or CLI tool) sends this to unblock a waiting worker.
/// Published to `workflow.interact.respond.{nats_job_key}` where
/// `nats_job_key` is the job key with `/` replaced by `.`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelpResponse {
    pub message: String,
}

/// Metadata about an active interactive agent session.
/// Stored in the `workflow-sessions` NATS KV bucket (key = nats job key).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub job_key: String,
    pub worker_id: String,
    /// URL of the ttyd web terminal for this session.
    pub session_url: String,
    /// tmux session name (useful for local attach).
    pub session_name: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// "peek" = read-only view of headless output, "interactive" = full TUI.
    #[serde(default = "default_session_mode")]
    pub mode: String,
}

fn default_session_mode() -> String {
    "interactive".into()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionInfo>,
}

// ── Dispatch observability ───────────────────────────────────────────────────

/// Current state of a worker in the dispatcher's registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Idle,
    Busy,
    Transitioning,
}

/// Snapshot of a registered worker's status, exposed via `GET /dispatch/workers`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub worker_id: String,
    pub state: WorkerState,
    pub capabilities: Vec<String>,
    pub worker_type: String,
    pub platform: Vec<String>,
    pub current_job_key: Option<String>,
    pub current_job_priority: Option<u32>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerListResponse {
    pub workers: Vec<WorkerInfo>,
}

/// A single dispatcher journal entry — records an action the dispatcher took.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub timestamp: DateTime<Utc>,
    pub action: String,
    pub comment: String,
    /// Job key if the action relates to a specific job.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_key: Option<String>,
    /// Worker involved, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JournalResponse {
    pub entries: Vec<JournalEntry>,
}

// ── CDC: issue snapshot from database ────────────────────────────────────────

/// A fully denormalized issue snapshot produced by the CDC process.
/// One message per changed issue, published to the NATS stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueSnapshot {
    /// Forgejo internal issue ID (globally unique across repos).
    pub issue_id: u64,
    /// Repository owner username.
    pub repo_owner: String,
    /// Repository name.
    pub repo_name: String,
    /// Issue number within the repo (the human-visible `#N`).
    pub number: u64,
    pub title: String,
    pub body: String,
    pub is_closed: bool,
    /// True if a merged PR in the same repo references `Closes #N` for this issue.
    #[serde(default)]
    pub closed_by_merge: bool,
    /// True if an open (unmerged) PR referencing `Closes #N` exists for this issue.
    #[serde(default)]
    pub has_open_pr: bool,
    /// Label names attached to this issue.
    pub labels: Vec<String>,
    /// Assignee login names.
    pub assignees: Vec<String>,
    /// Unix timestamp of the last update (used as stream position).
    pub updated_unix: i64,
}

// ── Forgejo types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgejoIssue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub labels: Vec<ForgejoLabel>,
    pub assignees: Option<Vec<ForgejoUser>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ForgejoLabel {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub color: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ForgejoUser {
    pub login: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgejoRepo {
    pub owner: ForgejoUser,
    pub name: String,
}

// ── Label parsing helpers ────────────────────────────────────────────────────

/// Parse `priority:N` label, returning 50 if absent.
pub fn parse_priority(labels: &[ForgejoLabel]) -> u32 {
    for label in labels {
        if let Some(rest) = label.name.strip_prefix("priority:") {
            if let Ok(n) = rest.trim().parse::<u32>() {
                return n.min(100);
            }
        }
    }
    50
}

/// Parse `timeout:N` label, returning None if absent.
pub fn parse_timeout(labels: &[ForgejoLabel]) -> Option<u64> {
    for label in labels {
        if let Some(rest) = label.name.strip_prefix("timeout:") {
            if let Ok(n) = rest.trim().parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

/// Parse `retry:N` label, returning N (default 3 if absent).
pub fn parse_retries(labels: &[ForgejoLabel]) -> u32 {
    for label in labels {
        if let Some(rest) = label.name.strip_prefix("retry:") {
            if let Ok(n) = rest.trim().parse::<u32>() {
                return n;
            }
        }
    }
    3 // default
}

/// Parse `capability:X` labels, returning all capability tags.
pub fn parse_capabilities(labels: &[ForgejoLabel]) -> Vec<String> {
    labels
        .iter()
        .filter_map(|l| {
            l.name
                .strip_prefix("capability:")
                .map(|s| s.trim().to_string())
        })
        .collect()
}

/// Parse `worker:X` label, returning the required worker type (e.g. "action", "interactive").
pub fn parse_worker_type(labels: &[ForgejoLabel]) -> Option<String> {
    labels
        .iter()
        .find_map(|l| l.name.strip_prefix("worker:").map(|s| s.trim().to_string()))
}

/// Parse `platform:X` labels, returning all required platform tags.
pub fn parse_platform(labels: &[ForgejoLabel]) -> Vec<String> {
    labels
        .iter()
        .filter_map(|l| {
            l.name
                .strip_prefix("platform:")
                .map(|s| s.trim().to_string())
        })
        .collect()
}

/// Parse review requirement from labels.
///
/// - `review:human` → always escalate
/// - `review:confidence:N` → reviewer must meet confidence level N (0–100)
/// - absent → Auto (reviewer uses its default threshold)
pub fn parse_review_requirement(labels: &[ForgejoLabel]) -> ReviewRequirement {
    for label in labels {
        match label.name.as_str() {
            "review:human" => return ReviewRequirement::Human,
            "review:low" => return ReviewRequirement::Low,
            "review:medium" => return ReviewRequirement::Medium,
            "review:high" => return ReviewRequirement::High,
            _ => {}
        }
    }
    ReviewRequirement::High // default: require high confidence
}

/// Parse dep issue numbers from an issue body.
///
/// Convention: `<!-- workflow:deps:1,2,3 -->` anywhere in the body.
pub fn parse_deps(body: &str) -> Vec<u64> {
    let prefix = "<!-- workflow:deps:";
    if let Some(start) = body.find(prefix) {
        let rest = &body[start + prefix.len()..];
        if let Some(end) = rest.find("-->") {
            return rest[..end]
                .trim()
                .split(',')
                .filter_map(|s| s.trim().parse::<u64>().ok())
                .collect();
        }
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_priority() {
        let labels = vec![
            ForgejoLabel {
                id: 1,
                name: "priority:75".into(),
                color: String::new(),
            },
            ForgejoLabel {
                id: 2,
                name: "status:on-deck".into(),
                color: String::new(),
            },
        ];
        assert_eq!(parse_priority(&labels), 75);
    }

    #[test]
    fn test_parse_priority_default() {
        let labels: Vec<ForgejoLabel> = vec![];
        assert_eq!(parse_priority(&labels), 50);
    }

    #[test]
    fn test_parse_deps() {
        let body = "Some description\n<!-- workflow:deps:1,2,3 -->\nMore text";
        assert_eq!(parse_deps(body), vec![1, 2, 3]);
    }

    #[test]
    fn test_parse_deps_empty() {
        assert_eq!(parse_deps("no deps here"), Vec::<u64>::new());
    }

    #[test]
    fn test_job_state_label_roundtrip() {
        for state in [
            JobState::OnIce,
            JobState::Blocked,
            JobState::OnDeck,
            JobState::OnTheStack,
            JobState::NeedsHelp,
            JobState::InReview,
            JobState::ChangesRequested,
            JobState::Done,
            JobState::Failed,
            JobState::Revoked,
        ] {
            assert_eq!(JobState::from_label(state.label()), Some(state));
        }
    }

    #[test]
    fn test_claim_fresh_not_expired() {
        let claim = ClaimState::new("worker-1".into(), 3600, 30);
        assert!(!claim.is_timed_out());
        assert!(!claim.is_lease_expired());
    }

    #[test]
    fn test_lease_expires_after_deadline() {
        let mut claim = ClaimState::new("worker-1".into(), 3600, 2);
        // Simulate lease_deadline in the past.
        claim.lease_deadline = Utc::now() - chrono::Duration::seconds(1);
        assert!(claim.is_lease_expired());
        // Job deadline should NOT be exceeded (only 2s old vs 3600s timeout).
        assert!(!claim.is_timed_out());
    }

    #[test]
    fn test_job_timeout_after_deadline() {
        let mut claim = ClaimState::new("worker-1".into(), 10, 30);
        // Simulate claimed_at far in the past.
        claim.claimed_at = Utc::now() - chrono::Duration::seconds(11);
        assert!(claim.is_timed_out());
        // Lease should still be valid (just created).
        assert!(!claim.is_lease_expired());
    }

    #[test]
    fn test_renew_lease_extends_deadline() {
        let mut claim = ClaimState::new("worker-1".into(), 3600, 30);
        let original_deadline = claim.lease_deadline;
        // Simulate time passing.
        claim.lease_deadline = Utc::now() - chrono::Duration::seconds(1);
        assert!(claim.is_lease_expired());
        // Renew should fix it.
        claim.renew_lease();
        assert!(!claim.is_lease_expired());
        assert!(claim.lease_deadline > original_deadline);
        assert!(claim.last_heartbeat > claim.claimed_at - chrono::Duration::seconds(1));
    }

    #[test]
    fn test_lease_secs_default_on_deserialize() {
        // Simulate a legacy ClaimState without lease fields.
        let json = r#"{
            "worker_id": "worker-1",
            "claimed_at": "2026-01-01T00:00:00Z",
            "last_heartbeat": "2026-01-01T00:00:00Z",
            "timeout_secs": 3600
        }"#;
        let claim: ClaimState = serde_json::from_str(json).unwrap();
        assert_eq!(claim.lease_secs, 60);  // default_lease_secs()
    }

    #[test]
    fn test_parse_capabilities() {
        let labels = vec![
            ForgejoLabel {
                id: 1,
                name: "capability:rust".into(),
                color: String::new(),
            },
            ForgejoLabel {
                id: 2,
                name: "capability:frontend".into(),
                color: String::new(),
            },
            ForgejoLabel {
                id: 3,
                name: "status:on-deck".into(),
                color: String::new(),
            },
        ];
        let mut caps = parse_capabilities(&labels);
        caps.sort();
        assert_eq!(caps, vec!["frontend", "rust"]);
    }

    #[test]
    fn test_parse_capabilities_empty() {
        let labels: Vec<ForgejoLabel> = vec![ForgejoLabel {
            id: 1,
            name: "status:on-deck".into(),
            color: String::new(),
        }];
        assert!(parse_capabilities(&labels).is_empty());
    }

    #[test]
    fn test_parse_worker_type() {
        let labels = vec![ForgejoLabel {
            id: 1,
            name: "worker:interactive".into(),
            color: String::new(),
        }];
        assert_eq!(parse_worker_type(&labels), Some("interactive".into()));
    }

    #[test]
    fn test_parse_worker_type_absent() {
        let labels: Vec<ForgejoLabel> = vec![];
        assert_eq!(parse_worker_type(&labels), None);
    }

    #[test]
    fn test_parse_platform() {
        let labels = vec![
            ForgejoLabel { id: 1, name: "platform:macos".into(), color: String::new() },
            ForgejoLabel { id: 2, name: "platform:arm64".into(), color: String::new() },
            ForgejoLabel { id: 3, name: "status:on-deck".into(), color: String::new() },
        ];
        let mut plat = parse_platform(&labels);
        plat.sort();
        assert_eq!(plat, vec!["arm64", "macos"]);
    }

    #[test]
    fn test_parse_platform_empty() {
        let labels: Vec<ForgejoLabel> = vec![];
        assert!(parse_platform(&labels).is_empty());
    }

    #[test]
    fn test_parse_review_requirement_human() {
        let labels = vec![ForgejoLabel {
            id: 1,
            name: "review:human".into(),
            color: String::new(),
        }];
        assert_eq!(parse_review_requirement(&labels), ReviewRequirement::Human);
    }

    #[test]
    fn test_parse_review_requirement_low() {
        let labels = vec![ForgejoLabel {
            id: 1,
            name: "review:low".into(),
            color: String::new(),
        }];
        assert_eq!(parse_review_requirement(&labels), ReviewRequirement::Low);
    }

    #[test]
    fn test_parse_review_requirement_medium() {
        let labels = vec![ForgejoLabel {
            id: 1,
            name: "review:medium".into(),
            color: String::new(),
        }];
        assert_eq!(parse_review_requirement(&labels), ReviewRequirement::Medium);
    }

    #[test]
    fn test_parse_review_requirement_high() {
        let labels = vec![ForgejoLabel {
            id: 1,
            name: "review:high".into(),
            color: String::new(),
        }];
        assert_eq!(parse_review_requirement(&labels), ReviewRequirement::High);
    }

    #[test]
    fn test_parse_review_requirement_default_is_high() {
        let labels: Vec<ForgejoLabel> = vec![];
        assert_eq!(parse_review_requirement(&labels), ReviewRequirement::High);
    }

    #[test]
    fn test_review_requirement_human_takes_precedence() {
        let labels = vec![
            ForgejoLabel { id: 1, name: "review:human".into(), color: String::new() },
            ForgejoLabel { id: 2, name: "review:low".into(), color: String::new() },
        ];
        // review:human appears first, should win
        assert_eq!(parse_review_requirement(&labels), ReviewRequirement::Human);
    }
}
