#!/usr/bin/env bash
# restart.sh — stop and restart CDC + sidecar without destroying state.
#
# Requires:
#   - Docker containers (forgejo + nats) already running
#   - .sidecar.env already generated (by init.sh)
#   - Binaries already built
#
# Usage:
#   ./scripts/restart.sh              # restart CDC + sidecar
#   ./scripts/restart.sh --build      # rebuild before restarting

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BUILD=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --build) BUILD=true; shift ;;
        *)       echo "Unknown arg: $1"; exit 1 ;;
    esac
done

cd "$ROOT"

# ── Validate prerequisites ─────────────────────────────────────────────────

if [[ ! -f .sidecar.env ]]; then
    echo "❌ .sidecar.env not found — run ./scripts/init.sh first"
    exit 1
fi

if ! docker compose ps forgejo --format '{{.State}}' 2>/dev/null | grep -q running; then
    echo "⏳ Docker containers not running, starting them ..."
    docker compose up -d forgejo nats
    echo "⏳ Waiting for Forgejo ..."
    FORGEJO_URL=$(grep FORGEJO_URL .sidecar.env | cut -d= -f2-)
    for i in $(seq 1 30); do
        curl -sf "$FORGEJO_URL/api/v1/version" > /dev/null 2>&1 && break
        sleep 2
    done
    curl -sf "$FORGEJO_URL/api/v1/version" > /dev/null 2>&1 || { echo "❌ Forgejo did not start"; exit 1; }
    echo "✓ Forgejo is up"
fi

# ── Stop running processes ─────────────────────────────────────────────────

echo "🧹 Stopping running processes ..."
pkill -f "target/debug/workflow-sidecar" 2>/dev/null || true
pkill -f "target/debug/workflow-cdc" 2>/dev/null || true
sleep 1
echo "✓ Processes stopped"

# ── Optional rebuild ───────────────────────────────────────────────────────

if [[ "$BUILD" == "true" ]]; then
    echo "🔨 Building binaries ..."
    cargo build -p workflow-sidecar -p workflow-cdc -p worker-cli 2>&1 | grep -v "^$" | tail -3
    echo "✓ Build complete"
fi

# ── Start CDC + sidecar ───────────────────────────────────────────────────

echo "🚀 Starting CDC process ..."
FORGEJO_DB_PATH="$ROOT/.data/forgejo/gitea/gitea.db" \
NATS_URL=nats://localhost:4223 \
RUST_LOG=workflow_cdc=info \
    "$ROOT/target/debug/workflow-cdc" > /tmp/workflow-cdc.log 2>&1 &
CDC_PID=$!
echo "✓ CDC started (pid $CDC_PID)"

echo "🚀 Starting sidecar ..."
env $(cat .sidecar.env | xargs) \
RUST_LOG=workflow_sidecar=info \
    "$ROOT/target/debug/workflow-sidecar" > /tmp/workflow-sidecar.log 2>&1 &
SIDECAR_PID=$!
echo "✓ Sidecar started (pid $SIDECAR_PID)"

# Wait for sidecar to be ready
echo "⏳ Waiting for sidecar ..."
for i in $(seq 1 30); do
    curl -sf http://localhost:8080/jobs > /dev/null 2>&1 && break
    sleep 1
done
curl -sf http://localhost:8080/jobs > /dev/null 2>&1 || { echo "❌ Sidecar did not start. Check /tmp/workflow-sidecar.log"; exit 1; }
echo "✓ Sidecar is up"

# ── Show state ────────────────────────────────────────────────────────────

echo ""
echo "📊 Current graph state:"
curl -sf http://localhost:8080/jobs | python3 -c "
import sys,json
jobs = json.load(sys.stdin)['jobs']
for j in sorted(jobs, key=lambda x: x['number']):
    deps = j['dependency_numbers']
    print(f'  #{j[\"number\"]:2d} {j[\"state\"]:12s} deps={str(deps):16s} {j[\"title\"]}')
print(f'\n  Total: {len(jobs)} jobs')
" 2>/dev/null || echo "  (no jobs yet)"

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Restarted — all state preserved"
echo ""
echo "  Graph viewer:  http://localhost:8080/graph"
echo "  Sidecar PID:   $SIDECAR_PID  (log: /tmp/workflow-sidecar.log)"
echo "  CDC PID:       $CDC_PID  (log: /tmp/workflow-cdc.log)"
echo ""
echo "  To stop:  pkill -f workflow-sidecar; pkill -f workflow-cdc"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
