use super::*;

#[test]
fn only_consumed_is_complete_when_the_voice_route_projects_a_start() {
    for (status, consumed) in [("accepted", false), ("started", false), ("consumed", true)] {
        let started = started_turn(("discord:7:123".into(), status));
        assert_eq!(started.turn_id, "discord:7:123");
        assert_eq!(started.consumed, consumed);
    }
}
