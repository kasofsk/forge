# CLAUDE.md — Graph Viewer (Web UI)

Single-page application served at `GET /graph`. A real-time DAG visualization of the workflow system. All contained in one file: `graph.html` (~2020 lines, HTML+CSS+JS inline).

## Tech stack

- **Cytoscape.js** with the **dagre** layout plugin for DAG rendering (left-to-right)
- Nodes are invisible Cytoscape rectangles; the visual representation is HTML overlay cards ("node widgets") positioned via Cytoscape's coordinate system
- No build step, no framework — vanilla JS, loaded from CDN (`unpkg.com`)
- Dark theme (`#0a0e17` background), glassmorphism panels with `backdrop-filter: blur`

## Layout sections

### Header bar (`#header`, fixed top)
- **Title**: "Workflow Graph"
- **Repo selector** (`#repo-select`): Dropdown of all repos that have jobs. Shows job count per repo. Auto-selects the last non-test repo on load.
- **Repo filter** (`#repo-filter`): Text input that filters the repo dropdown in real-time
- **"+ New Issue" button** (`#new-issue-btn`): Opens the new issue creation panel
- **Legend**: Color-coded dots for all states: on-deck (green), blocked (amber), on-stack (blue), in-review (purple), changes-requested (yellow), done (muted green), failed (red), revoked (gray), on-ice (gray)

### Roster strip (`#roster`, fixed below header)
Two categories with sub-groups:

**Agents category:**
- **Dispatch group**: `workflow-dispatcher` avatar. Click opens the journal panel.
- **Workers group**: `worker-aria`, `worker-blake`, `worker-casey` avatars. Avatar border color reflects dispatch state: green=idle, blue=busy, amber=transitioning, dim=unregistered. Hover highlights that worker's assigned jobs in the graph. Click pins the highlight. Badge shows active claim count.
- **Review group**: `workflow-reviewer` avatar. Click opens the journal panel.

**Humans category:**
- `you` (human reviewer) avatar.

Each avatar has:
- **Tooltip** (on hover): Display name + state text (e.g. "working on #7", "idle", "not registered", "sync service")
- **Claim count badge**: Blue pill, top-right, visible when worker has active (non-terminal) jobs
- **State border**: Color matches `WorkerState` from `/dispatch/workers`

### Graph area (`#cy` + `#node-widgets`)
- **Cytoscape canvas** (`#cy`): Handles pan/zoom/layout. Invisible node rectangles (240×90px).
- **Node widget overlay** (`#node-widgets`): HTML cards positioned to match Cytoscape node coordinates. Repositioned on every pan/zoom event.
- **Edges**: Taxi-style (right-angle) lines, dim gray. Highlighted edges are brighter and thicker.

### Node widget cards (`.node-widget`)
Each card shows:
- **Issue number** (`#N`) + **state badge** (color-coded pill)
- **Title** (single line, truncated)
- **Links row** (bottom):
  - **Issue link**: Always present, opens Forgejo issue
  - **Session link** (on-the-stack only, if interactive worker): "Interactive" (orange) or "Peek" (purple) link to ttyd terminal
  - **Action run pills**: Fetched from Forgejo API, matched by PR head branch (`prettyref`) or `issue_number` in workflow_dispatch payload. Color-coded: green=success, red=failure, blue=running, amber=waiting. Links to Forgejo action run URL. Grouped by workflow, showing only the latest run per workflow.
  - **PR link** (in-review/changes-requested/done): Shows PR number with merge status icon (check=merged, dot=open, X=closed)
- **Agent badge** (bottom-right, overlapping): Avatar of the active worker (blue glow) or reviewer (purple glow) when job is busy/in-review
- **Escalation wave** (top-right): "wave" emoji appears on jobs escalated to human reviewer (detected from journal entries)
- **Dim/highlight**: Clicking a node dims all unrelated nodes and brightens the clicked node + its direct neighbors + their connecting edges

### Detail panel (`#detail`, right side, slide-in)
Opens on node click. Shows:
- Job title
- Ref (owner/repo/#N)
- State (color badge)
- Priority
- Assignees (with avatars)
- Timeout (if set)
- **Dependencies list**: Clickable, color-coded by state. Clicking navigates to that node.
- **Dependents list**: Reverse deps (jobs that depend on this one). Same clickable behavior.
- **PR section** (in-review/changes-requested/done): Async-loaded linked PR with state (merged/open/closed)
- **Session controls** (on-the-stack + interactive worker): Buttons for "Open terminal" / "Peek output" + "Attach" / "Detach" to toggle headless↔interactive mode
- **Forgejo link**: "Open in Forgejo" link

### Journal panel (`#journal`, left side, slide-in)
Opens when clicking the dispatcher or reviewer avatar. Shows dispatcher actions in reverse chronological order. Each entry has:
- **Timestamp** (local time, HH:MM:SS)
- **Action badge**: Color-coded pill — assign (blue), complete (green), fail (red), abandon (amber), preempt (purple), changes-requested (yellow), register (gray), approve (green), escalate (blue)
- **Comment**: Human-readable description
- **Metadata**: Worker ID + issue number (if applicable)

### New issue panel (`#new-issue-panel`, right side, slide-in)
Form to create a new Forgejo issue via the sidecar API:
- **Title** (text input, required)
- **Body** (textarea)
- **Dependencies** (text input, accepts `#1, #2` or `1, 2` format — auto-appends `<!-- workflow:deps:... -->` to body)
- **Initial state** (radio: On-Ice or On-Deck — maps to `status:on-ice` or `status:on-deck` label)
- **Submit button**: POSTs to `/repos/{owner}/{repo}/issues`, shows success/error, auto-closes after 1.5s

## Data sources (sidecar API endpoints)

| Endpoint | Method | Used for | Response type |
|---|---|---|---|
| `/events` | GET | **SSE stream** — real-time state updates | `text/event-stream` |
| `/repos/{owner}/{repo}/users` | GET | Collaborator list for roster (on repo select) | `UserListResponse` |
| `/repos/{owner}/{repo}/labels` | GET | Label IDs for new issue creation | `LabelListResponse` |
| `/repos/{owner}/{repo}/issues` | POST | Create new issue | `CreateIssueResponse` |
| `/interact/attach` | POST | Switch agent to interactive mode | — |
| `/interact/detach` | POST | Switch agent to headless mode | — |

The following endpoints are still available but **no longer polled by the UI** (data comes via SSE):
- `/jobs` — all jobs (replaced by `snapshot` + `job_update` events)
- `/dispatch/workers` — worker registry (replaced by `snapshot` + `worker_update` events)
- `/dispatch/journal` — journal entries (replaced by `snapshot` + `journal_entry` events)
- `/interact/sessions` — active sessions (replaced by `snapshot` + `session_update` events)

## External API calls (Forgejo, via CORS)

| Forgejo endpoint | Used for |
|---|---|
| `/api/v1/repos/{owner}/{repo}/pulls?state=all&limit=50` | Find linked PR by `Closes #N` in body |
| `/api/v1/repos/{owner}/{repo}/actions/runs?limit=50` | Fetch action run status for widget pills |

These require CORS headers on Forgejo (configured in `infra/forgejo/app.ini`).

## Caching

- **Linked PRs** (`linkedPRs` map): Cached permanently per session. Stale entries (e.g. done job with un-merged PR) are evicted on refresh. Populated lazily for in-review/changes-requested/done jobs.
- **Action runs** (`actionRunsCache` map): Per-repo cache with 15-second TTL. Fetched for all repos with visible jobs.
- **Escalated jobs** (`escalatedJobs` set): Rebuilt from journal entries on each snapshot. Jobs with an "escalate" journal entry get the wave emoji. Incrementally updated by `journal_entry` events.
- **Journal entries** (`journalEntries` array): Kept in memory, initialized from snapshot, prepended by SSE events. Used by both the journal panel and escalation detection.

## Real-time updates (SSE)

The UI connects to `GET /events` using `EventSource` (Server-Sent Events). No polling.

### Connection lifecycle

1. `EventSource` connects to `/events`
2. Server subscribes to the broadcast channel, then reads current state
3. Server sends a `snapshot` event with full state (jobs, workers, journal, sessions)
4. Client applies snapshot: sets all data, picks a repo, renders graph + roster
5. Server streams incremental events as they happen
6. On disconnect, `EventSource` auto-reconnects; server sends a fresh snapshot

### Event types

| SSE event | Payload | Client action |
|---|---|---|
| `snapshot` | `{ jobs, workers, journal, sessions }` | Full state replace + re-render |
| `job_update` | `{ data: { job } }` | Upsert in `allJobs`, incremental graph update |
| `worker_update` | `{ data: { worker } }` | Upsert in `dispatchWorkers` + `allWorkers`, update roster |
| `worker_removed` | `{ data: { worker_id } }` | Remove from maps, update roster |
| `journal_entry` | `{ data: { entry } }` | Prepend to journal, update escalation set, refresh journal panel |
| `session_update` | `{ data: { session } }` | Upsert in `allSessions`, re-render widgets |
| `session_removed` | `{ data: { job_key } }` | Remove from `allSessions`, re-render widgets |

### Lag recovery

The server-side broadcast channel holds 256 events. If a client falls behind (e.g. slow network), the server detects the lag and resends a full `snapshot` event instead of the missed individual events. The client handles `snapshot` idempotently.

### Incremental graph updates

When a `job_update` event arrives, `applyIncrementalUpdate()` runs:
1. Filters jobs for the currently selected repo
2. Detects structural changes (new/removed nodes)
3. If structure changed: incremental add/remove of Cytoscape nodes, rebuild edges, re-run dagre layout with 300ms animation, preserve viewport
4. If no structural change: update node data in-place (state, color, job data)
5. Updates roster claim counts, dispatch state borders, and node widgets
6. Re-highlights pinned user's jobs if any

## State colors

| State | Color | Hex |
|---|---|---|
| on-deck | Green | `#2ecc71` |
| blocked | Amber | `#e4a84a` |
| on-the-stack | Blue | `#3498db` |
| needs-help | Orange | `#f97316` |
| in-review | Purple | `#8b5cf6` |
| changes-requested | Yellow | `#f59e0b` |
| done | Muted green | `#4a6356` |
| failed | Red | `#e74c3c` |
| revoked | Gray | `#6b7280` |
| on-ice | Gray | `#636e7d` |

## Interactions

| Action | Behavior |
|---|---|
| Click node | Opens detail panel, highlights node + neighbors, dims rest |
| Click background | Closes detail panel, clears highlights |
| Hover worker avatar | Temporarily highlights that worker's active jobs |
| Click worker avatar | Pins job highlight for that worker |
| Click dispatcher/reviewer avatar | Opens journal panel |
| Click pinned avatar again | Unpins, clears highlights |
| Click dependency in detail | Navigates to that node (zoom+center), opens its detail |
| Scroll/pinch | Zoom (0.15x–3x range, sensitivity 0.3) |
| Drag | Pan |
| Click "+ New Issue" | Opens issue creation form |
| Click "Attach"/"Detach" in detail | POSTs to sidecar, refreshes session state after 3s |

## Key implementation details

- **SSE-driven, no polling**: The UI uses `EventSource` for real-time updates via `GET /events`. The server sends a full snapshot on connect, then streams incremental events. The old 5-second polling interval has been removed.
- **Widget positioning**: Widgets use `transform: translate(-50%, -50%) scale(zoom)` and are repositioned on every Cytoscape `pan`/`zoom` event. This keeps HTML overlays pixel-aligned with the invisible Cytoscape nodes.
- **Content diffing**: Widget `innerHTML` is only updated when the generated HTML string changes (`el._lastHtml` comparison). This avoids destroying hover states on links during auto-refresh.
- **Hardcoded Forgejo URL**: `http://localhost:3000` is hardcoded in multiple places (PR lookups, action runs, issue links). Not configurable.
- **System accounts**: `workflow-sync`, `workflow-dispatcher`, `workflow-reviewer`, and `you` are hardcoded as constants for roster categorization.
