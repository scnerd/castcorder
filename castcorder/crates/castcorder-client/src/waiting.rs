//! "Pat is waiting for a producer" on a room's card in the home (cast-n56.9;
//! pure, so native tests cover it).

/// The line for the people waiting in a room (none: no line).
pub fn line(names: &[String]) -> Option<String> {
    let who = match names {
        [] => return None,
        [a] => return Some(format!("{a} is waiting for a producer")),
        [a, b] => format!("{a} and {b}"),
        [a, b, rest @ ..] => format!(
            "{a}, {b} and {} other{}",
            rest.len(),
            if rest.len() == 1 { "" } else { "s" }
        ),
    };
    Some(format!("{who} are waiting for a producer"))
}

#[cfg(test)]
mod tests {
    use super::line;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn who_is_waiting_in_words() {
        assert_eq!(line(&[]), None);
        assert_eq!(
            line(&names(&["Pat"])).unwrap(),
            "Pat is waiting for a producer"
        );
        assert_eq!(
            line(&names(&["Pat", "Sam"])).unwrap(),
            "Pat and Sam are waiting for a producer"
        );
        assert_eq!(
            line(&names(&["A", "B", "C"])).unwrap(),
            "A, B and 1 other are waiting for a producer"
        );
        assert_eq!(
            line(&names(&["A", "B", "C", "D"])).unwrap(),
            "A, B and 2 others are waiting for a producer"
        );
    }
}
