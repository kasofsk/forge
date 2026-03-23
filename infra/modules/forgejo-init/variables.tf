# ── Forgejo connection (needed for null_resource provisioners) ─────────────────

variable "forgejo_url" {
  description = "Base URL of the Forgejo instance (e.g. http://localhost:3000)."
  type        = string
}

variable "forgejo_admin_token" {
  description = "Admin API token used to create labels and webhooks via the REST API."
  type        = string
  sensitive   = true
}

# ── Repository ────────────────────────────────────────────────────────────────

variable "repo_owner" {
  description = "Forgejo username or org that will own the workflow repository."
  type        = string
}

variable "repo_name" {
  description = "Name of the workflow repository to create."
  type        = string
  default     = "workflow"
}

variable "repo_description" {
  description = "Repository description."
  type        = string
  default     = "Workflow task tracking"
}

variable "repo_private" {
  description = "Whether the repository is private."
  type        = bool
  default     = false
}

# ── Sidecar service account ───────────────────────────────────────────────────

variable "sidecar_login" {
  description = "Username for the sync service account (label/dep management)."
  type        = string
  default     = "workflow-sync"
}

variable "sidecar_email" {
  description = "Email address for the sync service account."
  type        = string
}

variable "sidecar_password" {
  description = "Password for the sync service account. Auto-generated if empty."
  type        = string
  default     = ""
  sensitive   = true
}

variable "dispatcher_login" {
  description = "Username for the dispatcher service account (assignees/comments)."
  type        = string
  default     = "workflow-dispatcher"
}

variable "dispatcher_email" {
  description = "Email address for the dispatcher service account."
  type        = string
}

variable "dispatcher_password" {
  description = "Password for the dispatcher service account. Auto-generated if empty."
  type        = string
  default     = ""
  sensitive   = true
}

# ── Workers ───────────────────────────────────────────────────────────────────

variable "workers" {
  description = <<-EOT
    Map of worker login → configuration.
    Example:
      workers = {
        "worker-alpha" = { email = "alpha@example.com" }
      }
  EOT
  type = map(object({
    email = string
  }))
  default = {}
}

variable "worker_default_password" {
  description = "Initial password for worker accounts. Auto-generated per worker if empty."
  type        = string
  default     = ""
  sensitive   = true
}

# ── Reviewer service account ─────────────────────────────────────────────────

variable "reviewer_login" {
  description = "Username for the automated reviewer service account."
  type        = string
  default     = "workflow-reviewer"
}

variable "reviewer_email" {
  description = "Email address for the reviewer service account."
  type        = string
}

variable "reviewer_password" {
  description = "Password for the reviewer service account. Auto-generated if empty."
  type        = string
  default     = ""
  sensitive   = true
}

# ── Human reviewer ───────────────────────────────────────────────────────────

variable "human_login" {
  description = "Username for the human reviewer account."
  type        = string
  default     = "you"
}

variable "human_email" {
  description = "Email address for the human reviewer account."
  type        = string
}

variable "human_password" {
  description = "Password for the human reviewer account. Auto-generated if empty."
  type        = string
  default     = ""
  sensitive   = true
}

# ── Repo secrets (Forgejo Actions) ───────────────────────────────────────────

variable "repo_secrets" {
  description = <<-EOT
    Map of Forgejo Actions secret name → value to set on the repo.
    The provider has no native resource for secrets, so these are set via the REST API.
    Example: { "CLAUDE_CODE_OAUTH_TOKEN" = "sk-ant-..." }
  EOT
  type      = map(string)
  default   = {}
  sensitive = true
}
