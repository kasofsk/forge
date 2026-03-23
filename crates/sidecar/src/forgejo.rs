use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::Mutex;
use workflow_types::{FailureRecord, ForgejoIssue, ForgejoLabel, JobState, UserInfo};

pub struct ForgejoClient {
    base_url: String,
    token: String,
    http: Client,
    /// (owner, repo, label_name) → label_id
    label_cache: Mutex<HashMap<(String, String, String), u64>>,
}

// ── Forgejo API request/response types ───────────────────────────────────────

#[derive(Serialize)]
struct ReplaceLabelsBody {
    labels: Vec<u64>,
}

#[derive(Serialize)]
struct EditIssueBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    assignees: Option<Vec<String>>,
}

#[derive(Serialize)]
struct CreateCommentBody<'a> {
    body: &'a str,
}

#[derive(Serialize)]
struct CreateLabelBody<'a> {
    name: &'a str,
    color: &'a str,
}

#[derive(Deserialize)]
struct CreatedLabel {
    id: u64,
}

// ── Implementation ────────────────────────────────────────────────────────────

impl ForgejoClient {
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http: Client::new(),
            label_cache: Mutex::new(HashMap::new()),
        }
    }

    fn api(&self, path: &str) -> String {
        format!("{}/api/v1{}", self.base_url, path)
    }

    fn auth(&self) -> String {
        format!("token {}", self.token)
    }

    // ── Generic helpers ──────────────────────────────────────────────────────

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn list_prs(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
    ) -> Result<Vec<serde_json::Value>> {
        let url = self.api(&format!("/repos/{owner}/{repo}/pulls?state={state}&limit=50"));
        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("list PRs")?
            .error_for_status()
            .context("list PRs response")?;
        Ok(resp.json().await?)
    }

    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = self.api(path);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("GET request")?
            .error_for_status()
            .context("GET response")?;
        Ok(resp.json().await?)
    }

    pub async fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<()> {
        let url = self.api(path);
        self.http
            .post(&url)
            .header("Authorization", self.auth())
            .json(body)
            .send()
            .await
            .context("POST request")?
            .error_for_status()
            .context("POST response")?;
        Ok(())
    }

    // ── Issue fetch ───────────────────────────────────────────────────────────

    pub async fn get_issue(&self, owner: &str, repo: &str, number: u64) -> Result<ForgejoIssue> {
        let url = self.api(&format!("/repos/{owner}/{repo}/issues/{number}"));
        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("get issue")?
            .error_for_status()
            .context("get issue response")?;
        Ok(resp.json().await?)
    }

    // ── Issue creation ─────────────────────────────────────────────────────────

    pub async fn create_issue(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        labels: &[u64],
    ) -> Result<u64> {
        #[derive(Serialize)]
        struct CreateBody<'a> {
            title: &'a str,
            body: &'a str,
            #[serde(skip_serializing_if = "<[u64]>::is_empty")]
            labels: &'a [u64],
        }
        #[derive(Deserialize)]
        struct Created {
            number: u64,
        }
        let url = self.api(&format!("/repos/{owner}/{repo}/issues"));
        let resp: Created = self
            .http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&CreateBody {
                title,
                body,
                labels,
            })
            .send()
            .await
            .context("create issue")?
            .error_for_status()
            .context("create issue response")?
            .json()
            .await?;
        Ok(resp.number)
    }

    // ── Label listing ────────────────────────────────────────────────────────

    pub async fn list_labels(&self, owner: &str, repo: &str) -> Result<Vec<ForgejoLabel>> {
        let url = self.api(&format!("/repos/{owner}/{repo}/labels?limit=50"));
        let labels: Vec<ForgejoLabel> = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("list labels")?
            .error_for_status()
            .context("list labels response")?
            .json()
            .await?;
        Ok(labels)
    }

    // ── Label management ──────────────────────────────────────────────────────

    /// Look up a label by name, creating it if absent. Returns the label ID.
    pub async fn ensure_label(&self, owner: &str, repo: &str, name: &str) -> Result<u64> {
        let cache_key = (owner.to_string(), repo.to_string(), name.to_string());

        {
            let cache = self.label_cache.lock().await;
            if let Some(&id) = cache.get(&cache_key) {
                return Ok(id);
            }
        }

        // Fetch all repo labels and populate cache
        self.refresh_label_cache(owner, repo).await?;

        {
            let cache = self.label_cache.lock().await;
            if let Some(&id) = cache.get(&cache_key) {
                return Ok(id);
            }
        }

        // Not found — create it
        let color = label_color(name);
        let id = self.create_label(owner, repo, name, color).await?;
        self.label_cache.lock().await.insert(cache_key, id);
        Ok(id)
    }

    async fn refresh_label_cache(&self, owner: &str, repo: &str) -> Result<()> {
        let url = self.api(&format!("/repos/{owner}/{repo}/labels?limit=50"));
        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("list labels")?
            .error_for_status()
            .context("list labels response")?;
        let labels: Vec<ForgejoLabel> = resp.json().await?;

        let mut cache = self.label_cache.lock().await;
        for label in labels {
            cache.insert(
                (owner.to_string(), repo.to_string(), label.name.clone()),
                label.id,
            );
        }
        Ok(())
    }

    async fn create_label(&self, owner: &str, repo: &str, name: &str, color: &str) -> Result<u64> {
        let url = self.api(&format!("/repos/{owner}/{repo}/labels"));
        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&CreateLabelBody { name, color })
            .send()
            .await
            .context("create label")?
            .error_for_status()
            .context("create label response")?;
        let created: CreatedLabel = resp.json().await?;
        Ok(created.id)
    }

    // ── State transition ──────────────────────────────────────────────────────

    /// Transition an issue to a new job state:
    /// - Ensures the new `status:*` label exists
    /// - Replaces all `status:*` labels while preserving others
    pub async fn set_job_state(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        state: &JobState,
    ) -> Result<()> {
        let new_label_name = state.label();
        let new_label_id = self.ensure_label(owner, repo, new_label_name).await?;

        // Get current labels on the issue
        let issue = self.get_issue(owner, repo, number).await?;
        let mut label_ids: Vec<u64> = issue
            .labels
            .iter()
            .filter(|l| JobState::from_label(&l.name).is_none())
            .map(|l| l.id)
            .collect();
        label_ids.push(new_label_id);

        self.replace_labels(owner, repo, number, label_ids).await?;
        Ok(())
    }

    async fn replace_labels(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        label_ids: Vec<u64>,
    ) -> Result<()> {
        let url = self.api(&format!("/repos/{owner}/{repo}/issues/{number}/labels"));
        self.http
            .put(&url)
            .header("Authorization", self.auth())
            .json(&ReplaceLabelsBody { labels: label_ids })
            .send()
            .await
            .context("replace labels")?
            .error_for_status()
            .context("replace labels response")?;
        Ok(())
    }

    // ── Assignee management ───────────────────────────────────────────────────

    pub async fn set_assignees(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        assignees: Vec<String>,
    ) -> Result<()> {
        let url = self.api(&format!("/repos/{owner}/{repo}/issues/{number}"));
        self.http
            .patch(&url)
            .header("Authorization", self.auth())
            .json(&EditIssueBody {
                assignees: Some(assignees),
            })
            .send()
            .await
            .context("set assignees")?
            .error_for_status()
            .context("set assignees response")?;
        Ok(())
    }

    pub async fn clear_assignees(&self, owner: &str, repo: &str, number: u64) -> Result<()> {
        self.set_assignees(owner, repo, number, vec![]).await
    }

    // ── Comment posting ───────────────────────────────────────────────────────

    pub async fn post_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()> {
        let url = self.api(&format!("/repos/{owner}/{repo}/issues/{number}/comments"));
        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&CreateCommentBody { body })
            .send()
            .await
            .context("post comment")?;

        // 201 = created, 200 = ok (some Forgejo versions differ)
        if resp.status() != StatusCode::CREATED && !resp.status().is_success() {
            resp.error_for_status().context("post comment response")?;
        }
        Ok(())
    }

    pub async fn post_failure_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        failure: &FailureRecord,
    ) -> Result<()> {
        self.post_comment(owner, repo, number, &failure.to_comment_body())
            .await
    }

    // ── Collaborators ──────────────────────────────────────────────────────────

    pub async fn get_repo_collaborators(&self, owner: &str, repo: &str) -> Result<Vec<UserInfo>> {
        let url = self.api(&format!("/repos/{owner}/{repo}/collaborators"));
        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("get collaborators")?
            .error_for_status()
            .context("get collaborators response")?;
        Ok(resp.json().await?)
    }
}

/// Pick a consistent color for well-known label names.
fn label_color(name: &str) -> &'static str {
    match name {
        "status:on-ice" => "#cccccc",
        "status:blocked" => "#e11d48",
        "status:on-deck" => "#16a34a",
        "status:on-the-stack" => "#2563eb",
        "status:in-review" => "#9333ea",
        "status:rework" => "#f59e0b",
        "status:done" => "#6b7280",
        "status:failed" => "#dc2626",
        "status:revoked" => "#6b7280",
        _ => "#ededed",
    }
}
