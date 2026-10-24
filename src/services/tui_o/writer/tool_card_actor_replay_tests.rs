use super::*;
use crate::services::tui_o::store::tool_card_switch::{self, Published};

fn card_and_body(posts: Vec<String>) -> Vec<String> {
    posts
        .into_iter()
        .filter(|post| post.starts_with("-# ▸ ") || post == "answer")
        .collect()
}

fn native_key(channel: u64, provider: ShadowProvider, native_key: &str) -> UnitKey {
    UnitKey {
        channel_id: channel,
        provider,
        native_key: native_key.into(),
        kind: UnitKind::Tool,
    }
}

fn bindings_for(channel: u64, provider: ShadowProvider, source: SourceId) -> Arc<FakeBindings> {
    let bindings = Arc::new(FakeBindings::new());
    let mut startup = bound(
        1,
        None,
        BindingTarget::Source(source),
        BindingCause::Startup,
        None,
    );
    startup.channel_id = channel;
    startup.provider = provider;
    bindings.commit(startup);
    bindings
}

async fn wait_for_posts(harness: &Harness, task: &Actor, count: usize) {
    for _ in 0..20 {
        polls(1).await;
        if card_and_body(harness.port.posts()).len() >= count || task.is_finished() {
            break;
        }
    }
}

async fn recorded_card_restart(provider: ShadowProvider, channel: u64) {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let (harness, path, source) = switched_over_on(channel, b"");
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        harness._runtime.path(),
    );
    crate::config_live_reload::install(crate::config::Config {
        tui_o: Some(TuiOConfig::default()),
        ..crate::config::Config::default()
    });
    admission::set_ready_for_tests(channel, false);
    assert!(!admission::owed_delivery_ready(channel));
    let bindings = bindings_for(channel, provider, source);
    let (records, id) = native_records(provider);
    let key = native_key(channel, provider, id);
    let prefix = match provider {
        ShadowProvider::Claude => 1,
        ShadowProvider::Codex => 2,
    };
    harness.gate.acquired();
    let (stop, task) = spawn_with(harness.writer(), provider, Arc::clone(&bindings));
    for record in &records[..prefix] {
        append(&path, record);
    }
    for _ in 0..20 {
        polls(1).await;
        if harness.channel().ledger().excluded(&key) == Some(pieces::TOOL_CALL_CARD)
            || task.is_finished()
        {
            break;
        }
    }
    assert_eq!(
        harness.channel().ledger().excluded(&key),
        Some(pieces::TOOL_CALL_CARD),
        "{provider:?}: the first actor must persist native Card disposition"
    );
    assert!(card_and_body(harness.port.posts()).is_empty());
    halt(stop, task).await;
    assert_eq!(
        tool_card_switch::set(harness._runtime.path(), channel, false, BOT),
        Ok(Published::Durable)
    );
    assert!(
        !tool_card_switch::status(harness._runtime.path(), channel)
            .unwrap()
            .unwrap()
            .enabled
    );
    let (stop, task) = spawn_with(harness.writer(), provider, Arc::clone(&bindings));
    for record in &records[prefix..] {
        append(&path, record);
    }
    wait_for_posts(&harness, &task, 2).await;
    halt(stop, task).await;
    let status = match provider {
        ShadowProvider::Claude => '✓',
        ShadowProvider::Codex => '·',
    };
    let expected = vec![
        format!("-# ▸ Bash  cargo check  {status}"),
        "answer".to_string(),
    ];
    assert_eq!(
        card_and_body(harness.port.posts()),
        expected,
        "{provider:?}"
    );
    let store = harness.channel();
    let ledger = store.ledger();
    assert_eq!(ledger.excluded(&key), Some(pieces::TOOL_CALL_CARD));
    let card_key = UnitKey {
        native_key: format!("{id}#card"),
        ..key
    };
    let (_, card) = ledger.latest_piece(&card_key, 0).unwrap();
    assert_eq!(card.payload, expected[0]);
    assert!(matches!(card.outcome, Some(PieceOutcome::Posted(_))));
    assert_eq!(ledger.violation(), None);
    drop(store);
    let (stop, task) = spawn_with(harness.writer(), provider, bindings);
    polls(4).await;
    halt(stop, task).await;
    assert_eq!(card_and_body(harness.port.posts()), expected);
    assert!(
        !harness
            .port
            .posts()
            .iter()
            .any(|post| post.contains("PRIVATE_TOOL_RESULT"))
    );
    assert!(harness.alarms.taken().is_empty());
}

async fn old_panel_startup(provider: ShadowProvider, channel: u64) {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let (harness, path, source) = switched_over_on(channel, b"");
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        harness._runtime.path(),
    );
    crate::config_live_reload::install(crate::config::Config {
        tui_o: Some(TuiOConfig::default()),
        ..crate::config::Config::default()
    });
    admission::set_ready_for_tests(channel, false);
    assert!(!admission::owed_delivery_ready(channel));
    let bindings = bindings_for(channel, provider, source);
    let (records, id) = native_records(provider);
    let key = native_key(channel, provider, id);
    let mut store = harness.channel();
    store
        .append_ledger(LedgerEntry::Excluded {
            unit_key: key.clone(),
            reason: pieces::TOOL_CALL_PANEL.into(),
        })
        .unwrap();
    drop(store);
    for record in records {
        append(&path, &record);
    }
    harness.gate.acquired();
    let (stop, task) = spawn_with(harness.writer(), provider, Arc::clone(&bindings));
    wait_for_posts(&harness, &task, 1).await;
    polls(2).await;
    halt(stop, task).await;
    assert_eq!(
        card_and_body(harness.port.posts()),
        ["answer"],
        "{provider:?}"
    );
    if provider == ShadowProvider::Codex {
        assert_eq!(harness.port.posts(), ["answer"]);
    }
    let store = harness.channel();
    assert_eq!(store.ledger().excluded(&key), Some(pieces::TOOL_CALL_PANEL));
    let card_key = UnitKey {
        native_key: format!("{id}#card"),
        ..key
    };
    assert!(store.ledger().latest_piece(&card_key, 0).is_none());
    assert_eq!(store.ledger().violation(), None);
    drop(store);
    let (stop, task) = spawn_with(harness.writer(), provider, bindings);
    polls(4).await;
    halt(stop, task).await;
    assert_eq!(card_and_body(harness.port.posts()), ["answer"]);
    if provider == ShadowProvider::Codex {
        assert_eq!(harness.port.posts(), ["answer"]);
    }
    assert!(harness.alarms.taken().is_empty());
}

#[tokio::test(start_paused = true)]
async fn actor_replays_durable_card_after_off_with_one_card_then_body() {
    for (provider, channel) in [
        (ShadowProvider::Claude, 584_671),
        (ShadowProvider::Codex, 584_672),
    ] {
        recorded_card_restart(provider, channel).await;
    }
}

#[tokio::test(start_paused = true)]
async fn actor_startup_on_never_backfills_a_durable_panel_tool() {
    for (provider, channel) in [
        (ShadowProvider::Claude, 584_673),
        (ShadowProvider::Codex, 584_674),
    ] {
        old_panel_startup(provider, channel).await;
    }
}
