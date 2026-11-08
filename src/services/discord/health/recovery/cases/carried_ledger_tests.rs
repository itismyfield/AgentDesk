use super::*;
use crate::services::tui_input::human_frame::history::v2::Registry;
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
use serde_json::{Value, json};
use std::os::unix::fs::MetadataExt;

async fn fixture(channel: u64, command: bool, competitor: bool) -> (Fixture, Value) {
    let frame = crate::services::tui_input::human_frame::idle("ann", "7", "carried input");
    let user = |uuid| {
        json!({"type":"user","uuid":uuid,"sessionId":"parent",
        "message":{"role":"user","content":frame}})
        .to_string()
    };
    let mut rows = vec![];
    if command {
        rows.push(
            json!({"type":"user","uuid":"native-n","sessionId":"parent",
            "message":{"role":"user","content":"native N"}})
            .to_string(),
        );
    }
    if competitor {
        rows.push(user("before-a"));
    }
    let offset = rows.iter().map(|r| r.len() as u64 + 1).sum::<u64>();
    rows.push(if command {
        json!({"type":"attachment","uuid":"actual-a","sessionId":"parent",
            "attachment":{"type":"queued_command","prompt":frame,"commandMode":"prompt",
            "origin":{"kind":"human"},"humanTurn":true}})
        .to_string()
    } else {
        user("actual-a")
    });
    if !command {
        rows.push(END.into());
    }
    let fx = Fixture::new(
        channel,
        channel + 100,
        Options {
            episode: false,
            transcript: rows,
            ..Options::default()
        },
    )
    .await;
    let mut row = fx.row().unwrap();
    row.managed_submission =
        Some(serde_json::from_value(json!({"protocol":1,"phase":"waiting"})).unwrap());
    row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    row.session_id = Some("parent".into());
    row.readopted_from_inflight = false;
    let row_path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &PROVIDER,
        channel,
    );
    std::fs::write(row_path, serde_json::to_vec(&row).unwrap()).unwrap();
    assert_eq!(fx.row().unwrap().session_id.as_deref(), Some("parent"));
    assert!(fx.row().unwrap().managed_submission.is_some());
    let session = row.tmux_session_name.as_deref().unwrap();
    let execution =
        PreparedIncarnation::prepare("claude", session, Some(channel), Some("parent"), true)
            .unwrap();
    let meta = std::fs::metadata(&fx.path).unwrap();
    let input = |primary, nonce: &str, offset| {
        json!({"identity":{"primary":primary,
        "queued_generation":5,"sources":[{"message_id":primary,"queued_generation":5}]},
        "attempt":{"nonce":nonce,"provider":"claude","target":{"host":"tmux","owner":session,
            "root":execution.context.owner_runtime_root,"execution_nonce":execution.context.execution_nonce},
            "source":{"kind":"existing","session_id":"parent","transcript":fx.path,
            "file":[meta.dev(),meta.ino()],"offset":offset}},
        "original":{"author_id":7},"payload":{"frame":frame}})
    };
    let registry = Registry::runtime().unwrap();
    let namespace = format!("claude/{}", fx.shared.token_hash);
    if competitor {
        registry
            .prepare(&namespace, &input(channel + 99, "before-a", 0))
            .unwrap();
    }
    let input = input(row.user_msg_id, "attempt-a", offset);
    registry.prepare(&namespace, &input).unwrap();
    (fx, input)
}

fn edit_history(apply: impl FnOnce(&mut Value)) {
    let mut state = Registry::runtime().unwrap().json().unwrap();
    apply(&mut state);
    let path = crate::config::runtime_root()
        .unwrap()
        .join("runtime/injected_frames.json");
    std::fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
}

async fn assert_unconsumed(fx: &Fixture) {
    let row = fx.row().expect("ambiguous input is preserved");
    assert!(
        row.native_stop_user_pin.is_none(),
        "no pin may escape before confirmation"
    );
    assert!(row.terminal_receipt.is_none());
    assert_eq!(fx.archive_count(), 0);
    let receipts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM turns WHERE turn_id=$1 AND terminal_outcome IS NOT NULL",
    )
    .bind(format!("discord:{}:{}", fx.channel, fx.user_msg_id))
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(receipts, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn carried_ledger_primary_duplicate_before_pin_keeps_both_passes_pg() {
    let (fx, mut input) = fixture(7_828_311, false, false).await;
    let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = reached.clone();
    let namespace = format!("claude/{}", fx.shared.token_hash);
    on("consumed", fx.channel.get(), move || {
        input["attempt"]["nonce"] = json!("duplicate-a");
        Registry::runtime()
            .unwrap()
            .prepare(&namespace, &input)
            .unwrap();
        seen.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    fx.pass().await;
    assert!(
        reached.load(std::sync::atomic::Ordering::SeqCst),
        "race must reach the pre-pin boundary"
    );
    assert_unconsumed(&fx).await;
    fx.pass().await;
    assert_unconsumed(&fx).await;
}

#[tokio::test(flavor = "current_thread")]
async fn carried_ledger_competitor_change_before_pin_keeps_both_passes_pg() {
    let (fx, _) = fixture(7_828_312, false, true).await;
    let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = reached.clone();
    on("consumed", fx.channel.get(), move || {
        edit_history(|state| {
            state["live"][0]["registration"]["source"]["target"]["owner"] = json!("foreign-owner")
        });
        seen.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    fx.pass().await;
    assert!(
        reached.load(std::sync::atomic::Ordering::SeqCst),
        "race must reach the pre-pin boundary"
    );
    assert_unconsumed(&fx).await;
    fx.pass().await;
    assert_unconsumed(&fx).await;
}

#[tokio::test(flavor = "current_thread")]
async fn carried_ledger_duplicate_or_wrong_nonce_stays_unconsumed_pg() {
    for (channel, duplicate) in [(7_828_313, true), (7_828_314, false)] {
        let (fx, mut input) = fixture(channel, false, false).await;
        if duplicate {
            input["attempt"]["nonce"] = json!("duplicate-a");
            Registry::runtime()
                .unwrap()
                .prepare(&format!("claude/{}", fx.shared.token_hash), &input)
                .unwrap();
        } else {
            edit_history(|state| state["live"][0]["id"] = json!("wrong-owner-nonce"));
        }
        for _ in 0..2 {
            fx.pass().await;
            assert_unconsumed(&fx).await;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn carried_ledger_command_waits_for_native_and_o_without_stop_authority_pg() {
    let (fx, _) = fixture(7_828_315, true, false).await;
    fx.pass().await;
    assert_unconsumed(&fx).await;
    append(&fx.path, &[END]);
    discord::health::settled_reason(&fx.shared, &PROVIDER, fx.channel.get()).await;
    fx.o.lock().unwrap().debt.owed = 1;
    fx.pass().await;
    assert_unconsumed(&fx).await;
    fx.o.lock().unwrap().debt = Undelivered::default();
    let channel = fx.channel.get();
    on("archive", channel, move || {
        let row = inflight::load_inflight_state_read_only(&PROVIDER, channel).unwrap();
        assert!(
            row.native_stop_user_pin.is_none(),
            "CommandConsumed cannot publish a stop pin"
        );
        let meta = std::fs::metadata(row.output_path.as_ref().unwrap()).unwrap();
        assert!(!row.native_input_within(
            (meta.dev(), meta.ino()),
            row.tmux_session_name.as_deref().unwrap(),
            (0, meta.len())
        ));
        let token = Arc::new(CancelToken::from_persisted_turn_nonce(
            row.turn_nonce.clone(),
        ));
        inflight::restore_native_stop_pin(&row, &token);
        assert!(!token.native_stop.receipt_sink().has_restored_input());
        assert!(token.native_stop.receipt_sink().stop_pin().is_none());
    });
    fx.pass().await;
    assert!(
        fx.row().is_none(),
        "CommandConsumed closes after N and O finish"
    );
    assert_eq!(fx.archive_count(), 1);
    let outcome: String = sqlx::query_scalar("SELECT terminal_outcome FROM turns WHERE turn_id=$1")
        .bind(format!("discord:{}:{}", fx.channel, fx.user_msg_id))
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(outcome, "completed");
    assert_eq!(fx.told().await, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn carried_ledger_command_rechecks_registration_under_transition_pg() {
    let (fx, mut input) = fixture(7_828_316, true, false).await;
    append(&fx.path, &[END]);
    discord::health::settled_reason(&fx.shared, &PROVIDER, fx.channel.get()).await;
    let namespace = format!("claude/{}", fx.shared.token_hash);
    let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = reached.clone();
    on("locked", fx.channel.get(), move || {
        input["attempt"]["nonce"] = json!("duplicate-a");
        Registry::runtime()
            .unwrap()
            .prepare(&namespace, &input)
            .unwrap();
        seen.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    fx.pass().await;
    assert!(reached.load(std::sync::atomic::Ordering::SeqCst));
    assert_unconsumed(&fx).await;
    fx.pass().await;
    assert_unconsumed(&fx).await;
}

#[tokio::test(flavor = "current_thread")]
async fn carried_ledger_confirmed_user_pin_is_independent_of_later_registrations_pg() {
    let (fx, mut input) = fixture(7_828_317, false, false).await;
    fx.o.lock().unwrap().debt.owed = 1;
    fx.pass().await;
    let row = fx.row().unwrap();
    assert!(row.native_stop_user_pin.is_some());
    assert!(inflight::historical_input_verified(&row));
    input["attempt"]["nonce"] = json!("later-duplicate-a");
    Registry::runtime()
        .unwrap()
        .prepare(&format!("claude/{}", fx.shared.token_hash), &input)
        .unwrap();
    fx.o.lock().unwrap().debt = Undelivered::default();
    fx.pass().await;
    assert!(fx.row().is_none());
    assert_eq!(fx.archive_count(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn carried_ledger_unrelated_registration_does_not_delay_pin_pg() {
    let (fx, mut input) = fixture(7_828_318, false, false).await;
    let namespace = format!("claude/{}", fx.shared.token_hash);
    let unrelated = fx.user_msg_id + 100;
    on("consumed", fx.channel.get(), move || {
        input["identity"]["primary"] = json!(unrelated);
        input["identity"]["sources"][0]["message_id"] = json!(unrelated);
        input["attempt"]["nonce"] = json!("unrelated-attempt");
        input["payload"]["frame"] = json!(crate::services::tui_input::human_frame::idle(
            "ann",
            "7",
            "unrelated input"
        ));
        Registry::runtime()
            .unwrap()
            .prepare(&namespace, &input)
            .unwrap();
    });
    fx.pass().await;
    assert!(
        fx.row().is_none(),
        "only the selection and its competitors gate publication"
    );
    assert_eq!(fx.archive_count(), 1);
    assert_eq!(fx.told().await, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn carried_ledger_command_rechecks_registration_after_receipt_pg() {
    let (fx, mut input) = fixture(7_828_319, true, false).await;
    append(&fx.path, &[END]);
    discord::health::settled_reason(&fx.shared, &PROVIDER, fx.channel.get()).await;
    let namespace = format!("claude/{}", fx.shared.token_hash);
    let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = reached.clone();
    on("receipt", fx.channel.get(), move || {
        input["attempt"]["nonce"] = json!("duplicate-after-receipt");
        Registry::runtime()
            .unwrap()
            .prepare(&namespace, &input)
            .unwrap();
        seen.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    fx.pass().await;
    assert!(reached.load(std::sync::atomic::Ordering::SeqCst));
    for _ in 0..2 {
        let row = fx.row().expect("a changed command witness keeps its row");
        assert!(row.native_stop_user_pin.is_none());
        assert!(row.terminal_receipt.is_none());
        assert_eq!(fx.archive_count(), 0);
        fx.pass().await;
    }
    let outcome: String = sqlx::query_scalar("SELECT terminal_outcome FROM turns WHERE turn_id=$1")
        .bind(format!("discord:{}:{}", fx.channel, fx.user_msg_id))
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(outcome, "completed");
    assert_eq!(fx.told().await, 0);
}
