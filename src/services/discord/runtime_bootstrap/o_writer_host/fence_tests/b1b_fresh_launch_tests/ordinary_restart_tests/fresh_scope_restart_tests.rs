//! An installed old-source suspension survives the real managed writer restart.

use super::*;
use crate::services::codex::actual_submit_tests::source_io::Watch;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::CanaryFixture;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::store::rotation::{Rotation, SourceLink};
use crate::services::tui_o::store::spool::Cursor;
use crate::services::tui_o::store::{ChannelStore, OEra};

const FIRST_INCARNATION_REPLY: &str = "N1 reply before the actual clear";

#[derive(Clone, Copy)]
pub(super) enum Fault {
    OtherNonce,
    Malformed,
    Version,
    Partial,
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_canary_restart_after_fresh_relaunch_with_operator_approval_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ordinary_canary_restart_after_fresh_relaunch_with_operator_approval_pg"
    )) {
        restart_case(Pending::Approval, true, None).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn fresh_relaunch_restart_settles_prepared_once_without_duplicate_post_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::fresh_relaunch_restart_settles_prepared_once_without_duplicate_post_pg"
    )) {
        restart_case(Pending::Prepared, true, None).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn fresh_relaunch_restart_with_not_found_local_retry_registers_live_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::fresh_relaunch_restart_with_not_found_local_retry_registers_live_pg"
    )) {
        restart_case(Pending::LocalRetry, true, None).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn different_nonce_cannot_restore_the_installed_suspension_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::different_nonce_cannot_restore_the_installed_suspension_pg"
    )) {
        restart_case(Pending::Approval, true, Some(Fault::OtherNonce)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_installed_suspension_holds_before_live_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::malformed_installed_suspension_holds_before_live_pg"
    )) {
        restart_case(Pending::Approval, true, Some(Fault::Malformed)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unsupported_installed_suspension_version_holds_before_live_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::unsupported_installed_suspension_version_holds_before_live_pg"
    )) {
        restart_case(Pending::Approval, true, Some(Fault::Version)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn partial_installed_suspension_holds_before_live_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::partial_installed_suspension_holds_before_live_pg"
    )) {
        restart_case(Pending::Approval, true, Some(Fault::Partial)).await;
    }
}

pub(super) struct Preserved {
    source: SourceId,
    cursor: Cursor,
    link: Option<SourceLink>,
    rotation: Rotation,
    spool: BTreeMap<PathBuf, Vec<u8>>,
    channel_dir: PathBuf,
    bytes: Vec<u8>,
    watch: Watch,
}

impl Preserved {
    pub(super) fn assert_preserved(&self, store: &ChannelStore) {
        let key = source_key(&self.source);
        assert_eq!(store.cursor(&self.source), Some(&self.cursor));
        assert!(!store.cursor(&self.source).unwrap().retired);
        let rotation = store.rotation().unwrap();
        assert_eq!(rotation.links.get(&key), self.link.as_ref());
        assert_eq!(
            rotation.successors.get(&key),
            self.rotation.successors.get(&key)
        );
        assert_eq!(source_spool(&self.channel_dir, &key), self.spool);
        assert_eq!(fs::read(&self.source.path).unwrap(), self.bytes);
        assert_eq!(
            self.watch.stats(),
            Default::default(),
            "restart cannot read or advance the suspended N1 source"
        );
    }
}

pub(super) async fn install(
    fixture: &mut CanaryFixture,
    store: &OStore,
    era: &OEra,
    output: &Discord,
) -> Preserved {
    let api = b1::recorder(fixture).await;
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let _cli = fixture.cold_cli();
    let prepared = parts(
        &fixture.shared,
        &ProviderKind::Codex,
        None,
        Some(Arc::clone(&output.http)),
    );
    prepared.gate.acquired();
    let readiness = host::process_readiness();
    let mut handles = host::start_managed(
        ShadowProvider::Codex,
        true,
        cutover::boot_ownership(),
        || prepared,
    );
    assert_eq!(handles.len(), 1);
    let result = std::panic::AssertUnwindSafe(async {
        eventually(
            || readiness.is_ready(CANARY_CHANNEL) && readiness.accepts(CANARY_CHANNEL),
            "the original N1 writer to resume",
        )
        .await;
        native_output::terminal(
            &fixture.old_source.path,
            "N1-before-clear",
            FIRST_INCARNATION_REPLY,
        );
        eventually(
            || {
                output
                    .texts()
                    .iter()
                    .any(|text| text.contains(FIRST_INCARNATION_REPLY))
            },
            "the actual N1 response POST before clear",
        )
        .await;
        let bytes = fs::read(&fixture.old_source.path).unwrap();
        eventually(
            || {
                let channel = store.open_channel(era, CANARY_CHANNEL).unwrap().unwrap();
                channel.ledger().unresolved().is_none()
                    && channel
                        .cursor(&fixture.old_source)
                        .is_some_and(|cursor| cursor.captured_through == bytes.len() as u64)
            },
            "N1 response ledger and cursor settlement",
        )
        .await;
        let current = store.open_channel(era, CANARY_CHANNEL).unwrap().unwrap();
        let cursor = current.cursor(&fixture.old_source).unwrap().clone();
        let rotation = current.rotation().unwrap();
        let key = source_key(&fixture.old_source);
        let channel_dir = fixture
            .root
            .path()
            .join("o_store")
            .join(CANARY_CHANNEL.to_string());
        let spool = source_spool(&channel_dir, &key);
        drop(current);
        crate::services::discord::commands::clear_channel_session_state(
            &api.http,
            &fixture.shared,
            &ProviderKind::Codex,
            serenity::ChannelId::new(CANARY_CHANNEL),
            "!clear",
            crate::services::discord::commands::SoftClearNotifyMode::Suppress,
        )
        .await
        .unwrap();
        assert!(dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
        let mut old_io = Some(Watch::start(&fixture.old_source.path));
        let context = run_fifo(
            fixture,
            &api,
            output,
            &readiness,
            &mut old_io,
            BeforeInput::InPlaceClear,
            &mut handles,
        )
        .await;
        b1::wait_recovery_custody_settled(fixture, &context, false).await;
        eventually(
            || {
                let channel = store.open_channel(era, CANARY_CHANNEL).unwrap().unwrap();
                channel.ledger().unresolved().is_none() && channel.ledger().next_serial() == 3
            },
            "both N2 actual responses to settle after actor selection install",
        )
        .await;
        assert_ne!(context.execution_nonce, fixture.context.execution_nonce);
        assert!(!dedupe::codex_verified_o_source_allowed(
            CANARY_CHANNEL,
            &fixture.old_source
        ));
        assert_eq!(
            output.texts().len(),
            3,
            "N1 and both actual fresh N2 responses must POST"
        );
        Preserved {
            source: fixture.old_source.clone(),
            cursor,
            link: rotation.links.get(&key).cloned(),
            rotation,
            spool,
            channel_dir,
            bytes,
            watch: old_io.unwrap(),
        }
    })
    .catch_unwind()
    .await;
    if let Some(token) = fixture
        .shared
        .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
        .snapshot()
        .await
        .cancel_token
    {
        token.cancelled.store(true, Ordering::SeqCst);
        b1::wait_idle(fixture).await;
    }
    handles.pop().unwrap().stop_and_join().await.unwrap();
    let preserved = result.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    preserved.assert_preserved(&store.open_channel(era, CANARY_CHANNEL).unwrap().unwrap());
    preserved
}

fn selection_path(fixture: &CanaryFixture) -> PathBuf {
    fixture
        .root
        .path()
        .join("o_store")
        .join(CANARY_CHANNEL.to_string())
        .join("fresh_selection")
}

pub(super) fn assert_selection(fixture: &CanaryFixture) {
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(selection_path(fixture)).unwrap()).unwrap();
    assert_eq!(value["schema"], 1);
    assert_eq!(value["channel"], CANARY_CHANNEL);
    let dedupe::binding_context::SpawnNonceMarker::Known(nonce) =
        dedupe::binding_context::observe_spawn_nonce_marker(CANARY_TMUX)
    else {
        panic!("the real N2 marker must remain known through restart")
    };
    assert_eq!(value["nonce"], nonce);
    assert_eq!(
        value["keys"],
        json!([source_key(&fixture.old_source)]),
        "the durable selection must be the actual actor's single suspended N1 key"
    );
}

pub(super) fn corrupt(fixture: &CanaryFixture, fault: Fault) {
    let path = selection_path(fixture);
    let bytes = fs::read(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let changed = match fault {
        Fault::OtherNonce => {
            value["nonce"] = json!(uuid::Uuid::new_v4().simple().to_string());
            serde_json::to_vec(&value).unwrap()
        }
        Fault::Malformed => b"not a fresh selection".to_vec(),
        Fault::Version => {
            value["schema"] = json!(2);
            serde_json::to_vec(&value).unwrap()
        }
        Fault::Partial => bytes[..bytes.len() / 2].to_vec(),
    };
    fs::write(path, changed).unwrap();
}
