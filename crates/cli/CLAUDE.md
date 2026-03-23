# CLAUDE.md — worker-cli

Command-line binary for manual interaction with the workflow system. Combines worker execution, fixture seeding, admin operations, and interactive agent session management.

## Subcommands

| Command | Purpose |
|---|---|
| `jobs` | List jobs, optionally filtered by state |
| `show` | Show full details of a single job |
| `requeue` | Requeue a failed or on-ice job (via NATS admin request-reply) |
| `sim` | Run a dispatched sim worker (NATS-based, auto-completes after delay) |
| `action` | Run a dispatched action worker (NATS-based, triggers Forgejo Actions) |
| `agent` | Run a dispatched interactive agent worker (NATS-based, Claude + ttyd terminal) |
| `interact respond` | Send a response to an agent waiting for human help |
| `interact attach` | Switch a headless agent to interactive mode |
| `interact detach` | Switch an interactive agent back to headless mode |
| `seed` | Seed a Forgejo repo with jobs from a fixture JSON file |

## Worker modes

### SimWorker (`workers/sim.rs`)
Creates a branch, commits a file, opens a PR with `Closes #N`, sleeps for a configurable delay, then yields. Used for testing the full lifecycle without Forgejo Actions.

### ActionWorker (`workers/action.rs`)
Creates a `work/action/{N}` branch, dispatches a Forgejo Actions workflow on that branch, polls until the action run completes, then yields. Tolerates transient API errors (up to 5 consecutive failures). Does not manage its own timeout — relies on the dispatcher's lease/timeout monitoring. The branch name ties the action run to the PR via `prettyref`.

### InteractiveWorker (`workers/interactive.rs`)
Spawns a Claude Code CLI session inside a tmux session, fronted by a ttyd web terminal. Publishes `SessionInfo` to the `workflow-sessions` NATS KV bucket. Supports headless (peek) and interactive modes, switchable via attach/detach NATS signals. Can request human help mid-task, pausing the job in `NeedsHelp` state.

## Job routing

Both `action` and `agent` workers support job routing via:
- `--capability` / `ACTION_CAPABILITY` / `AGENT_CAPABILITY` — matches `capability:X` labels on issues
- `--platform` / `WORKER_PLATFORM` — matches `platform:X` labels on issues (comma-separated, e.g. `linux,arm64`)

The dispatcher also checks the `worker:X` label on issues against the worker's type (`action`, `interactive`, `sim`).

## Environment variables

| Var | Default | Notes |
|---|---|---|
| `SIDECAR_URL` | `http://localhost:3000` | Sidecar HTTP API (for legacy/admin commands) |
| `FORGEJO_URL` | (empty) | Forgejo instance URL |
| `FORGEJO_TOKEN` | (empty) | Worker's Forgejo API token |
| `WORKER_ID` | `cli-worker` | Worker identity for claims and assignments |
| `NATS_URL` | `nats://localhost:4223` | NATS server for dispatched modes |
| `ACTION_WORKFLOW` | `agent-work.yml` | Workflow file for action worker |
| `ACTION_RUNNER` | (none) | Runner label for action worker |
| `ACTION_CAPABILITY` | (none) | Capability filter for action worker |
| `WORKER_PLATFORM` | (none) | Platform tags (comma-separated) for action/agent workers |
| `AGENT_CAPABILITY` | (none) | Capability filter for agent worker |
| `AGENT_HOST` | `localhost` | Host for ttyd session URLs |
| `AGENT_ALLOWED_TOOLS` | (empty) | Comma-separated Claude tool allowlist |
| `AGENT_PLUGIN_DIRS` | (auto-discover) | Comma-separated Claude Code plugin dirs. If empty, scans `/opt/plugins/*/` for installed plugins |
| `TTYD_PORT` | `0` | Fixed ttyd port (0 = auto) |
