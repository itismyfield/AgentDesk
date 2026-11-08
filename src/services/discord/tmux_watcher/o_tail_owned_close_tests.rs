//! An anchorless managed row whose soft terminal the watcher may not send: O's proof that the
//! frame posted ends the row only when the row's own verified input lies inside that frame.

use super::*;
use crate::services::discord::inflight::TurnSource;
use crate::services::discord::outbound::o_delivery_evidence::tests::Fixture;

const BODY: &str = "I7889 answer O already posted for the anchorless turn";
/// Transcript bytes before the turn, so the watcher reads it from a nonzero start as in production.
const PREFIX: usize = 4096;
const CHANNELS: &str =
    "[[6284231,\"claude_tui\"],[6284232,\"claude_tui\"],[6284233,\"claude_tui\"]]";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Case {
    /// O posted the frame holding the row's own input.
    Owned,
    /// O posted an older turn's frame; the live row's input comes after it.
    NewerRow,
    /// The row's input is in the frame but O's post does not read back.
    NoEvidence,
}

/// The production row shape of an anchorless managed Claude TUI turn (offset 0, no relay
/// owner, no output path), carrying the input pin recorded when its prompt landed at `input`.
fn anchorless_row(f: &Fixture, tmux: &str, input: (u64, u64)) -> InflightTurnState {
    let mut row = f.row.clone();
    row.tmux_session_name = Some(tmux.to_string());
    row.output_path = None;
    row.turn_start_offset = Some(0);
    row.last_offset = 0;
    row.full_response.clear();
    row.anchorless_turn_mode = true;
    row.turn_source = TurnSource::Managed;
    let mut value = serde_json::to_value(&row).unwrap();
    value["native_stop_user_pin"] = serde_json::json!({
        "input": {
            "uuid": "input", "source_file": [f.source.dev, f.source.ino],
            "range": [input.0, input.1], "receipt_start": input.0,
            "row_digest": "row", "frame_digest": "frame", "session_id": "native-session",
            "output_path": f.source.path.display().to_string(), "tmux_session": tmux,
            "spawn_nonce": "pane", "direct": false
        },
        "provider": row.provider, "channel_id": row.channel_id,
        "request_owner_user_id": row.request_owner_user_id, "user_msg_id": row.user_msg_id,
        "turn_nonce": row.turn_nonce, "started_at": row.started_at
    });
    serde_json::from_value(value).unwrap()
}

async fn run(case: Case, index: u64) {
    let mut h = Harness::new(131 + index, "").await;
    let f = Fixture::typed_with_prefix(h.channel.get(), &[BODY], PREFIX).await;
    if case == Case::NoEvidence {
        f.port.messages.lock().unwrap()[0].content = "wrong payload".into();
    }
    let (http, _rest) = f.http().await;
    h.http = http;
    h.path = f.source.path.display().to_string();
    let _bound =
        crate::services::tui_o::cutover::test_override::bind_claude_tui_session(&h.tmux, &h.path);
    let frame_end = h.len();
    let start = PREFIX as u64;
    let raw = std::fs::read(&h.path).unwrap();
    let input_end = start + raw[PREFIX..].iter().position(|b| *b == b'\n').unwrap() as u64 + 1;
    let input = if case == Case::NewerRow {
        let next = user("the next input");
        h.append(next.as_bytes());
        (frame_end, frame_end + next.len() as u64)
    } else {
        (start, input_end)
    };
    let row = anchorless_row(&f, &h.tmux, input);
    h.save(&row);
    assert!(
        h.row().is_some_and(|r| r.native_input_within(
            (f.source.dev, f.source.ino),
            &h.tmux,
            input
        )),
        "{case:?}: the stored row keeps its input pin"
    );
    let token =
        crate::services::provider::CancelToken::from_persisted_turn_nonce(row.turn_nonce.clone());
    let token = Arc::new(token);
    token.bind_claude_tmux_session(&h.tmux);
    let author = serenity::UserId::new(row.request_owner_user_id);
    let user_msg = serenity::MessageId::new(row.user_msg_id);
    let begin = crate::services::discord::mailbox_try_start_turn;
    assert!(begin(&h.shared, h.channel, token, author, user_msg).await);
    let sends = f.sends.load(Ordering::SeqCst);

    h.spawn(start);
    h.drained("terminal frame").await;
    let frames = h.frames();
    assert!(
        matches!(frames.as_slice(), [(s, _, route)]
            if *s == start && route == "soft_terminal_no_authority:turn_start_outside_frame"),
        "{case:?}: one frame, denied direct send: {frames:?}"
    );
    assert_eq!(
        f.sends.load(Ordering::SeqCst),
        sends,
        "{case:?}: nothing posts or edits a second copy"
    );
    let open = h
        .shared
        .mailbox(h.channel)
        .snapshot()
        .await
        .cancel_token
        .is_some();
    let kept = h.row().map(|r| r.turn_nonce);
    if case == Case::Owned {
        assert_eq!(kept, None, "the owned frame ends the row");
        assert!(!open, "and releases its mailbox turn");
    } else {
        assert_eq!(
            kept,
            Some(row.turn_nonce.clone()),
            "{case:?}: the row stays"
        );
        assert!(open, "{case:?}: its mailbox turn stays");
    }
    assert_eq!(
        h.unowned_drops(),
        u64::from(case == Case::NoEvidence),
        "{case:?}: only an unproven body takes the orphan path"
    );
    h.cancel();
    h.exited("watcher exit").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn o_proven_frame_ends_only_the_anchorless_row_whose_input_it_holds() {
    let test = "o_proven_frame_ends_only_the_anchorless_row_whose_input_it_holds";
    let envs = [(
        crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
        CHANNELS,
    )];
    if !isolated_in("o_tail_owned_close_tests", test, &envs) {
        return;
    }
    for (index, case) in [Case::Owned, Case::NewerRow, Case::NoEvidence]
        .into_iter()
        .enumerate()
    {
        run(case, index as u64).await;
    }
}
