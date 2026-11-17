# Castcorder

Peer-to-peer podcast recording: participants talk over a live call while each machine records its own sources locally at full quality, then delivers the files to the host.

## Language

### People

**Host**:
The desktop app install that owns a room and its calls: it forwards the live streams, receives the tracks, and carries out producer instructions. A host is a machine, not a person, and need not be a participant in its own call.
_Avoid_: Owner, server, admin

**Participant**:
Someone present in a call with their own sources, seen and heard by the others.
_Avoid_: Attendee, caller, peer, user

**Guest**:
Any participant who is not at the host.

**Producer**:
A participant holding host controls for a call: starting and stopping takes, selecting sources, locking the room, removing people. Whoever is at the host is a producer; the host can also grant the role to a trusted device.
_Avoid_: Co-host, admin, moderator

**Device**:
One app install or browser profile, identified by a key it generates for itself. Tracks, rejoins, and roles attach to devices, not to people.
_Avoid_: Client, peer, account

**Trusted Device**:
A device that joined a room while a producer was present. Only trusted devices may enter a locked room or be granted the producer role, and a trusted device is told a room's current call link, so it can rejoin from its list of rooms after the link is regenerated.
_Avoid_: Paired device, known device

**App Guest**:
A guest who joins using the desktop app.

**Browser Guest**:
A guest who joins through the call link in a web browser, without installing anything.
_Avoid_: Web guest, web client

### Calls and recordings

**Room**:
A call factory owned by a host and reachable through one call link, such as a recurring show. Joining a room puts you in its current call: the active or recently active one, or a new call if the last one has been idle for a while. It keeps its settings, trusted devices, producers and bans across calls.
_Avoid_: Channel, space, project

**Call**:
A persistent recording container in a room: every take and track of one recording effort, saved together in one folder. A call can be rejoined at any time, even days later, so a call where everyone dropped carries on where it left off. A room starts a new call only after its current one has been idle for a while, or when a producer starts or continues a particular call from the lobby.
_Avoid_: Session, meeting, episode

**Call Link**:
The URL a host shares so guests can join a room. It stays the same for every call in the room until a producer regenerates it.
_Avoid_: Invite, join URL, room link

**Take**:
One start-to-stop recording span within a call. A call can have many takes.
_Avoid_: Recording, clip, segment

**Track**:
One participant's recording of a single source for one take (e.g. Alice's microphone for take 2). A track's audio is exactly what the device delivered: no echo cancellation, noise suppression, gain, or alignment is ever applied to it.
_Avoid_: File, stem, recording

**Timing Metadata**:
The clock-synchronisation record captured alongside a track that places its samples on the call's shared timeline.
_Avoid_: Sync data, timecode

**Aligned Track**:
A derived copy of an audio track, rendered by the host's app from the track and its timing metadata, so that all aligned tracks of a take start together, stay in step, and have equal length. The original track is always kept. Video tracks are never aligned.
_Avoid_: Synced track, processed track

**Live Stream**:
The lossy real-time media participants see and hear during a call. It may be echo-cancelled and noise-suppressed, and it is never the source of a track.
_Avoid_: Feed, call audio

**Safety Track**:
The host's own recording of one guest's live stream for a take, saved as received, kept as a fallback if that guest's tracks never arrive.
_Avoid_: Safety mix, backup recording

**Recording Profile**:
The settings a producer chooses for a call's tracks: whether video is recorded at all, and how audio and video are encoded. Guests do not alter it; a guest that cannot comply is reported to the host.
_Avoid_: Preset, quality settings

### Devices and delivery

**Source**:
One capture device on a participant's machine, such as a microphone or webcam. Each participant has one live source per kind (what others hear and see) and a set of recorded sources, each of which produces its own track. A producer can see and select any participant's sources.
_Avoid_: Input, capture device

**Segment**:
A continuous run of a track. A source dropout or crash ends one segment; the track continues in a new segment when the source returns.
_Avoid_: Part, fragment, chunk

**Outbox**:
A participant's local store of tracks the host has not yet confirmed receiving. It syncs to the host whenever the host is connected and no take is recording, audio before video, oldest take first.
_Avoid_: Upload queue, pending uploads

**Sync-only Connection**:
A device's connection to the host that only delivers its outbox: no media, not a participant, never counted as anyone in a call. A device opens one whenever it is in no call and its outbox holds tracks for that host (a guest that left, the app in the tray, a page reopened later). The host admits it by device key beside any live call, refuses a device kicked from the room, and accepts each track only for a call the device was in.
_Avoid_: Background connection, upload session
