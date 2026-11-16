//! Resolves an earlier input's hold under the composer lock, right before the next first key.
//! Delivery is only a transcript record of the exact frame; deletion only of the exact frame.
use super::hold_record::{self, CleanupPhase, Held, HoldRecord, SendOutcome, UnsentProof};
use crate::services::claude_tui::busy_inject::{HeldRead, Screen, held_read};
use crate::services::claude_tui::readiness::Reservation;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const PASTE_SETTLE: Duration = Duration::from_secs(2);
/// Longer than a transcript record takes after an Enter; it never decides "undelivered" alone.
const RECEIPT: Duration = Duration::from_secs(15);
const SELF_RETRY: Duration = Duration::from_secs(30);
const UNCLEAR_LIMIT: u32 = 2;

/// What a resolver pass may touch: the gated pane and this hold's one notice row.
pub(crate) trait Ports {
    fn confirmed(&self) -> bool;
    fn capture(&self) -> Option<String>;
    /// Gated backspaces over our own frame; true only when every key was applied.
    fn clear(&self, count: usize) -> bool;
    fn notice(&self, hold_id: &str) -> Result<i64, String>;
}

/// The gated pane a hold's resolver reads, clears and notices through.
pub(crate) struct LivePorts<'a> {
    pub(crate) target: &'a crate::services::session_host::HerdrTarget,
    pub(crate) notice: Option<&'a super::first_input::Notice>,
}

impl Ports for LivePorts<'_> {
    fn confirmed(&self) -> bool {
        self.target.execution_confirmed()
    }
    fn capture(&self) -> Option<String> {
        self.target.capture_ansi(-80)
    }
    fn clear(&self, count: usize) -> bool {
        let plan = [crate::services::claude_tui::input::TuiInputAction::Backspace(count)];
        let run = crate::services::claude_tui::host_input::run_herdr(self.target, &plan, None);
        matches!(run, super::InputRun::Applied)
    }
    fn notice(&self, hold: &str) -> Result<i64, String> {
        self.notice.ok_or("herdr turn: no hold notice outbox")?(hold)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    Delivered,
    Undelivered,
    /// Released after the hold's notice row: the original may or may not have gone.
    Unconfirmed,
    /// A v1 hold over an empty composer.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// No hold is left; `Some` names how this pass released one.
    Clear(Option<Disposition>),
    Kept,
}

#[derive(Default)]
struct Memory {
    /// Reservations touched writes left, by hold; only this process can settle them.
    entrusted: HashMap<String, Reservation>,
    /// When a cleanup phase first read an empty composer, by hold.
    clear_seen: HashMap<String, Instant>,
    /// Unlinked holds whose directory sync failed, by execution.
    unsynced: HashMap<String, ()>,
    /// v1 holds cannot be rewritten: their passes and notice row live here.
    v1: HashMap<String, (u32, bool)>,
}
static MEMORY: LazyLock<Mutex<Memory>> = LazyLock::new(Mutex::default);
fn memory() -> MutexGuard<'static, Memory> {
    MEMORY.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn entrust(hold_id: &str, reservation: Reservation) {
    memory().entrusted.insert(hold_id.to_owned(), reservation);
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn since(at: &str) -> Duration {
    chrono::DateTime::parse_from_rfc3339(at)
        .ok()
        .and_then(|at| {
            (chrono::Utc::now() - at.with_timezone(&chrono::Utc))
                .to_std()
                .ok()
        })
        .unwrap_or(Duration::ZERO)
}

/// The windows run from the write's recorded outcome, or from the hold when none was recorded.
fn windows(record: &HoldRecord) -> (bool, bool) {
    let elapsed = since(record.outcome_at.as_ref().unwrap_or(&record.recorded_at));
    let enter_may = matches!(
        record.send_outcome,
        SendOutcome::InFlight | SendOutcome::EnterSendFailed
    );
    (elapsed >= PASTE_SETTLE, !enter_may || elapsed >= RECEIPT)
}

/// One pass over execution `nonce`'s hold; the caller holds the composer lock.
pub(crate) fn resolve(logical: &str, nonce: &str, ports: &dyn Ports) -> Result<Verdict, String> {
    judge(logical, nonce, ports, &mut None)
}

/// [`resolve`], leaving in `seen` the evidence the pass decided on for the held front's watch.
pub(crate) fn judge(
    logical: &str,
    nonce: &str,
    ports: &dyn Ports,
    seen: &mut Option<Basis>,
) -> Result<Verdict, String> {
    if memory().unsynced.contains_key(nonce) {
        hold_record::ensure_absent_durable(nonce).map_err(|error| {
            tracing::warn!(logical, %error, "herdr turn: released hold not yet durable");
            error
        })?;
        memory().unsynced.remove(nonce);
    }
    let Some(held) = hold_record::read(nonce)? else {
        return Ok(Verdict::Clear(None));
    };
    let capture = ports.capture();
    *seen = Some(basis_of(&held, capture.as_deref()));
    let mine = |record: &HoldRecord| record.logical_key == logical;
    if matches!(&held, Held::V2(record) if !mine(record)) || !ports.confirmed() {
        return Ok(Verdict::Kept);
    }
    let read = capture.map_or(HeldRead::Unread, |c| held_read(&c));
    let record = match (held, &read) {
        (_, HeldRead::Busy) => return Ok(Verdict::Kept),
        (Held::V1 { hold_id }, _) => return legacy(nonce, &hold_id, &read, ports),
        (Held::V2(record), _) => *record,
    };
    if delivered(&record) {
        return release(record, Disposition::Delivered);
    }
    if read == HeldRead::Unread {
        return unclear(record, ports);
    }
    let (settled, receipt) = windows(&record);
    if !settled {
        return Ok(Verdict::Kept);
    }
    match read {
        HeldRead::Empty => empty(record, receipt, ports),
        HeldRead::Text(Some(body))
            if record.screen_exact
                && hold_record::sha256_hex(body.as_bytes()) == record.frame_sha256 =>
        {
            match receipt {
                true => clean(record, &body, ports),
                false => Ok(Verdict::Kept),
            }
        }
        _ => unclear(record, ports),
    }
}

/// Rules 5b and 7: "undelivered" only when our side explains the empty composer.
fn empty(record: HoldRecord, receipt: bool, ports: &dyn Ports) -> Result<Verdict, String> {
    if record.cleanup_phase != CleanupPhase::None {
        let first = *memory()
            .clear_seen
            .entry(record.hold_id.clone())
            .or_insert_with(Instant::now);
        if first.elapsed() < RECEIPT {
            return Ok(Verdict::Kept);
        }
        if record.cleanup_phase == CleanupPhase::Clearing {
            return unconfirmed(record, ports);
        }
        return release(record, Disposition::Undelivered);
    }
    if record.unsent_proof != UnsentProof::None {
        return release(record, Disposition::Undelivered);
    }
    match receipt {
        true => unconfirmed(record, ports),
        false => Ok(Verdict::Kept),
    }
}

/// Rule 8: record clearing with its proof, read again, clear through the gate, confirm empty.
fn clean(mut record: HoldRecord, body: &str, ports: &dyn Ports) -> Result<Verdict, String> {
    if record.unsent_proof == UnsentProof::None {
        record.unsent_proof = UnsentProof::ExactTextSeen;
    }
    record.cleanup_phase = CleanupPhase::Clearing;
    record.cleanup_at = Some(now());
    hold_record::write(&record)?;
    let same = |read: Option<HeldRead>| read == Some(HeldRead::Text(Some(body.to_owned())));
    if !same(ports.capture().map(|c| held_read(&c))) || !ports.clear(body.chars().count()) {
        return Ok(Verdict::Kept);
    }
    if ports.capture().map(|c| held_read(&c)) == Some(HeldRead::Empty) {
        record.cleanup_phase = CleanupPhase::Cleared;
        record.cleanup_at = Some(now());
        hold_record::write(&record)?;
        let mut memory = memory();
        memory
            .clear_seen
            .insert(record.hold_id.clone(), Instant::now());
    }
    Ok(Verdict::Kept)
}

/// Rules 3 and 9: count the pass; at the limit secure the hold's one notice row.
fn unclear(mut record: HoldRecord, ports: &dyn Ports) -> Result<Verdict, String> {
    record.unclear_passes = (record.unclear_passes + 1).min(UNCLEAR_LIMIT);
    if record.unclear_passes >= UNCLEAR_LIMIT && record.notice_intent_at.is_none() {
        record.notice_intent_at = Some(now());
    }
    hold_record::write(&record)?;
    if record.notice_intent_at.is_some() {
        secure_notice(&mut record, ports)?;
    }
    Ok(Verdict::Kept)
}

/// Intent, enqueue under the hold's key, then the durable row record.
fn secure_notice(record: &mut HoldRecord, ports: &dyn Ports) -> Result<bool, String> {
    if record.notice_enqueued_at.is_some() {
        return Ok(true);
    }
    if record.notice_intent_at.is_none() {
        record.notice_intent_at = Some(now());
        hold_record::write(record)?;
    }
    if let Err(error) = ports.notice(&record.hold_id) {
        tracing::warn!(nonce = %record.execution_nonce, %error, "herdr turn: hold notice not enqueued");
        return Ok(false);
    }
    record.notice_enqueued_at = Some(now());
    hold_record::write(record)?;
    Ok(true)
}

/// Rule 7: no proof either way; the notice row first, then the release unit.
fn unconfirmed(mut record: HoldRecord, ports: &dyn Ports) -> Result<Verdict, String> {
    match secure_notice(&mut record, ports)? {
        true => release(record, Disposition::Unconfirmed),
        false => Ok(Verdict::Kept),
    }
}

/// Rules 3, 6 and 9 for a v1 hold: an empty composer releases it, nothing else does.
fn legacy(
    nonce: &str,
    hold_id: &str,
    read: &HeldRead,
    ports: &dyn Ports,
) -> Result<Verdict, String> {
    if *read == HeldRead::Empty {
        memory().v1.remove(hold_id);
        return unlink(nonce, Disposition::Unknown);
    }
    let (passes, noticed) = {
        let mut memory = memory();
        let state = memory.v1.entry(hold_id.to_owned()).or_default();
        state.0 = (state.0 + 1).min(UNCLEAR_LIMIT);
        *state
    };
    if passes >= UNCLEAR_LIMIT && !noticed && ports.notice(hold_id).is_ok() {
        memory().v1.entry(hold_id.to_owned()).or_default().1 = true;
    }
    Ok(Verdict::Kept)
}

/// The release unit: settle this input's own reservation, then unlink durably.
fn release(record: HoldRecord, disposition: Disposition) -> Result<Verdict, String> {
    let entrusted = memory().entrusted.remove(&record.hold_id);
    memory().clear_seen.remove(&record.hold_id);
    // A delivered turn's terminal settles its reservation; any other release settles it here.
    if let Some(reservation) = entrusted.filter(|_| disposition != Disposition::Delivered) {
        reservation.not_sent();
    }
    unlink(&record.execution_nonce, disposition)
}

fn unlink(nonce: &str, disposition: Disposition) -> Result<Verdict, String> {
    tracing::info!(%nonce, ?disposition, "herdr turn: input hold released");
    match super::release_hold(nonce) {
        super::HoldRelease::Released => Ok(Verdict::Clear(Some(disposition))),
        super::HoldRelease::Kept(error) => Err(error),
        super::HoldRelease::NotDurable(error) => {
            memory().unsynced.insert(nonce.to_owned(), ());
            match hold_record::ensure_absent_durable(nonce) {
                Ok(()) => {
                    memory().unsynced.remove(nonce);
                    Ok(Verdict::Clear(Some(disposition)))
                }
                Err(_) => Err(error),
            }
        }
    }
}

/// Rule 2: a user record after the hold's offset in the same file whose body is the frame.
fn delivered(record: &HoldRecord) -> bool {
    use std::io::{BufRead, Seek};
    use std::os::unix::fs::MetadataExt;
    let Ok(mut file) = std::fs::File::open(&record.transcript) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    if record
        .file
        .is_some_and(|file| file != (meta.dev(), meta.ino()))
        || meta.len() < record.offset_before
        || file
            .seek(std::io::SeekFrom::Start(record.offset_before))
            .is_err()
    {
        return false;
    }
    let lines = std::io::BufReader::new(file).lines().map_while(Result::ok);
    lines
        .filter(|line| super::is_turn_prompt(line))
        .any(|line| {
            prompt_bodies(&line)
                .any(|body| hold_record::sha256_hex(body.as_bytes()) == record.frame_sha256)
        })
}

fn prompt_bodies(line: &str) -> impl Iterator<Item = String> {
    let json: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
    let bodies = match json.pointer("/message/content") {
        Some(serde_json::Value::String(text)) => vec![text.clone()],
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text")?.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    bodies.into_iter()
}

/// What a pass decided on; a change in it is the only wake besides the notice self-retry.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Basis {
    read: Option<HeldRead>,
    transcript: Option<u64>,
    windows: (bool, bool, bool),
}

fn basis(nonce: &str, capture: Option<String>) -> Option<Basis> {
    let held = hold_record::read(nonce).ok()??;
    Some(basis_of(&held, capture.as_deref()))
}

fn basis_of(held: &Held, capture: Option<&str>) -> Basis {
    let (transcript, windows) = match held {
        Held::V2(record) => {
            let (settled, receipt) = windows(record);
            let seen = memory().clear_seen.get(&record.hold_id).copied();
            let cleared = seen.is_some_and(|seen| seen.elapsed() >= RECEIPT);
            let length = std::fs::metadata(&record.transcript).ok().map(|m| m.len());
            (length, (settled, receipt, cleared))
        }
        Held::V1 { .. } => (None, (true, true, true)),
    };
    Basis {
        read: capture.map(held_read),
        transcript,
        windows,
    }
}

fn noticed(nonce: &str) -> bool {
    match hold_record::read(nonce) {
        Ok(Some(Held::V2(record))) => record.notice_enqueued_at.is_some(),
        Ok(Some(Held::V1 { hold_id })) => memory().v1.get(&hold_id).is_some_and(|state| state.1),
        _ => true,
    }
}

/// Ready, until the watch wakes or ends, once the evidence differs from what the pass decided
/// on, or per self-retry interval while the notice row is unsecured. A wake only re-runs the pass.
pub(crate) fn watch(
    nonce: String,
    seen: Option<Basis>,
    capture: impl Fn() -> Option<String> + Send + 'static,
) -> crate::services::claude_tui::owner_input::PreparationWatch {
    let (mut ready, armed) = (false, Instant::now());
    Box::new(move || {
        if !ready {
            let current = basis(&nonce, capture());
            ready = current.is_none()
                || current != seen
                || (!noticed(&nonce) && armed.elapsed() >= SELF_RETRY);
        }
        match ready {
            true => Screen::Ready,
            false => Screen::Unknown,
        }
    })
}

/// After a busy injection's receipt, one pass over the hold of the execution it wrote to.
pub(crate) fn after_busy(logical: &str, nonce: &str, ports: &dyn Ports) {
    if !matches!(hold_record::read(nonce), Ok(Some(_))) {
        return;
    }
    let lock = crate::services::claude_tui::composer_lock::with_composer_mutation_lock;
    let verdict = lock(logical, || resolve(logical, nonce, ports));
    tracing::info!(logical, ?verdict, "herdr turn: hold pass after busy input");
}

#[cfg(test)]
#[path = "hold_resolve_tests.rs"]
mod tests;
