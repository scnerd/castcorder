//! Query-string edits for the page's own address (pure, so native tests cover
//! them; the wasm app applies the result with `history.replaceState`).

/// The query string `search` (with or without its leading `?`) minus every
/// `key` parameter, as `?a=b` or `""` when nothing is left. Other parameters
/// are kept exactly as written.
pub fn without_param(search: &str, key: &str) -> String {
    let kept: Vec<&str> = search
        .trim_start_matches('?')
        .split('&')
        .filter(|kv| !kv.is_empty())
        .filter(|kv| kv.split_once('=').map_or(*kv, |(k, _)| k) != key)
        .collect();
    if kept.is_empty() {
        String::new()
    } else {
        format!("?{}", kept.join("&"))
    }
}

/// A remote tile whose tracks have all ended (and which has at least one) is
/// orphaned: its owner is gone. `live` holds one flag per track in the tile.
pub fn tile_is_orphaned(live: &[bool]) -> bool {
    !live.is_empty() && live.iter().all(|l| !l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_call_parameter_is_dropped() {
        assert_eq!(without_param("?call=abc", "call"), "");
        assert_eq!(without_param("?room=r1&call=abc", "call"), "?room=r1");
        assert_eq!(without_param("?call=abc&room=r1", "call"), "?room=r1");
        assert_eq!(without_param("?call=a&x=1&call=b", "call"), "?x=1");
    }

    #[test]
    fn other_parameters_and_lookalikes_stay() {
        assert_eq!(without_param("", "call"), "");
        assert_eq!(without_param("?room=r1", "call"), "?room=r1");
        assert_eq!(
            without_param("?recall=1&call2=2", "call"),
            "?recall=1&call2=2"
        );
    }

    #[test]
    fn a_tile_goes_only_when_every_track_has_ended() {
        assert!(tile_is_orphaned(&[false]));
        assert!(tile_is_orphaned(&[false, false]));
        assert!(!tile_is_orphaned(&[false, true]), "a replacement is live");
        assert!(!tile_is_orphaned(&[true]));
        assert!(!tile_is_orphaned(&[]), "no track yet: leave it");
    }
}
