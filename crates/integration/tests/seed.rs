/// Integration tests for the full seed → sidecar pipeline.
///
/// # Prerequisites
///
/// Before running these tests, apply the test Terraform environment:
///
///   cd infra/test
///   terraform init
///   terraform apply -var-file=test.tfvars -var-file=test.secrets.tfvars
///
/// Then export the required env vars (or source the helper output):
///
///   source <(terraform output -raw env_exports)
///
/// Each test creates an ephemeral repository for full isolation, then
/// deletes it during teardown.
///
/// Run with:
///   cargo test -p workflow-integration-tests -- --include-ignored
use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use workflow_types::JobState;
use workflow_worker::{client::SidecarClient, forgejo::ForgejoClient};

// ── Fixture types (mirrors demo/fixtures/*.json) ───────────────────────────

#[derive(Deserialize)]
struct FixtureJob {
    id: String,
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    depends_on: Vec<String>,
}

#[derive(Deserialize)]
struct Fixture {
    jobs: Vec<FixtureJob>,
}

// ── Environment ────────────────────────────────────────────────────────────

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

// ── Test context ───────────────────────────────────────────────────────────

/// Each test gets its own ephemeral repo for full isolation.
/// On teardown the repo is deleted, removing all issues and webhooks.
struct TestContext {
    forgejo: ForgejoClient,
    sidecar: SidecarClient,
    owner: String,
    repo: String,
    /// Issue numbers created by this test (used by seed/poll helpers).
    created: Vec<u64>,
}

impl TestContext {
    /// Create an ephemeral repo with a unique name and configure the
    /// sidecar webhook on it.
    async fn new(test_name: &str) -> Self {
        let forgejo_url = env("FORGEJO_URL", "http://localhost:3000");
        let forgejo_token = env("FORGEJO_TOKEN", "");
        let sidecar_url = env("SIDECAR_URL", "http://localhost:8080");
        // Repos are created via /user/repos under the token owner's account.
        let owner = env("TEST_REPO_OWNER", "workflow-sync");

        let forgejo = ForgejoClient::new(&forgejo_url, &forgejo_token);
        let sidecar = SidecarClient::new(&sidecar_url);

        // Unique repo name per test run to avoid collisions.
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let repo = format!("test-{test_name}-{ts}");

        forgejo
            .create_repo(&repo)
            .await
            .expect("create ephemeral repo");

        // Create status labels on the ephemeral repo (Terraform only sets them
        // on the main repo, but tests need them too).
        for (name, color) in [
            ("status:on-deck", "#0075ca"),
            ("status:blocked", "#e4e669"),
            ("status:on-the-stack", "#fbca04"),
            ("status:in-review", "#8b5cf6"),
            ("status:done", "#0e8a16"),
            ("status:failed", "#b60205"),
            ("status:rework", "#f59e0b"),
            ("status:revoked", "#6b7280"),
            ("status:on-ice", "#cfd3d7"),
        ] {
            let _ = forgejo.create_label(&owner, &repo, name, color).await;
        }

        // Create the test-worker user (if it doesn't exist) and add it +
        // the sidecar service accounts as collaborators on the ephemeral repo.
        let admin_user = env("ADMIN_USER", "sysadmin");
        let admin_pass = env("ADMIN_PASS", "admin1234");
        let admin = ForgejoClient::new_basic_auth(&forgejo_url, &admin_user, &admin_pass);
        let _ = admin
            .admin_create_user("test-worker", "test-worker@test.local", "testpass1234")
            .await;

        for user in ["workflow-sync", "workflow-dispatcher", "test-worker"] {
            let _ = forgejo.add_collaborator(&owner, &repo, user).await;
        }

        Self {
            forgejo,
            sidecar,
            owner,
            repo,
            created: Vec::new(),
        }
    }

    /// Delete the ephemeral repo, which removes all issues and webhooks.
    async fn teardown(self) {
        let _ = self.forgejo.delete_repo(&self.owner, &self.repo).await;
    }
}

// ── Seed helper ────────────────────────────────────────────────────────────

/// Seed a fixture into the test repo, returning a map of symbolic id → issue number.
///
/// Single-pass: each issue is created with deps and status labels in one shot.
/// Fixtures are DAGs so deps always reference earlier jobs.
async fn seed(ctx: &mut TestContext, fixture: &Fixture) -> Result<HashMap<String, u64>> {
    let mut id_to_number: HashMap<String, u64> = HashMap::new();

    // Look up status label IDs for the ephemeral repo.
    let repo_labels = ctx.forgejo.list_repo_labels(&ctx.owner, &ctx.repo).await?;
    let blocked_label_id = repo_labels
        .iter()
        .find(|l| l.name.as_deref() == Some("status:blocked"))
        .and_then(|l| l.id.map(|id| id as u64));
    let on_deck_label_id = repo_labels
        .iter()
        .find(|l| l.name.as_deref() == Some("status:on-deck"))
        .and_then(|l| l.id.map(|id| id as u64));

    for job in &fixture.jobs {
        // Resolve deps (all known since they reference earlier jobs).
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

        // Build body with dep marker.
        let base_body = if job.body.is_empty() {
            &job.title
        } else {
            &job.body
        };
        let body = if dep_numbers.is_empty() {
            base_body.to_string()
        } else {
            let dep_csv: Vec<String> = dep_numbers.iter().map(|n| n.to_string()).collect();
            format!(
                "{base_body}\n\n<!-- workflow:deps:{} -->",
                dep_csv.join(",")
            )
        };

        // Status label: on-deck if no deps, blocked if has deps.
        let label_ids: Vec<u64> = if dep_numbers.is_empty() {
            on_deck_label_id.into_iter().collect()
        } else {
            blocked_label_id.into_iter().collect()
        };

        let number = ctx
            .forgejo
            .create_issue_with_labels(&ctx.owner, &ctx.repo, &job.title, &body, &label_ids)
            .await?;
        id_to_number.insert(job.id.clone(), number);
        ctx.created.push(number);
    }

    Ok(id_to_number)
}

// ── Polling helper ─────────────────────────────────────────────────────────

async fn poll_until<F, Fut, T>(timeout: Duration, f: F) -> Option<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f().await {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn load_fixture(filename: &str) -> Result<Fixture> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../demo/fixtures")
        .join(filename);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read fixture {}", path.display()))?;
    serde_json::from_str(&raw).context("parse fixture")
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// After seeding the linear chain, only the first job (no deps) should be
/// on-deck; every other job should be blocked.
#[tokio::test]
#[ignore = "requires: terraform apply infra/test + env vars from terraform output -raw env_exports"]
async fn chain_initial_states() {
    let mut ctx = TestContext::new("chain-init").await;
    let fixture = load_fixture("chain.json").unwrap();

    let id_map = seed(&mut ctx, &fixture).await.unwrap();

    let expected_blocked = fixture
        .jobs
        .iter()
        .filter(|j| !j.depends_on.is_empty())
        .count();

    let jobs = poll_until(Duration::from_secs(60), || async {
        let all = ctx.sidecar.list_jobs(None).await.ok()?;
        let mine: Vec<_> = all
            .into_iter()
            .filter(|j| {
                ctx.created.contains(&j.number)
                    && j.repo_owner == ctx.owner
                    && j.repo_name == ctx.repo
            })
            .collect();
        let stable = mine
            .iter()
            .all(|j| j.state == JobState::OnDeck || j.state == JobState::Blocked);
        let blocked = mine.iter().filter(|j| j.state == JobState::Blocked).count();
        if mine.len() == fixture.jobs.len() && stable && blocked == expected_blocked {
            Some(mine)
        } else {
            None
        }
    })
    .await
    .expect("timed out waiting for sidecar to process chain jobs");

    let by_number: HashMap<u64, _> = jobs.iter().map(|j| (j.number, j)).collect();

    assert_eq!(
        by_number[&id_map["setup"]].state,
        JobState::OnDeck,
        "setup should be on-deck"
    );
    for id in &["schema", "api", "tests", "docs", "release"] {
        assert_eq!(
            by_number[&id_map[*id]].state,
            JobState::Blocked,
            "'{id}' should be blocked"
        );
    }

    ctx.teardown().await;
}

/// Completing the first job in the chain should unblock only the immediate
/// dependent; everything further down remains blocked.
#[tokio::test]
#[ignore = "requires: terraform apply infra/test + env vars from terraform output -raw env_exports"]
async fn chain_completing_head_unblocks_next() {
    let mut ctx = TestContext::new("chain-complete").await;
    let fixture = load_fixture("chain.json").unwrap();

    let id_map = seed(&mut ctx, &fixture).await.unwrap();

    let expected_blocked = fixture
        .jobs
        .iter()
        .filter(|j| !j.depends_on.is_empty())
        .count();

    // Wait for stable initial state.
    poll_until(Duration::from_secs(60), || async {
        let all = ctx.sidecar.list_jobs(None).await.ok()?;
        let mine: Vec<_> = all
            .into_iter()
            .filter(|j| {
                ctx.created.contains(&j.number)
                    && j.repo_owner == ctx.owner
                    && j.repo_name == ctx.repo
            })
            .collect();
        let stable = mine
            .iter()
            .all(|j| j.state == JobState::OnDeck || j.state == JobState::Blocked);
        let blocked = mine.iter().filter(|j| j.state == JobState::Blocked).count();
        if mine.len() == fixture.jobs.len() && stable && blocked == expected_blocked {
            Some(())
        } else {
            None
        }
    })
    .await
    .expect("timed out waiting for initial state");

    // Close the "setup" issue with a status:done label so the CDC detects it
    // as Done and unblocks dependents. No HTTP claim/complete needed — the CDC
    // handles the full lifecycle.
    let setup_n = id_map["setup"];

    // Add the done label before closing so the CDC sees it.
    let done_label_id = {
        let labels = ctx
            .forgejo
            .list_repo_labels(&ctx.owner, &ctx.repo)
            .await
            .unwrap();
        labels
            .iter()
            .find(|l| l.name.as_deref() == Some("status:done"))
            .and_then(|l| l.id)
            .expect("status:done label must exist") as u64
    };
    ctx.forgejo
        .add_issue_labels(&ctx.owner, &ctx.repo, setup_n, &[done_label_id])
        .await
        .unwrap();
    ctx.forgejo
        .close_issue(&ctx.owner, &ctx.repo, setup_n)
        .await
        .unwrap();

    // "schema" (direct dependent) should become on-deck.
    let schema_n = id_map["schema"];
    poll_until(Duration::from_secs(60), || async {
        let resp = ctx
            .sidecar
            .get_job(&ctx.owner, &ctx.repo, schema_n)
            .await
            .ok()?;
        if resp.job.state == JobState::OnDeck {
            Some(())
        } else {
            None
        }
    })
    .await
    .expect("timed out waiting for schema to become on-deck");

    // "api" should still be blocked (schema not done yet).
    let api_resp = ctx
        .sidecar
        .get_job(&ctx.owner, &ctx.repo, id_map["api"])
        .await
        .unwrap();
    assert_eq!(
        api_resp.job.state,
        JobState::Blocked,
        "api should still be blocked"
    );

    ctx.teardown().await;
}

/// After seeding the hub, exactly the four root jobs should be on-deck;
/// all downstream jobs should be blocked.
#[tokio::test]
#[ignore = "requires: terraform apply infra/test + env vars from terraform output -raw env_exports"]
async fn hub_initial_states() {
    let mut ctx = TestContext::new("hub-init").await;
    let fixture = load_fixture("hub.json").unwrap();

    let id_map = seed(&mut ctx, &fixture).await.unwrap();

    // Count how many jobs have dependencies — they should end up Blocked.
    let expected_blocked = fixture
        .jobs
        .iter()
        .filter(|j| !j.depends_on.is_empty())
        .count();

    let jobs = poll_until(Duration::from_secs(60), || async {
        let all = ctx.sidecar.list_jobs(None).await.ok()?;
        let mine: Vec<_> = all
            .into_iter()
            .filter(|j| {
                ctx.created.contains(&j.number)
                    && j.repo_owner == ctx.owner
                    && j.repo_name == ctx.repo
            })
            .collect();
        let stable = mine
            .iter()
            .all(|j| j.state == JobState::OnDeck || j.state == JobState::Blocked);
        let blocked = mine.iter().filter(|j| j.state == JobState::Blocked).count();
        if mine.len() == fixture.jobs.len() && stable && blocked == expected_blocked {
            Some(mine)
        } else {
            None
        }
    })
    .await
    .expect("timed out waiting for sidecar to process hub jobs");

    let by_number: HashMap<u64, _> = jobs.iter().map(|j| (j.number, j)).collect();

    // Root jobs (no deps) should be on-deck, rest should be blocked.
    let root_ids: Vec<&str> = fixture
        .jobs
        .iter()
        .filter(|j| j.depends_on.is_empty())
        .map(|j| j.id.as_str())
        .collect();
    let dep_ids: Vec<&str> = fixture
        .jobs
        .iter()
        .filter(|j| !j.depends_on.is_empty())
        .map(|j| j.id.as_str())
        .collect();

    for id in &root_ids {
        assert_eq!(
            by_number[&id_map[*id]].state,
            JobState::OnDeck,
            "'{id}' should be on-deck"
        );
    }
    for id in &dep_ids {
        assert_eq!(
            by_number[&id_map[*id]].state,
            JobState::Blocked,
            "'{id}' should be blocked"
        );
    }

    ctx.teardown().await;
}

/// Test the full review cycle: issue → PR → InReview → ChangesRequested →
/// rework → re-review. Repeats 2 times, then approves and merges → Done.
///
/// Simulates worker and reviewer via direct API calls + NATS transitions.
/// The ChangesRequested transition is published via NATS (like the real
/// reviewer does), not via CDC database polling.
#[tokio::test]
#[ignore = "requires: terraform apply infra/test + env vars from terraform output -raw env_exports"]
async fn review_rework_cycle() {
    use bytes::Bytes;
    use workflow_types::{Job, JobTransition};

    let mut ctx = TestContext::new("review-cycle").await;
    let nats_url = env("NATS_URL", "nats://localhost:4223");
    let nats = async_nats::connect(&nats_url).await.expect("connect to NATS");

    let _ = ctx
        .forgejo
        .add_collaborator(&ctx.owner, &ctx.repo, "workflow-reviewer")
        .await;

    // Create issue.
    let on_deck_id = {
        let labels = ctx.forgejo.list_repo_labels(&ctx.owner, &ctx.repo).await.unwrap();
        labels.iter().find(|l| l.name.as_deref() == Some("status:on-deck"))
            .and_then(|l| l.id).unwrap() as u64
    };
    let issue_number = ctx.forgejo
        .create_issue_with_labels(&ctx.owner, &ctx.repo, "Test review cycle", "Test issue.", &[on_deck_id])
        .await.unwrap();
    ctx.created.push(issue_number);

    // Wait for on-deck.
    poll_until(Duration::from_secs(30), || async {
        ctx.sidecar.get_job(&ctx.owner, &ctx.repo, issue_number).await.ok()
            .filter(|r| r.job.state == JobState::OnDeck).map(|_| ())
    }).await.expect("timed out waiting for on-deck");

    // Create branch + file + PR.
    let branch = format!("work/agent/{issue_number}");
    ctx.forgejo.create_branch(&ctx.owner, &ctx.repo, &branch, "main").await.expect("create branch");
    ctx.forgejo.create_file(&ctx.owner, &ctx.repo, &format!("work/issue-{issue_number}.txt"),
        "initial work", "Initial implementation", &branch).await.expect("create file");
    let pr = ctx.forgejo.create_pr(&ctx.owner, &ctx.repo, "Test PR",
        &format!("Closes #{issue_number}\n\nInitial work."), &branch, "main").await.expect("create PR");
    let pr_number = pr.number.unwrap() as u64;

    // Reviewer setup.
    let reviewer_token = env("REVIEWER_TOKEN", "");
    let reviewer_forgejo = ForgejoClient::new(&env("FORGEJO_URL", "http://localhost:3000"), &reviewer_token);

    // Helper: publish a transition via NATS (simulates dispatcher yield → InReview
    // or reviewer → ChangesRequested).
    let publish_transition = |nats: async_nats::Client, owner: String, repo: String, number: u64, prev: JobState, new: JobState| async move {
        let transition = JobTransition {
            job: Job {
                repo_owner: owner, repo_name: repo, number,
                title: String::new(), state: prev.clone(),
                assignees: vec![], dependency_numbers: vec![],
                priority: 50, timeout_secs: None, capabilities: vec![],
                max_retries: 3, retry_attempt: 0, worker_type: None, platform: vec![],
                review: Default::default(), activities: vec![], is_rework: false,
            },
            previous_state: Some(prev),
            new_state: new,
        };
        let payload = serde_json::to_vec(&transition).unwrap();
        nats.publish("workflow.jobs.transition", Bytes::from(payload)).await.unwrap();
    };

    // Simulate dispatcher yield → InReview (dispatcher-driven, not CDC-driven).
    publish_transition(nats.clone(), ctx.owner.clone(), ctx.repo.clone(), issue_number,
        JobState::OnTheStack, JobState::InReview).await;
    poll_until(Duration::from_secs(30), || async {
        ctx.sidecar.get_job(&ctx.owner, &ctx.repo, issue_number).await.ok()
            .filter(|r| r.job.state == JobState::InReview).map(|_| ())
    }).await.expect("timed out waiting for in-review");
    println!("  ✓ Cycle 0: in-review");

    let max_rework_cycles = 2;
    for cycle in 0..max_rework_cycles {
        // Submit REQUEST_CHANGES review (for audit trail on PR).
        reviewer_forgejo.submit_review(&ctx.owner, &ctx.repo, pr_number,
            &format!("Cycle {}: please fix.", cycle + 1),
            "REQUEST_CHANGES")
            .await.expect("submit REQUEST_CHANGES review");

        // Publish ChangesRequested transition via NATS (like the reviewer does).
        publish_transition(nats.clone(), ctx.owner.clone(), ctx.repo.clone(), issue_number,
            JobState::InReview, JobState::ChangesRequested).await;

        // Wait for ChangesRequested.
        poll_until(Duration::from_secs(30), || async {
            ctx.sidecar.get_job(&ctx.owner, &ctx.repo, issue_number).await.ok()
                .filter(|r| r.job.state == JobState::ChangesRequested).map(|_| ())
        }).await.unwrap_or_else(|| panic!("timed out waiting for changes-requested (cycle {})", cycle + 1));
        println!("  ✓ Cycle {}: changes-requested", cycle + 1);

        // Simulate rework: push a commit to the PR branch.
        ctx.forgejo.create_file(&ctx.owner, &ctx.repo,
            &format!("work/fix-{}.txt", cycle + 1),
            &format!("fix for cycle {}", cycle + 1),
            &format!("Address review feedback (cycle {})", cycle + 1),
            &branch).await.expect("push rework commit");

        // Simulate dispatcher yield → InReview (after worker pushes rework).
        publish_transition(nats.clone(), ctx.owner.clone(), ctx.repo.clone(), issue_number,
            JobState::OnTheStack, JobState::InReview).await;
        poll_until(Duration::from_secs(30), || async {
            ctx.sidecar.get_job(&ctx.owner, &ctx.repo, issue_number).await.ok()
                .filter(|r| r.job.state == JobState::InReview).map(|_| ())
        }).await.unwrap_or_else(|| panic!("timed out waiting for re-review (cycle {})", cycle + 1));
        println!("  ✓ Cycle {}: back to in-review", cycle + 1);
    }

    // Final: approve and merge.
    reviewer_forgejo.submit_review(&ctx.owner, &ctx.repo, pr_number,
        "Looks good!", "APPROVED").await.expect("approve");
    // Merge using the repo owner's token (reviewer may lack merge permission).
    tokio::time::sleep(Duration::from_millis(500)).await;
    ctx.forgejo.merge_pr(&ctx.owner, &ctx.repo, pr_number, "merge").await.expect("merge");

    // Wait for Done (CDC: closed_by_merge).
    poll_until(Duration::from_secs(30), || async {
        ctx.sidecar.get_job(&ctx.owner, &ctx.repo, issue_number).await.ok()
            .filter(|r| r.job.state == JobState::Done).map(|_| ())
    }).await.expect("timed out waiting for done");
    println!("  ✓ Final: done (merged)");

    ctx.teardown().await;
}
