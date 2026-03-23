use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, stream};
use tokio_postgres::{Client, NoTls};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use workflow_types::IssueSnapshot;

const STREAM_NAME: &str = "workflow-changes";
const SUBJECT: &str = "workflow.changes";

// ── Config ──────────────────────────────────────────────────────────────────

struct Config {
    database_url: String,
    nats_url: String,
    poll_ms: u64,
}

impl Config {
    fn from_env() -> Self {
        Self {
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgres://forgejo:forgejo@localhost:5432/forgejo".into()),
            nats_url: std::env::var("NATS_URL")
                .unwrap_or_else(|_| "nats://localhost:4223".into()),
            poll_ms: std::env::var("CDC_POLL_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(250),
        }
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("workflow_cdc=info".parse().unwrap()))
        .init();

    let config = Config::from_env();
    tracing::info!(db = %config.database_url, nats = %config.nats_url, poll_ms = config.poll_ms, "starting CDC");

    // Connect to NATS and ensure the stream exists.
    let nats = async_nats::connect(&config.nats_url)
        .await
        .context("connect to NATS")?;
    let js = jetstream::new(nats);

    js.get_or_create_stream(stream::Config {
        name: STREAM_NAME.into(),
        subjects: vec![SUBJECT.into()],
        retention: stream::RetentionPolicy::Limits,
        max_age: Duration::from_secs(7 * 24 * 3600),
        storage: stream::StorageType::File,
        ..Default::default()
    })
    .await
    .context("create/get NATS stream")?;

    // Fingerprint of last published snapshot per issue — only publish on change.
    let mut last_published: HashMap<i64, u64> = HashMap::new();
    let mut interval = tokio::time::interval(Duration::from_millis(config.poll_ms));

    // Postgres connection — reconnect on failure.
    let mut client: Option<Client> = None;

    loop {
        interval.tick().await;

        // Ensure we have a live Postgres connection.
        let db = match &client {
            Some(c) if !c.is_closed() => c,
            _ => {
                match connect_db(&config.database_url).await {
                    Ok(c) => {
                        tracing::info!("connected to Postgres");
                        client = Some(c);
                        client.as_ref().unwrap()
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to connect to Postgres, retrying next cycle");
                        continue;
                    }
                }
            }
        };

        match poll_and_publish(db, &js, &mut last_published).await {
            Ok(count) => {
                if count > 0 {
                    tracing::info!(count, "published snapshots");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "poll cycle failed");
                // Drop the connection so we reconnect next cycle.
                client = None;
            }
        }
    }
}

// ── DB ──────────────────────────────────────────────────────────────────────

async fn connect_db(url: &str) -> Result<Client> {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .context("connect to Postgres")?;

    // The connection must be driven in a background task.
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::error!(error = %e, "Postgres connection error");
        }
    });

    Ok(client)
}

/// Query ALL non-pull issues with their full denormalized state.
async fn query_all_issues(client: &Client) -> Result<Vec<IssueSnapshot>> {
    let rows = client
        .query(
            r#"SELECT
                i.id,
                r.owner_name,
                r.name AS repo_name,
                i."index" AS number,
                i.name AS title,
                COALESCE(i.content, '') AS body,
                i.is_closed,
                i.updated_unix
             FROM issue i
             JOIN repository r ON r.id = i.repo_id
             WHERE i.is_pull = false
             ORDER BY i.updated_unix ASC"#,
            &[],
        )
        .await
        .context("query all issues")?;

    let mut snapshots = Vec::new();
    for row in &rows {
        let issue_id: i64 = row.get(0);
        let number: i64 = row.get(3);
        let is_closed: bool = row.get(6);

        let labels = query_labels(client, issue_id).await?;
        let assignees = query_assignees(client, issue_id).await?;
        let closed_by_merge = if is_closed {
            query_closed_by_merge(client, issue_id, number).await?
        } else {
            false
        };
        let has_open_pr = query_has_open_pr(client, issue_id, number).await?;
        snapshots.push(IssueSnapshot {
            issue_id: issue_id as u64,
            repo_owner: row.get(1),
            repo_name: row.get(2),
            number: number as u64,
            title: row.get(4),
            body: row.get(5),
            is_closed,
            closed_by_merge,
            has_open_pr,
            labels,
            assignees,
            updated_unix: row.get(7),
        });
    }

    Ok(snapshots)
}

async fn query_labels(client: &Client, issue_id: i64) -> Result<Vec<String>> {
    let rows = client
        .query(
            "SELECT l.name
             FROM issue_label il
             JOIN label l ON l.id = il.label_id
             WHERE il.issue_id = $1",
            &[&issue_id],
        )
        .await
        .context("query labels")?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

async fn query_assignees(client: &Client, issue_id: i64) -> Result<Vec<String>> {
    let rows = client
        .query(
            r#"SELECT u.lower_name
             FROM issue_assignees ia
             JOIN "user" u ON u.id = ia.assignee_id
             WHERE ia.issue_id = $1"#,
            &[&issue_id],
        )
        .await
        .context("query assignees")?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

/// Check if a merged PR in the same repo references `Closes #N`.
async fn query_closed_by_merge(client: &Client, issue_id: i64, issue_number: i64) -> Result<bool> {
    let pattern = format!("%Closes #{}%", issue_number);
    let rows = client
        .query(
            "SELECT EXISTS(
                SELECT 1
                FROM pull_request pr
                JOIN issue pi ON pi.id = pr.issue_id
                WHERE pi.repo_id = (SELECT repo_id FROM issue WHERE id = $1)
                  AND pr.has_merged = true
                  AND pi.content LIKE $2
            )",
            &[&issue_id, &pattern],
        )
        .await
        .context("query closed_by_merge")?;
    Ok(rows.first().map(|r| r.get(0)).unwrap_or(false))
}

/// Check if an open (unmerged) PR references `Closes #N`.
async fn query_has_open_pr(client: &Client, issue_id: i64, issue_number: i64) -> Result<bool> {
    let pattern = format!("%Closes #{}%", issue_number);
    let rows = client
        .query(
            "SELECT EXISTS(
                SELECT 1
                FROM pull_request pr
                JOIN issue pi ON pi.id = pr.issue_id
                WHERE pi.repo_id = (SELECT repo_id FROM issue WHERE id = $1)
                  AND pi.is_closed = false
                  AND pr.has_merged = false
                  AND pi.content LIKE $2
            )",
            &[&issue_id, &pattern],
        )
        .await
        .context("query has_open_pr")?;
    Ok(rows.first().map(|r| r.get(0)).unwrap_or(false))
}

// ── Fingerprinting ──────────────────────────────────────────────────────────

fn fingerprint(snap: &IssueSnapshot) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    snap.is_closed.hash(&mut h);
    snap.closed_by_merge.hash(&mut h);
    snap.has_open_pr.hash(&mut h);
    snap.title.hash(&mut h);
    snap.body.hash(&mut h);
    snap.labels.hash(&mut h);
    snap.assignees.hash(&mut h);
    h.finish()
}

// ── NATS publish ────────────────────────────────────────────────────────────

async fn poll_and_publish(
    client: &Client,
    js: &jetstream::Context,
    last_published: &mut HashMap<i64, u64>,
) -> Result<usize> {
    let snapshots = query_all_issues(client).await?;
    let mut published = 0;

    for snap in &snapshots {
        let fp = fingerprint(snap);
        let id = snap.issue_id as i64;

        if last_published.get(&id) == Some(&fp) {
            continue;
        }

        let payload = serde_json::to_vec(snap)?;
        js.publish(SUBJECT, payload.into())
            .await
            .context("publish to NATS")?
            .await
            .context("ack from NATS")?;

        last_published.insert(id, fp);
        published += 1;
    }

    Ok(published)
}
