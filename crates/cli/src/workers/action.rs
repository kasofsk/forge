use std::time::Duration;

use anyhow::{bail, Result};
use async_trait::async_trait;
use workflow_types::Job;
use workflow_worker::{
    forgejo::ForgejoClient,
    worker::{Outcome, Worker},
};

const MAX_POLL_ERRORS: u32 = 5;

pub struct ActionWorker {
    worker_id: String,
    workflow: String,
    runner: Option<String>,
    capability: Option<String>,
    platform_tags: Vec<String>,
    git_ref: String,
    poll_secs: u64,
}

impl ActionWorker {
    pub fn new(
        worker_id: String,
        workflow: String,
        runner: Option<String>,
        capability: Option<String>,
        platform_tags: Vec<String>,
        git_ref: String,
        poll_secs: u64,
    ) -> Self {
        Self { worker_id, workflow, runner, capability, platform_tags, git_ref, poll_secs }
    }
}

#[async_trait]
impl Worker for ActionWorker {
    fn worker_id(&self) -> &str {
        &self.worker_id
    }

    fn worker_type(&self) -> &str {
        "action"
    }

    fn capabilities(&self) -> Vec<String> {
        self.capability.iter().cloned().collect()
    }

    fn platform(&self) -> Vec<String> {
        self.platform_tags.clone()
    }

    fn accepts(&self, job: &Job) -> bool {
        // Accept any job whose required capabilities are a subset of ours.
        let w_caps: Vec<&String> = self.capability.iter().collect();
        job.capabilities.iter().all(|r| w_caps.contains(&r))
    }

    async fn execute(&self, job: &Job, forgejo: &ForgejoClient) -> Result<Outcome> {
        let runner_label = self.runner.as_deref().unwrap_or("ubuntu-latest");

        let branch_name = format!("work/action/{}", job.number);
        match forgejo
            .create_branch(&job.repo_owner, &job.repo_name, &branch_name, &self.git_ref)
            .await
        {
            Ok(()) => {}
            Err(e) => {
                tracing::debug!(key = %job.key(), error = %e, "branch may already exist, continuing");
            }
        }

        tracing::info!(
            key = %job.key(),
            title = %job.title,
            runner = runner_label,
            workflow = %self.workflow,
            branch = %branch_name,
            "dispatching action"
        );

        let mut inputs = std::collections::HashMap::new();
        inputs.insert("issue_number".to_string(), job.number.to_string());
        inputs.insert("runner_label".to_string(), runner_label.to_string());
        inputs.insert(
            "forgejo_url".to_string(),
            std::env::var("FORGEJO_URL").unwrap_or_default(),
        );

        let run_id = match forgejo
            .dispatch_workflow(
                &job.repo_owner,
                &job.repo_name,
                &self.workflow,
                &branch_name,
                &inputs,
            )
            .await?
        {
            Some(id) => id,
            None => {
                bail!(
                    "dispatch_workflow returned no run ID for {} (workflow={}, ref={})",
                    job.key(),
                    self.workflow,
                    branch_name,
                );
            }
        };

        tracing::info!(key = %job.key(), run_id, "tracking action run");

        // Poll until the action run completes. The dispatcher's lease/timeout
        // monitoring handles deadlines — we don't duplicate that here. Transient
        // API errors are tolerated up to MAX_POLL_ERRORS consecutive failures.
        let mut consecutive_errors: u32 = 0;

        loop {
            tokio::time::sleep(Duration::from_secs(self.poll_secs)).await;

            let run = match forgejo
                .get_action_run(&job.repo_owner, &job.repo_name, run_id)
                .await
            {
                Ok(run) => {
                    consecutive_errors = 0;
                    run
                }
                Err(e) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_POLL_ERRORS {
                        return Ok(Outcome::Fail {
                            reason: format!(
                                "action run {run_id}: {MAX_POLL_ERRORS} consecutive poll errors, last: {e:#}"
                            ),
                            logs: None,
                        });
                    }
                    tracing::warn!(
                        key = %job.key(),
                        run_id,
                        error = %e,
                        attempt = consecutive_errors,
                        "transient poll error, retrying"
                    );
                    continue;
                }
            };

            if run.is_completed() {
                if run.is_success() {
                    tracing::info!(key = %job.key(), run_id, "action run succeeded, yielding to CDC");
                    return Ok(Outcome::Yield);
                } else {
                    let reason =
                        format!("action run {run_id} finished with status={}", run.status);
                    tracing::warn!(key = %job.key(), run_id, %reason, "action run failed");
                    return Ok(Outcome::Fail { reason, logs: None });
                }
            }

            tracing::debug!(
                key = %job.key(),
                run_id,
                status = %run.status,
                "action still running"
            );
        }
    }
}
