# The browser client is a project-hosted static site, pinned by version in the call link

A browser guest must download the client over trusted HTTPS before it can open a peer-to-peer connection, and a host's machine cannot reliably serve that (no inbound reachability behind NAT/CGNAT, no trusted certificate). So the project hosts the client as a static site, every release is published at its own `/v/<version>/` path, and the host's app generates call links pinned to its own version, with the host key and call secret in the URL fragment so the site never sees them. We rejected host-served pages (direct with DDNS + ACME, or tunnelled through the relay) because they add a DNS service, a certificate broker and an HTTP relay only to serve a bundle that is identical for every host.

## Consequences

- Hosts set up nothing; the project domain becomes a dependency for browser guests.
- A "release" must exist as a concept, and old client versions stay published for as long as the matching app version is supported (indefinitely for now).
- A protocol handshake is still the backstop; a revoked version tells the guest to ask the host to update.
- The app embeds the same client bundle, so letting a host serve it from their own base URL is a cheap post-v1 option.
