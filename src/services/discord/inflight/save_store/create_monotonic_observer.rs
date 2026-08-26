//! Process-local, observe-only continuity witness for real turn creation (#5490).
//!
//! A successful real-turn `O_CREAT|O_EXCL` write stamps the output JSONL length
//! and exact 64 bytes immediately before `turn_start_offset`. A later
//! lower offset is reported only when the current file is at least as long as
//! that stamp and the exact window still matches. This avoids the wrapper
//! `.generation` mtime as a coordinate frame.
//!
//! The witness is process-local, keyed by `(provider, channel)`, and does not
//! cover process restarts, multiple dcserver processes,
//! synthetic/manual rebind creation, or later RMW saves.  A replacement file
//! whose length is at least the stamped length and whose exact proof window is
//! identical is a deterministic alias; the observer may report it because no
//! stronger incarnation proof is available at this boundary.

use super::*;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{LazyLock, Mutex};

const COORDINATE_WINDOW_LEN: u64 = 64;

type WitnessKey = (ProviderKind, u64);

#[derive(Clone, Debug, PartialEq, Eq)]
struct CoordinateWitness {
    turn_start_offset: u64,
    len_at_stamp: u64,
    prior_window: [u8; COORDINATE_WINDOW_LEN as usize],
}

#[derive(Clone, Debug, Default)]
struct WitnessSlot {
    revision: u64,
    witness: Option<CoordinateWitness>,
}

static WITNESSES: LazyLock<Mutex<HashMap<WitnessKey, WitnessSlot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn read_coordinate_witness(path: &Path, turn_start_offset: u64) -> Option<CoordinateWitness> {
    if turn_start_offset < COORDINATE_WINDOW_LEN {
        return None;
    }
    #[cfg(test)]
    test_seams::before_anchor_io();
    let mut file = fs::File::open(path).ok()?;
    let len_at_stamp = file.metadata().ok()?.len();
    if len_at_stamp < turn_start_offset {
        return None;
    }
    let mut prior_window = [0; COORDINATE_WINDOW_LEN as usize];
    file.seek(SeekFrom::Start(turn_start_offset - COORDINATE_WINDOW_LEN))
        .ok()?;
    file.read_exact(&mut prior_window).ok()?;
    Some(CoordinateWitness {
        turn_start_offset,
        len_at_stamp,
        prior_window,
    })
}

fn coordinate_proof_matches(path: &Path, previous: &CoordinateWitness) -> bool {
    read_coordinate_witness(path, previous.turn_start_offset).is_some_and(|current| {
        current.len_at_stamp >= previous.len_at_stamp
            && current.prior_window == previous.prior_window
    })
}

pub(super) fn observe_successful_real_create(state: &InflightTurnState) {
    let (Some(provider), Some(turn_start_offset), Some(output_path)) = (
        state.provider_kind(),
        state.turn_start_offset,
        state.output_path.as_deref(),
    ) else {
        return;
    };
    let key = (provider, state.channel_id);
    let (observed_revision, previous) = {
        let witnesses = WITNESSES.lock().unwrap_or_else(|error| error.into_inner());
        let slot = witnesses.get(&key).cloned().unwrap_or_default();
        (slot.revision, slot.witness)
    };
    let path = Path::new(output_path);

    if let Some(previous) = previous.as_ref()
        && turn_start_offset < previous.turn_start_offset
        && coordinate_proof_matches(path, previous)
    {
        record_inflight_invariant_with_severity(
            false,
            state,
            "turn_start_offset_monotonic",
            "src/services/discord/inflight/save_store/create_monotonic_observer.rs:observe_successful_real_create",
            "real-turn inflight turn_start_offset moved backwards in a proven continuous output coordinate",
            serde_json::json!({
                "previous": previous.turn_start_offset,
                "next": turn_start_offset,
                "len_at_stamp": previous.len_at_stamp,
                "proof_window_start": previous.turn_start_offset - COORDINATE_WINDOW_LEN,
                "proof_window_end": previous.turn_start_offset,
                "output_path": output_path,
                "observe_only": true,
            }),
            ObsSeverity::Warn,
        );
    }

    // File I/O stays outside WITNESSES.  The revision check prevents a stale
    // snapshot from replacing a newer witness if an interleaving occurs.
    let candidate = read_coordinate_witness(path, turn_start_offset);
    let mut witnesses = WITNESSES.lock().unwrap_or_else(|error| error.into_inner());
    let slot = witnesses.entry(key).or_default();
    if slot.revision == observed_revision
        && slot.witness == previous
        && let Some(candidate) = candidate
    {
        slot.revision = slot.revision.wrapping_add(1);
        slot.witness = Some(candidate);
    }
}

#[cfg(test)]
pub(super) mod test_seams {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static SYNC_ERROR: RefCell<Option<std::io::ErrorKind>> = const { RefCell::new(None) };
        static ANCHOR_IO_HOOK: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
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

    pub(in crate::services::discord::inflight) fn before_anchor_io() {
        ANCHOR_IO_HOOK.with(|slot| {
            let Some(mut hook) = slot.borrow_mut().take() else {
                return;
            };
            hook();
            if slot.borrow().is_none() {
                *slot.borrow_mut() = Some(hook);
            }
        });
    }

    pub(in crate::services::discord::inflight) fn set_anchor_io_hook(hook: impl FnMut() + 'static) {
        ANCHOR_IO_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    pub(in crate::services::discord::inflight) fn clear_anchor_io_hook() {
        ANCHOR_IO_HOOK.with(|slot| slot.borrow_mut().take());
    }

    pub(in crate::services::discord::inflight) fn witness_mutex_is_available() -> bool {
        WITNESSES.try_lock().is_ok()
    }

    #[cfg(unix)]
    pub(in crate::services::discord::inflight) fn sidecar_flock_is_held(path: &Path) -> bool {
        crate::services::discord::inflight::second_fd_cannot_take_lock_nonblocking(path)
    }

    pub(in crate::services::discord::inflight) fn clear_key(
        provider: ProviderKind,
        channel_id: u64,
    ) {
        WITNESSES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&(provider, channel_id));
    }

    pub(in crate::services::discord::inflight) fn replace_with_revision(
        provider: ProviderKind,
        channel_id: u64,
        turn_start_offset: u64,
        byte: u8,
    ) {
        let mut witnesses = WITNESSES.lock().unwrap_or_else(|error| error.into_inner());
        let slot = witnesses.entry((provider, channel_id)).or_default();
        slot.revision = slot.revision.wrapping_add(1);
        slot.witness = Some(CoordinateWitness {
            turn_start_offset,
            len_at_stamp: turn_start_offset,
            prior_window: [byte; COORDINATE_WINDOW_LEN as usize],
        });
    }

    pub(in crate::services::discord::inflight) fn witness_offset(
        provider: ProviderKind,
        channel_id: u64,
    ) -> Option<u64> {
        WITNESSES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&(provider, channel_id))
            .and_then(|slot| slot.witness.as_ref())
            .map(|witness| witness.turn_start_offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    static TEST_SERIAL: Mutex<()> = Mutex::new(());

    fn state(channel_id: u64, output: &Path, offset: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-test".to_string()),
            1,
            offset + 10,
            2,
            "turn".to_string(),
            None,
            Some("AgentDesk-codex-observer-5490".to_string()),
            Some(output.display().to_string()),
            None,
            offset,
        )
    }

    fn write_bytes(path: &Path, bytes: &[u8]) {
        let mut file = fs::File::create(path).expect("create output");
        file.write_all(bytes).expect("write output");
        file.sync_all().expect("sync output");
    }

    #[test]
    fn exact_continuous_window_reports_lower_real_create_as_warn() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::TempDir::new().expect("temp");
        let output = temp.path().join("turn.jsonl");
        write_bytes(&output, &vec![b'a'; 512]);
        let channel_id = 54_900_001;
        test_seams::clear_key(ProviderKind::Codex, channel_id);

        observe_successful_real_create(&state(channel_id, &output, 256));
        let (_, events) = invariant_test_capture::capture(|| {
            observe_successful_real_create(&state(channel_id, &output, 128));
        });
        assert_eq!(
            events,
            vec![invariant_test_capture::CapturedInvariant {
                invariant: "turn_start_offset_monotonic",
                severity: ObsSeverity::Warn,
            }]
        );
    }

    #[test]
    fn changed_exact_window_suppresses_lower_coordinate_alert() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::TempDir::new().expect("temp");
        let output = temp.path().join("turn.jsonl");
        write_bytes(&output, &vec![b'a'; 512]);
        let channel_id = 54_900_002;
        test_seams::clear_key(ProviderKind::Codex, channel_id);
        observe_successful_real_create(&state(channel_id, &output, 256));

        let mut replacement = vec![b'a'; 512];
        replacement[192] = b'b';
        write_bytes(&output, &replacement);
        let (_, events) = invariant_test_capture::capture(|| {
            observe_successful_real_create(&state(channel_id, &output, 128));
        });
        assert!(events.is_empty());
    }

    #[test]
    fn same_exact_window_is_the_documented_deterministic_alias() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::TempDir::new().expect("temp");
        let output = temp.path().join("turn.jsonl");
        let bytes = vec![b'a'; 512];
        write_bytes(&output, &bytes);
        let channel_id = 54_900_003;
        test_seams::clear_key(ProviderKind::Codex, channel_id);
        observe_successful_real_create(&state(channel_id, &output, 256));

        let longer_alias = vec![b'a'; 1024];
        write_bytes(&output, &longer_alias);
        let (_, events) = invariant_test_capture::capture(|| {
            observe_successful_real_create(&state(channel_id, &output, 128));
        });
        assert_eq!(
            events.len(),
            1,
            "an exact-window alias is indistinguishable"
        );
    }

    #[test]
    fn unreadable_anchor_preserves_last_proven_witness() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::TempDir::new().expect("temp");
        let output = temp.path().join("turn.jsonl");
        write_bytes(&output, &vec![b'a'; 512]);
        let channel_id = 54_900_009;
        test_seams::clear_key(ProviderKind::Codex, channel_id);
        observe_successful_real_create(&state(channel_id, &output, 256));

        fs::remove_file(&output).expect("make anchor unreadable");
        let (_, unreadable_events) = invariant_test_capture::capture(|| {
            observe_successful_real_create(&state(channel_id, &output, 192));
        });
        assert!(unreadable_events.is_empty());
        assert_eq!(
            test_seams::witness_offset(ProviderKind::Codex, channel_id),
            Some(256),
            "failed anchor I/O must preserve the last proven witness"
        );

        write_bytes(&output, &vec![b'a'; 512]);
        let (_, events) = invariant_test_capture::capture(|| {
            observe_successful_real_create(&state(channel_id, &output, 128));
        });
        assert_eq!(
            events,
            vec![invariant_test_capture::CapturedInvariant {
                invariant: "turn_start_offset_monotonic",
                severity: ObsSeverity::Warn,
            }]
        );
    }

    #[test]
    fn anchor_io_does_not_hold_witness_mutex() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::TempDir::new().expect("temp");
        let output = temp.path().join("turn.jsonl");
        write_bytes(&output, &vec![b'a'; 512]);
        let channel_id = 54_900_004;
        test_seams::clear_key(ProviderKind::Codex, channel_id);
        let hook_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_fired_in_hook = std::sync::Arc::clone(&hook_fired);
        test_seams::set_anchor_io_hook(move || {
            hook_fired_in_hook.store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(
                test_seams::witness_mutex_is_available(),
                "anchor file I/O must stay outside the process witness mutex"
            );
        });
        observe_successful_real_create(&state(channel_id, &output, 256));
        test_seams::clear_anchor_io_hook();
        assert!(
            hook_fired.load(std::sync::atomic::Ordering::SeqCst),
            "the mutex assertion hook must execute"
        );
    }

    #[test]
    fn revision_barrier_keeps_newer_interleaved_witness() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::TempDir::new().expect("temp");
        let output = temp.path().join("turn.jsonl");
        write_bytes(&output, &vec![b'a'; 512]);
        let channel_id = 54_900_005;
        test_seams::clear_key(ProviderKind::Codex, channel_id);
        test_seams::set_anchor_io_hook(move || {
            test_seams::replace_with_revision(ProviderKind::Codex, channel_id, 384, b'z');
        });
        observe_successful_real_create(&state(channel_id, &output, 256));
        test_seams::clear_anchor_io_hook();
        assert_eq!(
            test_seams::witness_offset(ProviderKind::Codex, channel_id),
            Some(384)
        );
    }
}
