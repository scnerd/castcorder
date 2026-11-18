# The public services run serverless on Cloudflare's free tier

Browser guests need the client over trusted HTTPS, every host needs a rendezvous it can reach from behind NAT, and some calls need a relay. We want these to cost nothing at our scale and need no server to patch. So the project's public services run on Cloudflare:

- **Client and rendezvous: one Worker.** The Worker serves the client bundles as static assets at `/v/<version>/` (ADR 0002) and is the rendezvous. Each host key gets one Durable Object holding the host's WebSocket with hibernation, so an idle host costs nothing while connected, and guests' sealed offers are routed to it. The Worker is written in Rust (workers-rs). The mailbox logic moves into an I/O-free crate shared with the existing Rust rendezvous, which stays the self-hosted and test implementation. Both are AGPL.
- **STUN:** `stun.cloudflare.com`.
- **Relay: Cloudflare Realtime TURN.** The Worker mints short-lived credentials per host key, as the rendezvous does for coturn, with a cap on how many credentials a host key may mint per day. 1,000 GB a month is free, then $0.05/GB. Cloudflare's TURN also listens on TCP and TLS 443, which gives the TCP fallback almost for free. Stock coturn remains the self-hosted relay and the one CI tests against.
- **Domain:** the Worker's free `*.workers.dev` hostname until the project buys its domain. The Worker keeps answering on that hostname afterwards and redirects pages to the new one (the fragment holding the host key and call secret survives a redirect), so call links already handed out keep working. Hosts take the rendezvous origin from configuration, so the move is a new default, not a protocol change.
- **Everything is public.** The Worker code, the Wrangler configuration, the deploy workflow and the infrastructure that sets it up live in this repository. The credentials the deploy uses (the deploy API token, the TURN key) live in GitHub Actions secrets and `wrangler secret`, never in the tree in the clear. There is no private repository.
- **Setup is code (amended 2026-11-18).** OpenTofu in `deploy/tofu` creates the deploy API token and the Realtime TURN key, and writes them into the GitHub `production` environment (restricted to `main`) and the repository secrets CI uses. Wrangler still deploys the Worker. The only inputs a person supplies are a Cloudflare bootstrap token (allowed to create tokens and TURN keys) and the passphrase for the OpenTofu state. They live in one committed SOPS file, `deploy/secrets.sops.yaml`, encrypted to the maintainer's age key, and reach tofu only through `sops exec-env`. The state is committed too, encrypted by OpenTofu's native state encryption with that passphrase, and the configuration refuses unencrypted state or plans.

## Considered options

- **A small VM running the Rust rendezvous and coturn.** Simplest code path, but a server to patch and pay for, and relay bandwidth billed by the VM provider.
- **Cloudflare Pages for the client, a separate Worker for the rendezvous.** Two deployments and two origins. One Worker keeps the client and rendezvous on one origin, which the call link and linking code already assume.
- **coturn behind Cloudflare.** Cloudflare does not proxy arbitrary UDP on the free tier, so coturn would still need its own server.
- **A private repository for deployment details.** Nothing in the deployment is secret except a few credentials, and those fit the platforms' own secret stores or one encrypted file.
- **Click-by-click setup with no encrypted file (the original decision).** No secret in the tree at all, but every credential was created and copied by hand through two dashboards, nothing recorded what existed, and rotating meant repeating it. The maintainer chose SOPS and OpenTofu instead: the setup is reviewable and repeatable, rotation is `tofu apply -replace=...`, and the cost is one committed encrypted file and an encrypted state whose safety rests on the age key and the passphrase.
- **A remote state backend (Terraform Cloud, R2).** No file in the tree, but another account or bucket to bootstrap before the first apply. With one maintainer and rare changes, encrypted state in git needs nothing more; concurrent applies are not a concern.

## Consequences

- Two rendezvous implementations must agree. The shared mailbox crate holds the logic, and the browser e2e runs against both (the Worker under `wrangler dev`).
- Relay metering by coturn's Prometheus exporter does not carry over. On Cloudflare, credential-minting caps bound each host key's relay use, and byte metering comes from Cloudflare's analytics if it can attribute usage per credential.
- Whoever applies `deploy/tofu` needs the age key that decrypts `deploy/secrets.sops.yaml` and a `gh` login with admin rights on the repository. Losing the age key or the passphrase means starting the state afresh: delete the old deploy token and TURN key in the dashboard, remove `terraform.tfstate`, write a new SOPS file and apply again.
- Cloudflare is a dependency for browser guests and for hosts using the default rendezvous. Self-hosting stays possible with the Rust rendezvous and coturn.
