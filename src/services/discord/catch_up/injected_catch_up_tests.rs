//! Neither catch-up phase replays a message an injection already put into a pane, from the
//! process table or the provider file; a channel that never injected scans as before.

use super::*;
use crate::services::discord;
use crate::services::discord::inject_disposition::{self, InjectionOutcome, test_support};
use crate::services::turn_orchestrator::{InjectionSettlement, ReserveOutcome};

async fn authorize(shared: &discord::SharedData) {
    let mut settings = shared.settings.write().await;
    settings.owner_user_id = Some(OWNER_ID);
    settings.allow_all_users = true;
}

async fn queued_sources(shared: &discord::SharedData, channel_id: ChannelId) -> Vec<MessageId> {
    discord::mailbox_snapshot(shared, channel_id)
        .await
        .intervention_queue
        .iter()
        .flat_map(|intervention| intervention.source_message_ids.clone())
        .collect()
}

fn on_disk(
    provider: &ProviderKind,
    channel_id: ChannelId,
    message: MessageId,
    outcome: InjectionOutcome,
) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    inject_disposition::record_terminal(provider, channel_id, message, outcome, now_ms)
        .expect("ring write");
}

fn checkpoint(shared: &discord::SharedData, channel_id: ChannelId) -> Option<u64> {
    shared.last_message_ids.get(&channel_id).map(|id| *id)
}

/// Phase 2's capacity defer after `injected`: fills the queue, so the fresh message defers and
/// publishes where the frontier stopped. `also_queued` puts `injected` itself among the entries.
async fn phase2_after(
    shared: &Arc<discord::SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    injected: MessageId,
    also_queued: bool,
) -> (u64, u64) {
    let root = std::path::PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    let bot_id = message_id_with_age(1, Duration::from_secs(300));
    let fresh_id = message_id_with_age(3, Duration::from_secs(30));
    write_checkpoint(&root, provider, channel_id, bot_id.get());
    for index in 0..MAX_INTERVENTIONS_PER_CHANNEL {
        let id = if also_queued && index == 0 {
            injected
        } else {
            MessageId::new(8_300_000_000_000_000_000 + channel_id.get() % 1000 * 100 + index as u64)
        };
        let intervention = queued_intervention(id, index);
        let outcome =
            discord::mailbox_enqueue_intervention(shared, provider, channel_id, intervention);
        assert!(discord::catch_up::catch_up_enqueue_accepted(&outcome.await));
    }
    let api = TestCatchUpApi::new(Vec::new()).with_phase2_messages(vec![
        discord_message(
            channel_id,
            fresh_id,
            HUMAN_ID,
            false,
            "newer unanswered request",
        ),
        discord_message(
            channel_id,
            injected,
            HUMAN_ID,
            false,
            "typed into the busy turn",
        ),
        discord_message(
            channel_id,
            bot_id,
            CURRENT_BOT_ID,
            true,
            "previous bot response",
        ),
    ]);
    run_catch_up_sweep(CatchUpDeps::new(&api, shared, provider)).await;
    let retry = shared.catch_up_retry_pending.get(&channel_id);
    let retry = retry.expect("the capacity-blocked fresh message stays recoverable");
    (retry.checkpoint, bot_id.get())
}

/// Phase 1 skips a message the provider file records as injected, observed or unconfirmed,
/// with nothing in memory (a restart), and the checkpoint passes it.
#[tokio::test(flavor = "current_thread")]
async fn phase1_skips_a_message_the_ring_records_and_passes_it() {
    let root = scoped_runtime_root();
    let shared = discord::make_shared_data_for_tests();
    authorize(&shared).await;
    let provider = ProviderKind::Claude;
    let mut observed = Vec::new();
    for (n, outcome) in [InjectionOutcome::Observed, InjectionOutcome::Unconfirmed]
        .into_iter()
        .enumerate()
    {
        let channel_id = ChannelId::new(5_845_301 + n as u64);
        let message = message_id_with_age(11 + n as u64, Duration::from_secs(60));
        write_checkpoint(root.path(), &provider, channel_id, message.get() - 1);
        on_disk(&provider, channel_id, message, outcome);
        let api = TestCatchUpApi::new(vec![discord_message(
            channel_id,
            message,
            HUMAN_ID,
            false,
            "typed into the busy turn",
        )]);
        run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;
        let queued = queued_sources(&shared, channel_id).await;
        let passed = checkpoint(&shared, channel_id) == Some(message.get());
        observed.push(format!(
            "{outcome:?}: queued={} passed={passed}",
            queued.len()
        ));
    }
    assert_eq!(
        observed,
        [
            "Observed: queued=0 passed=true",
            "Unconfirmed: queued=0 passed=true"
        ]
    );
}

/// Phase 1 counts and skips a message the process table holds after a settled injection, before
/// any write reaches the provider file.
#[tokio::test(flavor = "current_thread")]
async fn phase1_skips_a_message_the_process_table_holds() {
    let root = scoped_runtime_root();
    let shared = discord::make_shared_data_for_tests();
    authorize(&shared).await;
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(5_845_311);
    let message = message_id_with_age(1, Duration::from_secs(60));
    write_checkpoint(root.path(), &provider, channel_id, message.get() - 1);
    let context = discord::queue_persistence_context(&shared, &provider, channel_id);
    let handle = shared.mailboxes.handle(channel_id);
    let reserve = handle.reserve_injection(Some(message), None, context.clone(), None);
    let ReserveOutcome::Reserved(ticket) = reserve.await else {
        panic!("an idle mailbox reserves");
    };
    let settle = InjectionSettlement::Delivered(InjectionOutcome::Observed);
    let _ = handle
        .settle_injected_input(ticket, settle, context, None)
        .await;
    let api = TestCatchUpApi::new(vec![discord_message(
        channel_id,
        message,
        HUMAN_ID,
        false,
        "typed into the busy turn",
    )]);
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;
    let ring = test_support::ring_file(&provider).exists();
    let observed = (
        queued_sources(&shared, channel_id).await.len(),
        checkpoint(&shared, channel_id),
        ring,
        test_support::hits(channel_id),
    );
    assert_eq!(observed, (0, Some(message.get()), false, (1, 0)));
}

/// Phase 2 counts an injected message as passed, unconfirmed included, so its frontier moves
/// past it; a message the queue also holds keeps the queue's open arm and stops the frontier.
#[tokio::test(flavor = "current_thread")]
async fn phase2_passes_an_injected_message_unless_the_queue_also_holds_it() {
    let _root = scoped_runtime_root();
    let shared = discord::make_shared_data_for_tests();
    authorize(&shared).await;
    let provider = ProviderKind::Claude;
    let mut observed = Vec::new();
    for (n, (outcome, also_queued)) in [
        (InjectionOutcome::Unconfirmed, false),
        (InjectionOutcome::Observed, true),
    ]
    .into_iter()
    .enumerate()
    {
        let channel_id = ChannelId::new(5_845_321 + n as u64);
        let injected = message_id_with_age(21 + n as u64, Duration::from_secs(120));
        on_disk(&provider, channel_id, injected, outcome);
        let (retry, bot) =
            phase2_after(&shared, &provider, channel_id, injected, also_queued).await;
        let stop = if retry == injected.get() {
            "past_injected"
        } else if retry == bot {
            "at_bot"
        } else {
            "other"
        };
        observed.push(format!("{outcome:?} queued={also_queued}: {stop}"));
    }
    assert_eq!(
        observed,
        [
            "Unconfirmed queued=false: past_injected",
            "Observed queued=true: at_bot"
        ]
    );
}

/// The hit count is only what the view holds among fetched ids: a scan stopped by an earlier
/// undecided command counts the injected message it never reached.
#[tokio::test(flavor = "current_thread")]
async fn a_hit_is_counted_for_a_fetched_message_the_scan_never_reached() {
    let root = scoped_runtime_root();
    let shared = discord::make_shared_data_for_tests();
    authorize(&shared).await;
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(5_845_331);
    let command = message_id_with_age(1, Duration::from_secs(90));
    let injected = message_id_with_age(2, Duration::from_secs(60));
    write_checkpoint(root.path(), &provider, channel_id, command.get() - 1);
    on_disk(&provider, channel_id, injected, InjectionOutcome::Observed);
    let mut api = TestCatchUpApi::new(vec![
        discord_message(channel_id, command, HUMAN_ID, false, "!clear"),
        discord_message(
            channel_id,
            injected,
            HUMAN_ID,
            false,
            "typed into the busy turn",
        ),
    ]);
    api.current_user_id = None;
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;
    let pending = shared
        .catch_up_retry_pending
        .get(&channel_id)
        .map(|state| state.checkpoint);
    let reached = pending.is_some_and(|checkpoint| checkpoint >= injected.get());
    let observed = (
        test_support::hits(channel_id),
        reached,
        checkpoint(&shared, channel_id),
    );
    assert_eq!(observed, ((1, 0), false, None));
}

const VIRGIN_CHILD: &str = "ADK_INJECTED_VIRGIN_SCAN_CHILD";

/// A provider that never injected reads the absent ring once per phase and builds no table,
/// ring file, lock or log; it runs in its own process so the table check sees only itself.
#[tokio::test(flavor = "current_thread")]
async fn a_scan_without_injections_reads_the_absent_ring_once_per_phase_and_leaves_no_trace() {
    if std::env::var_os(VIRGIN_CHILD).is_none() {
        let module = module_path!().split_once("::").unwrap().1;
        let name =
            "a_scan_without_injections_reads_the_absent_ring_once_per_phase_and_leaves_no_trace";
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("{module}::{name}"),
                "--test-threads=1",
                "--nocapture",
            ])
            .env(VIRGIN_CHILD, "1")
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success() && log.contains("1 passed"), "{log}");
        return;
    }
    let root = scoped_runtime_root();
    let shared = discord::make_shared_data_for_tests();
    authorize(&shared).await;
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(5_845_341);
    let bot_id = message_id_with_age(1, Duration::from_secs(300));
    let fresh = message_id_with_age(2, Duration::from_secs(30));
    write_checkpoint(root.path(), &provider, channel_id, bot_id.get());
    let dir = test_support::ring_file(&provider)
        .parent()
        .unwrap()
        .to_path_buf();
    // The checkpoint writer adds its own lock; only the injected-input ring's files count here.
    let ring_files = || {
        let names = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name());
        let names = names.map(|name| name.to_string_lossy().into_owned());
        names
            .filter(|name| name.starts_with("injected_inputs"))
            .collect::<Vec<_>>()
    };
    let api = TestCatchUpApi::new(vec![discord_message(
        channel_id,
        fresh,
        HUMAN_ID,
        false,
        "after the gap",
    )])
    .with_phase2_messages(vec![
        discord_message(channel_id, fresh, HUMAN_ID, false, "after the gap"),
        discord_message(
            channel_id,
            bot_id,
            CURRENT_BOT_ID,
            true,
            "previous bot response",
        ),
    ]);
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = {
        let buffer = buffer.clone();
        move || CaptureWriter(buffer.clone())
    };
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(writer)
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    {
        let _default = tracing::subscriber::set_default(subscriber);
        run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;
    }
    let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    assert!(
        api.fetch_calls.load(Ordering::Relaxed) >= 2,
        "both phases fetched"
    );
    let quiet = !logs.contains("terminal_view_hits") && !logs.contains("injected-input ring");
    let observed = (
        test_support::reads(&provider),
        test_support::table_built(),
        ring_files().is_empty(),
        quiet,
        queued_sources(&shared, channel_id).await,
    );
    assert_eq!(observed, (2, false, true, true, vec![fresh]), "{logs}");
}

struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
