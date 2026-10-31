use super::*;

#[test]
fn symmetric_home_peer_selection_covers_64_placements() {
    use Availability::{Available as Up, Unavailable as Down};
    let cases = [
        ("A", "A", [Up, Up], "A"),
        ("A", "B", [Up, Up], "A"),
        ("A", "A", [Down, Up], "B"),
        ("A", "B", [Down, Up], "B"),
        ("B", "A", [Up, Up], "B"),
        ("B", "B", [Up, Up], "B"),
        ("B", "A", [Up, Down], "A"),
        ("B", "B", [Up, Down], "A"),
    ];
    let mut checked = 0;
    for (home, gateway, availability, expected) in cases {
        for preferred in ["A", "B"] {
            for provider in ["claude", "codex"] {
                for writer in ["legacy", "O"] {
                    let snapshot = Snapshot {
                        home: Home::Known(home),
                        pair: ["A", "B"],
                        availability,
                        gateway: Some(gateway),
                    };
                    assert_eq!(
                        choose_target(&snapshot),
                        Target::Selected(expected),
                        "home={home} gateway={gateway} preferred={preferred} provider={provider} writer={writer}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(checked, 64);
}

fn snapshot(home: Home<'_>) -> Snapshot<'_> {
    Snapshot {
        home,
        pair: ["A", "B"],
        availability: [Availability::Unavailable, Availability::Available],
        gateway: Some("B"),
    }
}

#[test]
fn unknown_conflict_and_storage_error_never_fall_back() {
    for (home, reason) in [
        (Home::Unknown, Held::HomeUnknown),
        (Home::Conflict, Held::HomeConflict),
        (Home::StorageUnknown, Held::StorageUnknown),
        (Home::Known("foreign"), Held::HomeUnknown),
    ] {
        assert_eq!(choose_target(&snapshot(home)), Target::Held(reason));
    }
}

#[test]
fn unavailable_and_unknown_nodes_hold_without_guessing_gateway() {
    let mut value = snapshot(Home::Known("A"));
    value.availability = [Availability::Unavailable; 2];
    assert_eq!(choose_target(&value), Target::Held(Held::NeitherAvailable));
    value.availability = [Availability::Unknown, Availability::Available];
    assert_eq!(
        choose_target(&value),
        Target::Held(Held::AvailabilityUnknown)
    );
    value.availability = [Availability::Unavailable, Availability::Unknown];
    assert_eq!(
        choose_target(&value),
        Target::Held(Held::AvailabilityUnknown)
    );
    value.availability = [Availability::Available, Availability::Unknown];
    assert_eq!(choose_target(&value), Target::Selected("A"));
}

#[test]
fn staged_input_recovers_after_gateway_gap_without_new_input() {
    let staged = ("source-42", "원래 본문", vec!["bundle:sha256:original"]);
    let preserved = staged.clone();
    let mut value = snapshot(Home::Known("A"));
    value.gateway = None;
    assert_eq!(choose_target(&value), Target::Held(Held::NoGateway));
    assert_eq!(staged, preserved);
    value.gateway = Some("B");
    let resumed = match choose_target(&value) {
        Target::Selected(target) => Some((target, staged)),
        Target::Held(_) => None,
    };
    assert_eq!(resumed, Some(("B", preserved)));
}

#[test]
fn gateway_replacement_preserves_target_and_invalid_pair_holds() {
    let mut value = snapshot(Home::Known("A"));
    value.availability = [Availability::Available; 2];
    for gateway in ["A", "B"] {
        value.gateway = Some(gateway);
        assert_eq!(choose_target(&value), Target::Selected("A"));
    }
    for pair in [["A", "A"], ["", "B"]] {
        value.pair = pair;
        assert_eq!(choose_target(&value), Target::Held(Held::InvalidPair));
    }
}
