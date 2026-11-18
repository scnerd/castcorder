# GitHub: the production environment the deploy job runs in, its secrets, and the
# repository secrets CI's test:worker:relay uses for real Cloudflare TURN.

locals {
  # The token and TURN key values are only returned when Cloudflare creates them. If a
  # refresh ever loses one, fail rather than overwrite a GitHub secret with nothing.
  environment_secrets = {
    CLOUDFLARE_API_TOKEN  = cloudflare_account_token.deploy.value
    CLOUDFLARE_ACCOUNT_ID = local.account_id
    CF_TURN_KEY_ID        = cloudflare_calls_turn_app.production.uid
    CF_TURN_API_TOKEN     = cloudflare_calls_turn_app.production.key
  }
  repository_secrets = {
    CF_TURN_KEY_ID    = cloudflare_calls_turn_app.production.uid
    CF_TURN_API_TOKEN = cloudflare_calls_turn_app.production.key
  }
}

# The first push to main after deploy.yml landed already created the environment.
import {
  to = github_repository_environment.production
  id = "${var.github_repository}:production"
}

resource "github_repository_environment" "production" {
  repository  = var.github_repository
  environment = "production"

  deployment_branch_policy {
    protected_branches     = false
    custom_branch_policies = true
  }
}

resource "github_repository_environment_deployment_policy" "main" {
  repository     = var.github_repository
  environment    = github_repository_environment.production.environment
  branch_pattern = "main"
}

resource "github_actions_environment_secret" "production" {
  for_each = toset(["CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID", "CF_TURN_KEY_ID", "CF_TURN_API_TOKEN"])

  repository  = var.github_repository
  environment = github_repository_environment.production.environment
  secret_name = each.value
  value       = local.environment_secrets[each.value]

  lifecycle {
    precondition {
      condition     = try(length(local.environment_secrets[each.value]) > 0, false)
      error_message = "No value for this secret: Cloudflare returns token and key values only on creation. Replace the source resource (tofu apply -replace=...) to mint a new one."
    }
  }
}

resource "github_actions_secret" "ci" {
  for_each = toset(["CF_TURN_KEY_ID", "CF_TURN_API_TOKEN"])

  repository  = var.github_repository
  secret_name = each.value
  value       = local.repository_secrets[each.value]

  lifecycle {
    precondition {
      condition     = try(length(local.repository_secrets[each.value]) > 0, false)
      error_message = "No value for this secret: Cloudflare returns key values only on creation. Replace cloudflare_calls_turn_app.production (tofu apply -replace=...) to mint a new one."
    }
  }
}
