use anyhow::{Context, Result};
use reqwest::Client;
use workflow_types::{DepsResponse, FactoryListResponse, Job, JobListResponse, JobResponse};

/// Typed HTTP client for the sidecar API (read-only endpoints).
///
/// Lifecycle operations (claim, heartbeat, complete, abandon, fail) are handled
/// exclusively via NATS by the `DispatchedWorkerLoop`. Admin operations (requeue,
/// factory poll) use NATS request-reply.
#[derive(Clone)]
pub struct SidecarClient {
    base_url: String,
    http: Client,
}

impl SidecarClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    // ── Discovery ─────────────────────────────────────────────────────────────

    /// List all jobs, optionally filtered by state label (e.g. `"status:on-deck"`).
    pub async fn list_jobs(&self, state: Option<&str>) -> Result<Vec<Job>> {
        let mut url = self.url("/jobs");
        if let Some(s) = state {
            url = format!("{url}?state={s}");
        }
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("list jobs")?
            .error_for_status()
            .context("list jobs response")?;
        let body: JobListResponse = resp.json().await?;
        Ok(body.jobs)
    }

    pub async fn get_job(&self, owner: &str, repo: &str, number: u64) -> Result<JobResponse> {
        let resp = self
            .http
            .get(self.url(&format!("/jobs/{owner}/{repo}/{number}")))
            .send()
            .await
            .context("get job")?
            .error_for_status()
            .context("get job response")?;
        Ok(resp.json().await?)
    }

    pub async fn get_deps(&self, owner: &str, repo: &str, number: u64) -> Result<DepsResponse> {
        let resp = self
            .http
            .get(self.url(&format!("/jobs/{owner}/{repo}/{number}/deps")))
            .send()
            .await
            .context("get deps")?
            .error_for_status()
            .context("get deps response")?;
        Ok(resp.json().await?)
    }

    // ── Factory observability ─────────────────────────────────────────────────

    pub async fn list_factories(&self) -> Result<FactoryListResponse> {
        let resp = self
            .http
            .get(self.url("/factories"))
            .send()
            .await
            .context("list factories")?
            .error_for_status()
            .context("list factories response")?;
        Ok(resp.json().await?)
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Return all `on-deck` jobs sorted by descending priority.
    pub async fn available_jobs(&self) -> Result<Vec<Job>> {
        let mut jobs = self.list_jobs(Some("on-deck")).await?;
        jobs.sort_by(|a, b| b.priority.cmp(&a.priority));
        Ok(jobs)
    }
}
