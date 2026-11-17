//! Many rooms, one live call (docs/design.md "Rooms and calls"; the disk side
//! is crate::rooms).
//!
//! The host thread has one room loaded (its security, `Call::room`) and at
//! most one of its calls open (`Call::library`). The call is live while
//! anyone is in it: a guest connection or the host's own participant. Then:
//!
//! - An offer for the loaded room enters as before. An offer for another
//!   room waits (`waiting:`, the page asks again) until the live call
//!   empties; once nothing is live, an offer for any room loads that room
//!   and is judged by its security.
//! - The first one admitted into a room that is not live opens the room's
//!   current call: the most recently active one if it was active less than
//!   the idle gap ago, else a new call ([`crate::rooms::route`]). A gated
//!   room admits only a producer (or the host machine) first, so it becomes
//!   live when a producer opens it; an open room becomes live with anyone.
//! - A producer moves everyone into a new or a past call of the room
//!   (`RoomAction::NewCall`, `ContinueCall`; the home's "Start new call" and
//!   "Continue") while no take records and every device has synced. Each
//!   device is told the call it is in (`Event::Call`) and keeps its tracks
//!   per call.
//! - The open call is marked active every minute while live and when it
//!   empties, so the idle gap counts from the last person leaving and
//!   survives a restart.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use castcorder_core::library::{Library, Puller};
use castcorder_core::safety::{self, SafetyRecorder, SafetyWriter};
use castcorder_core::sync::SyncConfig;
use castcorder_port_clock::Clock;
use castcorder_port_disk::{FsStorage, Storage};
use castcorder_protocol::{
    CallRef, CallSummary, ErrorKind, Event, Response, TakeId, TakeStatus, TrackOffer,
};
use castcorder_transport::GuestId;
use serde::Serialize;
use tracing::{info, warn};

use super::Call;
use crate::rendezvous::{LinkBase, SharedSecrets};
use crate::room::Room;
use crate::rooms;

/// How often a live call is marked active (host timeline, us).
const TOUCH_EVERY_US: i64 = 60_000_000;

/// The refusal for an offer to a room while another room's call is live.
pub const WAITING_OTHER: &str =
    "waiting: the host is in another call right now; you join when it is free";

/// Where the rooms live and how joiners are routed.
pub struct Rooms {
    /// The library root.
    pub library: PathBuf,
    /// Every room's call secret (the rendezvous thread reads it).
    pub secrets: SharedSecrets,
    /// The rest of every call link, when reachable through a rendezvous.
    pub link: Option<LinkBase>,
    /// What "now" is for routing (the clock port; a test drives it).
    pub clock: Arc<dyn Clock>,
    /// How long a room's last call stays current after everyone left.
    pub gap: Duration,
    /// The host's own device id (its microphone tracks' manifest device).
    pub host_device: String,
    /// Keep the host microphone's WAVs after a verified FLAC.
    pub keep_wav: bool,
}

impl Rooms {
    /// Wall time now, ms since the Unix epoch.
    pub fn now_ms(&self) -> u64 {
        self.clock.wall().as_millis() as u64
    }

    /// A room's folder.
    pub fn room_dir(&self, room: &str) -> PathBuf {
        self.library.join(room)
    }

    /// The room `--room` names (its link is printed at start).
    pub fn default_room(&self) -> String {
        self.secrets
            .lock()
            .map(|s| s.default.clone())
            .unwrap_or_default()
    }

    /// Whether the room exists; its secret is loaded if it was not yet.
    pub fn exists(&self, room: &str) -> bool {
        if rooms::room_slug(room) != room || room.is_empty() || room == rooms::LOGS {
            return false;
        }
        let dir = self.room_dir(room);
        if !dir.is_dir() {
            return false;
        }
        let Ok(mut s) = self.secrets.lock() else {
            return false;
        };
        if !s.rooms.contains_key(room) {
            match crate::rendezvous::call_secret(&dir) {
                Ok(secret) => {
                    s.rooms.insert(room.to_string(), secret);
                }
                Err(e) => {
                    warn!(room = %room, error = %e, "room has no readable call secret");
                    return false;
                }
            }
        }
        true
    }

    /// A room's call link, if the host is reachable through a rendezvous.
    pub fn link(&self, room: &str) -> Option<String> {
        let secret = self.secrets.lock().ok()?.rooms.get(room)?.clone();
        Some(self.link.as_ref()?.link(&secret))
    }

    /// Make an empty room named after `name`; its folder name, or why not.
    pub fn create_room(&self, name: &str) -> Result<String, String> {
        let slug = rooms::room_slug(name);
        if slug.is_empty() || slug == rooms::LOGS {
            return Err("a room needs a name with letters or digits".into());
        }
        let dir = self.room_dir(&slug);
        if dir.exists() {
            return Err(format!("there is already a room named {slug}"));
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot make the room: {e}"))?;
        if !self.exists(&slug) {
            return Err("the room has no call secret".into());
        }
        info!(room = %slug, "room made");
        Ok(slug)
    }

    /// Free bytes on the library's volume.
    pub fn free_space(&self) -> std::io::Result<u64> {
        FsStorage::new(&self.library)?.free_space()
    }
}

/// One call of a room, for the host machine's home.
#[derive(Clone, Debug, Serialize)]
pub struct CallView {
    /// Its folder name in the room.
    pub id: String,
    /// When it was made, ms since the Unix epoch.
    pub started_ms: u64,
    /// When someone was last in it, ms since the Unix epoch.
    pub last_active_ms: u64,
    /// How many takes it has.
    pub takes: u32,
    /// Someone is in it now.
    pub live: bool,
}

/// One room, for the host machine's home.
#[derive(Clone, Debug, Serialize)]
pub struct RoomView {
    /// Its name (folder).
    pub name: String,
    /// Its call link through the rendezvous, if any.
    pub link: Option<String>,
    /// Always open (else producer-gated).
    pub open: bool,
    /// Only trusted devices may enter.
    pub locked: bool,
    /// A password is set.
    pub password: bool,
    /// Its call is the live one.
    pub live: bool,
    /// The call a join lands in now (the live one, or the routed one);
    /// `None`: joining starts a new call.
    pub current: Option<String>,
    /// Its calls, oldest first.
    pub calls: Vec<CallView>,
    /// It is the room `--room` names.
    pub default: bool,
}

impl Call {
    /// Anyone in the call (a connection or the host's own participant), or a
    /// take recording.
    pub(super) fn is_live(&self) -> bool {
        !self.rpc.is_empty()
            || self.participant.is_some()
            || matches!(self.status, TakeStatus::Recording { .. })
    }

    /// The room an offer or join means: the one named, else the live one,
    /// else the default room.
    pub(super) fn resolve_room(&self, room: Option<&str>) -> String {
        match room.filter(|r| !r.is_empty()) {
            Some(r) => r.to_string(),
            None if self.is_live() => self.room_name.clone(),
            None => self.rooms.default_room(),
        }
    }

    /// Make `room` the loaded room, if no other room's call is live.
    pub(super) fn select_room(&mut self, room: Option<&str>) -> Result<(), String> {
        let room = self.resolve_room(room);
        if !self.rooms.exists(&room) {
            return Err(format!("unknown_room: no room named {room} on this host"));
        }
        if room == self.room_name {
            return Ok(());
        }
        if self.is_live() {
            return Err(WAITING_OTHER.to_string());
        }
        self.load_room(&room)
    }

    /// Load a room's security (nothing is live), with no call open.
    fn load_room(&mut self, room: &str) -> Result<(), String> {
        self.save_room();
        let path = self.rooms.room_dir(room).join("room.json");
        let loaded = match std::fs::read_to_string(&path) {
            Ok(text) => Room::from_json(&text).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(_) => Room::new(false),
        };
        info!(from = %self.room_name, to = %room, "room loaded");
        self.room = loaded;
        self.room_name = room.to_string();
        self.close_call();
        self.safety = SafetyWriter::spawn(
            SafetyRecorder::new(room, self.timeline.id()),
            safety::DEFAULT_CAPACITY,
        );
        self.names_sent.clear();
        Ok(())
    }

    /// Forget the open call (nothing is live).
    fn close_call(&mut self) {
        self.park_open();
        self.library = None;
        self.forget_elsewhere();
        self.call_id = None;
        self.call_dir = None;
        self.last_take = 0;
        self.reset_call_state();
    }

    fn reset_call_state(&mut self) {
        if let Ok(mut a) = self.aligned.lock() {
            a.clear();
        }
        self.aligned_seeded = false;
        self.tracks_sent.clear();
        self.calls_sent.clear();
        self.calls_dirty = true;
    }

    /// The first one in: open the room's current call (routing), unless a
    /// call is live already.
    pub(super) fn enter_call(&mut self) -> Result<(), String> {
        if self.is_live() && self.library.is_some() {
            return Ok(());
        }
        let room_dir = self.rooms.room_dir(&self.room_name);
        let now = self.rooms.now_ms();
        let calls = rooms::list_calls(&room_dir);
        let id = match rooms::route(&calls, now, self.rooms.gap) {
            Some(c) => c.id.clone(),
            None => {
                rooms::create_call(&room_dir, now)
                    .map_err(|e| format!("cannot make a new call: {e}"))?
                    .id
            }
        };
        if self.call_id.as_deref() == Some(id.as_str()) && self.library.is_some() {
            self.touch_call();
            return Ok(());
        }
        self.open_call(&id)
    }

    /// Open call `id` of the loaded room: everyone in moves into it (their
    /// syncing is idle: checked by the caller).
    fn open_call(&mut self, id: &str) -> Result<(), String> {
        let room_dir = self.rooms.room_dir(&self.room_name);
        let call_dir = room_dir.join(id);
        let root = FsStorage::new(&room_dir).map_err(|e| format!("room folder: {e}"))?;
        // One library per folder: a sync-only connection may be filing into
        // this call already, or into the one closing now.
        let key = format!("{}/{id}", self.room_name);
        let mut library = match self.elsewhere.remove(&key) {
            Some(l) => l,
            None => Library::open(&root, id, SyncConfig::default())
                .map_err(|e| format!("open call {id}: {e}"))?,
        };
        self.park_open();
        if rooms::read_meta(&call_dir).is_none()
            && let Err(e) = rooms::touch(&call_dir, self.rooms.now_ms())
        {
            warn!(call = %id, error = %e, "call.json not written");
        }
        // A take the host never finalised (Ctrl+C, a crash) is recovered.
        let takes = rooms::take_numbers(&call_dir);
        let recovered = crate::local::recover_takes(
            &mut library,
            &takes,
            &self.rooms.host_device,
            self.rooms.keep_wav,
        );
        let moving: Vec<GuestId> = self.pullers.keys().copied().collect();
        for g in &moving {
            self.drop_puller(*g);
        }
        // No puller is left to file into another call (and this one may
        // have been among them); sync-only connections may still be.
        self.forget_elsewhere();
        self.library = Some(library);
        self.last_take = takes.last().copied().unwrap_or(0);
        self.call_id = Some(id.to_string());
        self.call_dir = Some(call_dir);
        self.safety = SafetyWriter::spawn(
            SafetyRecorder::new(id, self.timeline.id()),
            safety::DEFAULT_CAPACITY,
        );
        self.reset_call_state();
        for g in moving {
            if let (Some(ch), Some(name)) = (self.data.get(&g), self.names.get(&g)) {
                self.pullers
                    .insert(g, Puller::new(id, name.clone(), ch.clone()));
            }
        }
        self.touch_call();
        info!(room = %self.room_name, call = %id, takes = self.last_take, "call open");
        self.align_recovered(recovered.iter().map(|r| r.take).collect::<Vec<TakeId>>());
        let everyone: Vec<GuestId> = self.rpc.keys().copied().collect();
        self.send_call(&everyone);
        self.broadcast();
        Ok(())
    }

    /// Move everyone in the loaded room's call into another call of it (a
    /// past one, or a new one for `None`).
    pub(super) fn switch_call(&mut self, target: Option<String>) -> Result<String, String> {
        if let TakeStatus::Recording { take, .. } = self.status {
            return Err(format!("stop take {} first", take.0));
        }
        if self.pullers.values().any(|p| !p.is_idle()) {
            return Err("wait until every device has synced its recordings".into());
        }
        if !self.local_busy.is_empty() {
            return Err("the host microphone is still finishing the last take".into());
        }
        let room_dir = self.rooms.room_dir(&self.room_name);
        let id = match target {
            None => {
                rooms::create_call(&room_dir, self.rooms.now_ms())
                    .map_err(|e| format!("cannot make a new call: {e}"))?
                    .id
            }
            Some(id) => {
                if !rooms::is_call_id(&id) || rooms::read_meta(&room_dir.join(&id)).is_none() {
                    return Err(format!("no call {id} in this room"));
                }
                id
            }
        };
        if self.call_id.as_deref() != Some(id.as_str()) {
            self.open_call(&id)?;
        }
        info!(room = %self.room_name, call = %id, "everyone moved to this call");
        Ok(id)
    }

    /// The home's "Start new call" / "Continue this call" for `room`: the
    /// live room switches at once; a room that is not live has the call made
    /// its current one, for whoever opens it next.
    pub(super) fn choose_call(
        &mut self,
        room: &str,
        call: Option<String>,
    ) -> Result<String, String> {
        if !self.rooms.exists(room) {
            return Err(format!("no room named {room}"));
        }
        if self.is_live() {
            if room == self.room_name {
                return self.switch_call(call);
            }
            return Err("another room's call is live: everyone must leave it first".into());
        }
        let room_dir = self.rooms.room_dir(room);
        let now = self.rooms.now_ms();
        let id = match call {
            None => {
                rooms::create_call(&room_dir, now)
                    .map_err(|e| format!("cannot make a new call: {e}"))?
                    .id
            }
            Some(id) => {
                let dir = room_dir.join(&id);
                if !rooms::is_call_id(&id) || rooms::read_meta(&dir).is_none() {
                    return Err(format!("no call {id} in room {room}"));
                }
                rooms::touch(&dir, now).map_err(|e| e.to_string())?;
                id
            }
        };
        info!(room = %room, call = %id, "call chosen: the next to join lands in it");
        self.calls_dirty = true;
        Ok(id)
    }

    /// Mark the open call active now.
    pub(super) fn touch_call(&mut self) {
        if let Some(dir) = &self.call_dir
            && let Err(e) = rooms::touch(dir, self.rooms.now_ms())
        {
            warn!(dir = %dir.display(), error = %e, "call not marked active");
        }
    }

    /// Every loop: mark the call active every minute while live, and once
    /// more when it empties.
    pub(super) fn keep_active(&mut self) {
        let live = self.is_live();
        if live {
            let now = self.timeline.now_us();
            if now >= self.touch_due_us {
                self.touch_due_us = now + TOUCH_EVERY_US;
                self.touch_call();
            }
        } else if self.was_live {
            info!(room = %self.room_name, call = ?self.call_id, "everyone left the call");
            self.touch_call();
        }
        self.was_live = live;
    }

    /// Tell these connections which call they are in.
    pub(super) fn send_call(&mut self, ids: &[GuestId]) {
        let Some(call) = self.call_id.clone() else {
            return;
        };
        let event = Event::Call(CallRef {
            room: self.room_name.clone(),
            call: call.clone(),
        });
        for id in ids {
            // In this call from now on: its tracks of this call are accepted
            // whichever call it delivers them from.
            if let (Some(device), Some(library)) =
                (self.room.device_of(id.0), self.library.as_mut())
                && let Err(e) = library.admit(device)
            {
                warn!(call = %call, device = %device, error = %e, "device not recorded in the call");
            }
            if let Some(rpc) = self.rpc.get_mut(id)
                && rpc.emit(event.clone()).is_err()
            {
                warn!(guest = id.0, "call not sent: channel closed");
            }
        }
    }

    /// `Request::OfferCallTrack`: a track this device recorded in `call`.
    /// The open call's go the usual way; another call's is filed into that
    /// call's folder only if the device was in it ([`call_library`]).
    pub(super) fn offer_call_track(
        &mut self,
        id: GuestId,
        call: &CallRef,
        offer: &TrackOffer,
    ) -> Response {
        if call.room == self.room_name && self.call_id.as_deref() == Some(call.call.as_str()) {
            return self.offer_track(id, offer);
        }
        let join_first = || Response::Error {
            kind: ErrorKind::InvalidState,
            message: "Join first".into(),
        };
        let Some(member) = self.room.device_of(id.0).map(str::to_string) else {
            return join_first();
        };
        let Some(mut p) = self.pullers.remove(&id) else {
            return join_first();
        };
        let device = self.room.offer_device(id.0, &offer.device);
        let offer = TrackOffer {
            device,
            ..offer.clone()
        };
        let r = self.deliver(id, &mut p, &member, call, &offer);
        self.pullers.insert(id, p);
        r
    }

    /// The loaded room's calls as producers see them.
    fn calls_summary(&self) -> Vec<CallSummary> {
        rooms::list_calls(&self.rooms.room_dir(&self.room_name))
            .into_iter()
            .map(|c| CallSummary {
                current: self.call_id.as_deref() == Some(c.id.as_str()),
                id: c.id,
                started_ms: c.meta.started_ms,
                last_active_ms: c.meta.last_active_ms,
                takes: c.takes,
            })
            .collect()
    }

    /// When the calls changed (or who produces): send producers the list.
    pub(super) fn publish_calls(&mut self) {
        if !self.calls_dirty {
            return;
        }
        self.calls_dirty = false;
        let producers = self.room.producers();
        self.calls_sent.retain(|c, _| producers.contains(c));
        if producers.is_empty() {
            return;
        }
        let list = self.calls_summary();
        for id in producers {
            if self.calls_sent.get(&id) == Some(&list) {
                continue;
            }
            if let Some(rpc) = self.rpc.get_mut(&GuestId(id)) {
                if rpc.emit(Event::Calls(list.clone())).is_err() {
                    warn!(guest = id, "calls not sent: channel closed");
                    continue;
                }
                self.calls_sent.insert(id, list.clone());
            }
        }
    }

    /// Every room, for the host machine's home.
    pub(super) fn rooms_view(&mut self) -> Vec<RoomView> {
        let now = self.rooms.now_ms();
        let live = self.is_live();
        let default = self.rooms.default_room();
        rooms::list_rooms(&self.rooms.library)
            .into_iter()
            .filter(|r| self.rooms.exists(r))
            .map(|name| {
                let dir = self.rooms.room_dir(&name);
                let state = if name == self.room_name {
                    self.room.state(None)
                } else {
                    read_room(&dir).state(None)
                };
                let calls = rooms::list_calls(&dir);
                let room_live = live && name == self.room_name;
                let current = if room_live {
                    self.call_id.clone()
                } else {
                    rooms::route(&calls, now, self.rooms.gap).map(|c| c.id.clone())
                };
                RoomView {
                    link: self.rooms.link(&name),
                    open: state.open,
                    locked: state.locked,
                    password: state.password,
                    live: room_live,
                    current,
                    calls: calls
                        .into_iter()
                        .map(|c| CallView {
                            live: room_live && self.call_id.as_deref() == Some(c.id.as_str()),
                            id: c.id,
                            started_ms: c.meta.started_ms,
                            last_active_ms: c.meta.last_active_ms,
                            takes: c.takes,
                        })
                        .collect(),
                    default: name == default,
                    name,
                }
            })
            .collect()
    }
}

/// A room's security as saved (a fresh one if unreadable).
/// The library of `call` for a track `device` (the connection's verified
/// device) delivers, if that device was in the call. Depends only on the
/// call's folder, never on which call or room the device is in now. A call
/// from before the device list counts a device named in its manifests.
pub fn call_library(
    rooms: &Rooms,
    call: &CallRef,
    device: &str,
) -> Result<Library<FsStorage>, String> {
    if !rooms.exists(&call.room) {
        return Err(format!("no room named {}", call.room));
    }
    let room_dir = rooms.room_dir(&call.room);
    let call_dir = room_dir.join(&call.call);
    // Opening a library creates its folder: only for a call that exists.
    if !rooms::is_call_id(&call.call) || rooms::read_meta(&call_dir).is_none() {
        return Err(format!("no call {} in room {}", call.call, call.room));
    }
    let root = FsStorage::new(&room_dir).map_err(|e| format!("room folder: {e}"))?;
    let mut lib = Library::open(&root, &call.call, SyncConfig::default())
        .map_err(|e| format!("open call {}: {e}", call.call))?;
    if lib.was_in(device).map_err(|e| e.to_string())? {
        return Ok(lib);
    }
    let mut takes: Vec<TakeId> = rooms::take_numbers(&call_dir)
        .into_iter()
        .map(TakeId)
        .collect();
    if call_dir.join("logs").is_dir() {
        takes.push(TakeId(0));
    }
    if lib
        .in_manifests(device, &takes)
        .map_err(|e| e.to_string())?
    {
        lib.admit(device).map_err(|e| e.to_string())?;
        return Ok(lib);
    }
    Err(format!("this device was not in call {}", call.call))
}

pub(super) fn read_room(dir: &Path) -> Room {
    std::fs::read_to_string(dir.join("room.json"))
        .ok()
        .and_then(|t| Room::from_json(&t).ok())
        .unwrap_or_else(|| Room::new(false))
}
