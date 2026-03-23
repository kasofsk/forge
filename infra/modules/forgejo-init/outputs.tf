output "repo_full_name" {
  description = "Full repository name in owner/repo format."
  value       = "${var.repo_owner}/${gitea_repository.workflow.name}"
}

output "repo_html_url" {
  description = "Browser URL for the repository."
  value       = gitea_repository.workflow.html_url
}

output "sidecar_login" {
  description = "Forgejo login for the sync service account."
  value       = gitea_user.sidecar.username
}

output "dispatcher_login" {
  description = "Forgejo login for the dispatcher service account."
  value       = gitea_user.dispatcher.username
}

output "worker_logins" {
  description = "Map of worker login → Forgejo user ID."
  value       = { for login, u in gitea_user.workers : login => u.id }
}

output "reviewer_login" {
  description = "Forgejo login for the reviewer service account."
  value       = gitea_user.reviewer.username
}

output "human_login" {
  description = "Forgejo login for the human reviewer account."
  value       = gitea_user.human.username
}

output "worker_initial_passwords" {
  description = "Initial passwords for worker accounts (sensitive)."
  sensitive   = true
  value       = { for login, _ in var.workers : login => local.worker_passwords[login] }
}
