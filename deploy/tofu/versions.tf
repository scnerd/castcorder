# Provider pins and state encryption. The state (terraform.tfstate) is committed, so it
# is always encrypted: OpenTofu's native encryption with a key derived (pbkdf2) from
# tofu_state_passphrase in deploy/secrets.sops.yaml. `enforced = true` and no fallback
# block mean tofu refuses to read or write unencrypted state or plans.
terraform {
  required_version = ">= 1.10.0"

  required_providers {
    cloudflare = {
      source  = "cloudflare/cloudflare"
      version = "= 5.29.0"
    }
    github = {
      source  = "integrations/github"
      version = "= 6.13.0"
    }
  }

  encryption {
    key_provider "pbkdf2" "passphrase" {
      passphrase = var.state_passphrase
    }

    method "aes_gcm" "passphrase" {
      keys = key_provider.pbkdf2.passphrase
    }

    state {
      method   = method.aes_gcm.passphrase
      enforced = true
    }

    plan {
      method   = method.aes_gcm.passphrase
      enforced = true
    }
  }
}

# CLOUDFLARE_API_TOKEN: the bootstrap token (cloudflare_bootstrap_token), set by the task.
provider "cloudflare" {}

# GITHUB_TOKEN: `gh auth token`, set by the task.
provider "github" {
  owner = var.github_owner
}
