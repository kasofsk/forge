# Forge

Issues-based workflow orchestration for AI agents. Forgejo issues are jobs, a sidecar coordinates, workers claim and execute.

## How it works

```
Forgejo (issues = jobs, labels = state)
    ↓ CDC polls PostgreSQL
NATS JetStream
    ↓ event stream
Sidecar (graph + dispatcher + reviewer)
    ↓ assigns via NATS
Workers (AI agents)
```

**CDC** watches Forgejo's database for issue changes and publishes snapshots to NATS.

**Sidecar** consumes those snapshots, maintains a dependency graph (IndraDB/RocksDB), and coordinates everything. It owns all state transitions — workers never touch the graph or claim store directly.

**Dispatcher** (sidecar module) assigns jobs to idle workers via NATS, manages heartbeats, and handles outcomes (complete, fail, yield).

**Reviewer** (separate process) reacts when a job enters review. Dispatches a Claude-based review action, then merges, requests changes, or escalates to a human based on the job's review label.

**Workers** are pure executors. They register capabilities, receive assignments, do content ops against Forgejo (branches, PRs, comments), and report outcomes. Three modes: sim (testing), action (Forgejo Actions workflows), and interactive (Claude Code agent in tmux).

## Job states

```
On Ice → Blocked → On Deck → On The Stack → In Review → Done
                                    ↓              ↓
                                  Failed    Changes Requested → back to worker
                                    ↓
                              (auto-retry up to 3x, then manual requeue)
```

Jobs start on-ice or blocked (if they have dependencies). When deps resolve, they move to on-deck. The dispatcher assigns on-deck jobs to idle workers (on-the-stack). Workers open a PR and yield — CDC detects the PR and transitions to in-review. Merging the PR closes the issue → done. Closing without merge → revoked.

## Key concepts

- **Forgejo is the human-visible truth.** Every state change is mirrored to labels/comments.
- **Exclusive claims via NATS KV CAS.** No two workers race on the same job.
- **Dependencies in issue bodies** as `<!-- workflow:deps:1,2,3 -->` HTML comments.
- **Three Forgejo identities** (`workflow-sync`, `workflow-dispatcher`, `workflow-reviewer`) for clear audit trails.
- **Workers route by labels.** `worker:X`, `platform:X`, `capability:X` labels constrain which workers can claim a job.
- **Flavored agents.** `flavors.conf` maps names to Dockerfiles with pre-installed toolchains (e.g. Rust). Workers started with `--flavor rust` get capability labels for routing.

## Quick start

**Prerequisites:** Set a Claude Code credential so workers and the reviewer can invoke Claude:

```bash
export FORGE_CLAUDE_CODE_OAUTH_TOKEN=...   # OAuth token (preferred)
# or
export ANTHROPIC_API_KEY=...               # API key
```

Then run:

```bash
./scripts/init.sh          # full setup: infra, build, seed
./scripts/workers.sh       # start action workers + runners
```

Graph viewer: `http://localhost:8080/graph` — Forgejo: `http://localhost:3000`

Stop: `pkill -f workflow-sidecar; pkill -f workflow-cdc` and `./scripts/workers.sh --down`

## Build & test

```bash
cargo build
cargo test -p workflow-types                               # unit tests (no infra)
cargo test -p workflow-integration-tests -- --include-ignored  # integration (needs init.sh)
```

See [DESIGN.md](DESIGN.md) for full architecture, API reference, and crate layout.
