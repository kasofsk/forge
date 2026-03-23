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
    null = {
      source  = "hashicorp/null"
      version = "~> 3.2"
    }
  }
}

# The Lerentis/gitea provider requires basic auth (username+password) to
# create repos and tokens for other users. API token auth is insufficient.
provider "gitea" {
  base_url = var.forgejo_url
  username = var.forgejo_admin_user
  password = var.forgejo_admin_password
  insecure = var.forgejo_insecure
}

module "forgejo" {
  source = "../modules/forgejo-init"

  # Passed through for null_resource curl commands (labels, webhooks).
  forgejo_url         = var.forgejo_url
  forgejo_admin_token = var.forgejo_admin_token

  repo_owner       = var.repo_owner
  repo_name        = var.repo_name
  repo_description = "Workflow integration test environment — managed by Terraform"
  repo_private     = false

  sidecar_login    = var.sidecar_login
  sidecar_email    = "${var.sidecar_login}@${var.email_domain}"
  sidecar_password = var.sidecar_password

  dispatcher_login    = var.dispatcher_login
  dispatcher_email    = "${var.dispatcher_login}@${var.email_domain}"
  dispatcher_password = var.dispatcher_password

  reviewer_login    = var.reviewer_login
  reviewer_email    = "${var.reviewer_login}@${var.email_domain}"
  reviewer_password = var.reviewer_password

  human_login    = var.human_login
  human_email    = "${var.human_login}@${var.email_domain}"
  human_password = var.human_password

  repo_secrets = var.repo_secrets

  workers = {
    for login in var.worker_logins :
    login => { email = "${login}@${var.email_domain}" }
  }

}

# ── Outputs ───────────────────────────────────────────────────────────────────

output "repo_owner" {
  value = var.repo_owner
}

output "repo_name" {
  value = var.repo_name
}

output "repo_full_name" {
  value = module.forgejo.repo_full_name
}

output "sidecar_login" {
  value = module.forgejo.sidecar_login
}

output "dispatcher_login" {
  value = module.forgejo.dispatcher_login
}

output "worker_passwords" {
  value     = module.forgejo.worker_initial_passwords
  sensitive = true
}

output "env_exports" {
  description = "Source this in your shell before running integration tests. FORGEJO_TOKEN must be set separately via init.sh."
  value = <<-ENV
    export FORGEJO_URL="${var.forgejo_url}"
    export SIDECAR_URL="${var.sidecar_api_url}"
    export TEST_OWNER="${var.repo_owner}"
    export TEST_REPO="${var.repo_name}"
  ENV
}
