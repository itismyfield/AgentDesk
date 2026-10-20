//! Source guard for the controller acquire sites.

// Source-level wiring invariant: each controller path must mint its holder at
// the acquisition site. Replacing either exact call with a constant makes this red.
pub(super) fn controller_bridge_acquire_sites_mint_fresh_attempts() {
    let source = include_str!("../../terminal_controller_cutover.rs");
    let (_, after_short) = source
        .split_once("pub(super) async fn deliver_short_replace_via_controller(")
        .expect("short-replace controller function");
    let (short_body, after_long) = after_short
        .split_once("pub(super) async fn deliver_long_chunks_via_controller(")
        .expect("long-chunks controller function");
    let (long_body, _) = after_long
        .split_once("/// #3998 S1-d: borrowed long-chunk locals")
        .expect("long-chunks controller boundary");
    let mint = "let holder = super::terminal_delivery::next_bridge_lease_holder();";
    assert_eq!(short_body.matches(mint).count(), 1);
    assert_eq!(long_body.matches(mint).count(), 1);
}
