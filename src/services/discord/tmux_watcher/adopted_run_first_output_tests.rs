//! A read-error fallback replaces the turn-start-paused watcher that relayed T0, so the fresh
//! successor never ends T1 at its first output as T0's continuation.

use super::*;
use crate::services::discord::inflight::TurnSource;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;

const T0: &str = "I7527 T0 delivered before the watcher started";
const T0_BODY: &str = "I7527 T0 answer the watcher relays itself";
const READ: &str = "I7527 T1 part the bridge reader got before its read failed";
const FIRST: &str = "I7527 T1 first output after the fallback";
const REST: &str = "I7527 T1 rest of the answer";

fn frames_from(h: &Harness, start: u64) -> Vec<(u64, u64, String)> {
    let frames = h.frames().into_iter();
    frames.filter(|frame| frame.0 >= start).collect()
}

/// T1's read fails at its start or mid-run; the TmuxReady claim replaces the paused T0 watcher,
/// whose successor ends T1 only at T1's own end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_error_fallback_replaces_the_paused_t0_watcher_with_a_fresh_one() {
    let test = "a_read_error_fallback_replaces_the_paused_t0_watcher_with_a_fresh_one";
    if !isolated_in("adopted_run_first_output_tests", test, &[]) {
        return;
    }
    for (case, mid_run) in [(91, false), (92, true)] {
        let seed = format!("{}{}{}", user("T0"), said(T0), stop());
        let mut h = Harness::new(case, &seed).await;
        // A gateway session, so the TmuxReady arm spawns its watcher rather than a standby relay.
        let (_, gateway_url, _gateway) =
            discord_mock::start(discord_mock::DiscordMockState::new()).await;
        let ctx = discord_mock::serenity_context_over(h.http.clone(), gateway_url).await;
        assert!(h.shared.http.cached_serenity_ctx.set(ctx).is_ok());
        let f = seed.len() as u64;
        h.commit(0, f);
        let mut first = h.row_at(f);
        first.turn_source = TurnSource::Managed;
        first.user_msg_id = 7_527_000_000 + case;
        h.save(&first);
        let restored = crate::services::discord::tmux::restored_watcher_turn_from_inflight(
            &first, &h.tmux, true,
        );
        h.spawn_restoring(f, restored);
        h.append(format!("{}{}{}", user("T0b"), said(T0_BODY), stop()).as_bytes());
        h.drained("T0 terminal frame").await;
        assert!(
            h.showing(T0_BODY),
            "case {case}: the watcher delivered T0 itself"
        );
        assert!(
            h.row().is_none(),
            "case {case}: the watcher ended T0 itself"
        );
        let incumbent = h.watcher.as_ref().unwrap().cancel.clone();

        // T1's turn start pauses the attached watcher before its provider input (watchdog.rs).
        let handle = h.shared.tmux_watchers.get(&h.channel).unwrap();
        handle.pause_epoch.fetch_add(1, Ordering::Relaxed);
        handle.paused.store(true, Ordering::Relaxed);
        drop(handle);
        let start = h.len();
        let mut next = h.row_at(start);
        next.turn_source = TurnSource::Managed;
        next.user_msg_id = 7_527_000_100 + case;
        h.save(&next);
        let token = crate::services::provider::CancelToken::from_persisted_turn_nonce(
            next.turn_nonce.clone(),
        );
        let token = Arc::new(token);
        token.bind_claude_tmux_session(&h.tmux);
        let author = serenity::UserId::new(7);
        let user_msg = serenity::MessageId::new(next.user_msg_id);
        let begin = crate::services::discord::mailbox_try_start_turn;
        assert!(begin(&h.shared, h.channel, token, author, user_msg).await);
        // The bridge's follow-up reader takes T1's output, then its read fails.
        if mid_run {
            h.append(format!("{}{}", user("T1"), said(READ)).as_bytes());
        }
        let failed_at = h.len();
        let root = std::env::var("AGENTDESK_ROOT_DIR").unwrap();
        let fifo = format!("{root}/input-{case}");
        std::fs::write(&fifo, "").unwrap();
        let hand =
            crate::services::discord::turn_bridge::hand_failed_followup_read_to_watcher_for_test;
        let paths = (h.tmux.as_str(), h.path.as_str(), fifo.as_str());
        let offsets = (start, failed_at);
        let successor = hand(&h.shared, &CLAUDE, &mut next, paths, offsets, &incumbent).await;
        h.exited("the replaced T0 watcher").await;

        // The successor's latch starts false: T1's first output streams, ending nothing.
        if !mid_run {
            h.append(user("T1").as_bytes());
        }
        h.append(said(FIRST).as_bytes());
        h.until("T1 first output streaming", |h| h.showing(FIRST))
            .await;
        h.settle().await;
        let early = frames_from(&h, start);
        assert_eq!(early, Vec::new(), "case {case}: no T1 terminal yet");
        let row = h.row().expect("T1's row stays until T1 ends");
        assert_eq!(row.turn_nonce, next.turn_nonce, "case {case}");
        let snapshot = h.shared.mailbox(h.channel).snapshot().await;
        assert!(
            snapshot.cancel_token.is_some(),
            "case {case}: T1's mailbox turn runs"
        );

        h.append(format!("{}{}", said(REST), stop()).as_bytes());
        h.until("T1 terminal frame", |h| !frames_from(h, start).is_empty())
            .await;
        h.settle().await;
        assert!(
            h.showing(REST),
            "case {case}: the successor relayed T1's end"
        );
        assert_eq!(
            frames_from(&h, start).len(),
            1,
            "case {case}: one T1 terminal"
        );
        if !mid_run {
            // A frame from T1's own start ends T1 there.
            assert!(h.row().is_none(), "case {case}: T1 ended at its own end");
        }
        cancel_for_test(&successor.cancel, Ordering::Release);
    }
}
