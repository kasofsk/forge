use anyhow::{Context, Result};
use forgejo_api::apis::configuration::Configuration;
use forgejo_api::apis::{issue_api, repository_api};
use forgejo_api::models;
use reqwest::header;

/// Forgejo client scoped to content operations safe for workers to perform
/// after they hold an exclusive claim.
///
/// Wraps the generated `forgejo-api` client with ergonomic methods.
/// Workers never touch state labels or assignees — those are owned by the
/// sidecar.
#[derive(Clone)]
pub struct ForgejoClient {
    config: Configuration,
}

// ── Re-export generated types we expose in our public API ────────────────────

pub use forgejo_api::models::Branch;
pub use forgejo_api::models::ChangedFile;
pub use forgejo_api::models::Comment;
pub use forgejo_api::models::Issue;
pub use forgejo_api::models::Label as RepoLabel;
pub use forgejo_api::models::PullRequest;

/// Simplified action run info extracted from the generated model.
#[derive(Debug, Clone)]
pub struct ActionRun {
    pub id: u64,
    pub status: String,
}

impl ActionRun {
    pub fn is_completed(&self) -> bool {
        matches!(
            self.status.as_str(),
            "success" | "failure" | "cancelled" | "skipped"
        )
    }

    pub fn is_success(&self) -> bool {
        self.status == "success"
    }
}

// ── Implementation ────────────────────────────────────────────────────────────

impl ForgejoClient {
    /// Create a client using basic auth (username/password).
    /// Used for admin operations like creating users.
    pub fn new_basic_auth(base_url: &str, username: &str, password: &str) -> Self {
        let base = base_url.trim_end_matches('/');
        Self {
            config: Configuration {
                base_path: format!("{base}/api/v1"),
                basic_auth: Some((username.to_string(), Some(password.to_string()))),
                ..Configuration::default()
            },
        }
    }

    pub fn new(base_url: &str, token: &str) -> Self {
        let base = base_url.trim_end_matches('/');
        Self {
            config: Configuration {
                base_path: format!("{base}/api/v1"),
                // Bake the token into the reqwest client's default headers.
                // This avoids the generated code's broken query-param auth
                // (it only adds query params when api_key is Some).
                client: reqwest::Client::builder()
                    .default_headers({
                        let mut h = header::HeaderMap::new();
                        h.insert(
                            header::AUTHORIZATION,
                            header::HeaderValue::from_str(&format!("token {token}"))
                                .expect("invalid token"),
                        );
                        h
                    })
                    .build()
                    .expect("build http client"),
                ..Configuration::default()
            },
        }
    }

    /// The base Forgejo URL (without /api/v1).
    pub fn base_url(&self) -> String {
        self.config.base_path.trim_end_matches("/api/v1").to_string()
    }

    /// Perform a GET request and deserialize the JSON response.
    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self.config.client.get(url).send().await.context("GET request")?;
        resp.json::<T>().await.context("parse JSON response")
    }

    /// Perform a POST request with a JSON body.
    pub async fn post_json(&self, url: &str, body: &serde_json::Value) -> Result<()> {
        let resp = self.config.client.post(url).json(body).send().await.context("POST request")?;
        if !resp.status().is_success() {
            anyhow::bail!("POST {} returned {}", url, resp.status());
        }
        Ok(())
    }

    // ── Issue creation / editing ──────────────────────────────────────────────

    pub async fn create_issue(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
    ) -> Result<u64> {
        self.create_issue_with_labels(owner, repo, title, body, &[])
            .await
    }

    pub async fn create_issue_with_labels(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        labels: &[u64],
    ) -> Result<u64> {
        let opts = models::CreateIssueOption {
            title: title.to_string(),
            body: Some(body.to_string()),
            labels: Some(labels.iter().map(|&id| id as i64).collect()),
            ..Default::default()
        };
        let issue = issue_api::issue_create_issue(&self.config, owner, repo, Some(opts))
            .await
            .context("create issue")?;
        Ok(issue.number.unwrap_or(0) as u64)
    }

    // ── Admin operations ───────────────────────────────────────────────────

    pub async fn admin_create_user(
        &self,
        username: &str,
        email: &str,
        password: &str,
    ) -> Result<()> {
        use forgejo_api::apis::admin_api;
        let opts = models::CreateUserOption {
            username: username.to_string(),
            email: email.to_string(),
            password: Some(password.to_string()),
            must_change_password: Some(false),
            ..Default::default()
        };
        admin_api::admin_create_user(&self.config, Some(opts))
            .await
            .context("admin create user")?;
        Ok(())
    }

    pub async fn add_issue_labels(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        label_ids: &[u64],
    ) -> Result<()> {
        let opts = models::IssueLabelsOption {
            labels: Some(
                label_ids
                    .iter()
                    .map(|&id| serde_json::Value::from(id as i64))
                    .collect(),
            ),
            updated_at: None,
        };
        issue_api::issue_add_label(&self.config, owner, repo, number as i64, Some(opts))
            .await
            .context("add issue labels")?;
        Ok(())
    }

    pub async fn create_label(
        &self,
        owner: &str,
        repo: &str,
        name: &str,
        color: &str,
    ) -> Result<RepoLabel> {
        let opts = models::CreateLabelOption {
            color: color.to_string(),
            name: name.to_string(),
            ..Default::default()
        };
        issue_api::issue_create_label(&self.config, owner, repo, Some(opts))
            .await
            .context("create label")
    }

    pub async fn list_repo_labels(&self, owner: &str, repo: &str) -> Result<Vec<RepoLabel>> {
        let labels = issue_api::issue_list_labels(&self.config, owner, repo, None, None, Some(50))
            .await
            .context("list repo labels")?;
        Ok(labels)
    }

    pub async fn close_issue(&self, owner: &str, repo: &str, number: u64) -> Result<()> {
        let opts = models::EditIssueOption {
            state: Some("closed".to_string()),
            ..Default::default()
        };
        issue_api::issue_edit_issue(&self.config, owner, repo, number as i64, Some(opts))
            .await
            .context("close issue")?;
        Ok(())
    }

    pub async fn edit_issue_body(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()> {
        let opts = models::EditIssueOption {
            body: Some(body.to_string()),
            ..Default::default()
        };
        issue_api::issue_edit_issue(&self.config, owner, repo, number as i64, Some(opts))
            .await
            .context("edit issue body")?;
        Ok(())
    }

    // ── Issue content ─────────────────────────────────────────────────────────

    pub async fn get_issue(&self, owner: &str, repo: &str, number: u64) -> Result<Issue> {
        issue_api::issue_get_issue(&self.config, owner, repo, number as i64)
            .await
            .context("get issue")
    }

    pub async fn get_issue_body(&self, owner: &str, repo: &str, number: u64) -> Result<String> {
        let issue = self.get_issue(owner, repo, number).await?;
        Ok(issue.body.unwrap_or_default())
    }

    pub async fn list_issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<Comment>> {
        issue_api::issue_get_comments(&self.config, owner, repo, number as i64, None, None)
            .await
            .context("list comments")
    }

    pub async fn post_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()> {
        let opts = models::CreateIssueCommentOption {
            body: body.to_string(),
            ..Default::default()
        };
        issue_api::issue_create_comment(&self.config, owner, repo, number as i64, Some(opts))
            .await
            .context("post comment")?;
        Ok(())
    }

    /// List comments on an issue (also works for PR comments via the issue API).
    pub async fn list_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<models::Comment>> {
        issue_api::issue_get_comments(
            &self.config,
            owner,
            repo,
            number as i64,
            None,
            None,
        )
        .await
        .context("list comments")
    }

    // ── Branch operations ─────────────────────────────────────────────────────

    pub async fn list_branches(&self, owner: &str, repo: &str) -> Result<Vec<Branch>> {
        repository_api::repo_list_branches(&self.config, owner, repo, None, None)
            .await
            .context("list branches")
    }

    pub async fn create_branch(
        &self,
        owner: &str,
        repo: &str,
        new_branch: &str,
        from_branch: &str,
    ) -> Result<()> {
        let opts = models::CreateBranchRepoOption {
            new_branch_name: new_branch.to_string(),
            old_branch_name: Some(from_branch.to_string()),
            old_ref_name: Some(from_branch.to_string()),
        };
        repository_api::repo_create_branch(&self.config, owner, repo, Some(opts))
            .await
            .context("create branch")?;
        Ok(())
    }

    // ── Pull request operations ───────────────────────────────────────────────

    pub async fn create_pr(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> Result<PullRequest> {
        let opts = models::CreatePullRequestOption {
            title: Some(title.to_string()),
            body: Some(body.to_string()),
            head: Some(head.to_string()),
            base: Some(base.to_string()),
            ..Default::default()
        };
        repository_api::repo_create_pull_request(&self.config, owner, repo, Some(opts))
            .await
            .context("create PR")
    }

    // ── File creation ─────────────────────────────────────────────────────────

    pub async fn create_file(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        content: &str,
        message: &str,
        branch: &str,
    ) -> Result<()> {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(content);
        let opts = models::CreateFileOptions {
            content: encoded,
            message: Some(message.to_string()),
            branch: Some(branch.to_string()),
            ..Default::default()
        };
        repository_api::repo_create_file(&self.config, owner, repo, path, opts)
            .await
            .context("create file")?;
        Ok(())
    }

    // ── Repository management ─────────────────────────────────────────────────

    pub async fn create_repo(&self, name: &str) -> Result<()> {
        let opts = models::CreateRepoOption {
            name: name.to_string(),
            auto_init: Some(true),
            private: Some(false),
            ..Default::default()
        };
        repository_api::create_current_user_repo(&self.config, Some(opts))
            .await
            .context("create repo")?;
        Ok(())
    }

    pub async fn add_collaborator(&self, owner: &str, repo: &str, username: &str) -> Result<()> {
        let opts = models::AddCollaboratorOption {
            permission: Some(models::add_collaborator_option::Permission::Write),
        };
        repository_api::repo_add_collaborator(&self.config, owner, repo, username, Some(opts))
            .await
            .context("add collaborator")?;
        Ok(())
    }

    pub async fn delete_repo(&self, owner: &str, repo: &str) -> Result<()> {
        repository_api::repo_delete(&self.config, owner, repo)
            .await
            .context("delete repo")?;
        Ok(())
    }

    pub async fn get_pr(&self, owner: &str, repo: &str, number: u64) -> Result<PullRequest> {
        repository_api::repo_get_pull_request(&self.config, owner, repo, number as i64)
            .await
            .context("get PR")
    }

    pub async fn list_prs(&self, owner: &str, repo: &str, state: &str) -> Result<Vec<PullRequest>> {
        repository_api::repo_list_pull_requests(
            &self.config,
            owner,
            repo,
            Some(state),
            None,
            None,
            None,
            None,
            None,
            Some(50),
        )
        .await
        .context("list PRs")
    }

    pub async fn submit_review(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        body: &str,
        event: &str,
    ) -> Result<()> {
        let opts = models::CreatePullReviewOptions {
            body: Some(body.to_string()),
            event: Some(event.to_string()),
            ..Default::default()
        };
        repository_api::repo_create_pull_review(&self.config, owner, repo, pr_number as i64, opts)
            .await
            .context("submit review")?;
        Ok(())
    }

    pub async fn merge_pr(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        merge_style: &str,
    ) -> Result<()> {
        let do_action = match merge_style {
            "squash" => models::merge_pull_request_option::Do::Squash,
            "rebase" => models::merge_pull_request_option::Do::Rebase,
            _ => models::merge_pull_request_option::Do::Merge,
        };
        let opts = models::MergePullRequestOption {
            r#do: do_action,
            ..Default::default()
        };
        repository_api::repo_merge_pull_request(
            &self.config,
            owner,
            repo,
            pr_number as i64,
            Some(opts),
        )
        .await
        .context("merge PR")?;
        Ok(())
    }

    pub async fn add_pr_reviewer(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        reviewer_login: &str,
    ) -> Result<()> {
        let opts = models::PullReviewRequestOptions {
            reviewers: Some(vec![reviewer_login.to_string()]),
            ..Default::default()
        };
        repository_api::repo_create_pull_review_requests(
            &self.config,
            owner,
            repo,
            pr_number as i64,
            opts,
        )
        .await
        .context("add PR reviewer")?;
        Ok(())
    }

    pub async fn list_pr_files(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
    ) -> Result<Vec<ChangedFile>> {
        repository_api::repo_get_pull_request_files(
            &self.config,
            owner,
            repo,
            pr_number as i64,
            None,
            None,
            None,
            None,
        )
        .await
        .context("list PR files")
    }

    // ── Forgejo Actions API ──────────────────────────────────────────────────

    /// Trigger a workflow and return the run ID.
    pub async fn dispatch_workflow(
        &self,
        owner: &str,
        repo: &str,
        workflow: &str,
        git_ref: &str,
        inputs: &std::collections::HashMap<String, String>,
    ) -> Result<Option<u64>> {
        let opts = models::DispatchWorkflowOption {
            inputs: Some(inputs.clone()),
            r#ref: git_ref.to_string(),
            return_run_info: Some(true),
        };
        let result =
            repository_api::dispatch_workflow(&self.config, owner, repo, workflow, Some(opts))
                .await
                .context("dispatch workflow")?;
        Ok(result.id.map(|id| id as u64))
    }

    /// List recent action runs for a repository.
    pub async fn list_action_runs(&self, owner: &str, repo: &str) -> Result<Vec<ActionRun>> {
        let resp = repository_api::list_action_runs(
            &self.config,
            owner,
            repo,
            None,
            Some(10),
            Some(vec!["workflow_dispatch".to_string()]),
            None,
            None,
            None,
        )
        .await
        .context("list action runs")?;
        Ok(resp
            .workflow_runs
            .unwrap_or_default()
            .iter()
            .map(|r| ActionRun {
                id: r.id.unwrap_or(0) as u64,
                status: r.status.clone().unwrap_or_default(),
            })
            .collect())
    }

    /// Get a specific action run by ID.
    pub async fn get_action_run(&self, owner: &str, repo: &str, run_id: u64) -> Result<ActionRun> {
        let r = repository_api::action_run(&self.config, owner, repo, run_id as i64)
            .await
            .context("get action run")?;
        Ok(ActionRun {
            id: r.id.unwrap_or(0) as u64,
            status: r.status.unwrap_or_default(),
        })
    }
}
