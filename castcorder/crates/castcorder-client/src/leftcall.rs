//! The words and numbers of the page shown after Leave (cast-n56.4; pure,
//! so native tests cover it).

/// What the "You left" page says about the call just left.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    /// The room's name (empty when the page never learned it).
    pub room: String,
    /// Whole minutes in the call, rounded.
    pub minutes: u64,
    /// Takes this device recorded in the call.
    pub takes: usize,
}

/// "You were in the call for 48 min, dot, 3 takes recorded".
pub fn line(s: &Summary) -> String {
    let time = match s.minutes {
        0 => "less than a minute".to_string(),
        m if m < 60 => format!("{m} min"),
        m => format!("{} h {} min", m / 60, m % 60),
    };
    let takes = match s.takes {
        0 => "no takes recorded".to_string(),
        1 => "1 take recorded".to_string(),
        n => format!("{n} takes recorded"),
    };
    format!("You were in the call for {time} \u{b7} {takes}")
}

/// Files sent of the whole, and the percentage, from what is left now and
/// the most that was left at once.
pub fn progress(left: usize, most: usize) -> (usize, usize, u8) {
    let whole = most.max(left);
    if whole == 0 {
        return (0, 0, 100);
    }
    let sent = whole - left;
    (sent, whole, (sent * 100 / whole) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_line_reads_plainly() {
        let s = |minutes, takes| Summary {
            room: "r".into(),
            minutes,
            takes,
        };
        assert_eq!(
            line(&s(48, 3)),
            "You were in the call for 48 min \u{b7} 3 takes recorded"
        );
        assert_eq!(
            line(&s(0, 1)),
            "You were in the call for less than a minute \u{b7} 1 take recorded"
        );
        assert_eq!(
            line(&s(125, 0)),
            "You were in the call for 2 h 5 min \u{b7} no takes recorded"
        );
    }

    #[test]
    fn progress_counts_against_the_most_left() {
        assert_eq!(progress(3, 5), (2, 5, 40));
        assert_eq!(progress(0, 5), (5, 5, 100));
        assert_eq!(progress(4, 0), (0, 4, 0));
        assert_eq!(progress(0, 0), (0, 0, 100));
    }
}
