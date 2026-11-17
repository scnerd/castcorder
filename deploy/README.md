# deploy

Infrastructure for the public services (see [docs/design.md](../docs/design.md),
"Services", and [ADR 0004](../docs/adr/0004-cloudflare-serverless-deployment.md)).
The project's rendezvous and client site run as one Cloudflare Worker,
[castcorder/worker](../castcorder/worker): the client bundles as static assets at
`/v/<version>/`, the rendezvous at `/ws`, one Durable Object per host key. Self-hosters
run `castcorder-rendezvous` and coturn instead (`task dev:up`, `docker-compose.yml`).

Deploying it (the workflow, the account, secrets) is cast-lxt.4, and Cloudflare TURN
credentials are cast-lxt.3; neither exists yet. Nothing here needs a Cloudflare account.

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
