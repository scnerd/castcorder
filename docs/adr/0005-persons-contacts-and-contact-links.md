# Persons own devices through a person key; contacts link devices outside calls

Until now every relationship attached to a device key: a room trusted devices, and a guest reached a host only through a call link or an offer sent to it. People use several devices, and they want trusted collaborators to see each other online, sync in the background, and invite each other into a room without passing links around. So we add a person layer above devices and a contact relationship between persons.

- **A person key vouches for devices.** The first app install of a person generates a person key. Linking a new device means approving it from an existing one, which gives it a certificate signed by the person key; the key is copied to each linked app device, and any of them can link or revoke a device. Revocations are signed and pushed to contacts and room hosts. Losing every device, or the person key leaking, means starting over as a new person. A browser profile can be linked like any device; one that never is stays an anonymous device, as before.
- **Contacts are mutual and grant no room access.** Both sides agree, started from a call ("add as contact") or a contact code shared as a link or QR code (CONTEXT.md "Contact Code"; not a contact link, which is the connection below): making and sending the code is the maker's agreement and using it the other's, so nobody accepts a second time (2026-11-27). There is no directory to search. Room trust stays per room, but can be granted to a whole person. A room belongs to a person and is hosted by the device holding its library; the owner's other devices are trusted producers in it.
- **Presence is a sealed ping, the contact link is on demand.** App installs hold a rendezvous connection whenever they run (a setting can turn this off). A device answers sealed presence pings from contacts only, so knowing a key reveals nothing to a stranger. A contact link is the sync-only connection's wire contract (WebRTC, data channels only) between any two contacts' devices, opened when there is something to deliver, an invite, or a status view, and closed when idle. It carries track delivery, invites, and shared room status; arbitrary file sending is out of scope for v1.
- **Invites carry the call link.** An invite names its sender and the room's call link, appears as a tray notification, and the room's normal join rules apply.

## Considered options

- **Device-only trust, persons as named groups.** Simpler, but every contact must accept every new device of every person, and revoking means touching every room by hand.
- **A separate transport (QUIC, a Syncthing-style protocol) for contact links.** Better for long transfers, but browsers can only speak WebRTC, and we already have a tested sync-only connection plus TURN fallback.
- **Persistent links between online contacts.** Lower latency for invites, but one TURN allocation per relayed pair at all times. Sealed pings through the rendezvous cost nothing while idle.
- **Hiding presence from the rendezvous.** The rendezvous necessarily sees which keys are online and who asks to reach whom; contents stay sealed. We accept that metadata on our own or a self-hosted rendezvous rather than add mixing or onion routing.

## Consequences

- The rendezvous now holds connections from every running app install, not only hosts, and routes pings and offers between any two devices.
- Room trust, producer grants and bans can name a person as well as a device.
- Losing the person key is unrecoverable by design: there is no account server to reset it.
