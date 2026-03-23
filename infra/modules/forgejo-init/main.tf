# ── Passwords ─────────────────────────────────────────────────────────────────

resource "random_password" "sidecar" {
  length  = 32
  special = true
}

# Always generate one per worker; locals decide whether to use it.
resource "random_password" "workers" {
  for_each = var.workers
  length   = 24
  special  = true
}

resource "random_password" "dispatcher" {
  length  = 32
  special = true
}

resource "random_password" "reviewer" {
  length  = 32
  special = true
}

resource "random_password" "human" {
  length  = 24
  special = true
}

locals {
  sidecar_password    = var.sidecar_password != "" ? var.sidecar_password : random_password.sidecar.result
  dispatcher_password = var.dispatcher_password != "" ? var.dispatcher_password : random_password.dispatcher.result
  reviewer_password   = var.reviewer_password != "" ? var.reviewer_password : random_password.reviewer.result
  human_password      = var.human_password != "" ? var.human_password : random_password.human.result

  worker_passwords = {
    for login, _ in var.workers :
    login => var.worker_default_password != "" ? var.worker_default_password : random_password.workers[login].result
  }
}

# ── Users ─────────────────────────────────────────────────────────────────────

resource "gitea_user" "sidecar" {
  username             = var.sidecar_login
  login_name           = var.sidecar_login
  password             = local.sidecar_password
  email                = var.sidecar_email
  must_change_password = false
  admin                = false
}

resource "gitea_user" "dispatcher" {
  username             = var.dispatcher_login
  login_name           = var.dispatcher_login
  password             = local.dispatcher_password
  email                = var.dispatcher_email
  must_change_password = false
  admin                = false
}

resource "gitea_user" "reviewer" {
  username             = var.reviewer_login
  login_name           = var.reviewer_login
  password             = local.reviewer_password
  email                = var.reviewer_email
  must_change_password = false
  admin                = false
}

resource "gitea_user" "human" {
  username             = var.human_login
  login_name           = var.human_login
  password             = local.human_password
  email                = var.human_email
  must_change_password = false
  admin                = false
}

resource "gitea_user" "workers" {
  for_each = var.workers

  username             = each.key
  login_name           = each.key
  password             = local.worker_passwords[each.key]
  email                = each.value.email
  must_change_password = true
  admin                = false
}

# ── Repository ────────────────────────────────────────────────────────────────

resource "gitea_repository" "workflow" {
  username          = var.repo_owner
  name              = var.repo_name
  description       = var.repo_description
  private           = var.repo_private
  auto_init         = true
  has_issues        = true
  has_pull_requests = true
  has_wiki          = false
  has_projects      = false
}

# ── Delete default labels ─────────────────────────────────────────────────────
#
# Forgejo creates default labels (bug, duplicate, enhancement, etc.) on new repos.
# Remove them so only our status labels remain.

resource "null_resource" "delete_default_labels" {
  triggers = {
    repo = "${var.repo_owner}/${gitea_repository.workflow.name}"
  }

  provisioner "local-exec" {
    command = <<-CMD
      # List all labels and delete any that aren't status: labels
      curl -sf \
        -H "Authorization: token ${var.forgejo_admin_token}" \
        "${var.forgejo_url}/api/v1/repos/${var.repo_owner}/${gitea_repository.workflow.name}/labels?limit=50" \
      | python3 -c "
import sys, json, subprocess
labels = json.load(sys.stdin)
for l in labels:
    if not l['name'].startswith('status:') and not l['name'].startswith('priority:') \
       and not l['name'].startswith('capability:') and not l['name'].startswith('timeout:') \
       and not l['name'].startswith('retry:'):
        subprocess.run([
            'curl', '-sf', '-X', 'DELETE',
            '-H', 'Authorization: token ${var.forgejo_admin_token}',
            '${var.forgejo_url}/api/v1/repos/${var.repo_owner}/${gitea_repository.workflow.name}/labels/' + str(l['id'])
        ], capture_output=True)
" || true
    CMD
  }

  depends_on = [gitea_repository.workflow]
}

# ── Status labels ─────────────────────────────────────────────────────────────
#
# The Lerentis/gitea provider has no gitea_label resource, so we use the
# Forgejo REST API directly via null_resource + local-exec.
# Labels are created idempotently (curl ignores 422 Unprocessable).

locals {
  status_labels = {
    # ── Status labels ──
    "status:on-deck" = {
      color       = "#0075ca"
      description = "Ready to be claimed by a worker"
    }
    "status:blocked" = {
      color       = "#e4e669"
      description = "Waiting on one or more dependencies"
    }
    "status:on-the-stack" = {
      color       = "#fbca04"
      description = "Claimed — a worker is actively executing this"
    }
    "status:in-review" = {
      color       = "#8b5cf6"
      description = "Work complete, awaiting human review"
    }
    "status:done" = {
      color       = "#0e8a16"
      description = "Reviewed and merged"
    }
    "status:failed" = {
      color       = "#b60205"
      description = "Worker reported failure"
    }
    "status:changes-requested" = {
      color       = "#f59e0b"
      description = "Reviewer requested changes — routed back to original worker"
    }
    "status:revoked" = {
      color       = "#6b7280"
      description = "Rejected without completion — dependents blocked"
    }
    "status:on-ice" = {
      color       = "#cfd3d7"
      description = "Deliberately paused — will not be claimed"
    }

    # ── Worker type labels ──
    "worker:action" = {
      color       = "#1d76db"
      description = "Restrict to action workers (Forgejo Actions)"
    }
    "worker:interactive" = {
      color       = "#5319e7"
      description = "Restrict to interactive Claude agent workers"
    }

    # ── Platform labels ──
    "platform:linux" = {
      color       = "#006b75"
      description = "Requires a Linux worker"
    }
    "platform:macos" = {
      color       = "#0e8a16"
      description = "Requires a macOS worker"
    }
    "platform:arm64" = {
      color       = "#bfdadc"
      description = "Requires an ARM64 worker"
    }

    # ── Review labels ──
    "review:human" = {
      color       = "#d93f0b"
      description = "Always escalate to human reviewer"
    }
    "review:low" = {
      color       = "#c2e0c6"
      description = "Reviewer needs only low confidence to auto-approve"
    }
    "review:medium" = {
      color       = "#fbca04"
      description = "Reviewer needs medium confidence to auto-approve"
    }
    "review:high" = {
      color       = "#e99695"
      description = "Reviewer needs high confidence to auto-approve (default)"
    }

    # ── Capability labels ──
    "capability:rust" = {
      color       = "#dea584"
      description = "Requires Rust toolchain (cargo, clippy, rustfmt)"
    }
  }
}

resource "null_resource" "labels" {
  for_each = local.status_labels

  triggers = {
    repo  = "${var.repo_owner}/${gitea_repository.workflow.name}"
    name  = each.key
    color = each.value.color
  }

  provisioner "local-exec" {
    command = <<-CMD
      curl -sf -X POST \
        -H "Authorization: token ${var.forgejo_admin_token}" \
        -H "Content-Type: application/json" \
        -d '{"name":"${each.key}","color":"${each.value.color}","description":"${each.value.description}"}' \
        "${var.forgejo_url}/api/v1/repos/${var.repo_owner}/${gitea_repository.workflow.name}/labels" \
        > /dev/null || true
    CMD
  }

  depends_on = [gitea_repository.workflow, null_resource.delete_default_labels]
}

# ── Sidecar repo access ─────────────────────────────────────────────────────
#
# The sidecar needs write access to the repo to manage labels on issues.
# Forgejo 14+ enforces repo-level permissions in addition to token scopes.

# ── Worker repo access ──────────────────────────────────────────────────────
#
# Workers get "write" permission: push branches, create PRs, comment on issues.
# They cannot merge to protected branches or administer the repo.

resource "null_resource" "worker_collaborators" {
  for_each = var.workers

  triggers = {
    repo  = "${var.repo_owner}/${gitea_repository.workflow.name}"
    login = gitea_user.workers[each.key].username
  }

  provisioner "local-exec" {
    command = <<-CMD
      curl -sf -X PUT \
        -H "Authorization: token ${var.forgejo_admin_token}" \
        -H "Content-Type: application/json" \
        -d '{"permission":"write"}' \
        "${var.forgejo_url}/api/v1/repos/${var.repo_owner}/${gitea_repository.workflow.name}/collaborators/${gitea_user.workers[each.key].username}" \
        > /dev/null || true
    CMD
  }

  depends_on = [gitea_repository.workflow, gitea_user.workers]
}

resource "null_resource" "dispatcher_collaborator" {
  triggers = {
    repo  = "${var.repo_owner}/${gitea_repository.workflow.name}"
    login = gitea_user.dispatcher.username
  }

  provisioner "local-exec" {
    command = <<-CMD
      curl -sf -X PUT \
        -H "Authorization: token ${var.forgejo_admin_token}" \
        -H "Content-Type: application/json" \
        -d '{"permission":"write"}' \
        "${var.forgejo_url}/api/v1/repos/${var.repo_owner}/${gitea_repository.workflow.name}/collaborators/${gitea_user.dispatcher.username}" \
        > /dev/null || true
    CMD
  }

  depends_on = [gitea_repository.workflow, gitea_user.dispatcher]
}

resource "null_resource" "reviewer_collaborator" {
  triggers = {
    repo  = "${var.repo_owner}/${gitea_repository.workflow.name}"
    login = gitea_user.reviewer.username
  }

  provisioner "local-exec" {
    command = <<-CMD
      curl -sf -X PUT \
        -H "Authorization: token ${var.forgejo_admin_token}" \
        -H "Content-Type: application/json" \
        -d '{"permission":"write"}' \
        "${var.forgejo_url}/api/v1/repos/${var.repo_owner}/${gitea_repository.workflow.name}/collaborators/${gitea_user.reviewer.username}" \
        > /dev/null || true
    CMD
  }

  depends_on = [gitea_repository.workflow, gitea_user.reviewer]
}

resource "null_resource" "human_collaborator" {
  triggers = {
    repo  = "${var.repo_owner}/${gitea_repository.workflow.name}"
    login = gitea_user.human.username
  }

  provisioner "local-exec" {
    command = <<-CMD
      curl -sf -X PUT \
        -H "Authorization: token ${var.forgejo_admin_token}" \
        -H "Content-Type: application/json" \
        -d '{"permission":"write"}' \
        "${var.forgejo_url}/api/v1/repos/${var.repo_owner}/${gitea_repository.workflow.name}/collaborators/${gitea_user.human.username}" \
        > /dev/null || true
    CMD
  }

  depends_on = [gitea_repository.workflow, gitea_user.human]
}

# ── Repo secrets (Forgejo Actions) ─────────────────────────────────────────
#
# The Lerentis/gitea provider has no secret resource, so we use the REST API
# via null_resource + local-exec (same pattern as labels above).

resource "null_resource" "repo_secrets" {
  for_each = nonsensitive(var.repo_secrets)

  triggers = {
    repo   = "${var.repo_owner}/${gitea_repository.workflow.name}"
    secret = each.key
  }

  provisioner "local-exec" {
    command = <<-CMD
      curl -sf -X PUT \
        -H "Authorization: token ${var.forgejo_admin_token}" \
        -H "Content-Type: application/json" \
        -d '{"data":"${each.value}"}' \
        "${var.forgejo_url}/api/v1/repos/${var.repo_owner}/${gitea_repository.workflow.name}/actions/secrets/${each.key}" \
        > /dev/null || true
    CMD
  }

  depends_on = [gitea_repository.workflow]
}

resource "null_resource" "sidecar_collaborator" {
  triggers = {
    repo  = "${var.repo_owner}/${gitea_repository.workflow.name}"
    login = gitea_user.sidecar.username
  }

  provisioner "local-exec" {
    command = <<-CMD
      curl -sf -X PUT \
        -H "Authorization: token ${var.forgejo_admin_token}" \
        -H "Content-Type: application/json" \
        -d '{"permission":"write"}' \
        "${var.forgejo_url}/api/v1/repos/${var.repo_owner}/${gitea_repository.workflow.name}/collaborators/${gitea_user.sidecar.username}" \
        > /dev/null || true
    CMD
  }

  depends_on = [gitea_repository.workflow, gitea_user.sidecar]
}

