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

All four live in the GitHub environment `production` (names exactly):

| Secret                  | What it is                                            |
| ----------------------- | ----------------------------------------------------- |
| `CLOUDFLARE_API_TOKEN`  | API token from the "Edit Cloudflare Workers" template |
| `CLOUDFLARE_ACCOUNT_ID` | the Cloudflare account id                             |
| `CF_TURN_KEY_ID`        | the Realtime TURN key's Token ID                      |
| `CF_TURN_API_TOKEN`     | the Realtime TURN key's API token                     |

### One-time setup, click by click

1. **Create the Cloudflare account.** Go to <https://dash.cloudflare.com/sign-up>, sign
   up with an email and password, and verify the email. The free plan is enough; no
   domain is needed.
2. **Choose the workers.dev subdomain.** In the dashboard open **Workers & Pages**. If
   asked, pick your subdomain there; otherwise select **Change** next to **Your
   subdomain**. Choose `castcorder` if it is free: the Worker is then
   `https://castcorder.castcorder.workers.dev`, which the apps already default to. With
   another subdomain `<sub>` the Worker is `https://castcorder.<sub>.workers.dev`:
   change `PROJECT_RENDEZVOUS` in `castcorder/crates/castcorder-host/src/lib.rs` to it
   (the one place the default lives; the deploy job warns while they differ).
3. **Find the account id.** On the **Workers & Pages** page, under **Account details**,
   select the copy button next to **Account ID** (or press Ctrl+K and run `Copy account ID`). Keep it for step 6.
4. **Create the deploy API token.** Go to the **Account API tokens** page
   (<https://dash.cloudflare.com/?to=/:account/api-tokens>) and select **Create Token**.
   Under **Permission policies**, open the **Custom** dropdown and select **Edit
   Cloudflare Workers**. Name the token `castcorder deploy` and scope it to this account
   only (if the form also asks for zones, pick all zones of this account; there are
   none). Select **Continue to summary**, then **Create Token**, and copy the token
   (shown only once).
5. **Create the Realtime TURN key.** Open the Realtime TURN page of the dashboard
   (<https://dash.cloudflare.com/?to=/:account/calls>) and create a TURN key, named
   `castcorder-production`. Copy its **Turn Token ID** and its **API token** (the token
   is shown only once). The Worker uses this long-lived key to mint short-lived
   credentials; it is never sent to hosts or guests.
6. **Add the GitHub secrets.** In the GitHub repository go to **Settings** >
   **Environments**. If `production` is already listed (the first push to `main` after
   this workflow landed creates it), select it; otherwise select **New environment**,
   name it `production`, and select **Configure environment**. Under **Deployment
   branches and tags** choose **Selected branches and tags** and add `main`. Under **Environment secrets** select **Add environment
   secret** four times: `CLOUDFLARE_API_TOKEN` (step 4), `CLOUDFLARE_ACCOUNT_ID` (step
   3), `CF_TURN_KEY_ID` and `CF_TURN_API_TOKEN` (step 5). Optional: also add the two
   `CF_TURN_*` values under **Settings** > **Secrets and variables** > **Actions** >
   **New repository secret**, so CI's `test:worker:relay` exercises real Cloudflare
   TURN too (it skips that part without them).
7. **Run the workflow.** **Actions** > **Deploy** > **Run workflow** on `main` (or push
   to `main`). The first run creates the Worker, its Durable Object class and its rate
   limiter.
8. **Verify.** The **deploy** job links the environment URL, and its "Smoke check the
   live Worker" step ends with `SMOKE OK`. By hand:
   - `https://castcorder.<sub>.workers.dev/healthz` answers `ok`;
   - `https://castcorder.<sub>.workers.dev/v/0.0.0/` loads the client;
   - `castcorder-host --dist castcorder/crates/castcorder-client/dist --rendezvous https://castcorder.<sub>.workers.dev`
     prints `CALL_LINK https://castcorder.<sub>.workers.dev/v/0.0.0/#...`; open that
     link in a browser on another network and join the call;
   - in the dashboard, **Workers & Pages** > **castcorder** > **Settings** lists
     `CF_TURN_KEY_ID` and `CF_TURN_API_TOKEN` as secrets, and **Logs** shows requests.

To rotate a credential, replace the GitHub secret and rerun the workflow. To remove a
secret from the Worker, run `npx wrangler secret delete NAME` in `castcorder/worker`.

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
  setup above, or deploy by hand: `task worker:assets` and `task worker:build`, then in
  `castcorder/worker` `npx wrangler login` and `npx wrangler deploy`. For the relay,
  either `npx wrangler secret put CF_TURN_KEY_ID` and `CF_TURN_API_TOKEN` (Cloudflare
  TURN), or `RELAY=coturn` with `TURN_URLS` and the secret `TURN_SECRET`, or
  `RELAY=none` (see `castcorder/worker/src/relay.rs`). Start hosts with
  `--rendezvous https://castcorder.<sub>.workers.dev`, or change `PROJECT_RENDEZVOUS`
  and build with `--features project-rendezvous` to make it your builds' default.
