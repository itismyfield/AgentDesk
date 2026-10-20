use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::services::tui_o::shadow::tap::{ToolCardsConfig, TuiOConfig};
use crate::services::tui_o::store::fault::{self, Step};

fn reader(root: &Path) -> Arc<dyn Fn() -> bool + Send + Sync> {
    ToolCardSwitch::for_root(root, 123, Arc::new(|| true))
}

#[test]
fn absent_config_and_missing_channel_override_default_on() {
    let config: TuiOConfig = serde_yaml::from_str("{}").unwrap();
    assert!(config.tool_cards.enabled);
    assert!(ToolCardsConfig::default().enabled);
    let root = tempfile::tempdir().unwrap();
    assert!(reader(root.path())());
    assert_eq!(status(root.path(), 123), Ok(None));
    assert!(
        !root.path().join(DIR_NAME).exists(),
        "reading creates no files"
    );
}

#[test]
fn live_toggle_updates_all_readers_and_persists_for_restart() {
    let root = tempfile::tempdir().unwrap();
    let first = reader(root.path());
    let second = reader(root.path());
    for enabled in [false, true, false] {
        assert_eq!(set(root.path(), 123, enabled, 456), Ok(Published::Durable));
        assert_eq!(first(), enabled);
        assert_eq!(second(), enabled);
        let restored = read(root.path(), 123).unwrap().unwrap();
        assert_eq!((restored.enabled, restored.by), (enabled, 456));
    }
    // A fresh cache entry is the next process's initial snapshot.
    VERSIONS
        .lock()
        .unwrap()
        .remove(&(root.path().to_path_buf(), 123));
    assert!(!reader(root.path())());
}

#[test]
fn actual_persisted_toggles_reach_existing_deriver_at_run_boundaries() {
    use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider, UnitKind};
    use crate::services::tui_o::writer::pieces::{
        Derived, TOOL_CALL_CARD, TOOL_CALL_PANEL, ToolCardInputs, UnitDeriver,
    };

    let root = tempfile::tempdir().unwrap();
    let mut deriver =
        UnitDeriver::new(123, ShadowProvider::Claude).with_tool_cards(ToolCardInputs {
            known: Default::default(),
            recorded_cards: Default::default(),
            switch: reader(root.path()),
        });
    let record = |id: &str, content: serde_json::Value| {
        let line = serde_json::to_vec(&serde_json::json!({
            "type":"assistant", "uuid":format!("u-{id}"), "apiBlockIndex":0,
            "message":{"id":id,"content":[content]},
        }))
        .unwrap();
        CapturedRecord {
            start: 0,
            end: line.len() as u64 + 1,
            line,
        }
    };
    let tool = |id: &str| {
        record(
            id,
            serde_json::json!({
                "type":"tool_use", "id":id, "name":"Bash", "input":{"command":"pwd"},
            }),
        )
    };
    let assert_tool = |work: Vec<Derived>, id: &str, expected: &str| {
        assert!(
            matches!(work.as_slice(), [Derived::Excluded { unit_key, reason }]
            if unit_key.channel_id == 123 && unit_key.kind == UnitKind::Tool
                && unit_key.native_key == format!("{id}:0") && reason == expected),
            "{work:?}"
        );
    };
    let persist = |enabled, by| {
        assert_eq!(set(root.path(), 123, enabled, by), Ok(Published::Durable));
        let saved = read(root.path(), 123).unwrap().unwrap();
        assert_eq!((saved.enabled, saved.by), (enabled, by));
    };

    persist(false, 456);
    assert_tool(
        deriver.derive(&tool("off-first")),
        "off-first",
        TOOL_CALL_PANEL,
    );
    persist(true, 457);
    assert_tool(
        deriver.derive(&tool("on-first")),
        "on-first",
        TOOL_CALL_CARD,
    );
    persist(false, 458);
    assert_tool(
        deriver.derive(&tool("in-open-run")),
        "in-open-run",
        TOOL_CALL_CARD,
    );
    let closed = deriver.derive(&record(
        "body",
        serde_json::json!({"type":"text", "text":"answer"}),
    ));
    assert!(
        matches!(closed.as_slice(), [Derived::Piece(card), Derived::Piece(body)]
        if card.unit_key.kind == UnitKind::Tool
            && card.unit_key.native_key == "in-open-run:0#card"
            && card.payload.lines().count() == 2
            && body.unit_key.kind == UnitKind::Body && body.payload == "answer"),
        "{closed:?}"
    );
    assert_tool(
        deriver.derive(&tool("after-close")),
        "after-close",
        TOOL_CALL_PANEL,
    );
    assert!(!read(root.path(), 123).unwrap().unwrap().enabled);
}

#[test]
fn malformed_wrong_schema_and_oversized_preferences_fail_closed() {
    for bytes in [
        b"broken".to_vec(),
        br#"{"schema":2,"enabled":true,"by":1,"at":"2026-10-19T00:00:00Z"}"#.to_vec(),
        vec![b' '; MAX_BYTES as usize + 1],
    ] {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(DIR_NAME)).unwrap();
        fs::write(path(root.path(), 123), bytes).unwrap();
        assert!(!reader(root.path())());
        assert!(status(root.path(), 123).is_err());
    }
}

#[test]
fn global_gate_is_live_and_overrides_saved_on_without_rewriting_it() {
    let root = tempfile::tempdir().unwrap();
    set(root.path(), 123, true, 456).unwrap();
    let global = Arc::new(AtomicBool::new(true));
    let current = Arc::clone(&global);
    let live = ToolCardSwitch::for_root(
        root.path(),
        123,
        Arc::new(move || current.load(Ordering::SeqCst)),
    );
    assert!(live());
    global.store(false, Ordering::SeqCst);
    assert!(!live());
    global.store(true, Ordering::SeqCst);
    assert!(live());
    assert!(read(root.path(), 123).unwrap().unwrap().enabled);
    let config: TuiOConfig = serde_yaml::from_str("tool_cards:\n  enabled: false\n").unwrap();
    assert!(!config.tool_cards.enabled);
}

#[test]
fn production_channel_reader_observes_live_config_without_reconstruction() {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    crate::config_live_reload::install(crate::config::Config::default());
    set(root.path(), 123, true, 456).unwrap();
    let live = ToolCardSwitch::for_channel(123);
    assert!(live());
    let mut config = crate::config::Config::default();
    config.tui_o = Some(TuiOConfig::default());
    config.tui_o.as_mut().unwrap().tool_cards.enabled = false;
    crate::config_live_reload::install(config.clone());
    assert!(!live());
    config.tui_o.as_mut().unwrap().tool_cards.enabled = true;
    crate::config_live_reload::install(config);
    assert!(live());
}

#[test]
fn published_unsynced_updates_file_watch_status_and_restart_but_unpublished_does_not() {
    let root = tempfile::tempdir().unwrap();
    let live = reader(root.path());
    let dir = root.path().join(DIR_NAME);
    let sync = fault::plant(&dir, Step::ReplaceDirSync, ErrorKind::Other, Some(1));
    assert!(matches!(
        set(root.path(), 123, false, 456),
        Ok(Published::PublishedUnsynced(_))
    ));
    drop(sync);
    assert!(!live());
    assert!(!status(root.path(), 123).unwrap().unwrap().enabled);
    assert!(!read(root.path(), 123).unwrap().unwrap().enabled);
    VERSIONS
        .lock()
        .unwrap()
        .remove(&(root.path().to_path_buf(), 123));
    let restarted = reader(root.path());
    assert!(!restarted());
    let write = fault::plant(&dir, Step::Write, ErrorKind::StorageFull, Some(1));
    assert!(set(root.path(), 123, true, 456).is_err());
    drop(write);
    assert!(!restarted());
    assert!(!status(root.path(), 123).unwrap().unwrap().enabled);
    assert!(!read(root.path(), 123).unwrap().unwrap().enabled);
}

#[test]
fn roots_are_isolated_and_old_config_shape_accepts_the_new_key() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    set(first.path(), 123, false, 456).unwrap();
    assert!(!reader(first.path())());
    assert!(reader(second.path())());
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct OldConfig {
        writer: super::super::super::shadow::tap::WriterChannelsConfig,
    }
    let old: OldConfig = serde_yaml::from_str("tool_cards:\n  enabled: true\n").unwrap();
    assert!(old.writer.channels.is_empty());
    let future: ToolCardsConfig =
        serde_yaml::from_str("enabled: true\nfuture_option: true\n").unwrap();
    assert!(future.enabled);
}
