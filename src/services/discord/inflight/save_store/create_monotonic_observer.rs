//! Process-local, observe-only continuity witness for real turn births (#5490).
//!
//! A readable output coordinate whose start is at least 64 stamps its canonical
//! path, length, and the exact preceding 64 bytes. A later lower start warns only
//! while that evidence still matches. An unobservable real birth clears the prior
//! witness; synthetic births are excluded. The process-local slot map is bounded;
//! eviction only ends observe-only continuity for the evicted key. Restarts, multiple
//! processes, later RMW saves, and path switches are outside continuity comparison.
//! Same-path replacement preserving the evidence window and at least the stamped
//! length remains an indistinguishable alias.

use super::*;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

const WINDOW_LEN: u64 = 64;
const MAX_WITNESS_SLOTS: usize = 4096;
type WitnessKey = (ProviderKind, u64);

#[derive(Clone, Debug, PartialEq, Eq)]
struct CoordinateWitness {
    path: PathBuf,
    start: u64,
    len: u64,
    prior: [u8; WINDOW_LEN as usize],
}

#[derive(Clone, Debug, Default)]
struct WitnessSlot {
    revision: u64,
    witness: Option<CoordinateWitness>,
}

static WITNESSES: LazyLock<Mutex<HashMap<WitnessKey, WitnessSlot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn read_witness(path: &Path, start: u64) -> Option<CoordinateWitness> {
    if start < WINDOW_LEN {
        return None;
    }
    #[cfg(test)]
    test_seams::before_io();
    let path = fs::canonicalize(path).ok()?;
    let mut file = fs::File::open(&path).ok()?;
    let len = file.metadata().ok()?.len();
    if len < start {
        return None;
    }
    let mut prior = [0; WINDOW_LEN as usize];
    file.seek(SeekFrom::Start(start - WINDOW_LEN)).ok()?;
    file.read_exact(&mut prior).ok()?;
    Some(CoordinateWitness {
        path,
        start,
        len,
        prior,
    })
}

fn proof_matches(path: &Path, previous: &CoordinateWitness) -> bool {
    read_witness(path, previous.start).is_some_and(|current| {
        current.path == previous.path
            && current.len >= previous.len
            && current.prior == previous.prior
    })
}

/// Observe an actually-durable delayed Claude witness. Runtime stamping calls
/// this only for the write-once `None -> Some` transition; idempotent restamps
/// and typed conflicts never double-install. The delayed kind remains distinct
/// from the pathname-based real-birth observer above.
pub(in crate::services::discord::inflight) fn observe_delayed_evidence_install(
    state: &InflightTurnState,
) {
    let Some(evidence) = state.claude_tui_source_evidence.as_ref() else {
        return;
    };
    debug_assert_eq!(
        evidence.witness.kind,
        crate::services::agent_protocol::ClaudeTuiSourceWitnessKind::DelayedPromptBoundary
    );
    tracing::debug!(
        channel_id = state.channel_id,
        generation = evidence.producer_generation,
        device = evidence.device,
        inode = evidence.inode,
        offset = evidence.offset,
        witness_kind = ?evidence.witness.kind,
        "installed durable delayed ClaudeTUI source observer"
    );
    #[cfg(test)]
    test_seams::record_delayed_install();
}

pub(super) fn observe_successful_real_create(state: &InflightTurnState) {
    let Some(provider) = state.provider_kind() else {
        return;
    };
    let key = (provider, state.channel_id);
    let (revision, previous) = {
        let map = WITNESSES.lock().unwrap_or_else(|error| error.into_inner());
        let slot = map.get(&key).cloned().unwrap_or_default();
        (slot.revision, slot.witness)
    };
    let coordinate = state
        .turn_start_offset
        .zip(state.output_path.as_deref())
        .map(|(start, output_path)| (start, output_path, Path::new(output_path)));
    if let (Some(previous), Some((start, output_path, path))) = (previous.as_ref(), coordinate)
        && start < previous.start
        && proof_matches(path, previous)
    {
        record_inflight_invariant_with_severity(
            false,
            state,
            "turn_start_offset_monotonic",
            "inflight/create_monotonic_observer:observe_successful_real_create",
            "real-turn inflight start moved backwards in an evidenced output coordinate",
            serde_json::json!({
                "previous": previous.start,
                "next": start,
                "len_at_stamp": previous.len,
                "output_path": output_path,
                "observe_only": true,
            }),
            ObsSeverity::Warn,
        );
    }
    let candidate = coordinate.and_then(|(start, _, path)| read_witness(path, start));
    let mut map = WITNESSES.lock().unwrap_or_else(|error| error.into_inner());
    let unchanged = map
        .get(&key)
        .map_or(revision == 0 && previous.is_none(), |slot| {
            slot.revision == revision && slot.witness == previous
        });
    if !unchanged {
        return;
    }
    // A cold/pathless first birth has no continuity to invalidate. Do not leave
    // an empty slot for every historical Discord channel.
    if candidate.is_none() && previous.is_none() && !map.contains_key(&key) {
        return;
    }
    if !map.contains_key(&key) && map.len() >= MAX_WITNESS_SLOTS {
        let evicted = map.keys().find(|candidate| **candidate != key).cloned();
        if let Some(evicted) = evicted {
            map.remove(&evicted);
        }
    }
    let slot = map.entry(key).or_default();
    slot.revision = slot.revision.wrapping_add(1);
    slot.witness = candidate;
}

#[cfg(test)]
pub(in crate::services::discord::inflight) mod test_seams {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static SYNC_ERROR: RefCell<Option<std::io::ErrorKind>> = const { RefCell::new(None) };
        static IO_HOOK: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
        static DELAYED_INSTALLS: RefCell<usize> = const { RefCell::new(0) };
    }

    pub(in crate::services::discord::inflight) fn record_delayed_install() {
        DELAYED_INSTALLS.with(|count| *count.borrow_mut() += 1);
    }

    pub(in crate::services::discord::inflight) fn reset_delayed_installs() {
        DELAYED_INSTALLS.with(|count| *count.borrow_mut() = 0);
    }

    pub(in crate::services::discord::inflight) fn delayed_installs() -> usize {
        DELAYED_INSTALLS.with(|count| *count.borrow())
    }

    pub(in crate::services::discord::inflight) fn sync_result(
        file: &fs::File,
    ) -> std::io::Result<()> {
        if let Some(kind) = SYNC_ERROR.with(|slot| slot.borrow_mut().take()) {
            return Err(std::io::Error::from(kind));
        }
        file.sync_all()
    }

    pub(in crate::services::discord::inflight) fn fail_next_sync(kind: std::io::ErrorKind) {
        SYNC_ERROR.with(|slot| *slot.borrow_mut() = Some(kind));
    }

    pub(in crate::services::discord::inflight) fn before_io() {
        IO_HOOK.with(|slot| {
            let pending = { slot.borrow_mut().take() };
            if let Some(mut hook) = pending {
                hook();
                if slot.borrow().is_none() {
                    *slot.borrow_mut() = Some(hook);
                }
            }
        });
    }

    pub(super) fn set_io_hook(hook: impl FnMut() + 'static) {
        IO_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn clear_io_hook() {
        IO_HOOK.with(|slot| slot.borrow_mut().take());
    }

    pub(super) fn clear(provider: ProviderKind, channel: u64) {
        WITNESSES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&(provider, channel));
    }

    pub(super) fn offset(provider: ProviderKind, channel: u64) -> Option<u64> {
        WITNESSES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&(provider, channel))
            .and_then(|slot| slot.witness.as_ref())
            .map(|witness| witness.start)
    }

    pub(super) fn clear_all() {
        WITNESSES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    pub(super) fn slot_count() -> usize {
        WITNESSES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    static SERIAL: Mutex<()> = Mutex::new(());

    fn state(channel: u64, path: &Path, start: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel,
            None,
            7,
            start + 10,
            9,
            "turn".into(),
            None,
            Some("AgentDesk-codex-observer-5490".into()),
            Some(path.display().to_string()),
            None,
            start,
        )
    }

    fn write_bytes(path: &Path, byte: u8, len: usize) {
        let mut file = fs::File::create(path).unwrap();
        file.write_all(&vec![byte; len]).unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn same_path_lower_start_warns() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("turn.jsonl");
        write_bytes(&output, b'a', 512);
        test_seams::clear(ProviderKind::Codex, 54_900_001);
        observe_successful_real_create(&state(54_900_001, &output, 256));
        let (_, events) = invariant_test_capture::capture(|| {
            observe_successful_real_create(&state(54_900_001, &output, 128));
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].invariant, "turn_start_offset_monotonic");
        assert_eq!(events[0].severity, ObsSeverity::Warn);
    }

    #[test]
    fn path_switch_is_silent_and_same_path_alias_warns() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.jsonl");
        let second = temp.path().join("second.jsonl");
        write_bytes(&first, b'a', 512);
        write_bytes(&second, b'a', 512);
        test_seams::clear(ProviderKind::Codex, 54_900_002);
        observe_successful_real_create(&state(54_900_002, &first, 256));
        let (_, switched) = invariant_test_capture::capture(|| {
            observe_successful_real_create(&state(54_900_002, &second, 128));
        });
        assert!(switched.is_empty());
        assert_eq!(
            test_seams::offset(ProviderKind::Codex, 54_900_002),
            Some(128)
        );
        let (_, alias) = invariant_test_capture::capture(|| {
            observe_successful_real_create(&state(54_900_002, &second, 64));
        });
        assert_eq!(alias.len(), 1);

        test_seams::clear(ProviderKind::Codex, 54_900_002);
        observe_successful_real_create(&state(54_900_002, &first, 256));
        let (_, unobservable_switch) = invariant_test_capture::capture(|| {
            observe_successful_real_create(&state(54_900_002, &second, 0));
        });
        assert!(unobservable_switch.is_empty());
        assert_eq!(test_seams::offset(ProviderKind::Codex, 54_900_002), None);
        let (_, return_to_first) = invariant_test_capture::capture(|| {
            observe_successful_real_create(&state(54_900_002, &first, 128));
        });
        assert!(return_to_first.is_empty());
    }

    #[test]
    fn create_paths_observe_real_but_exclude_all_synthetic_shapes() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("turn.jsonl");
        write_bytes(&output, b'a', 512);
        let real = state(54_900_003, &output, 256);
        test_seams::clear(ProviderKind::Codex, real.channel_id);
        save_inflight_state_create_new_in_root(temp.path(), &real).unwrap();
        assert_eq!(
            test_seams::offset(ProviderKind::Codex, real.channel_id),
            Some(256)
        );

        let adopted = crate::services::discord::recovery_engine::manual_rebind::build_external_adopted_inflight_state(
            state(54_900_011, &output, 128),
        );
        assert!(is_synthetic_create_state(&adopted));
        test_seams::clear(ProviderKind::Codex, adopted.channel_id);
        save_inflight_state_create_new_in_root(temp.path(), &adopted).unwrap();
        assert_eq!(
            test_seams::offset(ProviderKind::Codex, adopted.channel_id),
            None
        );

        let lease = crate::services::tui_prompt_dedupe::ExternalInputRelayLease::unassigned(Some(
            54_900_006,
        ));
        let tui = crate::services::discord::tui_prompt_relay::synthetic_start::build_tui_direct_synthetic_inflight_state(
            ProviderKind::Codex,
            crate::services::discord::ChannelId::new(54_900_006),
            crate::services::discord::MessageId::new(54_900_106),
            None,
            "observer prompt",
            "AgentDesk-codex-observer-5490",
            Some(&output),
            128,
            &lease,
            RelayOwnerKind::Watcher,
        );
        assert!(is_synthetic_create_state(&tui));
        test_seams::clear(ProviderKind::Codex, tui.channel_id);
        assert!(save_inflight_state_if_absent_in_root(temp.path(), &tui).unwrap());
        assert_eq!(
            test_seams::offset(ProviderKind::Codex, tui.channel_id),
            None
        );

        #[cfg(unix)]
        {
            let monitor = crate::services::discord::tmux::build_monitor_triggered_inflight_state(
                state(54_900_004, &output, 128),
            );
            assert!(is_synthetic_create_state(&monitor));
            test_seams::clear(ProviderKind::Codex, monitor.channel_id);
            save_inflight_state_create_new_in_root(temp.path(), &monitor).unwrap();
            assert_eq!(
                test_seams::offset(ProviderKind::Codex, monitor.channel_id),
                None
            );

            let watcher = crate::services::discord::tmux::tmux_watcher::liveness::build_watcher_reacquire_inflight_state(
                state(54_900_005, &output, 128),
            );
            assert!(is_synthetic_create_state(&watcher));
            test_seams::clear(ProviderKind::Codex, watcher.channel_id);
            assert!(save_inflight_state_if_absent_in_root(temp.path(), &watcher).unwrap());
            assert_eq!(
                test_seams::offset(ProviderKind::Codex, watcher.channel_id),
                None
            );
        }

        let real_if_absent = state(54_900_007, &output, 192);
        assert!(!is_synthetic_create_state(&real));
        assert!(!is_synthetic_create_state(&real_if_absent));
        assert!(save_inflight_state_if_absent_in_root(temp.path(), &real_if_absent).unwrap());
        assert_eq!(
            test_seams::offset(ProviderKind::Codex, real_if_absent.channel_id),
            Some(192)
        );
    }

    #[test]
    fn observer_runs_after_sync_while_sidecar_lock_is_held() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("turn.jsonl");
        write_bytes(&output, b'a', 512);
        let real = state(54_900_008, &output, 256);
        let row = inflight_state_path(temp.path(), &ProviderKind::Codex, real.channel_id);
        let hook_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired = hook_fired.clone();
        test_seams::set_io_hook(move || {
            fired.store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(matches!(
                second_handle_try_lock(&row),
                Err(std::fs::TryLockError::WouldBlock)
            ));
        });
        save_inflight_state_create_new_in_root(temp.path(), &real).unwrap();
        test_seams::clear_io_hook();
        assert!(hook_fired.load(std::sync::atomic::Ordering::SeqCst));

        let failed = state(54_900_009, &output, 256);
        test_seams::fail_next_sync(std::io::ErrorKind::Other);
        assert!(save_inflight_state_create_new_in_root(temp.path(), &failed).is_err());
        assert_eq!(
            test_seams::offset(ProviderKind::Codex, failed.channel_id),
            None
        );
    }

    #[test]
    fn unobservable_births_do_not_allocate_and_evidenced_slots_are_bounded() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("turn.jsonl");
        write_bytes(&output, b'a', 512);
        test_seams::clear_all();

        observe_successful_real_create(&state(54_900_012, &output, 0));
        assert_eq!(test_seams::slot_count(), 0);

        {
            let mut map = WITNESSES.lock().unwrap();
            for channel in 0..MAX_WITNESS_SLOTS as u64 {
                map.insert((ProviderKind::Codex, channel), WitnessSlot::default());
            }
        }
        observe_successful_real_create(&state(54_900_013, &output, 128));
        assert_eq!(test_seams::slot_count(), MAX_WITNESS_SLOTS);
        assert_eq!(
            test_seams::offset(ProviderKind::Codex, 54_900_013),
            Some(128)
        );
        test_seams::clear_all();
    }

    #[test]
    fn witness_io_is_outside_mutex_and_revision_blocks_stale_install() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("turn.jsonl");
        write_bytes(&output, b'a', 512);
        let channel = 54_900_010;
        test_seams::clear(ProviderKind::Codex, channel);
        test_seams::set_io_hook(move || {
            assert!(WITNESSES.try_lock().is_ok());
            let mut map = WITNESSES.lock().unwrap();
            let slot = map.entry((ProviderKind::Codex, channel)).or_default();
            slot.revision += 1;
            slot.witness = Some(CoordinateWitness {
                path: PathBuf::from("newer"),
                start: 384,
                len: 384,
                prior: [b'z'; WINDOW_LEN as usize],
            });
        });
        observe_successful_real_create(&state(channel, &output, 256));
        test_seams::clear_io_hook();
        assert_eq!(test_seams::offset(ProviderKind::Codex, channel), Some(384));
    }
}
