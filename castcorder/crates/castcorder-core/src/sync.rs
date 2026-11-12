//! What both ends of outbox sync share: the transfer geometry and the order in
//! which a participant's tracks are pulled.

use castcorder_protocol::{SourceKind, TrackOffer};
use castcorder_transfer::geometry::{
    DEFAULT_BLOCK_LOG, DEFAULT_FRAME_PAYLOAD, DEFAULT_UNIT_LOG, Geometry,
};

/// Transfer geometry. Guest and host must use the same values (they are not on
/// the wire); [`SyncConfig::default`] is what real calls use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncConfig {
    /// log2 of the bao chunk group in 1 KiB chunks.
    pub block_log: u8,
    /// log2 of the transfer unit in bytes (the unit of request and resume).
    pub unit_log: u8,
    /// Largest data payload per "data" channel message.
    pub frame_payload: usize,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            block_log: DEFAULT_BLOCK_LOG,
            unit_log: DEFAULT_UNIT_LOG,
            frame_payload: DEFAULT_FRAME_PAYLOAD,
        }
    }
}

impl SyncConfig {
    /// The geometry of a blob of `size` bytes.
    pub fn geometry(&self, size: u64) -> Geometry {
        Geometry::new(size, self.block_log, self.unit_log)
    }
}

/// The one ordering rule of the outbox: audio before video, oldest take first
/// (then by name, for determinism). Smaller sorts first.
pub fn sync_order(offer: &TrackOffer) -> (u8, u32, String) {
    let kind = match offer.kind {
        SourceKind::Audio => 0,
        SourceKind::Video => 1,
    };
    (kind, offer.take.0, offer.name.clone())
}

/// Lowercase hex of a root, for logs and manifests.
pub fn hex(root: &[u8; 32]) -> String {
    root.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_protocol::{SourceId, TakeId};

    fn offer(kind: SourceKind, take: u32, name: &str) -> TrackOffer {
        TrackOffer {
            device: "d".into(),
            take: TakeId(take),
            source: SourceId("s".into()),
            kind,
            name: name.into(),
            size: 0,
            root: [0; 32],
            format: "wav".into(),
        }
    }

    #[test]
    fn audio_before_video_then_oldest_take_first() {
        let mut v = [
            offer(SourceKind::Video, 1, "a"),
            offer(SourceKind::Audio, 2, "b"),
            offer(SourceKind::Audio, 1, "c"),
            offer(SourceKind::Video, 2, "d"),
        ];
        v.sort_by_key(sync_order);
        let names: Vec<&str> = v.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["c", "b", "a", "d"]);
    }
}
