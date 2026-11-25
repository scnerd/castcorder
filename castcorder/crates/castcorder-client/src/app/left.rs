//! After Leave (cast-n56.4): the page says "You left ROOM" with the time in
//! the call and the takes recorded, how far the recordings are on their way
//! to the host (keep the tab open until done), Rejoin and Your calls, above
//! the rooms this browser joined. Only Leave leads here: a refusal or a lost
//! connection goes back to the lobby with its reason beside Join.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

use leptos::prelude::*;

use super::Ui;
use crate::leftcall::Summary;

#[derive(Clone, Copy)]
pub(super) struct LeftUi {
    /// Set by Leave, cleared by Rejoin or "Change settings".
    pub(super) summary: RwSignal<Option<Summary>>,
    /// The most files left at once since Leave (the progress bar's whole).
    pub(super) most: RwSignal<usize>,
}

thread_local! {
    static LEFT: Cell<Option<LeftUi>> = const { Cell::new(None) };
    static JOINED_MS: Cell<f64> = const { Cell::new(0.0) };
    static TAKES: RefCell<BTreeSet<u32>> = const { RefCell::new(BTreeSet::new()) };
}

pub(super) fn left_ui() -> LeftUi {
    LEFT.with(|l| {
        l.get().unwrap_or_else(|| {
            let ui = LeftUi {
                summary: RwSignal::new(None),
                most: RwSignal::new(0),
            };
            l.set(Some(ui));
            ui
        })
    })
}

/// The call was joined (a new call or a rejoin): count from now.
pub(super) fn joined() {
    JOINED_MS.set(js_sys::Date::now());
    TAKES.with(|t| t.borrow_mut().clear());
    left_ui().summary.set(None);
}

/// This device recorded `take` in the call.
pub(super) fn recorded(take: u32) {
    TAKES.with(|t| t.borrow_mut().insert(take));
}

/// The person pressed Leave: say what they left, then leave.
pub(super) fn leave(ui: Ui) {
    let started = JOINED_MS.get();
    let minutes = if started > 0.0 {
        ((js_sys::Date::now() - started) / 60_000.0).round() as u64
    } else {
        0
    };
    let room = ui
        .call_ref
        .get_untracked()
        .map(|c| c.room)
        .unwrap_or_else(|| ui.room_name.get_untracked());
    let l = left_ui();
    l.most.set(0);
    l.summary.set(Some(Summary {
        room,
        minutes,
        takes: TAKES.with(|t| t.borrow().len()),
    }));
    super::leave(ui);
}
