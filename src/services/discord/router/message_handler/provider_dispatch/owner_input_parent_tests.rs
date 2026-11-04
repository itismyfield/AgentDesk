//! A busy front names its attempt to the running turn's token only when that token owns the row.
use super::*;
use poise::serenity_prelude::{MessageId, UserId};

/// The attempt pasted into a running turn is recorded on the mailbox's token and its episode only
/// when the inflight row carries that token's nonce; another row's turn gets nothing, paste kept.
#[test]
fn a_busy_front_names_its_attempt_only_to_the_token_whose_row_it_entered() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (n, owns) in [true, false].into_iter().enumerate() {
        let name = format!("hb-parent-{owns}");
        let rig = rig(&name, 7_212_810 + n as u64, "normal", view("", false), BUSY);
        let active = Arc::new(CancelToken::new());
        let started = rig
            .mailbox
            .try_start_turn(active.clone(), UserId::new(7), MessageId::new(1));
        assert!(rig.runtime.block_on(started));
        running_row(&rig, &rig.name);
        let mut row = crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            rig.channel,
        )
        .unwrap();
        row.turn_nonce = match owns {
            true => active.turn_nonce().map(str::to_owned),
            false => Some("another-turn".into()),
        };
        crate::services::discord::inflight::save_inflight_state(&row).unwrap();
        let handed = hand_busy(
            (rig.mailbox.clone(), None),
            ProviderKind::Claude,
            (rig.channel, rig.granted_front()),
        );
        assert_eq!(rig.runtime.block_on(handed), Ok(()), "owns={owns}");
        assert_eq!(rig.events(), ["paste", "Enter"], "owns={owns}");
        // The row Claude records for the pasted body is paired with its attempt by the ledger.
        let pasted = fs::read_to_string(rig.renderer.join("body")).unwrap();
        let row = format!("busy-row-{owns}");
        let command = json!({"type":"attachment","uuid":row,"attachment":{"type":"queued_command",
            "prompt":pasted,"commandMode":"prompt","origin":{"kind":"human"}}});
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(rig.transcript())
            .unwrap();
        std::io::Write::write_all(&mut file, format!("{command}\n").as_bytes()).unwrap();
        use crate::services::tui_input::human_frame::history::{RowOwner, row_owner};
        let owner = row_owner("claude", &rig.name, std::slice::from_ref(&pasted), &row);
        let RowOwner::Command(attempt) = owner else {
            panic!("the ledger pairs the queued row with the pasted attempt: {owner:?}");
        };
        assert_eq!(
            active.native_stop.is_busy_child(&attempt),
            owns,
            "owns={owns}"
        );
        // A recovered token learns the attempt only from the episode that recorded it.
        let durable = crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            rig.channel,
        );
        let kept = &serde_json::to_value(durable).unwrap()["native_stop_busy_children"];
        assert_eq!(
            kept["attempts"] == json!([attempt]),
            owns,
            "owns={owns}: {kept}"
        );
    }
}
