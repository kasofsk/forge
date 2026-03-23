terraform {
  required_version = ">= 1.5"

  required_providers {
    gitea = {
      source  = "Lerentis/gitea"
      version = "~> 0.16"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
  }
}

provider "gitea" {
  base_url = var.forgejo_url
  token    = var.forgejo_admin_token
  # Set insecure = true when using a self-signed cert in local dev.
  insecure = var.forgejo_insecure
}

module "forgejo" {
  source = "../modules/forgejo-init"

  # Repo
  repo_owner       = var.repo_owner
  repo_name        = var.repo_name
  repo_description = "Workflow task tracking (dev)"
  repo_private     = false

  # Sync service account (labels, deps)
  sidecar_login = "workflow-sync"
  sidecar_email = "workflow-sync@${var.email_domain}"

  # Dispatcher service account (assignees, comments)
  dispatcher_login = "workflow-dispatcher"
  dispatcher_email = "workflow-dispatcher@${var.email_domain}"

  # Reviewer service account (automated PR review)
  reviewer_login = "workflow-reviewer"
  reviewer_email = "workflow-reviewer@${var.email_domain}"

  # Human reviewer account (escalation target)
  human_login = "you"
  human_email = "you@${var.email_domain}"

  repo_secrets = var.repo_secrets

  # Workers — add as many as you need
  workers = {
    "worker-alpha" = { email = "worker-alpha@${var.email_domain}" }
    "worker-beta"  = { email = "worker-beta@${var.email_domain}" }
    "worker-gamma" = { email = "worker-gamma@${var.email_domain}" }
  }
}

# ── Outputs ───────────────────────────────────────────────────────────────────

output "repo" {
  value = module.forgejo.repo_html_url
}

output "sidecar_token" {
  value     = module.forgejo.sidecar_token
  sensitive = true
}

output "worker_passwords" {
  value     = module.forgejo.worker_initial_passwords
  sensitive = true
}
