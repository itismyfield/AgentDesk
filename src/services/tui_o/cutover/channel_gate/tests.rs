use super::*;
use crate::services::tui_o::cutover::test_override;

// A verified empty list is O off for every destination, unknown ones included; no snapshot holds.
#[test]
fn an_empty_list_leaves_even_an_unknown_destination_to_legacy_while_no_snapshot_holds() {
    {
        let _empty = test_override::force_channels(&[]);
        assert_eq!(o_owns_tui_output_for_channel(0, None), Ok(false));
        assert_eq!(
            o_owns_tui_output_for_channel_tmux(0, Some("unbound-session")),
            Ok(false)
        );
    }
    assert_eq!(
        o_owns_tui_output_for_channel(0, None),
        Err(IdentityError::MissingSnapshot),
        "an uninstalled snapshot must never read as the empty list"
    );
}
