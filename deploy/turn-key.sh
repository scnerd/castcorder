#!/usr/bin/env bash
# Creates the Cloudflare Realtime TURN key the Worker mints credentials from and stores its
# id and API token in deploy/secrets.sops.yaml (cf_turn_key_id, cf_turn_api_token).
#
# Run through `task deploy:turn-key`, which runs this inside `sops exec-env`, so the
# bootstrap token is only in this process's environment. The key's API token is shown by
# Cloudflare once, on creation; the OpenTofu provider does not keep it, so it lives in SOPS.
# Values travel on stdin, never in arguments or the log.
set -euo pipefail

secrets="${1:-deploy/secrets.sops.yaml}"
tfvars="deploy/tofu/terraform.tfvars"

if [ -z "${cloudflare_bootstrap_token:-}" ]; then
  echo "Run through: task deploy:turn-key" >&2
  exit 1
fi
if [ -n "${cf_turn_key_id:-}" ] && [ "${FORCE:-}" != "1" ]; then
  echo "$secrets already holds cf_turn_key_id. Set FORCE=1 to create a new key (delete the old one in the dashboard)." >&2
  exit 1
fi

account=$(sed -nE 's/^[[:space:]]*cloudflare_account_id[[:space:]]*=[[:space:]]*"([0-9a-f]{32})".*/\1/p' "$tfvars")
if [ -z "$account" ]; then
  echo "No cloudflare_account_id in $tfvars." >&2
  exit 1
fi

response=$(curl -sS --fail-with-body \
  -H @- \
  -H "Content-Type: application/json" \
  --data '{"name":"castcorder-production"}' \
  "https://api.cloudflare.com/client/v4/accounts/$account/calls/turn_keys" \
  <<<"Authorization: Bearer $cloudflare_bootstrap_token") || {
  echo "Creating the TURN key failed:" >&2
  printf '%s\n' "$response" | jq -c '.errors // .' >&2
  exit 1
}

uid=$(printf '%s' "$response" | jq -r '.result.uid // empty')
if [ -z "$uid" ] || [ -z "$(printf '%s' "$response" | jq -r '.result.key // empty')" ]; then
  echo "Cloudflare's answer has no uid or key:" >&2
  printf '%s' "$response" | jq -c 'del(.result.key)' >&2
  exit 1
fi

printf '%s' "$response" | jq '.result.uid' | sops set --value-stdin "$secrets" '["cf_turn_key_id"]'
printf '%s' "$response" | jq '.result.key' | sops set --value-stdin "$secrets" '["cf_turn_api_token"]'
echo "Created TURN key $uid and stored it in $secrets. Next: task deploy:infra"
