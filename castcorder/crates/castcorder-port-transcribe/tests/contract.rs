//! The transcription contract against the simulation. (castcorder-transcribe
//! runs it against whisper.cpp.)

use castcorder_port_transcribe::contract::{Speech, check};
use castcorder_port_transcribe::sim::{Simulated, speak};

#[test]
fn the_simulation_keeps_the_contract() {
    let text = "the quick brown fox jumps over the lazy dog".to_string();
    let speech = Speech {
        pcm: speak(&text),
        text,
    };
    let mut t = Simulated::new();
    check(&mut t, &speech);
    assert!(t.calls >= 6);
}
