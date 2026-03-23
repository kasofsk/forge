#!/usr/bin/env bash
# init.sh — sets up (or resets) the entire workflow dev/test environment.
#
# Idempotent: always tears down everything and rebuilds from scratch.
#
# What it does:
#   1. Tears down all Docker containers, clears local state
#   2. Starts Forgejo + NATS via docker compose (infra only)
#   3. Creates the admin user + API token
#   4. Runs Terraform (destroy + apply) for repos, users, labels
#   5. Sets worker/sidecar/reviewer passwords, creates API tokens
#   6. Writes .sidecar.env
#   7. Uploads avatars from infra/avatars/{username}.png
#   8. Builds sidecar Docker image + worker-cli binary
#   9. Starts CDC + sidecar via docker compose
#  10. Seeds a fixture and verifies the graph
#
# Usage:
#   ./scripts/init.sh                       # full setup with default hub fixture
#   ./scripts/init.sh --no-seed             # skip fixture seeding
#   ./scripts/init.sh --fixture chain.json  # seed a different fixture
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

FORGEJO_URL="${FORGEJO_URL:-http://localhost:3000}"
ADMIN_USER="${ADMIN_USER:-sysadmin}"
ADMIN_PASS="${ADMIN_PASS:-admin1234}"
ADMIN_EMAIL="${ADMIN_EMAIL:-sysadmin@test.local}"
ADMIN_TOKEN_NAME="${ADMIN_TOKEN_NAME:-terraform}"

SIDECAR_LOGIN="${SIDECAR_LOGIN:-workflow-sync}"
SIDECAR_PASS="${SIDECAR_PASS:-sidecar-test-1234}"
SIDECAR_TOKEN_NAME="workflow-sync"
DISPATCHER_LOGIN="${DISPATCHER_LOGIN:-workflow-dispatcher}"
DISPATCHER_PASS="${DISPATCHER_PASS:-dispatcher-test-1234}"
DISPATCHER_TOKEN_NAME="workflow-dispatcher"
REVIEWER_LOGIN="${REVIEWER_LOGIN:-workflow-reviewer}"
REVIEWER_PASS="${REVIEWER_PASS:-reviewer-test-1234}"
REVIEWER_TOKEN_NAME="workflow-reviewer"
HUMAN_LOGIN="${HUMAN_LOGIN:-you}"
HUMAN_PASS="${HUMAN_PASS:-human-test-1234}"
WORKER_PASS="${WORKER_PASS:-worker-test-1234}"

# Parse args
SEED=true
FIXTURE="demo/fixtures/forge-frontend.json"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-seed)       SEED=false; shift ;;
        --fixture)       FIXTURE="$2"; shift 2 ;;
        *)               echo "Unknown arg: $1"; exit 1 ;;
    esac
done

cd "$ROOT"

# ── 1. Tear down everything ─────────────────────────────────────────────────

echo "🗑  Tearing down containers and clearing state ..."
docker compose -f docker-compose.workers.yml --profile sim --profile action down -v 2>/dev/null || true
docker compose down -v 2>/dev/null || true
docker compose -f docker-compose.infra.yml down -v 2>/dev/null || true
rm -rf .data/forgejo .data/workflow.db
mkdir -p .data/forgejo/gitea/conf
cp infra/forgejo/app.ini .data/forgejo/gitea/conf/app.ini
echo "✓ Clean slate"

# ── 2. Start infra ──────────────────────────────────────────────────────────

echo "🐳 Starting infra (forgejo + nats) ..."
docker compose -f docker-compose.infra.yml up -d
echo "✓ Containers started"

# ── 3. Wait for Forgejo ─────────────────────────────────────────────────────

echo "⏳ Waiting for Forgejo at $FORGEJO_URL ..."
for i in $(seq 1 60); do
    curl -sf "$FORGEJO_URL/api/v1/version" > /dev/null 2>&1 && break
    sleep 2
done
curl -sf "$FORGEJO_URL/api/v1/version" > /dev/null 2>&1 || { echo "❌ Forgejo did not start"; exit 1; }
echo "✓ Forgejo is up"

# ── 4. Create admin user (idempotent) ───────────────────────────────────────

echo "👤 Creating admin user '$ADMIN_USER' ..."
docker compose -f docker-compose.infra.yml exec -T -u git forgejo \
    gitea admin user create \
    --username "$ADMIN_USER" \
    --password "$ADMIN_PASS" \
    --email    "$ADMIN_EMAIL" \
    --admin \
    --must-change-password=false 2>&1 | grep -v "already exists" || true
echo "✓ Admin user ready"

# ── 5. Create admin token via REST API ──────────────────────────────────────

echo "🔑 Creating admin token '$ADMIN_TOKEN_NAME' ..."
curl -sf -X DELETE \
    -u "$ADMIN_USER:$ADMIN_PASS" \
    "$FORGEJO_URL/api/v1/users/$ADMIN_USER/tokens/$ADMIN_TOKEN_NAME" \
    > /dev/null 2>&1 || true

ADMIN_TOKEN=$(curl -sf -X POST \
    -u "$ADMIN_USER:$ADMIN_PASS" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"$ADMIN_TOKEN_NAME\",\"scopes\":[\"write:admin\",\"write:repository\",\"write:issue\",\"write:user\",\"read:admin\",\"read:repository\",\"read:issue\",\"read:user\"]}" \
    "$FORGEJO_URL/api/v1/users/$ADMIN_USER/tokens" \
    | grep -o '"sha1":"[^"]*"' | cut -d'"' -f4)

if [[ -z "$ADMIN_TOKEN" ]]; then
    echo "❌ Failed to create admin token via API."
    exit 1
fi
echo "✓ Admin token obtained"

# ── 6. Terraform: fresh state ───────────────────────────────────────────────

cd "$ROOT/infra/test"
rm -rf .terraform terraform.tfstate terraform.tfstate.backup
terraform init -input=false -upgrade >/dev/null

echo "🏗  Applying Terraform ..."
# Build repo_secrets map from env vars (if set).
REPO_SECRETS="{}"
if [[ -n "${FORGE_CLAUDE_CODE_OAUTH_TOKEN:-}" ]]; then
  REPO_SECRETS="{\"CLAUDE_CODE_OAUTH_TOKEN\"=\"${FORGE_CLAUDE_CODE_OAUTH_TOKEN}\"}"
fi

TF_VAR_forgejo_admin_token="$ADMIN_TOKEN" \
TF_VAR_forgejo_admin_password="$ADMIN_PASS" \
TF_VAR_sidecar_password="$SIDECAR_PASS" \
TF_VAR_dispatcher_password="$DISPATCHER_PASS" \
TF_VAR_reviewer_password="$REVIEWER_PASS" \
TF_VAR_human_password="$HUMAN_PASS" \
TF_VAR_repo_secrets="$REPO_SECRETS" \
terraform apply -auto-approve -input=false -var-file=test.tfvars
echo "✓ Terraform apply complete"

# ── 7. Set passwords + create sidecar token ─────────────────────────────────

cd "$ROOT"

echo "👷 Setting worker passwords ..."
WORKER_LOGINS=$(cd "$ROOT/infra/test" && terraform output -json worker_passwords 2>/dev/null | grep -o '"[^"]*":' | tr -d '":' || true)
for login in $WORKER_LOGINS; do
    curl -sf -X PATCH \
        -H "Authorization: token $ADMIN_TOKEN" \
        -H "Content-Type: application/json" \
        -d "{\"login_name\":\"$login\",\"source_id\":0,\"password\":\"$WORKER_PASS\",\"must_change_password\":false}" \
        "$FORGEJO_URL/api/v1/admin/users/$login" > /dev/null 2>&1 || true
done
echo "✓ Worker passwords set"

echo "🔑 Creating sidecar API token ..."
curl -sf -X PATCH \
    -H "Authorization: token $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"login_name\":\"$SIDECAR_LOGIN\",\"source_id\":0,\"password\":\"$SIDECAR_PASS\",\"must_change_password\":false}" \
    "$FORGEJO_URL/api/v1/admin/users/$SIDECAR_LOGIN" > /dev/null

# Delete existing token if present, then create fresh.
curl -sf -X DELETE \
    -u "$SIDECAR_LOGIN:$SIDECAR_PASS" \
    "$FORGEJO_URL/api/v1/users/$SIDECAR_LOGIN/tokens/$SIDECAR_TOKEN_NAME" \
    > /dev/null 2>&1 || true

_token_resp=$(curl -sf -X POST \
    -u "$SIDECAR_LOGIN:$SIDECAR_PASS" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"$SIDECAR_TOKEN_NAME\",\"scopes\":[\"write:issue\",\"read:issue\",\"write:repository\",\"read:repository\",\"read:user\",\"write:user\"]}" \
    "$FORGEJO_URL/api/v1/users/$SIDECAR_LOGIN/tokens")
SIDECAR_TOKEN=$(echo "$_token_resp" | grep -o '"sha1":"[^"]*"' | cut -d'"' -f4)

if [[ -z "$SIDECAR_TOKEN" ]]; then
    echo "❌ Failed to create sidecar API token. Response: $_token_resp"
    exit 1
fi
echo "✓ Sidecar token created"

echo "🔑 Creating dispatcher API token ..."
curl -sf -X PATCH \
    -H "Authorization: token $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"login_name\":\"$DISPATCHER_LOGIN\",\"source_id\":0,\"password\":\"$DISPATCHER_PASS\",\"must_change_password\":false}" \
    "$FORGEJO_URL/api/v1/admin/users/$DISPATCHER_LOGIN" > /dev/null

# Delete existing token if present, then create fresh.
curl -sf -X DELETE \
    -u "$DISPATCHER_LOGIN:$DISPATCHER_PASS" \
    "$FORGEJO_URL/api/v1/users/$DISPATCHER_LOGIN/tokens/$DISPATCHER_TOKEN_NAME" \
    > /dev/null 2>&1 || true

_disp_token_resp=$(curl -sf -X POST \
    -u "$DISPATCHER_LOGIN:$DISPATCHER_PASS" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"$DISPATCHER_TOKEN_NAME\",\"scopes\":[\"write:issue\",\"read:issue\",\"write:repository\",\"read:repository\"]}" \
    "$FORGEJO_URL/api/v1/users/$DISPATCHER_LOGIN/tokens")
DISPATCHER_TOKEN=$(echo "$_disp_token_resp" | grep -o '"sha1":"[^"]*"' | cut -d'"' -f4)

if [[ -z "$DISPATCHER_TOKEN" ]]; then
    echo "❌ Failed to create dispatcher API token. Response: $_disp_token_resp"
    exit 1
fi
echo "✓ Dispatcher token created"

echo "🔑 Creating reviewer API token ..."
curl -sf -X PATCH \
    -H "Authorization: token $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"login_name\":\"$REVIEWER_LOGIN\",\"source_id\":0,\"password\":\"$REVIEWER_PASS\",\"must_change_password\":false}" \
    "$FORGEJO_URL/api/v1/admin/users/$REVIEWER_LOGIN" > /dev/null

# Delete existing token if present, then create fresh.
curl -sf -X DELETE \
    -u "$REVIEWER_LOGIN:$REVIEWER_PASS" \
    "$FORGEJO_URL/api/v1/users/$REVIEWER_LOGIN/tokens/$REVIEWER_TOKEN_NAME" \
    > /dev/null 2>&1 || true

_rev_token_resp=$(curl -sf -X POST \
    -u "$REVIEWER_LOGIN:$REVIEWER_PASS" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"$REVIEWER_TOKEN_NAME\",\"scopes\":[\"write:issue\",\"read:issue\",\"write:repository\",\"read:repository\"]}" \
    "$FORGEJO_URL/api/v1/users/$REVIEWER_LOGIN/tokens")
REVIEWER_TOKEN=$(echo "$_rev_token_resp" | grep -o '"sha1":"[^"]*"' | cut -d'"' -f4)

if [[ -z "$REVIEWER_TOKEN" ]]; then
    echo "❌ Failed to create reviewer API token. Response: $_rev_token_resp"
    exit 1
fi
echo "✓ Reviewer token created"

# Store the reviewer token as a Forgejo Actions repo secret so the review
# workflow can submit formal PR reviews using the reviewer identity.
echo "🔑 Setting REVIEWER_TOKEN repo secret ..."
_repo_owner="${REPO_OWNER:-sysadmin}"
_repo_name="${REPO_NAME:-workflow-test}"
curl -sf -X PUT \
    -H "Authorization: token $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"data\":\"$REVIEWER_TOKEN\"}" \
    "$FORGEJO_URL/api/v1/repos/$_repo_owner/$_repo_name/actions/secrets/REVIEWER_TOKEN" \
    > /dev/null
echo "✓ REVIEWER_TOKEN secret set"

echo "👤 Setting human reviewer password ..."
curl -sf -X PATCH \
    -H "Authorization: token $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"login_name\":\"$HUMAN_LOGIN\",\"source_id\":0,\"password\":\"$HUMAN_PASS\",\"must_change_password\":false}" \
    "$FORGEJO_URL/api/v1/admin/users/$HUMAN_LOGIN" > /dev/null
echo "✓ Human reviewer '$HUMAN_LOGIN' password set"

# ── 8. Upload avatars ─────────────────────────────────────────────────────
#
# Looks for PNG files in infra/avatars/ named {username}.png.
# Uses each user's credentials to POST to /api/v1/user/avatar.

AVATAR_DIR="$ROOT/infra/avatars"
if [[ -d "$AVATAR_DIR" ]] && ls "$AVATAR_DIR"/*.png &>/dev/null; then
    echo "🖼  Uploading avatars ..."

    upload_avatar() {
        local login="$1" pass="$2" file="$3"
        local b64
        b64=$(base64 -i "$file" | tr -d '\n')
        curl -sf -X POST \
            -u "$login:$pass" \
            -H "Content-Type: application/json" \
            -d "{\"image\": \"$b64\"}" \
            "$FORGEJO_URL/api/v1/user/avatar" > /dev/null 2>&1 \
            && echo "  ✓ $login" \
            || echo "  ⚠ $login (avatar upload failed)"
    }

    password_for() {
        case "$1" in
            "$SIDECAR_LOGIN")    echo "$SIDECAR_PASS" ;;
            "$DISPATCHER_LOGIN") echo "$DISPATCHER_PASS" ;;
            "$REVIEWER_LOGIN")   echo "$REVIEWER_PASS" ;;
            "$HUMAN_LOGIN")      echo "$HUMAN_PASS" ;;
            "$ADMIN_USER")       echo "$ADMIN_PASS" ;;
            *)                   echo "$WORKER_PASS" ;;
        esac
    }

    for avatar_file in "$AVATAR_DIR"/*.png; do
        username=$(basename "$avatar_file" .png)
        upload_avatar "$username" "$(password_for "$username")" "$avatar_file"
    done
    echo "✓ Avatars uploaded"
else
    echo "⏭  No avatars found in infra/avatars/ (skipping)"
fi

# ── 9. Write .sidecar.env ─────────────────────────────────────────────────── ───────────────────────────────────────────────────
#
# These are consumed by the sidecar Docker container. URLs use Docker
# service names, not localhost. The docker-compose.yml environment section
# overrides FORGEJO_URL, NATS_URL, DB_PATH, and LISTEN_ADDR with
# docker-internal values — the .env file just carries the tokens.

cat > .sidecar.env <<EOF
FORGEJO_URL=http://forgejo:3000
FORGEJO_TOKEN=$SIDECAR_TOKEN
DISPATCHER_FORGEJO_TOKEN=$DISPATCHER_TOKEN
REVIEWER_FORGEJO_TOKEN=$REVIEWER_TOKEN
REVIEWER_HUMAN_LOGIN=$HUMAN_LOGIN
NATS_URL=nats://nats:4222
DB_PATH=/data/workflow.db
LISTEN_ADDR=0.0.0.0:8080
EOF
echo "✓ Wrote .sidecar.env"

# ── 10. Push workflow files to repo ─────────────────────────────────────────

echo "📄 Pushing workflow files to repo ..."
REPO_OWNER="${REPO_OWNER:-sysadmin}"
REPO_NAME="${REPO_NAME:-workflow-test}"

for wf in action/sim-work.yml action/agent-work.yml action/review-work.yml action/ci-rust.yml; do
    fname=$(basename "$wf")
    target_path=".forgejo/workflows/$fname"
    wf_content=$(base64 < "$wf")

    # Check if file exists (get its sha for update)
    existing_sha=$(curl -sf \
        -H "Authorization: token $ADMIN_TOKEN" \
        "$FORGEJO_URL/api/v1/repos/$REPO_OWNER/$REPO_NAME/contents/$target_path" \
        2>/dev/null | grep -o '"sha":"[^"]*"' | cut -d'"' -f4 || true)

    if [[ -n "$existing_sha" ]]; then
        curl -sf -X PUT \
            -H "Authorization: token $ADMIN_TOKEN" \
            -H "Content-Type: application/json" \
            -d "{\"content\":\"$wf_content\",\"message\":\"update $fname\",\"sha\":\"$existing_sha\"}" \
            "$FORGEJO_URL/api/v1/repos/$REPO_OWNER/$REPO_NAME/contents/$target_path" \
            > /dev/null
    else
        curl -sf -X POST \
            -H "Authorization: token $ADMIN_TOKEN" \
            -H "Content-Type: application/json" \
            -d "{\"content\":\"$wf_content\",\"message\":\"add $fname workflow\"}" \
            "$FORGEJO_URL/api/v1/repos/$REPO_OWNER/$REPO_NAME/contents/$target_path" \
            > /dev/null
    fi
    echo "  ✓ $target_path"
done

# ── 11. Build images ─────────────────────────────────────────────────────

echo "🔨 Building sidecar image (includes CDC + reviewer) ..."
docker build -t workflow-sidecar:latest -f Dockerfile.sidecar . 2>&1 | while IFS= read -r line; do printf '\r\033[K  %s' "${line:0:120}"; done; echo

echo "🔨 Building workflow-runner-env image ..."
docker build -t workflow-runner-env:latest -f Dockerfile.runner-env . 2>&1 | while IFS= read -r line; do printf '\r\033[K  %s' "${line:0:120}"; done; echo

echo "🔨 Building worker-cli ..."
cargo build -p worker-cli 2>&1 | while IFS= read -r line; do printf '\r\033[K  %s' "${line:0:120}"; done; echo
echo "✓ Build complete"

# ── 11b. Build flavored agent images ──────────────────────────────────────

if [[ -f "$ROOT/flavors.conf" ]]; then
    echo "🔨 Building base agent image ..."
    docker build -t workflow-agent:latest -f Dockerfile.agent . 2>&1 | while IFS= read -r line; do printf '\r\033[K  %s' "${line:0:120}"; done; echo
    RUNNER_EXTRA_LABELS=""
    while IFS=: read -r flavor dockerfile capability; do
        [[ "$flavor" =~ ^#.*$ || -z "$flavor" ]] && continue
        echo "🔨 Building agent flavor '$flavor' ..."
        docker build -t "workflow-agent-${flavor}:latest" -f "$dockerfile" . 2>&1 | while IFS= read -r line; do printf '\r\033[K  %s' "${line:0:120}"; done; echo
        # Build matching runner-env image if Dockerfile exists
        runner_dockerfile="Dockerfile.runner-env-${flavor}"
        if [[ -f "$ROOT/$runner_dockerfile" ]]; then
            echo "🔨 Building runner-env-${flavor} image ..."
            docker build -t "workflow-runner-env-${flavor}:latest" -f "$runner_dockerfile" . 2>&1 | while IFS= read -r line; do printf '\r\033[K  %s' "${line:0:120}"; done; echo
            RUNNER_EXTRA_LABELS="${RUNNER_EXTRA_LABELS},${flavor}:docker://workflow-runner-env-${flavor}:latest"
        fi
    done < "$ROOT/flavors.conf"
    if [[ -n "$RUNNER_EXTRA_LABELS" ]]; then
        echo "RUNNER_EXTRA_LABELS=$RUNNER_EXTRA_LABELS" >> .sidecar.env
    fi
    echo "✓ Agent images built"
fi

# ── 12. Get runner registration token ────────────────────────────────────

echo "🔑 Getting runner registration token ..."
REG_TOKEN=$(curl -sf \
    -H "Authorization: token $ADMIN_TOKEN" \
    "$FORGEJO_URL/api/v1/admin/runners/registration-token" \
    | grep -o '"token":"[^"]*"' | cut -d'"' -f4 || true)

if [[ -z "$REG_TOKEN" ]]; then
    REG_TOKEN=$(curl -sf -X GET \
        -u "$ADMIN_USER:$ADMIN_PASS" \
        "$FORGEJO_URL/api/v1/admin/runners/registration-token" \
        | grep -o '"token":"[^"]*"' | cut -d'"' -f4 || true)
fi

if [[ -z "$REG_TOKEN" ]]; then
    echo "⚠  Failed to get runner registration token — shared runner will not start."
    echo "   Make sure Actions is enabled in app.ini."
else
    echo "RUNNER_REGISTRATION_TOKEN=$REG_TOKEN" >> .sidecar.env
    echo "✓ Runner registration token obtained"
fi

# ── 13. Start CDC + sidecar + reviewer + runner ─────────────────────────

echo "🚀 Starting CDC + sidecar + reviewer + runner ..."
# Export only the vars docker-compose.yml needs for variable substitution.
# Don't source all of .sidecar.env — it contains FORGEJO_URL=http://forgejo:3000
# which would break host-side commands that need localhost.
export RUNNER_REGISTRATION_TOKEN=$(grep '^RUNNER_REGISTRATION_TOKEN=' .sidecar.env | cut -d= -f2- || true)
export RUNNER_EXTRA_LABELS=$(grep '^RUNNER_EXTRA_LABELS=' .sidecar.env | cut -d= -f2- || true)
docker compose up -d cdc sidecar reviewer runner

# Wait for sidecar to be ready
echo "⏳ Waiting for sidecar at http://localhost:8080 ..."
for i in $(seq 1 60); do
    curl -sf http://localhost:8080/jobs > /dev/null 2>&1 && break
    sleep 2
done
curl -sf http://localhost:8080/jobs > /dev/null 2>&1 || { echo "❌ Sidecar did not start. Check: docker compose logs sidecar"; exit 1; }
echo "✓ Sidecar is up"

# ── 12. Seed fixture ───────────────────────────────────────────────────────

if [[ "$SEED" == "true" ]]; then
    echo "🌱 Seeding fixture: $FIXTURE ..."
    FORGEJO_URL="$FORGEJO_URL" \
    FORGEJO_TOKEN="$SIDECAR_TOKEN" \
    SIDECAR_URL=http://localhost:8080 \
        "$ROOT/target/debug/worker-cli" seed sysadmin/workflow-test "$FIXTURE"

    # Brief pause for CDC to catch up
    sleep 2

    echo ""
    echo "📊 Graph state:"
    curl -sf http://localhost:8080/jobs | python3 -c "
import sys,json
jobs = json.load(sys.stdin)['jobs']
for j in sorted(jobs, key=lambda x: x['number']):
    deps = j['dependency_numbers']
    print(f'  #{j[\"number\"]:2d} {j[\"state\"]:12s} deps={str(deps):16s} {j[\"title\"]}')
print(f'\n  Total: {len(jobs)} jobs')
"
fi

# ── Done ──────────────────────────────────────────────────────────────────

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Setup complete!"
echo ""
echo "  Graph viewer:  http://localhost:8080/graph"
echo "  Forgejo:       $FORGEJO_URL"
echo "  Sidecar API:   http://localhost:8080"
echo ""
echo "  Human reviewer: $HUMAN_LOGIN / $HUMAN_PASS"
echo "    Log in at $FORGEJO_URL to review PRs manually"
echo ""
echo "  Logs:  docker compose logs -f"
echo "  Stop:  docker compose down"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
