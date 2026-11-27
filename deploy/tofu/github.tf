# GitHub: the production environment the deploy job runs in, its secrets, and the
# repository secrets CI's test:worker:relay uses for real Cloudflare TURN.

locals {
  # The token and TURN key values are only returned when Cloudflare creates them. If a
  # refresh ever loses one, fail rather than overwrite a GitHub secret with nothing.
  environment_secrets = {
    CLOUDFLARE_API_TOKEN  = cloudflare_account_token.deploy.value
    CLOUDFLARE_ACCOUNT_ID = local.account_id
    CF_TURN_KEY_ID        = var.cf_turn_key_id
    CF_TURN_API_TOKEN     = var.cf_turn_api_token
    CF_ANALYTICS_TOKEN    = cloudflare_account_token.analytics.value
  }
  repository_secrets = {
    CF_TURN_KEY_ID    = var.cf_turn_key_id
    CF_TURN_API_TOKEN = var.cf_turn_api_token
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
  for_each = toset(["CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID", "CF_TURN_KEY_ID", "CF_TURN_API_TOKEN", "CF_ANALYTICS_TOKEN"])

  repository  = var.github_repository
  environment = github_repository_environment.production.environment
  secret_name = each.value
  value       = local.environment_secrets[each.value]

  lifecycle {
    precondition {
      condition     = try(length(local.environment_secrets[each.value]) > 0, false)
      error_message = "No value for this secret: Cloudflare returns token and key values only on creation. For CF_TURN_*, run `task deploy:turn-key`; for a token, replace it (tofu apply -replace=cloudflare_account_token.deploy or .analytics)."
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
      error_message = "No value for this secret: run `task deploy:turn-key` to create the TURN key and store it in deploy/secrets.sops.yaml."
    }
  }
}
