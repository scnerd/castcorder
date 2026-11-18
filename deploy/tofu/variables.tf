variable "state_passphrase" {
  description = "Passphrase for state and plan encryption (tofu_state_passphrase in deploy/secrets.sops.yaml)."
  type        = string
  sensitive   = true

  validation {
    condition     = length(var.state_passphrase) >= 16
    error_message = "tofu_state_passphrase must be at least 16 characters."
  }
}

variable "cloudflare_account_id" {
  description = "Cloudflare account id (not secret). Empty: read from the bootstrap token, which must see exactly one account."
  type        = string
  default     = ""

  validation {
    condition     = var.cloudflare_account_id == "" || can(regex("^[0-9a-f]{32}$", var.cloudflare_account_id))
    error_message = "cloudflare_account_id is 32 lowercase hex characters (Workers & Pages > Account details), or empty."
  }
}

variable "github_owner" {
  description = "GitHub owner of the repository."
  type        = string
  default     = "scnerd"
}

variable "github_repository" {
  description = "GitHub repository that deploys the Worker."
  type        = string
  default     = "castcorder"
}

variable "deploy_token_permission_groups" {
  description = "Account permission groups for the wrangler deploy token. Creating the Worker on its first deploy needs account-wide Workers Scripts Write (the Workers Admin role); Account Settings Read lets wrangler read the account and its workers.dev subdomain."
  type        = list(string)
  default     = ["Workers Scripts Write", "Account Settings Read"]
}
