//! The participant side of outbox sync, sans-IO.
//!
//! The caller owns storage and the two channels: it opens each finished track,
//! [`GuestOutbox::add`]s it (which hashes it), sends the returned
//! [`TrackOffer`] on "control" whenever it connects to the host, and moves
//! "data" channel messages through [`GuestOutbox::handle`] and
//! [`GuestOutbox::poll_transmit`] (or [`GuestOutbox::pump`]). The host decides
//! what to pull and when. `handle` returns the tracks the host confirmed it
//! holds verified (transfer DONE): only those may the caller delete, and it
//! must make the delete durable itself (`Storage::sync_dir`).
//!
//! Files are read one transfer unit per call (castcorder-transfer's sender),
//! which keeps per-call storage costs (OPFS) bounded.

use std::io;

use castcorder_port_disk::StorageFile;
use castcorder_protocol::TrackOffer;
use castcorder_transfer::channel::Buffered;
use castcorder_transfer::incremental::outboard_of_file;
use castcorder_transfer::sender::Sender;
use tracing::{info, warn};

use crate::sync::{SyncConfig, hex, sync_order};

/// An outbox item as a guest offers it to the host: its take, its name (unique
/// on the device) and its root. A caller that offers each item once per
/// connection must keep this, never the root alone: byte-identical items
/// (digital silence, a digital source, a tone of whole-sample period cut to
/// the same length in another take or call) share a root, and an item skipped
/// as "already offered" would stay on the device (cast-rxd).
pub type OfferKey = (u32, String, [u8; 32]);

/// The [`OfferKey`] of `offer`.
pub fn offer_key(offer: &TrackOffer) -> OfferKey {
    (offer.take.0, offer.name.clone(), offer.root)
}

/// One participant's outbox: the tracks it still has to deliver.
pub struct GuestOutbox<F: StorageFile> {
    call: String,
    cfg: SyncConfig,
    sender: Sender<F>,
    /// Aligned with the sender's blobs (which are never removed: its queue
    /// holds their indices).
    offers: Vec<TrackOffer>,
    reported: Vec<bool>,
}

impl<F: StorageFile> GuestOutbox<F> {
    /// An empty outbox for `call` (used in logs).
    pub fn new(call: impl Into<String>, cfg: SyncConfig) -> Self {
        Self {
            call: call.into(),
            cfg,
            sender: Sender::new(cfg.frame_payload),
            offers: Vec::new(),
            reported: Vec::new(),
        }
    }

    /// Add a finished track. `offer` describes it; its `size` and `root` are
    /// computed here from `file` (read once, one MiB per call). The file must not
    /// change afterwards. Adding a track already present returns its offer.
    pub fn add(&mut self, mut offer: TrackOffer, mut file: F) -> io::Result<TrackOffer> {
        if let Some(i) = self.offers.iter().position(|o| o.name == offer.name) {
            return Ok(self.offers[i].clone());
        }
        let outboard = outboard_of_file(&mut file, self.cfg.block_log)?;
        offer.size = outboard.tree.size();
        offer.root = *outboard.root.as_bytes();
        info!(call = %self.call, device = %offer.device, take = offer.take.0, track = %offer.name,
            size = offer.size, root = %hex(&offer.root), "track added to outbox");
        self.sender.add_item_blob(
            file,
            outboard,
            self.cfg.unit_log,
            offer.name.clone().into_bytes(),
        );
        self.offers.push(offer.clone());
        self.reported.push(false);
        Ok(offer)
    }

    /// Whether a track with this name was added (confirmed or not).
    pub fn contains(&self, name: &str) -> bool {
        self.offers.iter().any(|o| o.name == name)
    }

    /// The offers the host has not yet confirmed, in sync order (audio, then
    /// video, then logs; oldest take first: [`sync_order`]). Send them all on
    /// every connect, in this order: the host starts pulling the first offer
    /// it holds, so a video offered ahead of an audio track would go first.
    pub fn pending(&self) -> Vec<TrackOffer> {
        let mut v: Vec<TrackOffer> = self
            .offers
            .iter()
            .zip(&self.reported)
            .filter(|(_, r)| !**r)
            .map(|(o, _)| o.clone())
            .collect();
        v.sort_by_key(sync_order);
        v
    }

    /// Handle one message from the host's "data" channel. Returns the tracks the
    /// host has just confirmed: their local copies may now be deleted. A
    /// malformed message is logged and ignored.
    pub fn handle(&mut self, msg: &[u8]) -> Vec<TrackOffer> {
        if let Err(e) = self.sender.handle(msg) {
            warn!(call = %self.call, error = %e, "bad message from host on data; ignored");
            return Vec::new();
        }
        let mut confirmed = Vec::new();
        for (i, blob) in self.sender.blobs.iter().enumerate() {
            if blob.confirmed && !self.reported[i] {
                self.reported[i] = true;
                let o = &self.offers[i];
                info!(call = %self.call, device = %o.device, take = o.take.0, track = %o.name,
                    "host confirmed verified receipt; local copy may be deleted");
                confirmed.push(o.clone());
            }
        }
        confirmed
    }

    /// How far each unconfirmed track the host has started pulling has got,
    /// as (name, percent): the highest part served against the whole track,
    /// 99 at most until the host confirms it. Log files are not tracks and
    /// are left out.
    pub fn progress(&self) -> Vec<(String, u8)> {
        self.sender
            .blobs
            .iter()
            .zip(&self.offers)
            .filter(|(b, o)| {
                !b.confirmed
                    && b.units_reached > 0
                    && o.kind != castcorder_protocol::SourceKind::Log
            })
            .map(|(b, o)| {
                let total = b.geometry.n_units().max(1);
                let pct = (b.units_reached.min(total) * 100 / total).min(99) as u8;
                (o.name.clone(), pct)
            })
            .collect()
    }

    /// The next message for the host's "data" channel, if any.
    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.sender.poll_transmit()
    }

    /// Move everything that can move now over `channel`: handle what arrived,
    /// then send until `max_buffered` bytes are queued. Returns the confirmed
    /// tracks, as [`handle`](Self::handle) does.
    pub fn pump<C: Buffered>(
        &mut self,
        channel: &mut C,
        max_buffered: usize,
    ) -> Result<Vec<TrackOffer>, C::Error> {
        let mut confirmed = Vec::new();
        while let Some(m) = channel.try_recv()? {
            confirmed.extend(self.handle(&m));
        }
        while channel.buffered() < max_buffered {
            let Some(m) = self.poll_transmit() else {
                break;
            };
            channel.send(m)?;
        }
        Ok(confirmed)
    }

    /// The channel is gone: forget work in flight. The host asks again for what
    /// it still lacks once it is back.
    pub fn on_disconnect(&mut self) {
        self.sender.on_disconnect();
    }
}
