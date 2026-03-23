use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use workflow_types::Job;
use workflow_worker::{forgejo::ForgejoClient, worker::{Outcome, Worker}};

pub struct SimWorker {
    worker_id: String,
    delay_secs: u64,
}

impl SimWorker {
    pub fn new(worker_id: String, delay_secs: u64) -> Self {
        Self { worker_id, delay_secs }
    }
}

#[async_trait]
impl Worker for SimWorker {
    fn worker_id(&self) -> &str {
        &self.worker_id
    }

    fn worker_type(&self) -> &str {
        "sim"
    }

    async fn execute(&self, job: &Job, forgejo: &ForgejoClient) -> Result<Outcome> {
        tracing::info!(
            key = %job.key(),
            title = %job.title,
            priority = job.priority,
            "simulating work for {}s",
            self.delay_secs
        );

        let branch_name = format!("work/{}/{}", self.worker_id, job.number);
        let file_path = format!("work/{}.md", job.number);
        let timestamp = chrono::Utc::now().to_rfc3339();
        let file_content = format!(
            "# {}\n\nWorker: {}\nTimestamp: {}\n",
            job.title, self.worker_id, timestamp
        );
        let commit_msg = format!(
            "work: {} (#{}) by {}",
            job.title, job.number, self.worker_id
        );
        let pr_body = format!(
            "Closes #{}\n\nSimulated work by {}",
            job.number, self.worker_id
        );

        match forgejo
            .create_branch(&job.repo_owner, &job.repo_name, &branch_name, "main")
            .await
        {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(key = %job.key(), error = %e, "branch creation failed (may already exist from rework), continuing");
            }
        }

        match forgejo
            .create_file(
                &job.repo_owner,
                &job.repo_name,
                &file_path,
                &file_content,
                &commit_msg,
                &branch_name,
            )
            .await
        {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(key = %job.key(), error = %e, "file creation failed, continuing");
            }
        }

        match forgejo
            .create_pr(
                &job.repo_owner,
                &job.repo_name,
                &job.title,
                &pr_body,
                &branch_name,
                "main",
            )
            .await
        {
            Ok(pr) => {
                tracing::info!(key = %job.key(), pr_number = pr.number, "opened PR");
            }
            Err(e) => {
                tracing::warn!(key = %job.key(), error = %e, "PR creation failed (may already exist), continuing");
            }
        }

        tokio::time::sleep(Duration::from_secs(self.delay_secs)).await;
        tracing::info!(key = %job.key(), "sim complete, yielding for review");
        Ok(Outcome::Yield)
    }
}
