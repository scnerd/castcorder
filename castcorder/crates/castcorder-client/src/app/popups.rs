//! Popovers and dialogs that close themselves (cast-1ft): a click outside one,
//! or Escape, closes every popup that is open, wherever the app made it. Each
//! popup registers once with where it lives on the page (`root`: a CSS selector
//! for the element holding it and its button, so a click on either is "inside")
//! and how to ask whether it is open and close it. Native `<details>` menus
//! mark themselves `data-popup` and need no registration.

use std::cell::RefCell;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

struct Popup {
    key: &'static str,
    root: &'static str,
    is_open: Box<dyn Fn() -> bool>,
    close: Box<dyn Fn()>,
}

thread_local! {
    static POPUPS: RefCell<Vec<Popup>> = const { RefCell::new(Vec::new()) };
    /// The popups that were open when the current click began (by key): a
    /// popup that the click itself opens is not closed by it.
    static OPEN_AT_CLICK: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
}

/// Register the popup `key` (again: replaces the earlier one with that key).
pub(super) fn register(
    key: &'static str,
    root: &'static str,
    is_open: impl Fn() -> bool + 'static,
    close: impl Fn() + 'static,
) {
    POPUPS.with(|p| {
        let mut p = p.borrow_mut();
        p.retain(|x| x.key != key);
        p.push(Popup {
            key,
            root,
            is_open: Box::new(is_open),
            close: Box::new(close),
        });
    });
}

/// Register a popup held in a boolean signal.
pub(super) fn register_flag(key: &'static str, root: &'static str, flag: RwSignal<bool>) {
    register(
        key,
        root,
        move || flag.try_get_untracked().unwrap_or(false),
        move || {
            let _ = flag.try_set(false);
        },
    );
}

/// Whether `target` is inside an element matching `selector`.
fn within(target: &Option<web_sys::Element>, selector: &str) -> bool {
    target
        .as_ref()
        .is_some_and(|el| el.closest(selector).ok().flatten().is_some())
}

fn target_of(ev: &web_sys::Event) -> Option<web_sys::Element> {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
}

/// Close the native menus (`details[data-popup]`) that are open and, with a
/// `target`, do not hold it.
fn close_details(target: Option<&web_sys::Element>) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Ok(list) = doc.query_selector_all("details[data-popup][open]") else {
        return;
    };
    for i in 0..list.length() {
        let Some(d) = list
            .item(i)
            .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        if target.is_some_and(|t| d.contains(Some(t))) {
            continue;
        }
        let _ = d.remove_attribute("open");
    }
}

/// Install the page-wide listeners, once.
pub(super) fn install() {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    // Which popups are open when the click starts (capture), so that closing
    // after the button's own handler ran (bubble) spares one it just opened.
    let begin = Closure::<dyn FnMut(web_sys::Event)>::new(move |_ev: web_sys::Event| {
        let open: Vec<&'static str> = POPUPS.with(|p| {
            p.borrow()
                .iter()
                .filter(|x| (x.is_open)())
                .map(|x| x.key)
                .collect()
        });
        OPEN_AT_CLICK.with(|o| *o.borrow_mut() = open);
    });
    let _ = doc.add_event_listener_with_callback_and_bool(
        "click",
        begin.as_ref().unchecked_ref(),
        true,
    );
    begin.forget();
    let end = Closure::<dyn FnMut(web_sys::Event)>::new(move |ev: web_sys::Event| {
        let target = target_of(&ev);
        let was_open = OPEN_AT_CLICK.with(|o| std::mem::take(&mut *o.borrow_mut()));
        POPUPS.with(|p| {
            for x in p.borrow().iter() {
                if was_open.contains(&x.key) && (x.is_open)() && !within(&target, x.root) {
                    (x.close)();
                }
            }
        });
        close_details(target.as_ref());
    });
    let _ = doc.add_event_listener_with_callback("click", end.as_ref().unchecked_ref());
    end.forget();
    let esc =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |ev: web_sys::KeyboardEvent| {
            if ev.key() != "Escape" {
                return;
            }
            POPUPS.with(|p| {
                for x in p.borrow().iter() {
                    if (x.is_open)() {
                        (x.close)();
                    }
                }
            });
            close_details(None);
        });
    let _ = doc.add_event_listener_with_callback("keydown", esc.as_ref().unchecked_ref());
    esc.forget();
}
