use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use serde::Deserialize;
use workflow_types::{HelpResponse, Job, JobState, RequeueRequest, RequeueTarget};
use workflow_worker::{
    client::SidecarClient,
    forgejo::ForgejoClient,
    DispatchedWorkerLoop,
};

mod workers;
use workers::{ActionWorker, InteractiveWorker, SimWorker};

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "worker-cli",
    about = "Interactive CLI worker for testing the workflow system"
)]
struct Args {
    /// Sidecar base URL
    #[arg(long, env = "SIDECAR_URL", default_value = "http://localhost:3000")]
    sidecar_url: String,

    /// Forgejo base URL (required for content ops)
    #[arg(long, env = "FORGEJO_URL", default_value = "")]
    forgejo_url: String,

    /// Forgejo API token
    #[arg(long, env = "FORGEJO_TOKEN", default_value = "")]
    forgejo_token: String,

    /// Worker ID shown as the Forgejo assignee
    #[arg(long, env = "WORKER_ID", default_value = "cli-worker")]
    worker_id: String,

    /// Heartbeat interval in seconds (background task while a job is held)
    #[arg(long, default_value = "30")]
    heartbeat_secs: u64,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List jobs (optionally filter by state)
    Jobs {
        /// State filter, e.g. "on-deck", "on-the-stack", "failed"
        #[arg(long)]
        state: Option<String>,
    },

    /// Show full details of a single job
    Show {
        /// owner/repo/number
        job: String,
    },

    /// Requeue a failed or on-ice job
    Requeue {
        /// owner/repo/number
        job: String,
        /// Target state: "on-deck" or "on-ice"
        #[arg(long, default_value = "on-deck")]
        target: String,
        /// NATS server URL
        #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4223")]
        nats_url: String,
    },

    /// Run a simulated dispatched worker that auto-completes jobs after a delay
    Sim {
        /// NATS server URL
        #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4223")]
        nats_url: String,
        /// Seconds to "work" on each job before completing
        #[arg(long, default_value = "10")]
        delay_secs: u64,
    },

    /// Run an action worker that triggers Forgejo Actions workflows for each job
    Action {
        /// NATS server URL
        #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4223")]
        nats_url: String,
        /// Workflow filename to dispatch (e.g. "agent-work.yml")
        #[arg(long, env = "ACTION_WORKFLOW", default_value = "agent-work.yml")]
        workflow: String,
        /// Runner label this worker targets
        #[arg(long, env = "ACTION_RUNNER")]
        runner: Option<String>,
        /// Capability tag this worker accepts (jobs must have matching capability:X label)
        #[arg(long, env = "ACTION_CAPABILITY")]
        capability: Option<String>,
        /// Platform tags this worker advertises (comma-separated, e.g. "linux,arm64")
        #[arg(long, env = "WORKER_PLATFORM", value_delimiter = ',')]
        platform: Vec<String>,
        /// Git ref to dispatch the workflow on
        #[arg(long, default_value = "main")]
        git_ref: String,
        /// Poll interval in seconds when waiting for the action run to complete
        #[arg(long, default_value = "10")]
        poll_secs: u64,
    },

    /// Run an interactive Claude agent worker that opens a web terminal for each job
    Agent {
        /// NATS server URL
        #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4223")]
        nats_url: String,
        /// Capability tag this worker accepts (jobs must have matching capability:X label)
        #[arg(long, env = "AGENT_CAPABILITY")]
        capability: Option<String>,
        /// Platform tags this worker advertises (comma-separated, e.g. "macos,arm64")
        #[arg(long, env = "WORKER_PLATFORM", value_delimiter = ',')]
        platform: Vec<String>,
        /// Host/IP used when building the session URL posted to Forgejo
        #[arg(long, env = "AGENT_HOST", default_value = "localhost")]
        host: String,
        /// Path to ttyd binary (falls back to PATH lookup)
        #[arg(long, default_value = "ttyd")]
        ttyd_bin: String,
        /// Tools Claude is allowed to use (comma-separated, e.g. "Bash,Read,Write").
        /// Passed as --allowedTools to the claude CLI. Empty = claude's defaults.
        #[arg(long, env = "AGENT_ALLOWED_TOOLS", value_delimiter = ',')]
        allowed_tools: Vec<String>,
        /// Fixed port for the ttyd web terminal. 0 = pick a free port automatically.
        #[arg(long, env = "TTYD_PORT", default_value = "0")]
        ttyd_port: u16,
        /// Claude Code plugin directories (comma-separated paths).
        /// Each path is passed as --plugin-dir to the claude CLI.
        #[arg(long, env = "AGENT_PLUGIN_DIRS", value_delimiter = ',')]
        plugin_dirs: Vec<String>,
    },

    /// Interact with a running agent session
    Interact {
        #[command(subcommand)]
        cmd: InteractCmd,
    },

    /// Seed a Forgejo repo with jobs from a fixture file
    Seed {
        /// owner/repo to create issues in
        repo: String,
        /// Path to a fixture JSON file (e.g. demo/fixtures/chain.json)
        fixture: String,
        /// Create the repo if it doesn't exist
        #[arg(long)]
        create_repo: bool,
    },
}

// ── Interact subcommands ──────────────────────────────────────────────────────

#[derive(Subcommand)]
enum InteractCmd {
    /// Send a response to an agent that requested human help
    Respond {
        /// owner/repo/number
        job: String,
        /// Message to send to the agent
        message: String,
        /// NATS server URL
        #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4223")]
        nats_url: String,
    },
    /// Request that a running headless agent switch to interactive mode
    Attach {
        /// Worker ID to attach to (e.g. worker-aria)
        worker_id: String,
        /// NATS server URL
        #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4223")]
        nats_url: String,
    },
    /// Detach an interactive session back to headless mode
    Detach {
        /// Worker ID to detach (e.g. worker-aria)
        worker_id: String,
        /// NATS server URL
        #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4223")]
        nats_url: String,
    },
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("worker_cli=info".parse().unwrap()),
        )
        .init();

    let args = Args::parse();
    let sidecar = SidecarClient::new(&args.sidecar_url);

    match &args.command {
        Cmd::Jobs { state } => cmd_jobs(&sidecar, state.as_deref()).await,

        Cmd::Show { job } => cmd_show(&sidecar, job).await,

        Cmd::Requeue { job, target, nats_url } => {
            let (owner, repo, number) = parse_job_ref(job)?;
            let target = match target.as_str() {
                "on-deck" => RequeueTarget::OnDeck,
                "on-ice" => RequeueTarget::OnIce,
                other => bail!("unknown target: {other}; use 'on-deck' or 'on-ice'"),
            };
            let nats = async_nats::connect(nats_url)
                .await
                .context("connect to NATS")?;
            let req = RequeueRequest {
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
                target,
            };
            let payload = Bytes::from(serde_json::to_vec(&req)?);
            let reply = nats
                .request("workflow.admin.requeue".to_string(), payload)
                .await
                .context("NATS request to admin.requeue")?;
            let body: serde_json::Value = serde_json::from_slice(&reply.payload)
                .unwrap_or(serde_json::json!({"raw": String::from_utf8_lossy(&reply.payload).to_string()}));
            if body.get("error").is_some() {
                bail!("requeue failed: {}", body["error"]);
            }
            println!("Requeued {job}");
            Ok(())
        }

        Cmd::Sim { nats_url, delay_secs } => {
            let worker = SimWorker::new(args.worker_id.clone(), *delay_secs);
            let loop_ = DispatchedWorkerLoop::new(
                worker,
                &args.forgejo_url,
                &args.forgejo_token,
                nats_url,
                Duration::from_secs(args.heartbeat_secs),
            )
            .await?;
            println!(
                "Starting sim worker '{}' (delay={}s, nats={})",
                args.worker_id, delay_secs, nats_url
            );
            loop_.run().await
        }

        Cmd::Action {
            nats_url,
            workflow,
            runner,
            capability,
            platform,
            git_ref,
            poll_secs,
        } => {
            let worker = ActionWorker::new(
                args.worker_id.clone(),
                workflow.clone(),
                runner.clone(),
                capability.clone(),
                platform.clone(),
                git_ref.clone(),
                *poll_secs,
            );
            let runner_desc = runner.as_deref().unwrap_or("any");
            let cap_desc = capability.as_deref().unwrap_or("any");
            let plat_desc = if platform.is_empty() { "any".to_string() } else { platform.join(",") };
            let loop_ = DispatchedWorkerLoop::new(
                worker,
                &args.forgejo_url,
                &args.forgejo_token,
                nats_url,
                Duration::from_secs(args.heartbeat_secs),
            )
            .await?;
            println!(
                "Starting action worker '{}' (workflow={}, runner={}, capability={}, platform={}, nats={})",
                args.worker_id, workflow, runner_desc, cap_desc, plat_desc, nats_url
            );
            loop_.run().await
        }

        Cmd::Agent { nats_url, capability, platform, host, ttyd_bin, allowed_tools, ttyd_port, plugin_dirs } => {
            let nats = async_nats::connect(nats_url)
                .await
                .context("connect to NATS")?;
            let worker = InteractiveWorker::new(
                args.worker_id.clone(),
                capability.clone(),
                platform.clone(),
                host.clone(),
                ttyd_bin.clone(),
                allowed_tools.clone(),
                *ttyd_port,
                plugin_dirs.clone(),
                nats.clone(),
            );
            let loop_ = DispatchedWorkerLoop::new(
                worker,
                &args.forgejo_url,
                &args.forgejo_token,
                nats_url,
                Duration::from_secs(args.heartbeat_secs),
            )
            .await?;
            let plat_desc = if platform.is_empty() { "any".to_string() } else { platform.join(",") };
            println!(
                "Starting agent worker '{}' (capability={}, platform={}, nats={})",
                args.worker_id,
                capability.as_deref().unwrap_or("any"),
                plat_desc,
                nats_url
            );
            loop_.run().await
        }

        Cmd::Interact { cmd } => match cmd {
            InteractCmd::Respond { job, message, nats_url } => {
                let (owner, repo, number) = parse_job_ref(job)?;
                let nats_key = format!("{}.{}.{}", owner, repo, number);
                let subject = format!("workflow.interact.respond.{nats_key}");
                let nats = async_nats::connect(nats_url)
                    .await
                    .context("connect to NATS")?;
                let resp = HelpResponse { message: message.clone() };
                nats.publish(subject, Bytes::from(serde_json::to_vec(&resp)?))
                    .await
                    .context("publish help response")?;
                nats.flush().await.context("flush")?;
                println!("Response sent to {owner}/{repo}/#{number}.");
                Ok(())
            }
            InteractCmd::Attach { worker_id, nats_url } => {
                let nats = async_nats::connect(nats_url).await.context("connect to NATS")?;
                nats.publish(format!("workflow.interact.attach.{worker_id}"), Bytes::new()).await?;
                nats.flush().await?;
                println!("Attach request sent to {worker_id}.");
                Ok(())
            }
            InteractCmd::Detach { worker_id, nats_url } => {
                let nats = async_nats::connect(nats_url).await.context("connect to NATS")?;
                nats.publish(format!("workflow.interact.detach.{worker_id}"), Bytes::new()).await?;
                nats.flush().await?;
                println!("Detach request sent to {worker_id} — returning to headless mode.");
                Ok(())
            }
        },

        Cmd::Seed { repo, fixture, create_repo } => {
            let forgejo = ForgejoClient::new(&args.forgejo_url, &args.forgejo_token);
            let parts: Vec<&str> = repo.splitn(2, '/').collect();
            if parts.len() != 2 {
                bail!("expected owner/repo, got: {repo}");
            }
            if *create_repo {
                match forgejo.create_repo(parts[1]).await {
                    Ok(()) => println!("Created repo {repo}"),
                    Err(e) => {
                        let msg = format!("{e}");
                        if msg.contains("409") {
                            println!("Repo {repo} already exists");
                        } else {
                            return Err(e.context("create repo"));
                        }
                    }
                }
                // Provision standard labels if repo is new.
                let existing = forgejo.list_repo_labels(parts[0], parts[1]).await.unwrap_or_default();
                if existing.is_empty() {
                    provision_labels(&forgejo, parts[0], parts[1]).await?;
                    println!("Labels provisioned");
                }
            }
            cmd_seed(&forgejo, &sidecar, parts[0], parts[1], fixture).await
        }
    }
}

// ── commands ──────────────────────────────────────────────────────────────────

async fn cmd_jobs(sidecar: &SidecarClient, state: Option<&str>) -> Result<()> {
    let label = state.map(|s| {
        if s.starts_with("status:") {
            s.to_string()
        } else {
            format!("status:{s}")
        }
    });
    let jobs = sidecar.list_jobs(label.as_deref()).await?;

    if jobs.is_empty() {
        println!("No jobs found.");
        return Ok(());
    }

    print_job_table(&jobs);
    Ok(())
}

async fn cmd_show(sidecar: &SidecarClient, job_ref: &str) -> Result<()> {
    let (owner, repo, number) = parse_job_ref(job_ref)?;
    let resp = sidecar.get_job(owner, repo, number).await?;
    print_job_detail(&resp.job);
    if let Some(claim) = resp.claim {
        println!(
            "Claimed by: {}  (heartbeat: {})",
            claim.worker_id,
            claim.last_heartbeat.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }
    if let Some(failure) = resp.failure {
        println!("\nFailure ({:?}): {}", failure.kind, failure.reason);
    }
    Ok(())
}

// ── seed command ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FixtureJob {
    id: String,
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    depends_on: Vec<String>,
}

/// Create the standard workflow labels on a new repo.
async fn provision_labels(forgejo: &ForgejoClient, owner: &str, repo: &str) -> Result<()> {
    let labels = [
        ("status:on-ice", "#636e7d"),
        ("status:blocked", "#e4a84a"),
        ("status:on-deck", "#2ecc71"),
        ("status:on-the-stack", "#3498db"),
        ("status:needs-help", "#f97316"),
        ("status:in-review", "#8b5cf6"),
        ("status:changes-requested", "#f59e0b"),
        ("status:done", "#4a6356"),
        ("status:failed", "#e74c3c"),
        ("status:revoked", "#6b7280"),
        ("worker:interactive", "#3498db"),
        ("worker:action", "#2ecc71"),
        ("worker:sim", "#636e7d"),
        ("capability:rust", "#e74c3c"),
        ("capability:frontend", "#8b5cf6"),
        ("review:low", "#2ecc71"),
        ("review:medium", "#e4a84a"),
        ("review:high", "#e74c3c"),
        ("review:human", "#8b5cf6"),
    ];
    // Also create priority labels 25..=95 in steps of 5.
    for (name, color) in &labels {
        let _ = forgejo.create_label(owner, repo, name, color).await;
    }
    for p in (25..=95).step_by(5) {
        let name = format!("priority:{p}");
        let color = if p >= 80 { "#e74c3c" } else if p >= 60 { "#e4a84a" } else { "#636e7d" };
        let _ = forgejo.create_label(owner, repo, &name, color).await;
    }
    Ok(())
}

#[derive(Deserialize)]
struct Fixture {
    name: String,
    jobs: Vec<FixtureJob>,
}

async fn cmd_seed(
    forgejo: &ForgejoClient,
    sidecar: &SidecarClient,
    owner: &str,
    repo: &str,
    fixture_path: &str,
) -> Result<()> {
    let raw =
        std::fs::read_to_string(fixture_path).with_context(|| format!("read {fixture_path}"))?;
    let fixture: Fixture =
        serde_json::from_str(&raw).with_context(|| format!("parse {fixture_path}"))?;

    println!("Seeding \"{}\" into {owner}/{repo} …", fixture.name);

    let repo_labels = forgejo.list_repo_labels(owner, repo).await?;
    let blocked_label_id = repo_labels
        .iter()
        .find(|l| l.name.as_deref() == Some("status:blocked"))
        .and_then(|l| l.id.map(|id| id as u64));
    let on_deck_label_id = repo_labels
        .iter()
        .find(|l| l.name.as_deref() == Some("status:on-deck"))
        .and_then(|l| l.id.map(|id| id as u64));

    let mut id_to_number: HashMap<String, u64> = HashMap::new();
    // Track (issue_number, expected_dep_numbers) for the wait phase.
    let mut created: Vec<(u64, Vec<u64>)> = Vec::new();

    // Phase 1: Create all issues as fast as possible.
    for job in &fixture.jobs {
        let dep_numbers: Vec<u64> = job
            .depends_on
            .iter()
            .filter_map(|dep_id| {
                let n = id_to_number.get(dep_id).copied();
                if n.is_none() {
                    eprintln!("warning: unknown dep id '{dep_id}' in job '{}'", job.id);
                }
                n
            })
            .collect();

        let base_body = if job.body.is_empty() {
            job.title.clone()
        } else {
            job.body.clone()
        };
        let body = if dep_numbers.is_empty() {
            base_body
        } else {
            let dep_csv: Vec<String> = dep_numbers.iter().map(|n| n.to_string()).collect();
            let dep_links: Vec<String> = dep_numbers.iter().map(|n| format!("- #{n}")).collect();
            format!(
                "{base_body}\n\n## Dependencies\n\n{}\n\n<!-- workflow:deps:{} -->",
                dep_links.join("\n"),
                dep_csv.join(","),
            )
        };

        // Start with the status label (on-deck or blocked based on deps).
        let mut label_ids: Vec<u64> = if dep_numbers.is_empty() {
            on_deck_label_id.into_iter().collect()
        } else {
            blocked_label_id.into_iter().collect()
        };
        // Append any extra labels from the fixture (capability:, worker:, priority:, etc.)
        for label_name in &job.labels {
            if label_name.starts_with("status:") {
                continue; // Status is determined by dependency logic above.
            }
            if let Some(id) = repo_labels.iter()
                .find(|l| l.name.as_deref() == Some(label_name.as_str()))
                .and_then(|l| l.id.map(|id| id as u64))
            {
                label_ids.push(id);
            } else {
                eprintln!("warning: label '{label_name}' not found in repo, skipping");
            }
        }

        let number = forgejo
            .create_issue_with_labels(owner, repo, &job.title, &body, &label_ids)
            .await?;

        if dep_numbers.is_empty() {
            println!("  #{number}  {}", job.title);
        } else {
            let dep_str: Vec<String> = dep_numbers.iter().map(|n| format!("#{n}")).collect();
            println!("  #{number}  {} (deps: {})", job.title, dep_str.join(", "));
        }

        id_to_number.insert(job.id.clone(), number);
        created.push((number, dep_numbers));
    }

    // Phase 2: Wait for the sidecar to sync all issues + deps.
    print!("  Waiting for sidecar sync ...");
    for (number, expected_deps) in &created {
        wait_for_job(sidecar, owner, repo, *number).await?;
        if !expected_deps.is_empty() {
            wait_for_deps(sidecar, owner, repo, *number, expected_deps).await?;
        }
    }
    println!(" ok");

    println!("Done. {} issues created.", fixture.jobs.len());
    Ok(())
}

async fn wait_for_job(sidecar: &SidecarClient, owner: &str, repo: &str, number: u64) -> Result<()> {
    for _ in 0..120 {
        if sidecar.get_job(owner, repo, number).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("timed out waiting for sidecar to sync #{number}");
}

async fn wait_for_deps(
    sidecar: &SidecarClient,
    owner: &str,
    repo: &str,
    number: u64,
    expected: &[u64],
) -> Result<()> {
    for _ in 0..120 {
        if let Ok(resp) = sidecar.get_job(owner, repo, number).await {
            let mut got = resp.job.dependency_numbers.clone();
            let mut want = expected.to_vec();
            got.sort();
            want.sort();
            if got == want {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("timed out waiting for deps on #{number}");
}

// ── display helpers ───────────────────────────────────────────────────────────

fn print_job_table(jobs: &[Job]) {
    let w_ref = 24usize;
    let w_title = 38usize;
    let w_state = 14usize;
    let w_pri = 4usize;
    let w_deps = 4usize;

    println!(
        "{:<w_ref$}  {:<w_title$}  {:<w_state$}  {:>w_pri$}  {:>w_deps$}",
        "Ref", "Title", "State", "Prio", "Deps"
    );
    println!(
        "{}",
        "─".repeat(w_ref + w_title + w_state + w_pri + w_deps + 8)
    );

    for job in jobs {
        let job_ref = format!("{}/{}/#{}", job.repo_owner, job.repo_name, job.number);
        let title = truncate(&job.title, w_title);
        let state = state_label(&job.state);
        println!(
            "{:<w_ref$}  {:<w_title$}  {:<w_state$}  {:>w_pri$}  {:>w_deps$}",
            truncate(&job_ref, w_ref),
            title,
            state,
            job.priority,
            job.dependency_numbers.len(),
        );
    }
}

pub fn print_job_detail(job: &Job) {
    let separator = "─".repeat(60);
    println!("{separator}");
    println!(
        "Job:      {}/{}/#{} ",
        job.repo_owner, job.repo_name, job.number
    );
    println!("Title:    {}", job.title);
    println!("State:    {}", state_label(&job.state));
    println!("Priority: {}", job.priority);
    if let Some(t) = job.timeout_secs {
        println!("Timeout:  {t}s");
    }
    if !job.dependency_numbers.is_empty() {
        let deps: Vec<String> = job
            .dependency_numbers
            .iter()
            .map(|n| format!("#{n}"))
            .collect();
        println!("Deps:     {}", deps.join(", "));
    }
    if !job.assignees.is_empty() {
        println!("Assigned: {}", job.assignees.join(", "));
    }
    println!("{separator}");
}

fn state_label(state: &JobState) -> &'static str {
    match state {
        JobState::OnIce => "on-ice",
        JobState::Blocked => "blocked",
        JobState::OnDeck => "on-deck",
        JobState::OnTheStack => "on-the-stack",
        JobState::NeedsHelp => "needs-help",
        JobState::InReview => "in-review",
        JobState::ChangesRequested => "changes-requested",
        JobState::Done => "done",
        JobState::Failed => "failed",
        JobState::Revoked => "revoked",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_job_ref(s: &str) -> Result<(&str, &str, u64)> {
    let parts: Vec<&str> = s.splitn(3, '/').collect();
    if parts.len() != 3 {
        bail!("expected owner/repo/number, got: {s}");
    }
    let number: u64 = parts[2]
        .parse()
        .with_context(|| format!("invalid issue number: {}", parts[2]))?;
    Ok((parts[0], parts[1], number))
}
