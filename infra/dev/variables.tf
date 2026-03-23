variable "forgejo_url" {
  description = "Base URL of the Forgejo instance (e.g. http://localhost:3000)."
  type        = string
}

variable "forgejo_admin_token" {
  description = "Admin API token used by Terraform to manage resources."
  type        = string
  sensitive   = true
}

variable "forgejo_insecure" {
  description = "Skip TLS verification — only for local dev with self-signed certs."
  type        = bool
  default     = false
}

variable "repo_owner" {
  description = "Forgejo username or org that owns the workflow repo."
  type        = string
}

variable "repo_name" {
  description = "Repository name."
  type        = string
  default     = "workflow"
}

variable "email_domain" {
  description = "Domain used to generate email addresses for service accounts."
  type        = string
  default     = "example.com"
}

variable "repo_secrets" {
  description = "Map of Forgejo Actions secret name → value to set on the repo."
  type        = map(string)
  default     = {}
  sensitive   = true
}

