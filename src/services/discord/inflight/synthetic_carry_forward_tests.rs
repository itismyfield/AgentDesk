#[test]
fn synthetic_carry_forward_skipped_on_generation_mismatch_3358() {
    // #3358 round 2 — Finding 1 guard, at the production-helper level.
    //
    // After a tmux wrapper RESTART the output stream legitimately resets to
    // offset 0. The committed watermark from the PREVIOUS generation is stale;
    // the claim choke-point detects the generation mismatch and passes `None`
    // for the committed frontier. The helper MUST then fall back to the
    // synthetic's own (lagging) birth offset — NOT lift it over the stale
    // watermark, which would treat future bytes below that watermark as
    // already delivered (CONTENT SKIP, strictly worse than the original
    // ERROR-only bug). On HEAD (helper took a bare `u64` and always clamped)
    // there was no way to express "stale → do not clamp", so this guard
    // failed.
    let relay_last_offset: u64 = 100; // fresh post-restart birth (lagging).
    let stale_frontier: u64 = 2_838_484; // pre-restart, NUMERICALLY higher.

    // Generation mismatch → `None`: NO clamp, born at its own cursor.
    let birth = crate::services::discord::tui_prompt_relay::synthetic_start_offset_carry_forward(
        relay_last_offset,
        None,
    );
    assert_eq!(
        birth, relay_last_offset,
        "a stale (different-generation) watermark must NOT clamp the new synthetic forward"
    );

    // Same generation → `Some(..)`: clamp DOES carry the frontier forward.
    let same_gen_birth =
        crate::services::discord::tui_prompt_relay::synthetic_start_offset_carry_forward(
            relay_last_offset,
            Some(stale_frontier),
        );
    assert_eq!(
        same_gen_birth, stale_frontier,
        "a same-generation committed frontier must still carry forward (the #3358 fix)"
    );
}
