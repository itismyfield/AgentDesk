use super::*;
use crate::services::discord::input_runtime::fence::{Gate, test_health};
use crate::services::discord::placeholder_controller::PlaceholderKey;
use crate::services::provider::ProviderKind;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

static SNAPSHOT_ATTEMPTS: LazyLock<Mutex<HashMap<ChannelId, usize>>> =
    LazyLock::new(Default::default);

pub(in crate::services::discord) fn snapshot_attempt(channel: ChannelId) {
    *SNAPSHOT_ATTEMPTS
        .lock()
        .unwrap()
        .entry(channel)
        .or_default() += 1;
}

fn attempts(channel: ChannelId) -> usize {
    SNAPSHOT_ATTEMPTS
        .lock()
        .unwrap()
        .get(&channel)
        .copied()
        .unwrap_or(0)
}

fn hook(channel: ChannelId, stage: &'static str, work: impl FnOnce() + Send + 'static) {
    assert!(
        queued_card_gate::BOOT_TEST_HOOKS
            .lock()
            .unwrap()
            .insert((channel, stage), Box::new(work))
            .is_none()
    );
}

fn protect_at(shared: &SharedData, channel: ChannelId, stage: &'static str) {
    let provider = shared.provider.clone();
    hook(channel, stage, move || {
        Gate::protect(provider, channel.get()).unwrap();
    });
}

async fn pending<F: Future>(future: &mut std::pin::Pin<Box<F>>) {
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx)))
            .await
            .is_pending(),
        "actual consumer must reach its wait"
    );
}

fn map(shared: &SharedData, channel: ChannelId) -> Vec<(MessageId, MessageId)> {
    let mut rows: Vec<_> = shared
        .queued
        .queued_placeholders
        .iter()
        .filter(|row| row.key().0 == channel)
        .map(|row| (row.key().1, *row.value()))
        .collect();
    rows.sort_unstable();
    rows
}

fn disk_path(shared: &SharedData, channel: ChannelId) -> std::path::PathBuf {
    crate::services::discord::runtime_store::discord_queued_placeholders_root()
        .unwrap()
        .join(shared.provider.as_str())
        .join(&shared.token_hash)
        .join(format!("{}.json", channel.get()))
}

fn disk(shared: &SharedData, channel: ChannelId) -> (Vec<u8>, std::time::SystemTime) {
    let path = disk_path(shared, channel);
    (
        std::fs::read(&path).unwrap(),
        std::fs::metadata(path).unwrap().modified().unwrap(),
    )
}

fn saved(shared: &SharedData, channel: ChannelId, owner: MessageId, card: MessageId) {
    queued_placeholders_store::save_channel_queued_placeholders(
        &shared.provider,
        &shared.token_hash,
        channel,
        &[(owner, card)],
    );
}

fn controller(shared: &SharedData, channel: ChannelId, card: MessageId) -> PlaceholderKey {
    let key = PlaceholderKey {
        provider: shared.provider.clone(),
        channel_id: channel,
        message_id: card,
    };
    shared.ui.placeholder_controller.incarnation(&key);
    assert!(queued_card_gate::boot_controller_attached(shared, &key));
    key
}

#[tokio::test]
async fn install_lock_wait_rechecks_before_snapshot_and_preserves_sibling() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let (channel, sibling) = (ChannelId::new(6_325_710_001), ChannelId::new(6_325_710_002));
    let (owner, card) = (MessageId::new(1), MessageId::new(91));
    seed(&shared, channel, &[owner]).await;
    saved(&shared, channel, owner, card);
    let before = disk(&shared, channel);
    let lock = shared.queued_placeholders_persist_lock(channel);
    let guard = lock.lock().await;
    let stale = std::collections::HashSet::new();
    let mut install = Box::pin(install_restored_queued_placeholders(
        &shared,
        vec![((channel, owner), card)],
        &stale,
    ));
    pending(&mut install).await;
    assert_eq!(attempts(channel), 0);
    // Protection by a different provider still covers this channel.
    let gate = Gate::protect(ProviderKind::Codex, channel.get()).unwrap();
    let _health = test_health::Clear::new(&gate);
    drop(guard);
    assert!(install.await.is_empty());
    assert_eq!(
        attempts(channel),
        0,
        "post-lock guard must prevent snapshot initiation"
    );
    assert!(map(&shared, channel).is_empty());
    assert_eq!(disk(&shared, channel), before);
    seed(&shared, sibling, &[owner]).await;
    assert!(
        install_restored_queued_placeholders(&shared, vec![((sibling, owner), card)], &stale)
            .await
            .is_empty()
    );
    assert_eq!(
        map(&shared, sibling),
        vec![(owner, card)],
        "unprotected sibling must still install"
    );
    assert_eq!(attempts(sibling), 1);
}

#[tokio::test]
async fn install_snapshot_rechecks_all_candidate_shapes_without_persist() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let (owner, other, card, newer) = (
        MessageId::new(1),
        MessageId::new(2),
        MessageId::new(91),
        MessageId::new(92),
    );
    for case in 0..5 {
        let channel = ChannelId::new(6_325_710_010 + case);
        if case != 1 {
            seed(&shared, channel, &[owner]).await;
        }
        if case == 2 {
            shared
                .queued
                .queued_placeholders
                .insert((channel, owner), newer);
        }
        if case == 3 {
            shared
                .queued
                .queued_placeholders
                .insert((channel, other), card);
        }
        saved(&shared, channel, owner, card);
        let before = (map(&shared, channel), disk(&shared, channel));
        let persisted = Arc::new(AtomicUsize::new(0));
        let observed = persisted.clone();
        hook(channel, "install_persist", move || {
            observed.fetch_add(1, Ordering::SeqCst);
        });
        protect_at(&shared, channel, "install_snapshot");
        let live = if case == 4 {
            vec![]
        } else {
            vec![((channel, owner), card)]
        };
        let stale = [channel].into_iter().collect();
        let late = install_restored_queued_placeholders(&shared, live, &stale).await;
        assert_eq!(
            attempts(channel),
            1,
            "real snapshot must finish before protection"
        );
        assert!(
            late.is_empty(),
            "no protected cleanup candidate for case {case}"
        );
        assert_eq!(
            persisted.load(Ordering::SeqCst),
            0,
            "no persist attempt for case {case}"
        );
        assert_eq!((map(&shared, channel), disk(&shared, channel)), before);
        queued_card_gate::BOOT_TEST_HOOKS
            .lock()
            .unwrap()
            .remove(&(channel, "install_persist"));
    }
}

#[tokio::test]
async fn stale_cleanup_lock_wait_holds_rekey_and_idle_delete() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    for (case, queued_owner) in [(0, true), (1, false)] {
        let channel = ChannelId::new(6_325_710_020 + case);
        let (owner, card) = (MessageId::new(1), MessageId::new(91));
        if queued_owner {
            seed(&shared, channel, &[owner]).await;
        }
        saved(&shared, channel, owner, card);
        let before = disk(&shared, channel);
        let key = controller(&shared, channel, card);
        let snapshots = Arc::new(AtomicUsize::new(0));
        let observed = snapshots.clone();
        hook(channel, "gate_snapshot_attempt", move || {
            observed.fetch_add(1, Ordering::SeqCst);
        });
        let recorder = Recorder::default();
        let lock = shared.queued_placeholders_persist_lock(channel);
        let guard = lock.lock().await;
        let cards = [(channel, owner, card)];
        let mut cleanup = Box::pin(delete_stale_queued_placeholder_cards_with(
            &recorder, &shared, &cards,
        ));
        pending(&mut cleanup).await;
        let gate = Gate::protect(shared.provider.clone(), channel.get()).unwrap();
        let _health = test_health::Clear::new(&gate);
        drop(guard);
        cleanup.await;
        assert_eq!(
            snapshots.load(Ordering::SeqCst),
            0,
            "boot post-lock guard must prevent snapshot initiation"
        );
        queued_card_gate::BOOT_TEST_HOOKS
            .lock()
            .unwrap()
            .remove(&(channel, "gate_snapshot_attempt"));
        assert!(
            map(&shared, channel).is_empty(),
            "post-lock boot rekey must be held"
        );
        assert_eq!(disk(&shared, channel), before);
        assert!(recorder.0.lock().unwrap().is_empty());
        assert!(queued_card_gate::boot_controller_attached(&shared, &key));
    }
}

#[tokio::test]
async fn stale_cleanup_snapshot_rechecks_before_rekey() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let (channel, owner, card) = (
        ChannelId::new(6_325_710_030),
        MessageId::new(1),
        MessageId::new(91),
    );
    seed(&shared, channel, &[owner]).await;
    saved(&shared, channel, owner, card);
    let before = disk(&shared, channel);
    protect_at(&shared, channel, "gate_snapshot");
    let recorder = Recorder::default();
    delete_stale_queued_placeholder_cards_with(&recorder, &shared, &[(channel, owner, card)]).await;
    assert!(boot_skip(BootTarget::Channel(
        &shared.provider,
        channel.get()
    )));
    assert!(
        map(&shared, channel).is_empty(),
        "snapshot guard must prevent boot rekey"
    );
    assert_eq!(disk(&shared, channel), before);
    assert!(recorder.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stale_cleanup_delete_boundary_holds_http_and_controller() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let (channel, owner, card) = (
        ChannelId::new(6_325_710_040),
        MessageId::new(1),
        MessageId::new(91),
    );
    let key = controller(&shared, channel, card);
    protect_at(&shared, channel, "delete");
    let recorder = Recorder::default();
    delete_stale_queued_placeholder_cards_with(&recorder, &shared, &[(channel, owner, card)]).await;
    assert!(boot_skip(BootTarget::Channel(
        &shared.provider,
        channel.get()
    )));
    assert!(
        recorder.0.lock().unwrap().is_empty(),
        "held before DELETE must issue zero requests"
    );
    assert!(queued_card_gate::boot_controller_attached(&shared, &key));
}

async fn response_case(status: u16, index: u64, protect: bool) {
    use axum::{Router, http::StatusCode, response::IntoResponse, routing::any};
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(6_325_710_050 + index);
    let (owner, card) = (MessageId::new(1), MessageId::new(91));
    let key = controller(&shared, channel, card);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (seen, entered, latch) = (requests.clone(), started.clone(), release.clone());
    let app = Router::new().fallback(any(
        move |method: axum::http::Method, uri: axum::http::Uri| {
            let (seen, entered, latch) = (seen.clone(), entered.clone(), latch.clone());
            async move {
                seen.lock().unwrap().push((method, uri.path().to_owned()));
                entered.notify_one();
                latch.acquire().await.unwrap().forget();
                (
                    StatusCode::from_u16(status).unwrap(),
                    axum::Json(serde_json::json!({"code": 50013, "message": "fixture failure"})),
                )
                    .into_response()
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let deleter = SerenityStalePlaceholderDeleter { http };
    let cards = [(channel, owner, card)];
    let cleanup = delete_stale_queued_placeholder_cards_with(&deleter, &shared, &cards);
    tokio::pin!(cleanup);
    tokio::select! {
        _ = started.notified() => {},
        _ = &mut cleanup => panic!("DELETE must await its mock response"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => panic!("HTTP request never started"),
    }
    assert!(queued_card_gate::boot_controller_attached(&shared, &key));
    let gate = protect.then(|| Gate::protect(shared.provider.clone(), channel.get()).unwrap());
    let _health = gate.as_ref().map(test_health::Clear::new);
    release.add_permits(1);
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), cleanup).await;
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    result.expect("bounded mock HTTP completion");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "exactly one real HTTP DELETE");
    assert_eq!(requests[0].0, axum::http::Method::DELETE);
    assert!(
        requests[0]
            .1
            .ends_with(&format!("/channels/{channel}/messages/{card}"))
    );
    assert_eq!(
        queued_card_gate::boot_controller_attached(&shared, &key),
        protect,
        "response guard must retain the controller after protection; status={status}"
    );
}

#[tokio::test]
async fn stale_cleanup_success_response_after_protection_keeps_controller() {
    response_case(204, 0, true).await;
}

#[tokio::test]
async fn stale_cleanup_error_response_after_protection_keeps_controller() {
    response_case(403, 1, true).await;
}

#[tokio::test]
async fn stale_cleanup_unprotected_success_and_error_keep_existing_detach_policy() {
    response_case(204, 2, false).await;
    response_case(403, 3, false).await;
}

struct ResponseDeleter {
    started: AtomicUsize,
    protect: Option<ProviderKind>,
    error: bool,
}

impl StalePlaceholderDeleter for ResponseDeleter {
    fn delete<'a>(
        &'a self,
        channel: ChannelId,
        _: MessageId,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            if let Some(provider) = &self.protect {
                Gate::protect(provider.clone(), channel.get()).unwrap();
            }
            if self.error {
                Err("fixture failure".into())
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::test]
async fn boot_teardown_outcomes_and_normal_queue_policy_are_distinct() {
    use queued_card_gate::{BootTeardownOutcome, QueuedCardDisposition};
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let card = MessageId::new(91);
    for case in 0..5 {
        let channel = ChannelId::new(6_325_710_060 + case);
        let key = controller(&shared, channel, card);
        let Some(QueuedCardDisposition::Released(teardown)) =
            queued_card_gate::release_or_rekey_at_boot(&shared, channel, card, &[]).await
        else {
            panic!("unprotected empty queue releases a token");
        };
        let deleter = ResponseDeleter {
            started: AtomicUsize::new(0),
            protect: (case == 1 || case == 2).then(|| shared.provider.clone()),
            error: case == 2 || case == 4,
        };
        if case == 0 {
            Gate::protect(shared.provider.clone(), channel.get()).unwrap();
        }
        match queued_card_gate::teardown_at_boot(&shared, &deleter, teardown).await {
            BootTeardownOutcome::Held => assert_eq!(case, 0),
            BootTeardownOutcome::DetachHeld(result) => {
                assert!(case == 1 || case == 2);
                assert_eq!(result.is_err(), deleter.error);
            }
            BootTeardownOutcome::Finished(result) => {
                assert!(case == 3 || case == 4);
                assert_eq!(result.is_err(), deleter.error);
            }
        }
        assert_eq!(
            deleter.started.load(Ordering::SeqCst),
            usize::from(case != 0)
        );
        assert_eq!(
            queued_card_gate::boot_controller_attached(&shared, &key),
            case < 3
        );
    }
    let channel = ChannelId::new(6_325_710_065);
    let key = controller(&shared, channel, card);
    let QueuedCardDisposition::Released(teardown) =
        queued_card_gate::release_or_rekey(&shared, channel, card, &[]).await
    else {
        panic!("normal queue still releases a token");
    };
    Gate::protect(shared.provider.clone(), channel.get()).unwrap();
    let deleter = ResponseDeleter {
        started: AtomicUsize::new(0),
        protect: None,
        error: true,
    };
    assert!(
        queued_card_gate::teardown_via_deleter(&shared, &deleter, teardown)
            .await
            .is_err()
    );
    assert_eq!(deleter.started.load(Ordering::SeqCst), 1);
    assert!(
        !queued_card_gate::boot_controller_attached(&shared, &key),
        "normal queue retry keeps its existing detach policy even when protected"
    );
}
