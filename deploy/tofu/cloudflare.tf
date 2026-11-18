# Cloudflare: the token wrangler deploys with and the Realtime TURN key the Worker mints
# relay credentials from. The Worker itself is deployed by wrangler in CI
# (.github/workflows/deploy.yml), not here.

data "cloudflare_accounts" "visible" {
  count = var.cloudflare_account_id == "" ? 1 : 0
}

locals {
  # try(): with an id in terraform.tfvars the lookup does not exist; with none, the token
  # must see exactly one account (the precondition below names the fix otherwise).
  account_id = var.cloudflare_account_id != "" ? var.cloudflare_account_id : try(one(data.cloudflare_accounts.visible[0].result).id, "")
}

data "cloudflare_account_api_token_permission_groups_list" "deploy" {
  for_each   = toset(var.deploy_token_permission_groups)
  account_id = local.account_id
  name       = each.value
}

locals {
  # Exact name matches only; a name matching none (or several) drops out, and the
  # precondition below fails the plan.
  deploy_permission_group_ids = compact([
    for name in var.deploy_token_permission_groups :
    try(one([for g in data.cloudflare_account_api_token_permission_groups_list.deploy[name].result : g.id if g.name == name]), null)
  ])
}

# Account-owned, so it outlives any one member's login. Scoped to this account.
resource "cloudflare_account_token" "deploy" {
  account_id = local.account_id
  name       = "castcorder deploy (wrangler, GitHub Actions)"

  policies = [{
    effect            = "allow"
    permission_groups = [for id in local.deploy_permission_group_ids : { id = id }]
    resources = jsonencode({
      "com.cloudflare.api.account.${local.account_id}" = "*"
    })
  }]

  lifecycle {
    precondition {
      condition     = local.account_id != ""
      error_message = "No Cloudflare account id: set cloudflare_account_id in deploy/tofu/terraform.tfvars (the bootstrap token sees no account, or several)."
    }
    precondition {
      condition     = length(local.deploy_permission_group_ids) == length(var.deploy_token_permission_groups)
      error_message = "Every name in deploy_token_permission_groups must match exactly one Cloudflare permission group."
    }
  }
}

# The Realtime TURN key is not managed here: Cloudflare returns its API token only on
# creation and cloudflare_calls_turn_app (5.29.0) does not keep it in state, so
# `task deploy:turn-key` creates it and stores cf_turn_key_id and cf_turn_api_token in
# deploy/secrets.sops.yaml, which reach tofu as variables.
