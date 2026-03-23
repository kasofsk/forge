#!/usr/bin/env bash
# workers.sh — create Forgejo tokens for workers and launch workers.
#
# Prerequisites: ./scripts/init.sh must have run (Forgejo + sidecar up).
#
# Usage:
#   ./scripts/workers.sh              # start action workers + paired runners (default)
#   ./scripts/workers.sh --sim        # start sim workers (no runners needed)
#   ./scripts/workers.sh --agent      # start interactive agent workers on the host
#   ./scripts/workers.sh --count 2    # start first N workers only
#   ./scripts/workers.sh --down       # stop all worker/runner containers + agent procs
#   ./scripts/workers.sh --build      # rebuild worker image before starting
#
# Agent mode options (passed through to worker-cli agent):
#   AGENT_ALLOWED_TOOLS=Bash,Read,Write  tools Claude may use (default: claude's defaults)
#   AGENT_HOST=localhost                 host used in ttyd session URLs
#   ANTHROPIC_API_KEY=...               required for claude CLI (or CLAUDE_CODE_OAUTH_TOKEN)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

FORGEJO_URL="${FORGEJO_URL:-http://localhost:3000}"
WORKER_PASS="${WORKER_PASS:-worker-test-1234}"
ADMIN_USER="${ADMIN_USER:-sysadmin}"
ADMIN_PASS="${ADMIN_PASS:-admin1234}"
DELAY_SECS="${DELAY_SECS:-10}"
NATS_URL="${NATS_URL:-nats://localhost:4223}"

# All provisioned workers (must match Terraform worker_logins)
ALL_WORKERS=(worker-aria worker-blake worker-casey)

# Parse args
COUNT=${#ALL_WORKERS[@]}
ACTION="up"
MODE="action"
BUILD_FLAG=""
FLAVOR=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --count)   COUNT="$2"; shift 2 ;;
        --down)    ACTION="down"; shift ;;
        --build)   BUILD_FLAG="--build"; shift ;;
        --delay)   DELAY_SECS="$2"; shift 2 ;;
        --sim)     MODE="sim"; shift ;;
        --agent)   MODE="agent"; shift ;;
        --flavor)  FLAVOR="$2"; shift 2 ;;
        *)         echo "Unknown arg: $1"; exit 1 ;;
    esac
done

cd "$ROOT"

if [[ "$ACTION" == "down" ]]; then
    docker compose -f docker-compose.workers.yml --profile sim --profile action --profile agent down
    rm -f .workers.env
    exit 0
fi

# Stop existing worker containers before rebuilding to avoid the race where the
# old container briefly receives a job assignment during container recreation.
echo "Stopping existing worker containers ..."
docker compose -f docker-compose.workers.yml --profile sim --profile action --profile agent stop 2>/dev/null || true

# ── Create API tokens for each worker ────────────────────────────────────────

SELECTED=("${ALL_WORKERS[@]:0:$COUNT}")

echo "Creating Forgejo API tokens for ${#SELECTED[@]} workers ..."

: > .workers.env   # truncate

for login in "${SELECTED[@]}"; do
    TOKEN_NAME="dispatch"

    # Delete existing token (ignore errors)
    curl -sf -X DELETE \
        -u "$login:$WORKER_PASS" \
        "$FORGEJO_URL/api/v1/users/$login/tokens/$TOKEN_NAME" \
        > /dev/null 2>&1 || true

    # Create fresh token
    resp=$(curl -sf -X POST \
        -u "$login:$WORKER_PASS" \
        -H "Content-Type: application/json" \
        -d "{\"name\":\"$TOKEN_NAME\",\"scopes\":[\"write:issue\",\"read:issue\",\"write:repository\",\"read:repository\"]}" \
        "$FORGEJO_URL/api/v1/users/$login/tokens")

    token=$(echo "$resp" | grep -o '"sha1":"[^"]*"' | cut -d'"' -f4)
    if [[ -z "$token" ]]; then
        echo "  ❌ Failed to create token for $login: $resp"
        exit 1
    fi

    # Write env var: worker-aria → WORKER_ARIA_TOKEN
    var_name="WORKER_$(echo "${login#worker-}" | tr '[:lower:]' '[:upper:]')_TOKEN"
    echo "$var_name=$token" >> .workers.env
    echo "  ✓ $login → $var_name"
done

# Load token variables into the current shell (needed by agent mode).
set -a; source .workers.env; set +a

# ── Agent mode: verify auth ───────────────────────────────────────────────────

if [[ "$MODE" == "agent" ]]; then
    if [[ -z "${ANTHROPIC_API_KEY:-}" && -z "${FORGE_CLAUDE_CODE_OAUTH_TOKEN:-}" ]]; then
        echo "❌ Claude Code credentials not found."
        echo ""
        echo "  Set one of these environment variables before starting agent workers:"
        echo ""
        echo "    export FORGE_CLAUDE_CODE_OAUTH_TOKEN=...   # OAuth token (preferred)"
        echo "    export ANTHROPIC_API_KEY=...               # API key"
        echo ""
        echo "  Agent workers run Claude Code inside containers and need one of"
        echo "  these to authenticate with the Anthropic API."
        exit 1
    fi
fi

# ── Action mode: load runner registration token ──────────────────────────────
#
# The registration token is created by init.sh and stored in .sidecar.env.
# Paired action runners (runner-aria etc.) need it to register with Forgejo.

if [[ "$MODE" == "action" ]]; then
    REG_TOKEN=$(grep -o 'RUNNER_REGISTRATION_TOKEN=.*' .sidecar.env 2>/dev/null | cut -d= -f2- || true)
    if [[ -n "$REG_TOKEN" ]]; then
        echo "RUNNER_REGISTRATION_TOKEN=$REG_TOKEN" >> .workers.env
        echo "  ✓ Runner registration token loaded from .sidecar.env"
    else
        echo "  ⚠  No RUNNER_REGISTRATION_TOKEN in .sidecar.env — paired runners may fail to register."
        echo "     The shared infra runner (from init.sh) handles review actions independently."
    fi
fi

# ── Build images ─────────────────────────────────────────────────────────────
#
# Build shared images once before starting services to avoid parallel build
# races where multiple containers try to create the same image simultaneously.

# workflow-runner-env: the environment image that action runners execute jobs in.
# Referenced by runner GITEA_RUNNER_LABELS but not built by docker compose.
if [[ -n "$BUILD_FLAG" ]] || ! docker image inspect workflow-runner-env:latest &>/dev/null; then
    echo "🔨 Building workflow-runner-env image ..."
    docker build -t workflow-runner-env:latest -f Dockerfile.runner-env . 2>&1 \
        | while IFS= read -r line; do printf '\r\033[K  %s' "${line:0:120}"; done; echo
    echo "✓ workflow-runner-env ready"
fi

# workflow-worker: the worker binary image shared by all action/sim workers.
if [[ -n "$BUILD_FLAG" ]] || ! docker image inspect workflow-worker:latest &>/dev/null; then
    echo "🔨 Building workflow-worker image ..."
    docker compose -f docker-compose.workers.yml build "${SELECTED[0]}" 2>&1 \
        | while IFS= read -r line; do printf '\r\033[K  %s' "${line:0:120}"; done; echo
    echo "✓ workflow-worker ready"
fi

# workflow-agent: base agent image (built on demand for agent mode).
AGENT_IMAGE="workflow-agent:latest"
AGENT_CAPABILITY=""
if [[ "$MODE" == "agent" ]]; then
    if [[ -n "$BUILD_FLAG" ]] || ! docker image inspect workflow-agent:latest &>/dev/null; then
        echo "🔨 Building workflow-agent image ..."
        docker build -t workflow-agent:latest -f Dockerfile.agent . 2>&1 \
            | while IFS= read -r line; do printf '\r\033[K  %s' "${line:0:120}"; done; echo
        echo "✓ workflow-agent ready"
    fi

    if [[ -n "$FLAVOR" ]]; then
        # Look up flavor in flavors.conf
        FLAVOR_LINE=$(grep "^${FLAVOR}:" "$ROOT/flavors.conf" || true)
        if [[ -z "$FLAVOR_LINE" ]]; then
            echo "❌ Unknown flavor '$FLAVOR'. Check flavors.conf."
            exit 1
        fi
        FLAVOR_DOCKERFILE=$(echo "$FLAVOR_LINE" | cut -d: -f2)
        AGENT_CAPABILITY=$(echo "$FLAVOR_LINE" | cut -d: -f3)
        AGENT_IMAGE="workflow-agent-${FLAVOR}:latest"

        if [[ -n "$BUILD_FLAG" ]] || ! docker image inspect "$AGENT_IMAGE" &>/dev/null; then
            echo "🔨 Building agent flavor '$FLAVOR' ..."
            docker build -t "$AGENT_IMAGE" -f "$FLAVOR_DOCKERFILE" . 2>&1 \
                | while IFS= read -r line; do printf '\r\033[K  %s' "${line:0:120}"; done; echo
            echo "✓ $AGENT_IMAGE ready"
        fi
    fi
fi

# ── Launch ───────────────────────────────────────────────────────────────────

echo ""

if [[ "$MODE" == "action" ]]; then
    SERVICES=()
    for login in "${SELECTED[@]}"; do
        name="${login#worker-}"
        SERVICES+=("action-$name" "runner-$name")
    done

    echo "Starting ${#SELECTED[@]} action workers + runners ..."
    env $(cat .workers.env | xargs) \
        docker compose -f docker-compose.workers.yml --profile action up -d --no-build "${SERVICES[@]}"

    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  Workers running (action mode):"
    for s in "${SERVICES[@]}"; do echo "    $s"; done
    echo ""
    echo "  Logs:  docker compose -f docker-compose.workers.yml logs -f"
    echo "  Stop:  ./scripts/workers.sh --down"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

elif [[ "$MODE" == "sim" ]]; then
    SERVICES=("${SELECTED[@]}")

    echo "Starting ${#SERVICES[@]} sim workers (delay=${DELAY_SECS}s) ..."
    env $(cat .workers.env | xargs) \
        docker compose -f docker-compose.workers.yml --profile sim up -d --no-build "${SERVICES[@]}"

    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  Workers running (sim mode):"
    for s in "${SERVICES[@]}"; do echo "    $s"; done
    echo ""
    echo "  Logs:  docker compose -f docker-compose.workers.yml logs -f"
    echo "  Stop:  ./scripts/workers.sh --down"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

elif [[ "$MODE" == "agent" ]]; then
    SERVICES=()
    for login in "${SELECTED[@]}"; do
        name="${login#worker-}"
        SERVICES+=("agent-$name")
    done

    # Default plugin dirs based on flavor (user can override via AGENT_PLUGIN_DIRS env var)
    if [[ -n "$FLAVOR" && -z "${AGENT_PLUGIN_DIRS:-}" ]]; then
        case "$FLAVOR" in
            rust) AGENT_PLUGIN_DIRS="/opt/plugins/rust-lint" ;;
        esac
    fi

    echo "Starting ${#SELECTED[@]} agent workers ..."
    env $(cat .workers.env | xargs) \
        ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}" \
        CLAUDE_CODE_OAUTH_TOKEN="${FORGE_CLAUDE_CODE_OAUTH_TOKEN:-}" \
        AGENT_ALLOWED_TOOLS="${AGENT_ALLOWED_TOOLS:-}" \
        AGENT_IMAGE="$AGENT_IMAGE" \
        AGENT_CAPABILITY="$AGENT_CAPABILITY" \
        AGENT_PLUGIN_DIRS="${AGENT_PLUGIN_DIRS:-}" \
        docker compose -f docker-compose.workers.yml --profile agent \
            up -d $BUILD_FLAG "${SERVICES[@]}"

    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  Agent workers running:"
    for login in "${SELECTED[@]}"; do
        name="${login#worker-}"
        # Derive port from fixed mapping: aria=7681, blake=7682, casey=7683
        case "$name" in
            aria)  port=7681 ;;
            blake) port=7682 ;;
            casey) port=7683 ;;
        esac
        echo "    ${login}  →  http://localhost:${port}"
    done
    echo ""
    echo "  Logs:  docker compose -f docker-compose.workers.yml logs -f"
    echo "  Stop:  ./scripts/workers.sh --down"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
fi
