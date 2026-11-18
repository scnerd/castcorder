# deploy

Infrastructure for the public services (see [docs/design.md](../docs/design.md),
"Services", and [ADR 0004](../docs/adr/0004-cloudflare-serverless-deployment.md)).
The project's rendezvous and client site run as one Cloudflare Worker,
[castcorder/worker](../castcorder/worker): the client bundles as static assets at
`/v/<version>/`, the rendezvous at `/ws`, one Durable Object per host key. Self-hosters
run `castcorder-rendezvous` and coturn instead (`task dev:up`, `docker-compose.yml`).

[.github/workflows/deploy.yml](../.github/workflows/deploy.yml) deploys it on every push
to `main`; [Deploying to Cloudflare](#deploying-to-cloudflare) is the one-time setup.
Running the Worker locally needs no Cloudflare account.

## Running the Worker locally

Needs Rust with the wasm32 target (rust-toolchain.toml), trunk, and Node. Everything
else installs inside `castcorder/worker` (`worker-build` in `.tools/`, `wrangler` in
`node_modules/`); nothing global.

```sh
task worker:dev                      # build the client and the Worker, then wrangler dev on 127.0.0.1:8787
task worker:dev -- --port 8790       # extra wrangler arguments after --
```

`wrangler dev` runs the Worker and its Durable Objects in a local `workerd`, with local
state in `castcorder/worker/.wrangler/`. Then:

- `http://127.0.0.1:8787/v/0.0.0/` is the client.
- `castcorder-host --rendezvous http://127.0.0.1:8787` registers a host; its call links
  point at the Worker.
- `ws://127.0.0.1:8787/ws?mailbox=<host id>` is the mailbox. Connections that name no
  host are refused (see `castcorder_link::wire`).

The steps on their own:

| Task                      | What it does                                                           |
| ------------------------- | ---------------------------------------------------------------------- |
| `task worker:tools`       | installs worker-build (pinned) and wrangler inside `castcorder/worker` |
| `task worker:build`       | `worker-build --release` into `castcorder/worker/build`                |
| `task worker:assets`      | `trunk build` and stages it at `castcorder/worker/assets/v/0.0.0/`     |
| `task worker:clippy`      | clippy for wasm32 (part of `task check`)                               |
| `task test:system:worker` | the tier 3 system test against the Worker under `wrangler dev`         |

Configuration is `castcorder/worker/wrangler.jsonc`: the Durable Object binding and its
SQLite migration (the free plan's only kind), the per-IP connection limit
(`ratelimits`, simulated locally), and `STUN_URLS` (the STUN servers every hello carries;
the system test sets it empty, since loopback needs none).

## Deploying to Cloudflare

### What the workflow does

`deploy.yml` runs on every push to `main`, on a manual run (**Actions** > **Deploy** >
**Run workflow**), and on pull requests (build and `wrangler deploy --dry-run` only).

1. **build** (every event, no secrets): stages every published GitHub release's client
   ([stage-releases.sh](stage-releases.sh)), builds this commit's client with trunk and
   stages it at `/v/<workspace version>/` unless that version is already a published
   release (released versions are immutable, ADR 0002), builds the Worker, checks the
   free plan's static asset limits (20,000 files per version, 25 MiB per file; one
   client is about 11 files and 9 MB), and runs a dry run.
2. **deploy** (`main` only, in the `production` environment): `wrangler deploy` with the
   TURN secrets uploaded alongside the new version (`--secrets-file`, a private
   temporary file written by `jq`, never echoed), then [smoke.mjs](smoke.mjs) against
   the live Worker: `GET /v/<version>/` for every version, `/healthz`, and a WebSocket
   to `/ws?mailbox=` that must receive the mailbox's hello. Without
   `CLOUDFLARE_API_TOKEN` or `CLOUDFLARE_ACCOUNT_ID` this job skips with a notice and
   stays green. Without the TURN secrets it deploys with `RELAY=none` and a warning
   (calls that need a relay fail; direct calls work).

**Every released client stays deployed.** A Workers deploy replaces all static assets,
so each deploy must carry every version ever released. `release.yml` attaches the web
client as `castcorder-client-<version>.tar.gz` to each release, and `stage-releases.sh`
unpacks every published (non-draft) release's archive into
`castcorder/worker/assets/v/<version>/`. This ships exactly the bytes that were
released, needs no rebuild of old tags and no storage outside GitHub, and fails the
deploy if a published release lacks its archive. At about 11 files per version the
20,000-file limit allows over a thousand releases. Until the first release exists, only
`main`'s client (version `0.0.0`) is deployed.

### Secrets

OpenTofu ([tofu/](tofu)) creates every credential the deploy uses and writes it to
GitHub; nobody copies one by hand. In the GitHub environment `production` (deployments
from `main` only):

| Secret                  | What it is                                                     |
| ----------------------- | -------------------------------------------------------------- |
| `CLOUDFLARE_API_TOKEN`  | account-owned API token `castcorder deploy` that wrangler uses |
| `CLOUDFLARE_ACCOUNT_ID` | the Cloudflare account id                                      |
| `CF_TURN_KEY_ID`        | the Realtime TURN key `castcorder-production`'s Turn Token ID  |
| `CF_TURN_API_TOKEN`     | that TURN key's API token                                      |

`CF_TURN_KEY_ID` and `CF_TURN_API_TOKEN` are also repository secrets, so CI's
`test:worker:relay` exercises real Cloudflare TURN. The deploy token holds Workers Scripts
Write (creating the Worker on its first deploy needs account-wide Workers Admin) and
Account Settings Read, for this account only (`deploy_token_permission_groups` in
[tofu/variables.tf](tofu/variables.tf)).

The inputs OpenTofu needs live in `deploy/secrets.sops.yaml`, committed and encrypted
with [SOPS](https://github.com/getsops/sops) to the maintainer's age key
([.sops.yaml](../.sops.yaml)):

| Key                          | What it is                                                                                                  |
| ---------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `cloudflare_bootstrap_token` | account API token: Account API Tokens Edit, Realtime (Calls) Edit, Account Settings Read; this account only |
| `tofu_state_passphrase`      | 16 or more characters; encrypts the OpenTofu state and plans                                                |

The tasks hand them to tofu through `sops exec-env`, so they exist only in the
environment of the processes that need them: never decrypted to disk, never in the
log. The GitHub provider authenticates with `gh auth token` at run time. The state,
`deploy/tofu/terraform.tfstate`, is committed: OpenTofu encrypts it (and any saved plan)
with a key derived from the passphrase, and refuses to read or write it unencrypted.

### One-time setup

Needs [OpenTofu](https://opentofu.org) (`tofu`), [SOPS](https://github.com/getsops/sops),
[age](https://github.com/FiloSottile/age) with your key at the SOPS default location
(`%APPDATA%\sops\age\keys.txt` on Windows, `~/.config/sops/age/keys.txt` on Linux, or set
`SOPS_AGE_KEY_FILE`), [GitHub CLI](https://cli.github.com) logged in as a repository
admin (`gh auth login`), Task and Node. You need a Cloudflare account whose workers.dev
subdomain is `jexmax` (the Worker is then `https://castcorder.jexmax.workers.dev`, the
apps' default `PROJECT_RENDEZVOUS`) and the bootstrap token above.

1. **Write the inputs.** `task deploy:secrets:edit` opens a new
   `deploy/secrets.sops.yaml` in your editor (SOPS uses `$EDITOR`; for example
   `EDITOR="code --wait"`). Enter the two keys and save; SOPS encrypts on save:

   ```yaml
   cloudflare_bootstrap_token: <the bootstrap token>
   tofu_state_passphrase: <a long random passphrase>
   ```

   Keep the passphrase somewhere else too (a password manager): without it the state
   cannot be read.

2. **Check the account id.** `cloudflare_account_id` in
   [tofu/terraform.tfvars](tofu/terraform.tfvars) is the project's account (**Workers &
   Pages** > **Account details** > **Account ID**; it is not secret). A fork sets its
   own; left empty, tofu reads it from the bootstrap token, which then must see exactly
   one account.

3. **Create the TURN key.** `task deploy:turn-key` creates the Realtime TURN key
   `castcorder-production` with the bootstrap token and stores its id and API token in
   `deploy/secrets.sops.yaml` as `cf_turn_key_id` and `cf_turn_api_token`. Cloudflare
   shows the API token only on creation and the OpenTofu provider does not keep it, so
   SOPS holds it rather than tofu. Run it once; `task deploy:turn-key FORCE=1` makes a new key (then delete
   the old one under **Realtime** > **TURN Server**).

4. **Create the infrastructure.** `task deploy:infra:plan` shows what will change;
   `task deploy:infra` shows the plan again and applies it after you type `yes`. It
   creates the deploy token, adopts the existing `production`
   environment, restricts it to `main`, and writes the six secrets. Then commit
   `deploy/secrets.sops.yaml` and the encrypted `deploy/tofu/terraform.tfstate`.

5. **Deploy.** **Actions** > **Deploy** > **Run workflow** on `main`, or push to `main`.
   The first run creates the Worker, its Durable Object class and its rate limiter.

6. **Verify.** `task deploy:check` runs [smoke.mjs](smoke.mjs) against
   `PROJECT_RENDEZVOUS` (versions after `--`; default the workspace version) and ends
   with `SMOKE OK`. The deploy job runs the same check and links the environment URL.
   By hand:

   - `https://castcorder.jexmax.workers.dev/healthz` answers `ok`;
   - `castcorder-host --dist castcorder/crates/castcorder-client/dist --rendezvous https://castcorder.jexmax.workers.dev`
     prints `CALL_LINK https://castcorder.jexmax.workers.dev/v/0.0.0/#...`; open that
     link in a browser on another network and join the call;
   - in the dashboard, **Workers & Pages** > **castcorder** > **Settings** lists
     `CF_TURN_KEY_ID` and `CF_TURN_API_TOKEN` as secrets, and **Logs** shows requests.

**Changing it later.** Edit `deploy/tofu`, then `task deploy:infra` and commit the
state. To rotate a credential, replace its resource and rerun the workflow:
`sops exec-env deploy/secrets.sops.yaml "task deploy:infra:tofu -- apply -replace=cloudflare_account_token.deploy"`.
For the TURN key, `task deploy:turn-key FORCE=1` then `task deploy:infra`. Cloudflare
returns token and key values only when it creates them, so tofu refuses to write an empty GitHub secret rather than
overwrite a good one. `task deploy:infra:validate` (fmt and validate, no secrets) runs in
CI's hygiene job. To remove a secret from the Worker, run
`npx wrangler secret delete NAME` in `castcorder/worker`.

### The default rendezvous

`PROJECT_RENDEZVOUS` in `castcorder/crates/castcorder-host/src/lib.rs` is the project's
public rendezvous. The `castcorder-host` binary and the desktop app register with it
when built with the `project-rendezvous` feature, which `release.yml` turns on for the
installers; local, CI and test builds leave it off, so they never reach the public
service unasked. `--rendezvous URL` overrides it and `--rendezvous off` turns it off.
For a release-like host: `cargo run -p castcorder-host --features project-rendezvous -- --dist <client dist>`.

## Self-hosting

A host works on its LAN with no rendezvous at all. To serve call links from your own
infrastructure instead of the project's:

- **The Rust rendezvous and coturn** (no Cloudflare): run `castcorder-rendezvous`
  (`--client <version>=<client dist>` serves the client at `/v/<version>/`, `--turn` and
  `--turn-secret` mint coturn credentials) and coturn, behind TLS, as
  `docker-compose.yml` does for development (`task dev:up`). Start hosts with
  `--rendezvous https://your.host`.
- **Your own Worker** (your Cloudflare account): fork the repository and follow the
  setup above (put your age key in `.sops.yaml`, your owner in `github_owner` and
  `github_repository` in `deploy/tofu/terraform.tfvars`, and delete the committed
  `deploy/secrets.sops.yaml` and `terraform.tfstate` if present), or deploy by hand: `task worker:assets` and `task worker:build`, then in
  `castcorder/worker` `npx wrangler login` and `npx wrangler deploy`. For the relay,
  either `npx wrangler secret put CF_TURN_KEY_ID` and `CF_TURN_API_TOKEN` (Cloudflare
  TURN), or `RELAY=coturn` with `TURN_URLS` and the secret `TURN_SECRET`, or
  `RELAY=none` (see `castcorder/worker/src/relay.rs`). Start hosts with
  `--rendezvous https://castcorder.<sub>.workers.dev`, or change `PROJECT_RENDEZVOUS`
  and build with `--features project-rendezvous` to make it your builds' default.
