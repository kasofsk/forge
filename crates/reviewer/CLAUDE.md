# CLAUDE.md — workflow-reviewer

Standalone automated PR review process. Extracted from the sidecar to run as its own container.

## What it does

Subscribes to `workflow.jobs.transition` via NATS. When a job transitions to InReview:

1. Finds the linked PR (searches for `Closes #N` in open PRs)
2. Checks the `review:X` label on the job
3. If `review:human` → escalates directly to the human reviewer
4. Otherwise → dispatches a `review-work.yml` Forgejo Action
5. Polls until the action completes
6. Reads the decision from the PR review state (APPROVED/REQUEST_CHANGES/COMMENT) via the Forgejo API
7. Acts on the decision: merge, publish ChangesRequested transition via NATS, or escalate to human

## Communication with the sidecar

All communication is fire-and-forget via NATS — no HTTP calls to the sidecar:

| Subject | Purpose |
|---|---|
| `workflow.jobs.transition` | Subscribe — triggers review on InReview; Publish — ChangesRequested transition |
| `workflow.activity.append` | Publish — record review activities in graph |
| `workflow.journal.append` | Publish — record journal entries for dispatcher log |

## Forgejo identity

Uses the `workflow-reviewer` Forgejo account (token via `REVIEWER_FORGEJO_TOKEN`). This identity:
- Dispatches review action workflows
- Submits PR reviews (APPROVED/REQUEST_CHANGES)
- Merges approved PRs
- Adds human reviewer and posts escalation comments

## Environment variables

| Var | Default | Notes |
|---|---|---|
| `FORGEJO_URL` | (required) | Forgejo base URL |
| `REVIEWER_FORGEJO_TOKEN` | (required) | Reviewer identity token |
| `NATS_URL` | `nats://localhost:4222` | NATS connection |
| `REVIEWER_HUMAN_LOGIN` | `you` | Human reviewer login for escalation |
| `REVIEWER_DELAY_SECS` | `3` | Delay before reviewing (avoids PR creation race) |
| `REVIEWER_WORKFLOW` | `review-work.yml` | Workflow file for Claude-based review action |
| `REVIEWER_RUNNER` | `ubuntu-latest` | Runner label for review action |
| `REVIEWER_ACTION_TIMEOUT_SECS` | `600` | Timeout for review action completion |
| `REVIEWER_POLL_SECS` | `10` | Poll interval for review action status |
| `SIDECAR_URL` | `http://localhost:8080` | Sidecar HTTP API URL (for startup reconciliation) |

## Deployment

Runs as its own container in `docker-compose.yml` using the same `workflow-sidecar:latest` image (which includes the `workflow-reviewer` binary). Shares `.sidecar.env` with the sidecar for credentials.

## Key invariants

- **Dedup via DashSet**: Uses `in_flight` set to prevent double-handling when multiple InReview transitions fire for the same job (e.g. CDC path and dispatcher path racing).
- **Not a worker**: Does not claim jobs. Acts in a supervisory capacity.
- **Reviewer owns ChangesRequested**: On REQUEST_CHANGES decision, the reviewer publishes a `JobTransition{ChangesRequested}` directly to NATS. The dispatcher receives it, sets the graph state + Forgejo label, and routes the job back to the original worker. CDC is not involved in this transition.
- **Decision from review state, not comment markers**: The review action submits a PR review with the appropriate state (APPROVED/REQUEST_CHANGES/COMMENT). The reviewer reads the state field from the Forgejo API — no HTML comment markers are parsed. PR comments are human-readable only.
- **Rework cycle limit via NATS KV**: A `workflow-rework-counts` KV bucket tracks how many times each job has been sent back for changes (both conflict-rework and review-rework). After 3 cycles, the reviewer escalates to a human instead of looping.
- **Per-repo merge queue**: Merges are serialized per repo via a NATS KV lock (`workflow-merge-queue` bucket, key `owner.repo.lock`). If the lock is held, the PR is queued (`owner.repo.queue`). After each merge completes, the next queued entry is processed. Uses `rebase` merge strategy so Forgejo rebases onto main before fast-forwarding. Merge attempts have exponential backoff (5 attempts). On failure, requests rework.
