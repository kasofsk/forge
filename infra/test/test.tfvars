# Non-secret test environment defaults — safe to commit.
# Secrets (passwords, tokens) go in test.secrets.tfvars or TF_VAR_* env vars.

forgejo_url      = "http://localhost:3000"
forgejo_insecure = true

forgejo_admin_user = "sysadmin"   # must match ADMIN_USER in scripts/init.sh

repo_owner = "sysadmin"
repo_name  = "workflow-test"

sidecar_login = "workflow-sync"
dispatcher_login = "workflow-dispatcher"
worker_logins = ["worker-aria", "worker-blake", "worker-casey"]

email_domain = "test.local"

# Fixed passwords for service accounts so init.sh can use basic
# auth to create API tokens (the Forgejo tokens endpoint requires it).
sidecar_password    = "sidecar-test-1234"
dispatcher_password = "dispatcher-test-1234"

reviewer_login    = "workflow-reviewer"
reviewer_password = "reviewer-test-1234"
human_login       = "you"
human_password    = "human-test-1234"

sidecar_api_url = "http://localhost:8080"
