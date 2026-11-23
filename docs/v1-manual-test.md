# v1 manual test checklist

Run this against the draft release before publishing it (cast-3ya; step 5 of
[release.md](release.md)). Terms are those of [CONTEXT.md](../CONTEXT.md). Tick each box as you
go and note the version, OS and browser you used. When something fails, collect the logs (last
section) and stop; do not publish.

Setup: two or three machines (or one machine plus a phone), on different networks where you
can (one on mobile data is ideal), and the draft release's installers.

## 1. Install per OS

- [ ] **Windows.** Run the NSIS `.exe` from the draft. Expect: SmartScreen may warn (unsigned):
  More info, Run anyway. The app starts with a home window and a tray icon.
- [ ] **macOS (unsigned).** Open the `.dmg`, drag Castcorder to Applications. First launch:
  right-click the app, Open, Open (or System Settings, Privacy and Security, Open Anyway).
  Expect: the home window and a menu bar icon.
- [ ] **Linux .deb.** `sudo apt install ./castcorder_*.deb`, then start it from the menu.
  Expect: it starts with a tray icon and apt pulls in the GStreamer dependencies.
- [ ] **Linux AppImage.** `chmod +x Castcorder*.AppImage && ./Castcorder*.AppImage`. Expect:
  the same.
- [ ] **Version.** On each OS the app shows the draft's version `X.Y.Z`, not a dev build.
- [ ] **Close and quit.** Closing the window hides it to the tray and the host keeps serving;
  tray Quit stops it. Starting again works with no "port in use" error.

## 2. Host, browser guest and app guest over the internet

Use the default rendezvous (no flags), with the guests on a different network from the host.

- [ ] **Call link.** On the host, open a room. Expect: a call link
  `https://castcorder.app/v/X.Y.Z/#...` (this release's version) with a Copy button.
- [ ] **Browser guest (Chrome or Edge).** Open the link on another network, enter a name, allow
  the microphone, Join. Expect: both sides see each other's tile and hear each other within a
  few seconds.
- [ ] **Browser guest (Firefox).** Same. Expect: the same.
- [ ] **App guest.** On a second machine with the app installed, open the same link (it opens
  through `castcorder://`). Expect: a call window joins and all three hear each other.
- [ ] **Producer controls.** At the host, Start take, then Stop take. Expect: the REC pill and
  red rule show for everyone during the take only, and the producer page lists each
  participant's track.

## 3. Forced relay

- [ ] **Browser guest relayed.** Add `?iceTransportPolicy=relay` to the call link (before the
  `#`) and join. Expect: audio both ways; the guest's log shows `relay_candidates` above 0.
- [ ] **Host relayed.** Quit the app and start it with `--relay-only` (for example
  `Castcorder.exe --relay-only`, or `task host -- --relay-only` from a checkout), then have a
  browser guest join. Expect: the call works through TURN.
- [ ] **No secrets shown.** The page, console and logs never show the TURN API token.

## 4. Device selection and in-call switching

- [ ] **Lobby pickers.** The lobby lists Microphone, Camera and Speakers (Speakers only where
  the browser allows it), "System default" first. Pick a non-default microphone and reload.
  Expect: the pick is remembered.
- [ ] **Switch microphone.** In the call, open Devices and change the live microphone. Expect:
  others hear the new one with no rejoin, and a running take carries on.
- [ ] **Switch camera and speakers.** Change both in Devices. Expect: others see the new camera,
  and sound moves to the chosen speakers.
- [ ] **Backup microphone.** Click "Add backup microphone" and pick a second input. Expect: it
  records as its own source (`mic-2`) and is not heard live.
- [ ] **Unplug.** Unplug the in-use USB microphone mid-call, then plug it back in. Expect: no
  crash; the track continues in a new segment.

## 5. Native mic on the host

- [ ] **Join with the native mic.** On the installed app's producer page, use "Microphone on
  this machine" and pick an input. Expect: guests hear the host and no GStreamer error appears.
- [ ] **Change input.** Leave, then join with a different native input. Expect: it works
  without restarting the app.
- [ ] **Track.** The take holds `takeNN_<name>_mic.wav` (plus a FLAC copy) and it plays back.

## 6. Record a take and verify files

Record one take of at least 2 minutes with the host, a browser guest and an app guest all
talking, with a stretch where each speaks alone and a few seconds of silence at each end.

- [ ] **Where.** "Open recordings folder" at the host (or the tray item) opens
  `<room>\<call>\takeNN\`.
- [ ] **Per-participant tracks.** One audio file and one JSON sidecar per participant. Open
  each in an editor. Expect: each holds audio for the whole take (same length within a second
  or two, no cut-off at the end) and carries that participant's own voice.
- [ ] **States.** The producer page shows each track go recording, receiving N%, verified,
  then aligned.
- [ ] **Aligned tracks.** `takeNN\aligned\` holds one aligned copy per audio track, equal
  length, plus the markers and alignment summary. Dropped into a DAW at zero, a loud word lines
  up across tracks.
- [ ] **Safety tracks.** The host kept a safety track of each guest's live stream.
- [ ] **Second take.** Start another: it is `take02` with its own full set of tracks.

## 7. Leave and background sync

- [ ] **Browser guest leaves.** After a take, a guest clicks Leave. Expect: its page lists
  "Your recordings", which go waiting, uploading N%, then delivered to the host, with no
  microphone prompt.
- [ ] **App guest in the tray.** Close the app guest's call window after a take. Expect: the
  window hides, the tray stays, and the outbox keeps syncing (the tray status shows syncing).
- [ ] **Sync waits for the take.** A guest leaves while a take still records. Expect: nothing
  transfers until the take stops, then it does.
- [ ] **Host restart.** Quit the host while a guest's tracks are pending, then start it again.
  Expect: the transfer resumes by itself and the files verify.
- [ ] **Reopen later.** Reopen the guest page (or restart the app guest) some minutes later.
  Expect: pending tracks still arrive without joining.

## 8. Rejoin

- [ ] **Drop and return.** A browser guest closes its tab mid-call and reopens the call link
  within a minute. Expect: it rejoins the same call (same folder), not a new one.
- [ ] **Everyone leaves.** All leave, then rejoin the link a few minutes later. Expect: the
  same call carries on and take numbers continue.
- [ ] **Rooms list.** A guest's list of rooms shows the room; opening it joins the current call.
- [ ] **Regenerated link.** A producer regenerates the call link. Expect: the old link stops
  working and a trusted device still rejoins from its list of rooms.

## 9. Linking

- [ ] **Link a device.** On the host app's home "You" card, click "Link a device". Expect: a
  one-time link, good for 10 minutes.
- [ ] **Link a browser.** Open it in a browser. Expect: "Link this browser"; after confirming,
  the browser shows the person's name and the app lists the device.
- [ ] **Link a second app install.** Use a new link on another machine's app. Expect: it
  becomes a device of the same person.
- [ ] **Single use.** Open an already used link again. Expect: it is refused.

## 10. Phone browser

- [ ] **Join from a phone** (Chrome on Android, Safari on iPhone) over mobile data. Expect: the
  lobby fits the screen, Join works after allowing the microphone, and audio goes both ways.
- [ ] **Leave is visible.** In the call the Leave button is on screen without scrolling or
  zooming.
- [ ] **Camera off shows an avatar.** Turn the camera off. Expect: others see an avatar tile
  (not a frozen or black frame); turning it on restores video.
- [ ] **Take with the phone.** Record a short take. Expect: the phone's track reaches the host
  after Leave.

## 11. Release publish

After everything above passes and you publish the draft.

- [ ] **Publish run.** The Publish workflow starts Deploy on `main`, and the run is green.
- [ ] **Web client.** `https://castcorder.app/v/X.Y.Z/` loads the browser client.
- [ ] **Deploy check.** `task deploy:check -- X.Y.Z` passes.
- [ ] **Fresh call link.** A host on the published installer makes a call link pinned to
  `/v/X.Y.Z/` and a browser guest joins it.
- [ ] **Draft assets.** The release has every installer and `castcorder-client-X.Y.Z.tar.gz`,
  and its notes match the changelog entry.

## Logs

When something fails, collect these before retrying.

- **Host.** `host-<start>.jsonl` in `<library>\logs\`, where the library is
  `%USERPROFILE%\Castcorder` (`~/Castcorder` on macOS and Linux). `task host` prints the
  library and `Logs:` path at start.
- **Guests.** Their logs sync to the host and land in `<library>\<room>\<call>\logs\`. Look for
  `relay_candidates`, `gathering`, and any `error` or `warn` line.
- **Browser guest, before its logs sync.** The browser console (F12), and in the Network tab
  the WebSocket to the rendezvous, which should be open (status 101).
- **App guest.** Same `logs\` folder at the host once synced; until then they stay on that
  machine.
- **Worker and TURN.** The Cloudflare dashboard, Workers, the Castcorder Worker, Logs.
- **Report.** Note the version, OS, browser, whether forced relay was on and the time of the
  failure, and attach the host log and the guest's log.
