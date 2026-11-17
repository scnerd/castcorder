//! Background sync (docs/design.md "Background sync"): while the page is
//! open and this device is in no call, a sync-only connection delivers what
//! the outbox still holds for this host.
//!
//! - It starts after Leave, and when the page opens without joining (the
//!   storage worker's `ready`): the outbox is asked for every pending item of
//!   this host (`outbox {call, legacy}` with the host's scope, so every item
//!   recorded in a call names it). Items from before rooms name no call and
//!   wait for a join.
//! - With something to deliver, it connects with the call connection's code
//!   ([`connect`]) in sync-only mode: data channels only, no microphone or
//!   camera, no Join; the offer says `sync_only` and the host admits it by
//!   device key beside any live call. Each item goes as `OfferCallTrack`.
//!   A lost connection is retried with the usual backoff.
//! - Every confirmed item asks the outbox again. Once nothing deliverable is
//!   left (delivered, or refused by the host and kept here) the connection
//!   closes. A refusal of the device itself (kicked) stops it.
//! - The lobby shows what it is doing (`#sync-state`). Joining stops it.
//!
//! Test hook: while `window.__castcorderHoldSync` is set, it does not
//! connect (an e2e makes a call live elsewhere first).

use super::*;

thread_local! {
    /// The deliverable roots the outbox last listed.
    static LISTED: RefCell<Vec<[u8; 32]>> = const { RefCell::new(Vec::new()) };
    /// Seconds since the outbox was last listed while not connected.
    static IDLE_TICKS: Cell<u32> = const { Cell::new(0) };
}

/// The storage worker is ready: a page opened without joining delivers what
/// its outbox holds for this host (the home page has no host scope of its
/// own, so not there).
pub(super) fn worker_ready(ui: Ui) {
    if home::is_home_page() || ui.joined.get_untracked() {
        return;
    }
    // A join started meanwhile wins: never start under a join in flight.
    let before = join_epoch();
    spawn_local(async move {
        let key = match crate::identity::device().await {
            Ok(k) => k,
            Err(e) => {
                warn!(error = %describe(&e), "no device key; no background sync");
                return;
            }
        };
        if join_epoch() != before || ui.joined.get_untracked() || with_engine(|e| e.sync_only) {
            return;
        }
        let url = ui.signal_url.get_untracked();
        let scope = host_scope(&url);
        let name = stored_name().unwrap_or_else(|| default_name(&key.id));
        let ctx = Ctx {
            call: scope.clone(),
            host_scope: scope,
            url,
            device: key.id.clone(),
            key: Some(key),
            name,
            password: None,
        };
        start(ui, ctx);
    });
}

/// Start background sync with the call connection's context (after Leave)
/// or a fresh one (page load).
pub(super) fn start(ui: Ui, ctx: Ctx) {
    let epoch = join_epoch().wrapping_add(1);
    JOIN_EPOCH.set(epoch);
    let ctx = Ctx {
        call: ctx.host_scope.clone(),
        ..ctx
    };
    info!(host = %ctx.host_scope, device = %ctx.device, "background sync: checking the outbox");
    with_engine(|e| {
        e.ctx = ctx.clone();
        e.sync_only = true;
        e.sync_refused = false;
        e.sync_delivered = 0;
        e.refused.clear();
        e.attempts = 0;
    });
    LISTED.with(|l| l.borrow_mut().clear());
    ui.sync.set(String::new());
    ui.sync_left.set(0);
    ui.sync_done.set(String::new());
    let tick = every(1000, move || tick(ui, epoch));
    with_engine(|e| e.timers.extend(tick));
    ask_outbox();
}

/// Joining: the sync-only connection, if any, gives way.
pub(super) fn stop(ui: Ui) {
    if !with_engine(|e| e.sync_only) {
        return;
    }
    info!("background sync stopped: joining");
    JOIN_EPOCH.set(JOIN_EPOCH.get().wrapping_add(1));
    disconnect();
    let (timers, retry) = with_engine(|e| {
        e.sync_only = false;
        e.refused.clear();
        (std::mem::take(&mut e.timers), e.retry.take())
    });
    for id in timers {
        window().clear_interval_with_handle(id);
    }
    if let Some(id) = retry {
        window().clear_timeout_with_handle(id);
    }
    LISTED.with(|l| l.borrow_mut().clear());
    ui.sync.set(String::new());
    ui.sync_left.set(0);
    ui.sync_done.set(String::new());
}

/// Ask the worker for every pending item of this host.
fn ask_outbox() {
    let scope = with_engine(|e| e.ctx.host_scope.clone());
    let msg = worker_cmd("outbox");
    set(&msg, "call", scope.as_str());
    set(&msg, "legacy", scope.as_str());
    post_worker(&msg, None);
}

/// The worker listed the outbox (`outbox {offers, rooms, calls}`).
pub(super) fn on_outbox(ui: Ui, msg: &JsValue) {
    if !with_engine(|e| e.sync_only) {
        return;
    }
    let (rooms, calls) = (
        Array::from(&get(msg, "rooms")),
        Array::from(&get(msg, "calls")),
    );
    let mut roots = Vec::new();
    for (i, bytes) in Array::from(&get(msg, "offers")).iter().enumerate() {
        let named = rooms
            .get(i as u32)
            .as_string()
            .is_some_and(|r| !r.is_empty())
            && calls
                .get(i as u32)
                .as_string()
                .is_some_and(|c| !c.is_empty());
        let bytes = Uint8Array::new(&bytes).to_vec();
        if let (true, Ok(offer)) = (named, castcorder_protocol::decode::<TrackOffer>(&bytes)) {
            roots.push(offer.root);
        }
    }
    LISTED.with(|l| *l.borrow_mut() = roots);
    recount(ui);
}

/// The host confirmed an item (its local copy is deleted): list again.
pub(super) fn synced(ui: Ui) {
    if !with_engine(|e| e.sync_only) {
        return;
    }
    with_engine(|e| e.sync_delivered += 1);
    ui.sync_done.set(String::new());
    ask_outbox();
}

/// The sync-only control channel opened: offer the outbox.
pub(super) fn connected(ui: Ui) {
    with_engine(|e| e.attempts = 0);
    ui.sync.set("connected".into());
    ask_outbox();
}

/// The host refused this device (kicked from the room): stop.
pub(super) fn refused(ui: Ui, why: &str) {
    warn!(reason = %why, "background sync refused by the host; recordings stay on this device");
    disconnect();
    with_engine(|e| e.sync_refused = true);
    ui.sync_left.set(0);
    ui.sync.set(String::new());
    ui.sync_done.set(format!(
        "The host did not take your recordings ({}). They stay on this device.",
        refusal_state(why)
    ));
}

/// Work out what is left; connect, or close once nothing is.
pub(super) fn recount(ui: Ui) {
    let (sync_only, stopped, connected, waiting, refused, delivered) = with_engine(|e| {
        (
            e.sync_only,
            e.sync_refused,
            e.pc.is_some(),
            e.retry.is_some(),
            e.refused.clone(),
            e.sync_delivered,
        )
    });
    if !sync_only || stopped {
        return;
    }
    let listed = LISTED.with(|l| l.borrow().clone());
    let kept = listed.iter().filter(|r| refused.contains(*r)).count();
    let left = listed.len() - kept;
    ui.sync_left.set(left);
    if left == 0 {
        if connected {
            info!(
                delivered,
                kept, "background sync done; closing the sync-only connection"
            );
            disconnect();
            ui.sync.set(String::new());
        }
        if kept > 0 {
            ui.sync_done.set(format!(
                "{kept} file(s) could not be delivered: the host refused them. They stay on this device."
            ));
        } else if delivered > 0 {
            ui.sync_done
                .set("All your recordings reached the host.".into());
        }
        return;
    }
    if connected || waiting || get(&window(), "__castcorderHoldSync").is_truthy() {
        return;
    }
    let epoch = join_epoch();
    info!(left, "background sync: connecting sync-only");
    connect(ui, epoch);
}

/// Every second: heartbeat and watch the connection; while not connected,
/// look again (a hold released, an item finalised).
fn tick(ui: Ui, epoch: u32) {
    if stale(epoch) {
        return;
    }
    if with_engine(|e| e.pc.is_some()) {
        send_signal(&Signal::Hb);
        check_connection(ui, epoch);
    } else {
        // A track still being finalised when last listed shows up later.
        let n = IDLE_TICKS.get() + 1;
        IDLE_TICKS.set(n % RELIST_SECONDS);
        if n == RELIST_SECONDS && !with_engine(|e| e.sync_refused) {
            ask_outbox();
        }
        recount(ui);
    }
}

/// How often an idle background sync lists the outbox again.
const RELIST_SECONDS: u32 = 10;
