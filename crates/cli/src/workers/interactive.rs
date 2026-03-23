use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv};
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use workflow_types::{HelpRequest, HelpResponse, Job, SessionInfo};
use workflow_worker::{forgejo::ForgejoClient, worker::{Outcome, Worker}};

pub const SESSIONS_BUCKET: &str = "workflow-sessions";

pub struct InteractiveWorker {
    worker_id: String,
    capability: Option<String>,
    platform_tags: Vec<String>,
    host: String,
    ttyd_bin: String,
    allowed_tools: Vec<String>,
    ttyd_port: u16,
    plugin_dirs: Vec<String>,
    nats: async_nats::Client,
}

impl InteractiveWorker {
    pub fn new(
        worker_id: String,
        capability: Option<String>,
        platform_tags: Vec<String>,
        host: String,
        ttyd_bin: String,
        allowed_tools: Vec<String>,
        ttyd_port: u16,
        plugin_dirs: Vec<String>,
        nats: async_nats::Client,
    ) -> Self {
        Self { worker_id, capability, platform_tags, host, ttyd_bin, allowed_tools, ttyd_port, plugin_dirs, nats }
    }
}

#[async_trait]
impl Worker for InteractiveWorker {
    fn worker_id(&self) -> &str {
        &self.worker_id
    }

    fn worker_type(&self) -> &str {
        "interactive"
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
        run_interactive_job(
            job,
            forgejo,
            &self.worker_id,
            &self.host,
            &self.ttyd_bin,
            &self.allowed_tools,
            self.ttyd_port,
            &self.plugin_dirs,
            &self.nats,
        )
        .await
    }
}

// ── Mode transitions ─────────────────────────────────────────────────────────

/// Result of a poll loop iteration — either a final outcome or a mode switch.
enum LoopResult {
    Done(Outcome),
    SwitchToInteractive { reason: String },
    SwitchToHeadless,
}

// ── Cleanup guard ─────────────────────────────────────────────────────────────

struct SessionCleanup {
    session_name: String,
    ttyd_child: Option<std::process::Child>,
}

impl SessionCleanup {
    fn kill_ttyd(&mut self) {
        if let Some(mut child) = self.ttyd_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn kill_tmux(&self) {
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &self.session_name])
            .output();
    }
}

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        self.kill_ttyd();
        self.kill_tmux();
    }
}

// ── Shared context ───────────────────────────────────────────────────────────

/// All the context both modes need, avoiding 18-argument functions.
struct JobContext<'a> {
    work_dir: std::path::PathBuf,
    session_name: String,
    job_key: String,
    worker_id: &'a str,
    host: &'a str,
    ttyd_bin: &'a str,
    ttyd_port: u16,
    owner: &'a str,
    repo: &'a str,
    number: u64,
}

// ── Main entry point ──────────────────────────────────────────────────────────

pub async fn run_interactive_job(
    job: &Job,
    forgejo: &ForgejoClient,
    worker_id: &str,
    host: &str,
    ttyd_bin: &str,
    allowed_tools: &[String],
    ttyd_port: u16,
    plugin_dirs: &[String],
    nats: &async_nats::Client,
) -> Result<Outcome> {
    let owner = &job.repo_owner;
    let repo = &job.repo_name;
    let number = job.number;
    let job_key = job.key();

    // ── 1. Create work directory ─────────────────────────────────────────────
    let safe_key = job_key.replace('/', "-");
    let work_dir = std::env::temp_dir().join(format!("workflow-{safe_key}"));
    tokio::fs::create_dir_all(&work_dir).await?;
    write_helper_scripts(&work_dir).await?;

    // ── 2. Fetch issue body and comments for the prompt ──────────────────────
    let issue_body = forgejo.get_issue_body(owner, repo, number).await.unwrap_or_default();
    let comments = forgejo.list_issue_comments(owner, repo, number).await.unwrap_or_default();
    let human_comments: Vec<String> = comments
        .iter()
        .filter_map(|c| c.body.clone())
        .collect();

    // On rework, extract review feedback from PR reviews (REQUEST_CHANGES state).
    let review_feedback: Vec<String> = if job.is_rework {
        fetch_review_feedback(forgejo, owner, repo, number).await
    } else {
        vec![]
    };

    // ── 3. Build context ─────────────────────────────────────────────────────
    let forgejo_url = std::env::var("FORGEJO_URL").unwrap_or_default();
    let forgejo_token = std::env::var("FORGEJO_TOKEN").unwrap_or_default();
    let work_dir_str = work_dir.to_string_lossy().to_string();

    let initial_prompt = build_initial_prompt(job, &issue_body, &human_comments, &review_feedback, &forgejo_url, &work_dir_str);
    let prompt_file = work_dir.join("prompt.txt");
    tokio::fs::write(&prompt_file, &initial_prompt).await?;

    let session_name = {
        let s = format!("wf-{}-{}-{}", owner, repo, number);
        if s.len() > 50 { s[..50].to_string() } else { s }
    };

    let claude_session_id = uuid::Uuid::new_v4().to_string();

    let non_empty_tools: Vec<&str> = allowed_tools.iter().map(|s| s.as_str()).filter(|s| !s.is_empty()).collect();
    let tools_flag = if non_empty_tools.is_empty() {
        String::new()
    } else {
        format!("--allowedTools {} ", non_empty_tools.join(","))
    };

    // Plugin dirs: use explicit list if provided, otherwise auto-discover from /opt/plugins/.
    let discovered_dirs: Vec<String>;
    let effective_plugin_dirs: &[String] = if plugin_dirs.iter().any(|s| !s.is_empty()) {
        plugin_dirs
    } else {
        discovered_dirs = std::fs::read_dir("/opt/plugins")
            .ok()
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().join(".claude-plugin/plugin.json").exists())
                    .map(|e| e.path().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        &discovered_dirs
    };
    let plugin_flag = effective_plugin_dirs.iter()
        .filter(|s| !s.is_empty())
        .map(|s| format!("--plugin-dir {} ", s))
        .collect::<String>();
    if !plugin_flag.is_empty() {
        eprintln!("Plugins: {}", effective_plugin_dirs.join(", "));
    }

    // Write launcher scripts (used by both modes).
    // All scripts use env vars set by the parent; prompt is piped via stdin
    // to avoid shell quoting issues with multiline text.
    let prompt_path = prompt_file.to_string_lossy();
    let env_block = format!(
        "export WORKFLOW_WORK_DIR='{work_dir_str}'\n\
         export WORKFLOW_JOB_KEY='{job_key}'\n\
         export FORGEJO_URL='{forgejo_url}'\n\
         export FORGEJO_TOKEN='{forgejo_token}'\n\
         export PATH='{work_dir_str}':\"$PATH\"\n\
         cd '{work_dir_str}'\n",
    );

    let print_launcher = work_dir.join("launch-print.sh");
    write_executable(&print_launcher, &format!(
        "#!/bin/sh\n{env_block}\
         PROMPT=$(cat '{prompt_path}')\n\
         exec claude --print \
           --session-id {claude_session_id} \
           --output-format stream-json --verbose \
           {tools_flag}{plugin_flag}\
           --permission-mode bypassPermissions \
           -p \"$PROMPT\"\n",
    ))?;

    // Resume-print: for returning to headless after interactive.
    let resume_print_launcher = work_dir.join("launch-resume-print.sh");
    write_executable(&resume_print_launcher, &format!(
        "#!/bin/sh\n{env_block}\
         exec claude --print \
           --resume {claude_session_id} \
           --output-format stream-json --verbose \
           {tools_flag}{plugin_flag}\
           --permission-mode bypassPermissions \
           -p 'Continue working on the task. Pick up where you left off.'\n",
    ))?;

    // Interactive resume: for attach mode (full TUI).
    // Uses --continue to pick up the most recent conversation from the headless run.
    let resume_launcher = work_dir.join("launch-resume.sh");
    write_executable(&resume_launcher, &format!(
        "#!/bin/sh\n{env_block}\
         exec claude --continue {tools_flag}{plugin_flag}\
           --permission-mode bypassPermissions\n",
    ))?;

    let ctx = JobContext {
        work_dir: work_dir.clone(),
        session_name: session_name.clone(),
        job_key: job_key.clone(),
        worker_id,
        host,
        ttyd_bin,
        ttyd_port,
        owner,
        repo,
        number,
    };

    // ── 4. Subscribe to NATS channels ────────────────────────────────────────
    let deliver_subject = format!("workflow.interact.deliver.{worker_id}");
    let mut deliver_sub = nats.subscribe(deliver_subject).await.context("subscribe to help delivery")?;

    let attach_subject = format!("workflow.interact.attach.{worker_id}");
    let mut attach_sub = nats.subscribe(attach_subject).await.context("subscribe to attach")?;

    let detach_subject = format!("workflow.interact.detach.{worker_id}");
    let mut detach_sub = nats.subscribe(detach_subject).await.context("subscribe to detach")?;

    let mut cleanup = SessionCleanup {
        session_name: session_name.clone(),
        ttyd_child: None,
    };

    let nats_key = job_key.replace('/', ".");

    // Drain any stale attach/detach messages from previous runs.
    loop {
        tokio::select! {
            biased;
            Some(_) = attach_sub.next() => continue,
            Some(_) = detach_sub.next() => continue,
            _ = tokio::time::sleep(Duration::from_millis(100)) => break,
        }
    }

    // ── 5. State machine: Headless ⇄ Interactive ─────────────────────────────
    let output_log = work_dir.join("claude-output.log");
    let mut is_first_run = true;
    let outcome = loop {
        // ── Headless phase ───────────────────────────────────────────────────
        tracing::info!(job_key = %ctx.job_key, "entering headless mode");

        let headless_launcher = if is_first_run {
            is_first_run = false;
            print_launcher.to_string_lossy().to_string()
        } else {
            resume_print_launcher.to_string_lossy().to_string()
        };

        // Truncate the log file for this headless run.
        let _ = std::fs::File::create(&output_log)?;
        let output_log_str = output_log.to_string_lossy();

        let mut claude_child = tokio::process::Command::new("sh")
            .args(["-c", &format!("{headless_launcher} > '{output_log_str}' 2>&1")])
            .kill_on_drop(true)
            .spawn()
            .context("spawn claude --print")?;

        // Start ttyd in read-only peek mode (tail -f the output log).
        let port = if ctx.ttyd_port != 0 { ctx.ttyd_port } else { find_free_port() };
        let peek_url = format!("http://{}:{port}", ctx.host);

        let peek_script = ctx.work_dir.join("peek-stream.sh");
        let ttyd_child = std::process::Command::new(ctx.ttyd_bin)
            .args([
                "--port", &port.to_string(),
                "--ping-interval", "5",
                "--readonly",
                &*peek_script.to_string_lossy(),
                &*output_log.to_string_lossy(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("spawn ttyd for peek")?;

        cleanup.ttyd_child = Some(ttyd_child);

        // Register peek session so the graph viewer can show the link.
        let session_info = SessionInfo {
            job_key: ctx.job_key.clone(),
            worker_id: ctx.worker_id.to_string(),
            session_url: peek_url.clone(),
            session_name: ctx.session_name.clone(),
            started_at: chrono::Utc::now(),
            mode: "peek".into(),
        };
        let _ = register_session(nats, &nats_key, &session_info).await;

        tracing::info!(job_key = %ctx.job_key, peek_url = %peek_url, "peek available");

        let result = run_headless_loop(
            &ctx,
            &mut claude_child,
            &mut attach_sub,
        ).await;

        // Tear down peek ttyd before switching modes.
        cleanup.kill_ttyd();

        match result {
            LoopResult::Done(outcome) => {
                let _ = deregister_session(nats, &nats_key).await;
                break Ok(outcome);
            }
            LoopResult::SwitchToInteractive { reason } => {
                let _ = claude_child.kill().await;
                let _ = claude_child.wait().await;
                let _ = deregister_session(nats, &nats_key).await;

                // Publish help request if agent-initiated.
                if reason.starts_with("agent:") {
                    let help_reason = reason.strip_prefix("agent:").unwrap_or(&reason);
                    let req = HelpRequest {
                        job_key: ctx.job_key.clone(),
                        worker_id: ctx.worker_id.to_string(),
                        reason: help_reason.to_string(),
                        session_hint: None,
                    };
                    let _ = nats.publish(
                        "workflow.interact.help",
                        Bytes::from(serde_json::to_vec(&req).unwrap_or_default()),
                    ).await;
                }

                tracing::info!(job_key = %ctx.job_key, %reason, "switching to interactive mode");
            }
            LoopResult::SwitchToHeadless => unreachable!(),
        }

        // ── Interactive phase ────────────────────────────────────────────────
        start_interactive_session(&ctx, &mut cleanup, nats, &nats_key, forgejo).await?;

        let result = run_interactive_loop(
            &ctx,
            nats,
            &mut deliver_sub,
            &mut detach_sub,
        ).await;

        // Tear down the interactive session.
        cleanup.kill_ttyd();
        cleanup.kill_tmux();
        let _ = deregister_session(nats, &nats_key).await;

        match result {
            LoopResult::Done(outcome) => break Ok(outcome),
            LoopResult::SwitchToHeadless => {
                tracing::info!(job_key = %ctx.job_key, "detaching — returning to headless mode");
            }
            LoopResult::SwitchToInteractive { .. } => unreachable!(),
        }
    };

    // ── Upload session recording (best-effort) ────────────────────────────
    // Read the output log before cleanup deletes the work directory.
    if output_log.exists() {
        match tokio::fs::read(&output_log).await {
            Ok(data) if !data.is_empty() => {
                let recording_key = format!(
                    "sessions/{}/{}/{}/{}",
                    owner, repo, number, session_name
                );
                tracing::debug!(
                    job_key = %job_key,
                    recording_key = %recording_key,
                    bytes = data.len(),
                    "uploading session recording"
                );
                let js = async_nats::jetstream::new(nats.clone());
                match js
                    .get_object_store("workflow-session-recordings")
                    .await
                {
                    Ok(store) => {
                        let mut reader = &data[..];
                        if let Err(e) = store.put(recording_key.as_str(), &mut reader).await {
                            tracing::warn!(
                                job_key = %job_key,
                                error = %e,
                                "failed to upload session recording"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            job_key = %job_key,
                            error = %e,
                            "failed to access recording object store"
                        );
                    }
                }
            }
            _ => {}
        }
    }

    // ── Cleanup ──────────────────────────────────────────────────────────────
    drop(cleanup);
    let _ = deregister_session(nats, &nats_key).await;
    let _ = tokio::fs::remove_dir_all(&work_dir).await;

    outcome
}

// ── Headless loop ────────────────────────────────────────────────────────────

async fn run_headless_loop(
    ctx: &JobContext<'_>,
    claude_child: &mut tokio::process::Child,
    attach_sub: &mut async_nats::Subscriber,
) -> LoopResult {
    let result_path = ctx.work_dir.join("result.txt");
    let help_req_path = ctx.work_dir.join("help_request.txt");

    loop {
        let switch = tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                // Check for result.
                if let Ok(content) = tokio::fs::read_to_string(&result_path).await {
                    let content = content.trim().to_string();
                    if !content.is_empty() {
                        let _ = claude_child.kill().await;
                        return LoopResult::Done(parse_agent_outcome(&content));
                    }
                }
                // Check for help request.
                if let Ok(content) = tokio::fs::read_to_string(&help_req_path).await {
                    let content = content.trim().to_string();
                    if !content.is_empty() {
                        let _ = tokio::fs::remove_file(&help_req_path).await;
                        Some(format!("agent:{content}"))
                    } else { None }
                } else { None }
            }
            Some(_) = attach_sub.next() => {
                Some("user requested interactive session".into())
            }
        };

        if let Some(reason) = switch {
            return LoopResult::SwitchToInteractive { reason };
        }

        // Check if --print exited.
        if let Ok(Some(status)) = claude_child.try_wait() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(content) = tokio::fs::read_to_string(&result_path).await {
                let content = content.trim().to_string();
                if !content.is_empty() {
                    return LoopResult::Done(parse_agent_outcome(&content));
                }
            }
            if status.success() {
                return LoopResult::Done(Outcome::Yield);
            } else {
                return LoopResult::Done(Outcome::Fail {
                    reason: format!("claude --print exited with status {status}"),
                    logs: None,
                });
            }
        }
    }
}

// ── Interactive loop ─────────────────────────────────────────────────────────

async fn run_interactive_loop(
    ctx: &JobContext<'_>,
    nats: &async_nats::Client,
    deliver_sub: &mut async_nats::Subscriber,
    detach_sub: &mut async_nats::Subscriber,
) -> LoopResult {
    let result_path = ctx.work_dir.join("result.txt");
    let help_req_path = ctx.work_dir.join("help_request.txt");
    let help_res_path = ctx.work_dir.join("help_response.txt");

    loop {
        let detach = tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                // Check for result.
                if let Ok(content) = tokio::fs::read_to_string(&result_path).await {
                    let content = content.trim().to_string();
                    if !content.is_empty() {
                        return LoopResult::Done(parse_agent_outcome(&content));
                    }
                }
                // Check for help request (deliver response inline).
                if let Ok(content) = tokio::fs::read_to_string(&help_req_path).await {
                    let content = content.trim().to_string();
                    if !content.is_empty() {
                        let _ = tokio::fs::remove_file(&help_req_path).await;
                        tracing::info!(job_key = %ctx.job_key, reason = %content, "agent requested help (interactive)");
                        let req = HelpRequest {
                            job_key: ctx.job_key.clone(),
                            worker_id: ctx.worker_id.to_string(),
                            reason: content,
                            session_hint: Some(format!(
                                "tmux attach -t {}\nor open browser terminal",
                                ctx.session_name
                            )),
                        };
                        let _ = nats.publish(
                            "workflow.interact.help",
                            Bytes::from(serde_json::to_vec(&req).unwrap_or_default()),
                        ).await;
                        // Wait for human response.
                        if let Some(msg) = deliver_sub.next().await {
                            if let Ok(resp) = serde_json::from_slice::<HelpResponse>(&msg.payload) {
                                let _ = tokio::fs::write(&help_res_path, &resp.message).await;
                            }
                        }
                    }
                }
                // Check if tmux session died — treat as detach (return to headless).
                if !tmux_session_alive(&ctx.session_name).await {
                    tracing::info!(job_key = %ctx.job_key, "tmux session ended — returning to headless mode");
                    return LoopResult::SwitchToHeadless;
                }
                false
            }
            Some(_) = detach_sub.next() => { true }
        };

        if detach {
            return LoopResult::SwitchToHeadless;
        }
    }
}

// ── Start interactive session ────────────────────────────────────────────────

async fn start_interactive_session(
    ctx: &JobContext<'_>,
    cleanup: &mut SessionCleanup,
    nats: &async_nats::Client,
    nats_key: &str,
    forgejo: &ForgejoClient,
) -> Result<()> {
    // Kill any stale session.
    cleanup.kill_ttyd();
    cleanup.kill_tmux();

    let resume_launcher = ctx.work_dir.join("launch-resume.sh");

    // Start tmux.
    std::process::Command::new("tmux")
        .args([
            "new-session", "-d",
            "-s", &ctx.session_name,
            "-x", "220", "-y", "50",
            &*resume_launcher.to_string_lossy(),
        ])
        .status()
        .context("start tmux session")?;

    // Start ttyd.
    let port = if ctx.ttyd_port != 0 { ctx.ttyd_port } else { find_free_port() };
    let session_url = format!("http://{}:{port}", ctx.host);

    // Use a wrapper that re-attaches on disconnect so ttyd survives reconnects.
    let attach_wrapper = ctx.work_dir.join("ttyd-attach.sh");
    write_executable(&attach_wrapper, &format!(
        "#!/bin/sh\nwhile tmux has-session -t '{}' 2>/dev/null; do\n  tmux attach-session -t '{}'\n  sleep 0.5\ndone\n",
        ctx.session_name, ctx.session_name,
    ))?;

    let ttyd_child = std::process::Command::new(ctx.ttyd_bin)
        .args([
            "--port", &port.to_string(),
            "--ping-interval", "5",
            "-W",
            &*attach_wrapper.to_string_lossy(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawn ttyd")?;

    cleanup.ttyd_child = Some(ttyd_child);

    // Register session.
    let session_info = SessionInfo {
        job_key: ctx.job_key.clone(),
        worker_id: ctx.worker_id.to_string(),
        session_url: session_url.clone(),
        session_name: ctx.session_name.clone(),
        started_at: chrono::Utc::now(),
        mode: "interactive".into(),
    };
    let _ = register_session(nats, nats_key, &session_info).await;

    // Post comment.
    let comment = format!(
        "<!-- workflow:agent-session -->\n\n\
         🖥️ **Interactive session started** (worker `{}`)\n\n\
         **Open terminal:** [{session_url}]({session_url})\n\n\
         Or attach locally: `tmux attach -t {}`",
        ctx.worker_id, ctx.session_name,
    );
    if let Err(e) = forgejo.post_comment(ctx.owner, ctx.repo, ctx.number, &comment).await {
        tracing::warn!("failed to post session comment: {e:#}");
    }

    tracing::info!(
        job_key = %ctx.job_key,
        session_url = %session_url,
        "interactive session started"
    );

    Ok(())
}

// ── Review feedback extraction ───────────────────────────────────────────────

/// Fetch review feedback from PR reviews with REQUEST_CHANGES state.
/// Finds the linked PR for an issue and extracts the review body.
async fn fetch_review_feedback(
    forgejo: &ForgejoClient,
    owner: &str,
    repo: &str,
    issue_number: u64,
) -> Vec<String> {
    // Find linked PR by searching for "Closes #N".
    let prs = match forgejo.list_prs(owner, repo, "open").await {
        Ok(prs) => prs,
        Err(_) => return vec![],
    };
    let pattern = format!("Closes #{issue_number}");
    let pr = match prs.into_iter().find(|pr| {
        pr.body.as_deref().map(|b| b.contains(&pattern)).unwrap_or(false)
    }) {
        Some(pr) => pr,
        None => return vec![],
    };
    let pr_number = pr.number.unwrap_or(0);
    if pr_number == 0 {
        return vec![];
    }

    // Fetch PR reviews and extract REQUEST_CHANGES bodies.
    let url = format!(
        "{}/api/v1/repos/{owner}/{repo}/pulls/{pr_number}/reviews",
        forgejo.base_url()
    );
    let reviews: Vec<serde_json::Value> = match forgejo.get_json(&url).await {
        Ok(r) => r,
        Err(_) => return vec![],
    };

    reviews
        .iter()
        .filter(|r| r.get("state").and_then(|s| s.as_str()) == Some("REQUEST_CHANGES"))
        .filter_map(|r| {
            r.get("body")
                .and_then(|b| b.as_str())
                .filter(|b| !b.is_empty())
                .map(|b| b.to_string())
        })
        .collect()
}

// ── Outcome parsing ──────────────────────────────────────────────────────────

fn parse_agent_outcome(s: &str) -> Outcome {
    if s == "yield" {
        Outcome::Yield
    } else if s == "complete" {
        Outcome::Complete
    } else if let Some(reason) = s.strip_prefix("fail: ") {
        Outcome::Fail { reason: reason.to_string(), logs: None }
    } else {
        tracing::warn!("unrecognised agent result: {s:?}, defaulting to yield");
        Outcome::Yield
    }
}

// ── Helper scripts ────────────────────────────────────────────────────────────

async fn write_helper_scripts(work_dir: &Path) -> Result<()> {
    write_executable(
        &work_dir.join("workflow-request-help"),
        r#"#!/bin/sh
# Usage: workflow-request-help "reason"
REASON="${1:-I need help}"
printf '%s' "$REASON" > "$WORKFLOW_WORK_DIR/help_request.txt"
while [ ! -f "$WORKFLOW_WORK_DIR/help_response.txt" ]; do sleep 0.3; done
cat "$WORKFLOW_WORK_DIR/help_response.txt"
rm -f "$WORKFLOW_WORK_DIR/help_response.txt"
"#,
    )?;
    write_executable(
        &work_dir.join("workflow-yield"),
        "#!/bin/sh\necho yield > \"$WORKFLOW_WORK_DIR/result.txt\"\necho 'Reported: yield'\n",
    )?;
    write_executable(
        &work_dir.join("workflow-complete"),
        "#!/bin/sh\necho complete > \"$WORKFLOW_WORK_DIR/result.txt\"\necho 'Reported: complete'\n",
    )?;
    write_executable(
        &work_dir.join("workflow-fail"),
        "#!/bin/sh\nREASON=\"${1:-no reason given}\"\nprintf 'fail: %s' \"$REASON\" > \"$WORKFLOW_WORK_DIR/result.txt\"\necho 'Reported: fail'\n",
    )?;
    // Peek formatter: streams the json log and renders readable output with colors.
    write_executable(
        &work_dir.join("peek-stream.sh"),
        r#"#!/bin/sh
tail -f "$1" 2>/dev/null | jq -rj --unbuffered '
  if .type == "assistant" then
    (.message.content // [])[] |
    if .type == "thinking" then
      "\u001b[2m💭 " + (.thinking | split("\n")[0:3] | join("\n   ")) + "\u001b[0m\n"
    elif .type == "text" then
      "\u001b[37m" + .text + "\u001b[0m\n"
    elif .type == "tool_use" then
      "\u001b[36m🔧 " + .name + "\u001b[0m"
      + if .name == "Bash" then " $ " + (.input.command // "" | split("\n")[0][:120])
        elif .name == "Read" then " 📄 " + (.input.file_path // "")
        elif .name == "Write" then
          " ✏️  " + (.input.file_path // "")
          + " (" + ((.input.content // "") | length | tostring) + " chars)\n"
          + "\u001b[2m" + ((.input.content // "") | split("\n")[0:8] | join("\n") | .[:500]) + "\u001b[0m"
        elif .name == "Edit" then
          " ✏️  " + (.input.file_path // "") + "\n"
          + "\u001b[31m- " + ((.input.old_string // "") | split("\n")[0:5] | join("\n- ") | .[:300]) + "\u001b[0m\n"
          + "\u001b[32m+ " + ((.input.new_string // "") | split("\n")[0:5] | join("\n+ ") | .[:300]) + "\u001b[0m"
        elif .name == "Grep" then " 🔍 " + (.input.pattern // "")
        elif .name == "Glob" then " 📁 " + (.input.pattern // "")
        else " " + (.input | tostring | .[:100])
        end + "\n"
    else empty
    end
  elif .type == "tool_result" then
    "\u001b[2m   ↳ done\u001b[0m\n"
  elif .type == "result" then
    "\n\u001b[32m✅ Session complete\u001b[0m\n"
  else empty
  end
'
"#,
    )?;
    Ok(())
}

fn write_executable(path: &Path, content: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, content)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

// ── Prompt construction ───────────────────────────────────────────────────────

fn build_initial_prompt(
    job: &Job,
    issue_body: &str,
    comments: &[String],
    review_feedback: &[String],
    forgejo_url: &str,
    work_dir: &str,
) -> String {
    let deps = if job.dependency_numbers.is_empty() {
        String::new()
    } else {
        let ns: Vec<String> = job.dependency_numbers.iter().map(|n| format!("#{n}")).collect();
        format!("\nDependencies (already completed): {}", ns.join(", "))
    };
    let body_section = if issue_body.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n## Issue Description\n\n{issue_body}")
    };
    let comments_section = if comments.is_empty() {
        String::new()
    } else {
        format!("\n\n## Discussion\n\n{}", comments.join("\n\n---\n\n"))
    };

    // Detect merge conflict feedback vs code review feedback.
    let is_merge_conflict = job.is_rework
        && review_feedback.iter().any(|f| f.contains("Merge Conflict") || f.contains("rebase"));

    let (rework_section, instructions) = if job.is_rework {
        let feedback = if review_feedback.is_empty() {
            "The reviewer requested changes but no specific feedback was found.".to_string()
        } else {
            review_feedback.join("\n\n---\n\n")
        };

        let section = format!(
            "\n\n## Review Feedback (CHANGES REQUESTED)\n\n{feedback}"
        );
        let instr = if is_merge_conflict {
            format!(
                "## Instructions (rebase)\n\
                 \n\
                 Your PR has merge conflicts with the base branch. You ONLY need to rebase — do NOT re-implement the feature.\n\
                 \n\
                 1. Clone the repository: `git clone $FORGEJO_URL/{owner}/{repo}`\n\
                 2. Check out your existing branch: `git checkout work/agent/{number}`\n\
                 3. Rebase on main: `git rebase origin/main`\n\
                 4. Resolve any conflicts (keep your changes where possible).\n\
                 5. Force-push: `git push --force origin work/agent/{number}`\n\
                 6. Run `workflow-yield` to signal completion.\n\
                 \n\
                 Do NOT create a new branch or new PR. Do NOT re-implement the feature. Just rebase and resolve conflicts.",
                owner = job.repo_owner,
                repo = job.repo_name,
                number = job.number,
            )
        } else {
            format!(
                "## Instructions (rework)\n\
                 \n\
                 This is a REWORK assignment — a reviewer requested changes on your previous PR.\n\
                 \n\
                 1. Clone the repository and check out the existing branch `work/agent/{number}`.\n\
                 2. Read the review feedback above carefully.\n\
                 3. Address each point raised by the reviewer.\n\
                 4. Commit your fixes and push to the same branch.\n\
                 5. Run `workflow-yield` to signal completion.",
                number = job.number,
            )
        };
        (section, instr)
    } else {
        (
            String::new(),
            format!(
                "## Instructions\n\
                 \n\
                 1. Read the issue and understand what needs to be done.\n\
                 2. Clone the repository and explore the relevant code.\n\
                 3. Implement the changes.\n\
                 4. Commit on a branch named `work/agent/{number}` and open a PR with `Closes #{number}` in the body.\n\
                 5. Run `workflow-yield` to signal completion.",
                number = job.number,
            ),
        )
    };

    // For merge conflict rework, suppress the issue body and comments to keep
    // the prompt focused on rebasing. For normal work or code review rework,
    // include the full context.
    let body_context = if is_merge_conflict {
        String::new()
    } else {
        format!("{body_section}{comments_section}")
    };

    format!(
        "You are an AI agent in the Forge workflow system.\n\
         \n\
         ## Job\n\
         \n\
         Ref: {owner}/{repo}/#{number}\n\
         Title: {title}{deps}\n\
         Repository: {forgejo_url}/{owner}/{repo}{body_context}{rework_section}\n\
         \n\
         ## Working environment\n\
         \n\
         Your work directory: {work_dir}\n\
         Clone the repo: `git clone $FORGEJO_URL/{owner}/{repo}`\n\
         \n\
         ## Reporting outcomes\n\
         \n\
         When you finish and have opened a PR:\n\
           `workflow-yield`\n\
         \n\
         If you need human input before you can continue:\n\
           `workflow-request-help \"describe what you need\"`\n\
           (Execution pauses; the human's response is printed to stdout when they reply.)\n\
         \n\
         If the task cannot be completed:\n\
           `workflow-fail \"reason\"`\n\
         \n\
         {instructions}",
        owner = job.repo_owner,
        repo = job.repo_name,
        number = job.number,
        title = job.title,
    )
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn find_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind to find free port")
        .local_addr()
        .unwrap()
        .port()
}

async fn tmux_session_alive(session_name: &str) -> bool {
    tokio::process::Command::new("tmux")
        .args(["has-session", "-t", session_name])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn open_sessions_kv(nats: &async_nats::Client) -> Result<kv::Store> {
    let js = jetstream::new(nats.clone());
    match js.get_key_value(SESSIONS_BUCKET).await {
        Ok(kv) => Ok(kv),
        Err(_) => js
            .create_key_value(kv::Config {
                bucket: SESSIONS_BUCKET.to_string(),
                history: 1,
                ..Default::default()
            })
            .await
            .context("open workflow-sessions KV"),
    }
}

async fn register_session(nats: &async_nats::Client, nats_key: &str, info: &SessionInfo) -> Result<()> {
    let kv = open_sessions_kv(nats).await?;
    kv.put(nats_key, Bytes::from(serde_json::to_vec(info)?)).await.context("put session")?;
    Ok(())
}

async fn deregister_session(nats: &async_nats::Client, nats_key: &str) -> Result<()> {
    if let Ok(kv) = open_sessions_kv(nats).await {
        let _ = kv.purge(nats_key).await;
    }
    Ok(())
}
