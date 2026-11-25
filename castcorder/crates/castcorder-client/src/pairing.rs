//! Who a participant may offer to become contacts in a call (cast-40e.10):
//! someone else whose device is linked to a person, other than one's own
//! person, and only when one's own device is linked too.

/// Whether participant `me` may offer participant `id` to become contacts,
/// given every participant's person (`Event::Participants`).
pub fn offerable(me: Option<u64>, persons: &[(u64, Option<String>)], id: u64) -> bool {
    let person = |p: u64| {
        persons
            .iter()
            .find(|(q, _)| *q == p)
            .and_then(|(_, person)| person.as_deref())
    };
    let Some(me) = me else {
        return false;
    };
    me != id && matches!((person(me), person(id)), (Some(a), Some(b)) if a != b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_another_linked_person_can_be_offered() {
        let persons = vec![
            (1, Some("pAnn".to_string())),
            (2, Some("pBob".to_string())),
            (3, None),
            (4, Some("pAnn".to_string())),
        ];
        assert!(offerable(Some(1), &persons, 2));
        assert!(!offerable(Some(1), &persons, 1), "not oneself");
        assert!(!offerable(Some(1), &persons, 3), "not an anonymous device");
        assert!(!offerable(Some(1), &persons, 4), "not one's own person");
        assert!(!offerable(Some(1), &persons, 9), "not someone who left");
        assert!(
            !offerable(Some(3), &persons, 2),
            "not from an anonymous device"
        );
        assert!(!offerable(None, &persons, 2), "not before joining");
    }
}
