from pathlib import Path
import subprocess

def edit(f, a, b):
    p = Path(f)
    s = p.read_text()
    assert s.count(a) == 1, (f, s.count(a), a[:90])
    p.write_text(s.replace(a, b))

base = 'src/services/discord/tmux_watcher/'
edit('src/services/discord/inflight.rs', '''    let root = inflight_runtime_root()?;
    let path = inflight_state_path(&root, provider, channel_id);
    let data = fs::read_to_string(&path).ok()?;
    parse_inflight_state_content(&data).ok()
}
''', '''    load_inflight_state_read_only_result(provider, channel_id).ok().flatten()
}

/// Read without backfills, distinguishing genuine absence from failed observation.
pub(in crate::services::discord) fn load_inflight_state_read_only_result(
    provider: &ProviderKind,
    channel_id: u64,
) -> Result<Option<InflightTurnState>, String> {
    let root = inflight_runtime_root().ok_or("inflight root unavailable")?;
    let path = inflight_state_path(&root, provider, channel_id);
    match fs::read_to_string(path) {
        Ok(data) => parse_inflight_state_content(&data).map(Some).map_err(|e| e.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}
''')
edit(base+'cancel_handoff.rs', '''        let Some(row) = row else {
            return false;
        };
''', '''        let same_episode = match row {
            Some(row) => self.identity.as_ref().is_some_and(|id| id.matches_state(row))
                && self.nonce == row.turn_nonce
                && row.output_path.as_deref() == Some(path),
            None => {
                // Missing projection is not lost custody. Resume only an episode
                // captured while the original source/turn was known, never a fresh
                // rowless read or an identity reconstructed from the successor.
                // Existing publication/receipt/lease gates still decide delivery.
                crate::services::discord::relay_recovery::cohort::enforcement_admits(channel.get())
                    && self.turn.as_ref().and_then(|turn| turn.completion_actor.as_ref())
                        .and_then(std::sync::Weak::upgrade).is_some()
                    && self.turn.as_ref().and_then(|turn| turn.startup_inflight_snapshot.as_ref())
                        .is_some_and(|original| {
                            self.identity.as_ref().is_some_and(|id| id.matches_state(original))
                                && self.nonce.as_deref().is_some_and(|nonce| !nonce.is_empty())
                                && self.nonce == original.turn_nonce
                                && original.provider == provider.as_str()
                                && original.channel_id == channel.get()
                                && original.tmux_session_name.as_deref() == Some(session)
                                && original.output_path.as_deref() == Some(path)
                                && original.turn_start_offset.is_some_and(|start| start < self.offset)
                        })
            }
        };
''')
edit(base+'cancel_handoff.rs', '''            && self
                .identity
                .as_ref()
                .is_some_and(|identity| identity.matches_state(row))
            && self.nonce == row.turn_nonce
            && row.output_path.as_deref() == Some(path)
''', '            && same_episode\n')
edit(base+'cancel_handoff.rs', '''        let row =
            crate::services::discord::inflight::load_inflight_state(&self.provider, channel.get());
        let index = self
            .pending
            .iter()
            .position(|pending| pending.matches(self, shared, channel, row.as_ref()))?;
''', '')
edit(base+'cancel_handoff.rs', '            let pending = self.pending.remove(index);', '''            // Re-read at the incarnation-fenced take, and do not treat an I/O or
            // parse failure as rowless permission. This read cannot backfill a row.
            let row = crate::services::discord::inflight::load_inflight_state_read_only_result(
                &self.provider, channel.get(),
            ).ok()?;
            let index = self.pending.iter()
                .position(|pending| pending.matches(self, shared, channel, row.as_ref()))?;
            let pending = self.pending.remove(index);''')
p=Path(base+'cancel_handoff.rs');p.write_text(p.read_text()+'\n#[path = "cancel_handoff/completion.rs"]\npub(super) mod completion;\n')
Path(base+'cancel_handoff/completion.rs').write_text('''//! Same-process original-actor completion. Custody is not a delivery receipt.
use super::*;
use crate::services::provider::CancelToken;
use crate::services::discord::{inflight, outbound::delivery_record as records, turn_finalizer as finalizer};
use std::sync::Weak;

pub(in crate::services::discord::tmux::tmux_watcher) async fn capture_actor(
    shared: &SharedData, channel: ChannelId, original: Option<&InflightTurnState>,
) -> Option<Weak<CancelToken>> {
    let original = original?;
    let snapshot = shared.mailbox_peek(channel)?.snapshot().await;
    let actor = snapshot.cancel_token?;
    (original.turn_nonce.as_deref().is_some_and(|nonce| !nonce.is_empty())
        && actor.turn_nonce() == original.turn_nonce.as_deref()
        && snapshot.active_user_message_id.map(|id| id.get()) == Some(original.effective_finalizer_turn_id())
        && snapshot.active_request_owner.map(|id| id.get()) == Some(original.request_owner_user_id))
        .then(|| Arc::downgrade(&actor))
}

pub(in crate::services::discord::tmux::tmux_watcher) async fn finish_after_receipt(
    shared: &Arc<SharedData>, channel: ChannelId, provider: &ProviderKind,
    original: Option<&InflightTurnState>, actor: Option<&Weak<CancelToken>>,
    authority: WatcherSourceAuthority, range: (u64, u64),
) {
    let (Some(original), Some(actor)) = (original, actor) else { return; };
    let Some(session) = original.tmux_session_name.as_deref() else { return; };
    let Some(path) = original.output_path.as_deref() else { return; };
    // The ordinary live-row path retains its existing epilogue. Corruption is
    // not absence, and a successor projection must never be borrowed or cleared.
    if !crate::services::discord::relay_recovery::cohort::enforcement_admits(channel.get())
        || !matches!(inflight::load_inflight_state_read_only_result(provider, channel.get()), Ok(None))
        || original.provider != provider.as_str() || original.channel_id != channel.get()
        || original.effective_finalizer_turn_id() == 0
        || original.turn_start_offset != Some(range.0)
        || authority.source_file == crate::services::cluster::stream_relay::SourceFileIdentity::Unavailable
        || authority.generation_mtime_ns == 0
        || authority.generation_mtime_ns != read_generation_file_mtime_ns(session)
        || authority.reset_incarnation != shared.relay_frontier_token(channel).reset_incarnation
        || !std::fs::File::open(path).ok().is_some_and(|file|
            crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(&file) == authority.source_file)
        || recent_turn_stop_for_watcher_range(channel, session, range.0).is_some()
    { return; }
    let source = records::ExactJsonlSourceIdentity {
        provider: provider.as_str().into(), tmux_session_name: session.into(),
        turn_nonce: original.turn_nonce.clone().unwrap_or_default(), range,
        generation_mtime_ns: authority.generation_mtime_ns,
        offset_authority_channel_id: channel.get(), delivery_channel_id: channel.get(),
    };
    // An ACK high-watermark, a newer receipt, or merely retained bytes is not
    // proof. Reuse the existing exact source/destination receipt predicate.
    if !source.is_authoritative() || !records::read_record(provider, channel.get()).is_some_and(|record|
        record.confirmed_deliveries.iter().any(|receipt| receipt.source == source
            && records::confirmed_delivery_receipt_exists(provider, channel, receipt.message_id, &source)))
    { return; }
    let mut claim = finalizer::SyntheticClaimSnapshot::from_row(original);
    claim.recovery_actor = Some(actor.clone());
    shared.turn_finalizer.submit_terminal_with_claim_snapshot(
        finalizer::TurnKey::new(channel, original.effective_finalizer_turn_id(), shared.restart.current_generation)
            .with_episode_nonce(original.turn_nonce.as_deref()),
        provider.clone(), finalizer::TerminalEvent::Complete, finalizer::FinalizeContext::watcher(),
        Some(claim), shared.clone(),
    ).await;
}
''')
# Move the original collector's declarations, preserving their parent visibility.
p=Path(base+'turn_stream_collector.rs');s=p.read_text();a=s.index('#[allow(clippy::large_enum_variant)]');b=s.index('pub(super) async fn collect_turn_stream_until_terminal(')
state=s[a:b].replace('pub(super)', 'pub(in crate::services::discord::tmux::tmux_watcher)')
state=state.replace('    pub(in crate::services::discord::tmux::tmux_watcher) startup_inflight_snapshot: Option<InflightTurnState>,', '    pub(in crate::services::discord::tmux::tmux_watcher) startup_inflight_snapshot: Option<InflightTurnState>,\n    pub(in crate::services::discord::tmux::tmux_watcher) completion_actor: Option<std::sync::Weak<crate::services::provider::CancelToken>>,')
p.write_text(s[:a]+'#[path = "turn_stream_collector/state.rs"]\nmod state;\npub(super) use state::*;\n\n'+s[b:])
q=Path(base+'turn_stream_collector/state.rs');q.parent.mkdir(exist_ok=True);q.write_text('//! State carried by the watcher collector, including cooperative continuation.\nuse super::*;\n\n'+state+'''
impl TurnStreamCollectorContext {
    pub(in crate::services::discord::tmux::tmux_watcher) fn from_poll(
        context: &PollWatcherContext<'_>, controls: &PollWatcherControls<'_>,
        input_fifo_path: &str, turn_result_relayed: bool,
        restored_injected_prompt_message_id: Option<u64>,
    ) -> Self {
        Self {
            http: context.http.clone(), shared: context.shared.clone(), channel_id: context.channel_id,
            watcher_provider: context.watcher_provider.clone(), tmux_session_name: context.tmux_session_name.into(),
            output_path: context.output_path.into(), input_fifo_path: input_fifo_path.into(),
            watcher_thread_channel_id: context.watcher_thread_channel_id,
            cancel: controls.cancel.clone(), paused: controls.paused.clone(), pause_epoch: controls.pause_epoch.clone(),
            turn_delivered: controls.turn_delivered.clone(), last_heartbeat_ts_ms: controls.last_heartbeat_ts_ms.clone(),
            jsonl_notify: controls.jsonl_notify.clone(), dead_marker_notify: controls.dead_marker_notify.clone(),
            turn_result_relayed, restored_injected_prompt_message_id,
        }
    }
}
''')
edit(base+'turn_stream_collector.rs', "    // #3805 P2 (PR-C): this turn's status-panel generation epoch, SEEDED from", '''    let completion_actor = if let Some(turn) = continuation.as_ref() {
        turn.completion_actor.clone()
    } else {
        cancel_handoff::completion::capture_actor(shared, channel_id, startup_inflight_snapshot.as_ref()).await
    };
    // #3805 P2 (PR-C): this turn's status-panel generation epoch, SEEDED from''')
edit(base+'turn_stream_collector.rs', '        startup_inflight_snapshot,\n        this_turn_status_panel_generation,', '        startup_inflight_snapshot,\n        completion_actor,\n        this_turn_status_panel_generation,')
# Extract only initialization, without changing its operations/order.
p=Path('src/services/discord/tmux_watcher.rs');s=p.read_text();q=Path(base+'entry.rs');e=q.read_text();a=s.index('    // Guard against duplicate relay:');b=s.index('    let mut rotation_tick: u32 = 0;',a)
rewind='''    let mut pending_terminal_rewind_seed: Option<RestoredWatcherTurn> = None;
    let mut terminal_rewind_attempt_key: Option<WatcherRewindAttemptKey> = None;
    let mut terminal_rewind_attempts: u8 = 0;
'''
block=s[a:b];assert rewind in block;block=block.replace(rewind,'').replace('let mut watcher_turn_identity =','let watcher_turn_identity =').replace('let mut watcher_turn_nonce =','let watcher_turn_nonce =')
e+='''
/// Restore the original source position and keep the existing regression repairs.
pub(super) fn restore_delivery_position(
    shared: &Arc<SharedData>, channel_id: ChannelId,
    tmux_session_name: &str, output_path: &str,
) -> (Option<crate::services::discord::inflight::InflightTurnIdentity>, Option<String>, Option<u64>, Option<i64>) {
'''+block+'    (watcher_turn_identity, watcher_turn_nonce, last_relayed_offset, last_observed_generation_mtime_ns)\n}\n'
s=s[:a]+'''    let (mut watcher_turn_identity, mut watcher_turn_nonce,
        mut last_relayed_offset, mut last_observed_generation_mtime_ns) =
        entry::restore_delivery_position(&shared, channel_id, &tmux_session_name, &output_path);
'''+rewind+s[b:]
a=s.index('            &TurnStreamCollectorContext {');b=s.index('            TurnStreamCollectorIo {',a)
s=s[:a]+'''            &TurnStreamCollectorContext::from_poll(
                &poll_context, &poll_controls, &input_fifo_path,
                turn_result_relayed, restored_injected_prompt_message_id,
            ),
'''+s[b:]
a=s.index('    // E5 (#2412):');b=s.index('    // #1134:',a);block=s[a:b]
e+='''
/// Cache the supervisor producer exactly once when the watcher attaches.
pub(super) fn relay_producer(session: &str) -> (Arc<crate::services::cluster::relay_producer_registry::RelayProducerRegistry>, Option<RelayProducer>) {
'''+block.replace('let mut cached_relay_producer','let cached_relay_producer').replace('get_producer(&tmux_session_name)','get_producer(session)')+'    (producer_registry, cached_relay_producer)\n}\n'
s=s[:a]+'    let (producer_registry, mut cached_relay_producer) = entry::relay_producer(&tmux_session_name);\n\n'+s[b:]
a=s.index('    // #2441 (H1)');b=s.index('    let poll_context',a);block=s[a:b]
e+='''
/// Keep notification watchers alive for the entire reader, not one poll.
pub(super) fn source_notifications(output_path: &str, session: &str) -> (
    crate::services::discord::jsonl_watcher::JsonlWatcher,
    crate::services::discord::jsonl_watcher::JsonlWatcher,
) {
'''+block.replace('    let jsonl_notify = jsonl_watcher.notify();\n','').replace('    let dead_marker_notify = dead_marker_watcher.notify();\n','').replace('from(&output_path)','from(output_path)').replace('session_dead_marker_path(&tmux_session_name)','session_dead_marker_path(session)')+'    (jsonl_watcher, dead_marker_watcher)\n}\n'
s=s[:a]+'''    let (jsonl_watcher, dead_marker_watcher) = entry::source_notifications(&output_path, &tmux_session_name);
    let jsonl_notify = jsonl_watcher.notify();
    let dead_marker_notify = dead_marker_watcher.notify();
'''+s[b:];p.write_text(s);q.write_text(e)
edit(str(p), '            startup_inflight_snapshot,\n            this_turn_status_panel_generation,', '            startup_inflight_snapshot,\n            completion_actor,\n            this_turn_status_panel_generation,')
edit(str(p), '                    expected_turn: inflight_before_relay.as_ref(),', '                    expected_turn: startup_inflight_snapshot.as_ref(),')
edit(str(p), '        let relay_suppressed = relay_decision.suppressed;', '''        if relay_ok {
            cancel_handoff::completion::finish_after_receipt(
                &shared, channel_id, &watcher_provider, startup_inflight_snapshot.as_ref(),
                completion_actor.as_ref(), source_authority,
                (turn_data_start_offset, terminal_event_consumed_offset(current_offset, &all_data)),
            ).await;
        }
        let relay_suppressed = relay_decision.suppressed;''')
# New variants retain all four original native witnesses.
f=base+'streaming_status_tick/native_collector_tests.rs'
edit(f, '    row.provider = fx.provider.as_str().to_owned();', '''    row.provider = fx.provider.as_str().to_owned();
    fx.tmux = format!("native-5927-{}", uuid::Uuid::new_v4().simple());
    row.tmux_session_name = Some(fx.tmux.clone());''')
names=['rowless_cancelled_native_body_reaches_exact_receipt_without_append', 'rowless_cancelled_native_terminal_preserves_exact_ack_at_eof', 'rowless_cancelled_native_split_utf8_preserves_original_decoder']
wrappers=''.join('#[test]\nfn '+n+'() {\n    native_collector_case("'+n+'", '+str(i+4)+');\n}\n\n' for i,n in enumerate(names))
edit(f,'fn native_collector_case(test_name: &str, cancellation: u8) {',wrappers+'''fn native_collector_case(test_name: &str, mode: u8) {
    let rowless = mode >= 4;
    let cancellation = if rowless { mode - 3 } else { mode };''')
edit(f,'                cancel_handoff::interrupted_adoption_tests::assert_admission_fences(','''                if rowless {
                    cancel_handoff::interrupted_adoption_tests::remove_projection_without_granting_delivery(
                        &ctx, &mut custody, &row,
                    );
                }
                cancel_handoff::interrupted_adoption_tests::assert_admission_fences(''')
edit(f, '                expected_turn: before_relay.as_ref(),','                expected_turn: turn.startup_inflight_snapshot.as_ref(),')
edit(f, '        let before_relay = load_inflight_state(&fx.provider, fx.channel.get());','''        if rowless {
            cancel_handoff::completion::finish_after_receipt(
                &shared, fx.channel, &fx.provider, turn.startup_inflight_snapshot.as_ref(),
                turn.completion_actor.as_ref(), source_authority, (0, offset),
            ).await;
            assert!(shared.mailbox(fx.channel).has_active_turn().await, "no receipt: no actor release");
        }
        let before_relay = load_inflight_state(&fx.provider, fx.channel.get());''')
edit(f,'        drop(visible);\n        handle.shutdown().await;', '''        drop(visible);
        if rowless {
            assert!(load_inflight_state(&fx.provider, fx.channel.get()).is_none(), "custody must not recreate the row");
            cancel_handoff::completion::finish_after_receipt(
                &shared, fx.channel, &fx.provider, turn.startup_inflight_snapshot.as_ref(),
                turn.completion_actor.as_ref(), source_authority, (0, offset),
            ).await;
            assert!(!shared.mailbox(fx.channel).has_active_turn().await, "exact receipt releases original actor");
            let successor = Arc::new(crate::services::provider::CancelToken::new());
            assert!(crate::services::discord::mailbox_try_start_turn(
                &shared, fx.channel, successor.clone(), serenity::UserId::new(row.request_owner_user_id),
                serenity::MessageId::new(row.user_msg_id + 1),
            ).await, "next input can start without a forced clear");
            cancel_handoff::completion::finish_after_receipt(
                &shared, fx.channel, &fx.provider, turn.startup_inflight_snapshot.as_ref(),
                turn.completion_actor.as_ref(), source_authority, (0, offset),
            ).await;
            assert!(Arc::ptr_eq(&shared.mailbox(fx.channel).snapshot().await.cancel_token.unwrap(), &successor), "old completion cannot release successor");
        }
        handle.shutdown().await;''')
p=Path(base+'cancel_handoff/interrupted_adoption_tests.rs');p.write_text(p.read_text()+'''
/// A corrupt projection is NOT a missing projection. Test both before removal.
pub(in crate::services::discord) fn remove_projection_without_granting_delivery(
    ctx: &TurnStreamCollectorContext,
    custody: &mut Custody,
    original: &InflightTurnState,
) {
    let path = crate::services::discord::inflight::inflight_runtime_root().unwrap()
        .join(ctx.watcher_provider.as_str()).join(format!("{}.json", ctx.channel_id.get()));
    let frontier = ctx.shared.committed_relay_offset(ctx.channel_id);
    std::fs::write(&path, b"{broken").unwrap();
    assert!(custody.take_for_current(&ctx.shared, ctx.channel_id).is_none());
    assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
    let mut foreign = original.clone();
    foreign.turn_nonce = Some("rowless-foreign-successor".into());
    std::fs::write(&path, serde_json::to_vec(&foreign).unwrap()).unwrap();
    assert!(custody.take_for_current(&ctx.shared, ctx.channel_id).is_none());
    std::fs::remove_file(path).unwrap();
    assert_eq!(ctx.shared.committed_relay_offset(ctx.channel_id), frontier);
}
''')
# Isolate process-global tracing/runtime map tests; their assertions are unchanged.
for f,n,v in [('src/services/discord/runtime_store.rs','site_a_emits_one_detailed_record_for_counter_read_failure','ADK_5927_GENERATION_CAPTURE_CHILD'),('src/services/discord/session_relay_sink/delivery_orchestration_tests.rs','relay_deliver_preserves_tail_anchor_and_observes_persisted_proof','ADK_5927_RELAY_FIXTURE_CHILD')]:
    p=Path(f);s=p.read_text();a=s.index('{',s.index('fn '+n+'()'))+1
    code='''
if std::env::var_os("VAR").is_none() {
    let qualified = format!("{}::NAME", module_path!().split_once("::").unwrap().1);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &qualified, "--nocapture"])
        .env("VAR", "1").output().unwrap();
    assert!(output.status.success(), "child failed: {}\\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"), "child must execute this exact test");
    return;
}
'''.replace('VAR',v).replace('NAME',n)
    indent='        ' if 'runtime_store' in f else '    '
    p.write_text(s[:a]+'\n'+''.join(indent+x+'\n' for x in code.strip().splitlines())+s[a:])
# Undo the illegal frozen-baseline change, not the gate.
p=Path('scripts/audit_maintainability_giant_baseline.toml')
p.write_bytes(subprocess.check_output(['git','show','23eb9bba132ad71f0336645705ab8597a59d1c1c:'+str(p)]))
p=Path('scripts/lib_test_inventory_manifest.txt');s=p.read_text();header,rows=s.split('[tests]\n');prefix='services::discord::tmux::tmux_watcher::streaming_status_tick::committed_progress_tests::native_collector_tests::';ids=set(rows.splitlines());ids.update(prefix+n for n in names);p.write_text(header+'[tests]\n'+'\n'.join(sorted(ids))+'\n')
