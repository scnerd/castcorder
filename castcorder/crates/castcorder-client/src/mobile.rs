//! Guests on phones and tablets (pure, so native tests cover it): which kind
//! of device the page runs on, and the plain words that tell the person what
//! the system may do to their recording.
//!
//! A phone's browser may pause the microphone when the page goes to the
//! background, the screen locks, a phone call comes in or another app takes
//! the audio. iOS Safari always does; Android Chrome often keeps going but may
//! not. The page holds a Screen Wake Lock while joined so the screen stays on
//! by itself; the notice says what is left to the person.

/// A touch device the page treats as a phone or tablet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phone {
    /// iPhone or iPad (iPadOS reports a Mac with a touch screen).
    Ios,
    /// Android phone or tablet.
    Android,
    /// Another touch-first device.
    Other,
}

/// The device kind from the browser's user agent, whether its primary
/// pointer is coarse (`(pointer: coarse)`), and `navigator.maxTouchPoints`.
/// None on a computer.
pub fn phone(user_agent: &str, coarse_pointer: bool, max_touch_points: i32) -> Option<Phone> {
    let ua = user_agent.to_ascii_lowercase();
    if ua.contains("iphone") || ua.contains("ipad") || ua.contains("ipod") {
        return Some(Phone::Ios);
    }
    // iPadOS asks for desktop sites: it says "Macintosh" but has touch.
    if ua.contains("macintosh") && max_touch_points > 1 {
        return Some(Phone::Ios);
    }
    if ua.contains("android") {
        return Some(Phone::Android);
    }
    if coarse_pointer && max_touch_points > 0 && ua.contains("mobi") {
        return Some(Phone::Other);
    }
    None
}

/// The lobby's notice for a phone or tablet: what interrupts a recording
/// and what the person can do about it.
pub fn lobby_notice(phone: Phone) -> &'static str {
    match phone {
        Phone::Ios => {
            "On an iPhone or iPad, keep this page open and on screen until the take ends. \
             Switching apps, locking the screen, a phone call or Siri pauses your microphone, \
             and that part of the take is silent."
        }
        Phone::Android | Phone::Other => {
            "On a phone, keep this page open and on screen while you record. \
             Switching apps, locking the phone or a phone call can pause your microphone, \
             and that part of the take is silent."
        }
    }
}

/// The call note after the system paused the microphone for `secs` during a
/// take (the track was muted under us, then came back).
pub fn paused_note(secs: u64) -> String {
    format!(
        "Your microphone was paused for {} while recording (the device took it: \
         another app, the lock screen or a call). That part of the take is silent; \
         keep this page open and on screen while you record.",
        duration(secs)
    )
}

/// The call note after the microphone stopped during a take and was opened
/// again `secs` later (its track ended under us).
pub fn reopened_note(secs: u64) -> String {
    format!(
        "Your microphone stopped while recording and was turned back on after {}. \
         The take goes on; that part of it is missing. Keep this page open and on \
         screen while you record.",
        duration(secs)
    )
}

fn duration(secs: u64) -> String {
    match secs {
        0 | 1 => "about a second".to_string(),
        s if s < 90 => format!("{s} seconds"),
        s => format!("{} minutes", s.div_ceil(60)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1";
    const IPAD_DESKTOP: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Safari/605.1.15";
    const PIXEL: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36";
    const WINDOWS: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

    #[test]
    fn phones_and_tablets_are_told_apart_from_computers() {
        assert_eq!(phone(IPHONE, true, 5), Some(Phone::Ios));
        assert_eq!(phone(IPAD_DESKTOP, true, 5), Some(Phone::Ios));
        assert_eq!(phone(PIXEL, true, 5), Some(Phone::Android));
        // A Mac has no touch points; a touch-screen Windows laptop is no phone.
        assert_eq!(phone(IPAD_DESKTOP, false, 0), None);
        assert_eq!(phone(WINDOWS, true, 10), None);
        assert_eq!(phone(WINDOWS, false, 0), None);
    }

    #[test]
    fn the_notes_say_how_long_and_what_to_do() {
        assert!(paused_note(1).contains("about a second"));
        assert!(paused_note(12).contains("12 seconds"));
        assert!(paused_note(150).contains("3 minutes"));
        assert!(reopened_note(8).contains("8 seconds"));
        for n in [paused_note(5), reopened_note(5)] {
            assert!(n.to_lowercase().contains("keep this page open"), "{n}");
            assert!(n.is_ascii(), "{n}");
        }
        assert!(lobby_notice(Phone::Ios).contains("Siri"));
        assert!(lobby_notice(Phone::Android).contains("locking the phone"));
    }
}
