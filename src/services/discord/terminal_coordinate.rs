#[cfg(test)]
mod dormancy;
mod model;

pub(in crate::services::discord) use model::{
    CanonicalC, RouteFamily, SourceRange, TerminalCoordinate, TerminalCoordinateCandidate,
    TerminalCoordinateError, TurnIdentity, validate_terminal_coordinate,
};

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    fn message() -> TurnIdentity {
        TurnIdentity::message(41, "exact-start", Some(7)).expect("valid message identity")
    }

    fn candidate(
        canonical_c: u64,
        source_range: Option<(u64, u64)>,
    ) -> TerminalCoordinateCandidate {
        TerminalCoordinateCandidate::new(
            canonical_c,
            source_range,
            9,
            message(),
            RouteFamily::Watcher,
        )
    }

    #[test]
    fn canonical_c_accepts_only_positive_u64_and_hashes_only_c() {
        assert_eq!(CanonicalC::new(0), None);
        assert_eq!(CanonicalC::new(1).map(CanonicalC::get), Some(1));
        assert_eq!(
            CanonicalC::new(u64::MAX).map(CanonicalC::get),
            Some(u64::MAX)
        );

        let mut set = HashSet::new();
        set.insert(CanonicalC::new(44).unwrap());
        assert!(set.contains(&CanonicalC::new(44).unwrap()));
    }

    #[test]
    fn optional_range_is_strict_and_must_end_at_c() {
        let terminal = validate_terminal_coordinate(candidate(8, None)).unwrap();
        assert_eq!(terminal.canonical_c().get(), 8);
        assert_eq!(terminal.source_range(), None);

        let ranged = validate_terminal_coordinate(candidate(8, Some((3, 8)))).unwrap();
        assert_eq!(ranged.source_range().map(SourceRange::start), Some(3));
        assert_eq!(ranged.source_range().map(SourceRange::end), Some(8));
        assert_eq!(
            validate_terminal_coordinate(candidate(8, Some((8, 8)))),
            Err(TerminalCoordinateError::SourceRangeNotStrict { start: 8, end: 8 })
        );
        assert_eq!(
            validate_terminal_coordinate(candidate(8, Some((9, 8)))),
            Err(TerminalCoordinateError::SourceRangeNotStrict { start: 9, end: 8 })
        );
        assert_eq!(
            validate_terminal_coordinate(candidate(8, Some((2, 7)))),
            Err(TerminalCoordinateError::SourceRangeEndMismatch {
                end: 7,
                canonical_c: 8,
            })
        );
    }

    #[test]
    fn provenance_never_changes_c_hash_identity() {
        let without = validate_terminal_coordinate(candidate(12, None)).unwrap();
        let with = validate_terminal_coordinate(candidate(12, Some((4, 12)))).unwrap();
        let mut map = HashMap::new();
        map.insert(without.canonical_c(), "terminal");
        assert_eq!(map.get(&with.canonical_c()), Some(&"terminal"));
    }

    #[test]
    fn message_identity_preserves_exact_production_fields() {
        assert_eq!(
            TurnIdentity::message(0, "started", None),
            Err(TerminalCoordinateError::MessageUserIdZero)
        );
        assert_eq!(
            TurnIdentity::message(3, "", None),
            Err(TerminalCoordinateError::StartedAtEmpty)
        );
        let no_offset = TurnIdentity::message(3, "  exact  ", None).unwrap();
        assert_eq!(no_offset.user_message_id(), 3);
        assert_eq!(no_offset.started_at(), "  exact  ");
        assert_eq!(no_offset.start_offset(), None);

        let offset = TurnIdentity::message(3, "  exact  ", Some(0)).unwrap();
        assert_eq!(offset.start_offset(), Some(0));
        assert_ne!(no_offset, offset);
        assert_ne!(
            TurnIdentity::message(3, "same", Some(5)).unwrap(),
            TurnIdentity::message(3, "same", Some(6)).unwrap()
        );
    }

    #[test]
    fn external_identity_represents_id_zero_and_requires_offset() {
        assert_eq!(
            TurnIdentity::external("", 0),
            Err(TerminalCoordinateError::StartedAtEmpty)
        );
        let zero = TurnIdentity::external("same", 0).unwrap();
        let later = TurnIdentity::external("same", 1).unwrap();
        assert_eq!(zero.user_message_id(), 0);
        assert_eq!(zero.started_at(), "same");
        assert_eq!(zero.start_offset(), Some(0));
        assert_ne!(zero, later);
    }

    #[test]
    fn route_family_is_exhaustive_and_coordinate_getters_materialize_inputs() {
        let routes = [
            RouteFamily::Sink,
            RouteFamily::Watcher,
            RouteFamily::Controller,
            RouteFamily::Recovery,
        ];
        assert_eq!(routes.len(), 4);
        for route in routes {
            let identity = TurnIdentity::external("external", 0).unwrap();
            let terminal = validate_terminal_coordinate(TerminalCoordinateCandidate::new(
                22,
                Some((5, 22)),
                u64::MAX,
                identity.clone(),
                route,
            ))
            .unwrap();
            assert_eq!(terminal.canonical_c().get(), 22);
            assert_eq!(terminal.source_range().map(SourceRange::start), Some(5));
            assert_eq!(terminal.reset_incarnation(), u64::MAX);
            assert_eq!(terminal.turn_identity(), &identity);
            assert_eq!(terminal.route_family(), route);
        }
    }
}
