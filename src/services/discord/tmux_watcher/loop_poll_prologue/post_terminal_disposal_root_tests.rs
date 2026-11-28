#[test]
fn carry_forward_uses_proven_frontier_not_local_consumption() {
    use crate::services::discord::tui_prompt_relay::synthetic_start_offset_carry_forward;
    assert_eq!(synthetic_start_offset_carry_forward(3, Some(7)), 7);
    assert_eq!(synthetic_start_offset_carry_forward(80, Some(7)), 80);
}
