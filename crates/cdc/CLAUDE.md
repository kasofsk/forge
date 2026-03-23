# CLAUDE.md — workflow-cdc

Change data capture process. Polls Forgejo's PostgreSQL database for issue changes and publishes `IssueSnapshot` messages to a NATS JetStream stream.

## How it works

- Connects to Forgejo's PostgreSQL database (read-only queries)
- Polls ALL issues every cycle (no incremental timestamp filter)
- For each changed issue, builds a full `IssueSnapshot` (title, body, labels, assignees, PR status)
- Publishes snapshots to NATS JetStream stream `workflow-changes` on subject `workflow.changes`
- The stream is created with `WorkQueue` retention if it doesn't exist

## Key queries

- **Issue changes**: Joins `issue`, `repository`, `label`, `issue_label`, `issue_assignees`, `user` tables
- **Merged PR detection** (`closed_by_merge`): Checks `pull_request` table for merged PRs whose issue body contains `Closes #N`
- **Open PR detection** (`has_open_pr`): Checks for open (unmerged) PRs referencing `Closes #N`

Note: `has_changes_requested` was removed. The reviewer process now publishes `ChangesRequested` transitions directly via NATS, eliminating the brittle CDC-based review type detection from Forgejo's PostgreSQL.

## Environment variables

| Var | Default | Notes |
|---|---|---|
| `DATABASE_URL` | `postgres://forgejo:forgejo@localhost:5432/forgejo` | PostgreSQL connection string |
| `NATS_URL` | `nats://localhost:4223` | External port; Docker maps 4223→4222 internally |
| `CDC_POLL_MS` | `250` | Poll interval in milliseconds |

## Single file

The entire crate is `src/main.rs`. It uses `tokio_postgres` for database access and `async_nats` for NATS publishing.

## Deduplication

Uses a hash of each `IssueSnapshot` to avoid republishing unchanged issues. Only publishes when the snapshot content has actually changed since the last poll cycle.
