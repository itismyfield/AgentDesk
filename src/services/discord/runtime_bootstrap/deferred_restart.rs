use super::*;

use super::gateway_lease_recovery::{
    TerminalProof, publish_restart_terminal, restart_file_nonce, restart_request_artifact_path,
    terminal_proof,
};

pub(super) struct DeferredRestartPermit;

pub(super) fn restart_request_matches(root: &std::path::Path, name: &str, nonce: &str) -> bool {
    std::fs::read_to_string(root.join(name))
        .ok()
        .and_then(|request| {
            request
                .lines()
                .find_map(|line| line.strip_prefix("nonce="))
                .map(str::to_owned)
        })
        .as_deref()
        == Some(nonce)
}

/// Rolls back a restart cycle if its request was cancelled or its task is
/// dropped before its request has been superseded. The nonce prevents an old
/// poller from restoring admission for a newer restart request.
pub(super) struct DeferredRestartCancellationGuard {
    shared: Arc<SharedData>,
    root: std::path::PathBuf,
    nonce: String,
    armed: bool,
}

impl DeferredRestartCancellationGuard {
    pub(super) fn new(shared: Arc<SharedData>, root: std::path::PathBuf, nonce: String) -> Self {
        Self {
            shared,
            root,
            nonce,
            armed: true,
        }
    }

    pub(super) fn cancelled(&self) -> bool {
        restart_request_matches(&self.root, "restart_cancelled", &self.nonce)
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

/// #5254 ERRATUM §E8.1 as widened by §E8.5: has this process passed the point of
/// no return at all? D4④ makes commit and abort exclusive process-wide, so *any*
/// latched nonce — not only `nonce` — forbids a rollback: a committed process is
/// already bound for `exit(0)`, and unfencing on another request's behalf is what
/// reopens that window. The latch is a fast-path cache, not the authority: rename
/// and `latch_commit` are two calls, so a task preempted between them reads
/// `None` from a process that has already committed. The filesystem owns the
/// point of no return, so a `None` latch re-reads the identity artifact under
/// S1's authority — `Proven` only, the identity name present with its body nonce
/// agreeing, exactly as `terminal_proof` decides it everywhere else.
///
/// This narrows; it does not close. The residue is the stat↔rename race D4⑤
/// already accepts for its closest-practical pre-check; a mutex spanning the
/// commit is the direction §12-5 rejected. §E8.4: the re-read also carries §11
/// item 10's nonce-uniqueness load — a reissued nonce lets a past request's
/// artifact suppress this one's rollback — and that stays open until S3a.
pub(super) fn restart_commit_is_proven(root: &std::path::Path, nonce: &str) -> bool {
    committed_nonce().is_some() || terminal_proof(root, nonce) == TerminalProof::Proven
}

impl Drop for DeferredRestartCancellationGuard {
    fn drop(&mut self) {
        // #5254 D4④ + §E8.1/§E8.5: commit and abort are exclusive from the point
        // of no return onward, and the helper above — not the latch alone — is
        // what says we are past it. Returning early is what disarming means here.
        if !self.armed || restart_commit_is_proven(&self.root, &self.nonce) {
            return;
        }
        if self.cancelled() || restart_request_matches(&self.root, "restart_pending", &self.nonce) {
            rollback_deferred_restart(&self.shared);
        }
    }
}

/// Publish the admission fence before health can acknowledge the marker. The
/// per-provider CAS gives exactly one poller permission to wait, persist, and
/// consume that provider's shutdown-barrier slot.
pub(super) fn begin_deferred_restart(shared: &SharedData) -> Option<DeferredRestartPermit> {
    shared.restart.intake_worker_lifecycle.fence_admission();
    shared.restart.shutting_down.store(true, Ordering::SeqCst);
    shared
        .restart
        .shutdown_counted
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .ok()
        .map(|_| DeferredRestartPermit)
}

pub(super) async fn prepare_deferred_restart(
    shared: &Arc<SharedData>,
    root: &std::path::Path,
    nonce: String,
) -> Option<(DeferredRestartPermit, DeferredRestartCancellationGuard)> {
    let permit = begin_deferred_restart(shared)?;
    let guard = DeferredRestartCancellationGuard::new(shared.clone(), root.to_path_buf(), nonce);
    shared
        .restart
        .intake_worker_lifecycle
        .wait_until_drained()
        .await;
    if guard.cancelled() {
        return None;
    }
    // `restart_pending` is the health-visible acknowledgement consumed by the
    // wrapper. Publish it only after an accepted tick has fully executed.
    shared.restart.restart_pending.store(true, Ordering::SeqCst);
    Some((permit, guard))
}

pub(super) fn finish_deferred_restart(shared: &SharedData, _permit: DeferredRestartPermit) -> bool {
    let is_final = shared
        .restart
        .shutdown_remaining
        .fetch_sub(1, Ordering::AcqRel)
        == 1;
    shared
        .restart
        .shutdown_slot_consumed
        .store(true, Ordering::Release);
    is_final
}

/// Make the final cancellation decision at the closest practical point before
/// publishing, then publish. The rename inside `publish_restart_terminal` is the
/// point of no return and latches this nonce, so `Ok(true)` means committed:
/// #5254 D4③ retired the post-rename re-check that used to withdraw the
/// acknowledgement it had just published, because a third party removing our
/// marker must not be able to retract a durable terminal artifact.
pub(super) fn commit_deferred_restart_sentinel(
    root: &std::path::Path,
    provider: &ProviderKind,
    nonce: &str,
    guard: &DeferredRestartCancellationGuard,
) -> std::io::Result<bool> {
    if guard.cancelled() || !restart_request_matches(root, "restart_pending", nonce) {
        return Ok(false);
    }
    let body = format!(
        "nonce={nonce}\nprovider={}\ncommitted_at={}\n",
        provider.as_str(),
        chrono::Utc::now().to_rfc3339()
    );
    publish_restart_terminal(root, nonce, &body)?;
    Ok(true)
}

/// #5254 D4② / D11-4: dispose request `nonce`'s marker, never another's. The
/// canonical lease carries no nonce so it needs a CAS; the identity name *is*
/// the authority (I2d), so its unlink needs neither body check nor CAS — no
/// other request can spell that name — and it repeats harmlessly.
pub(super) fn identity_safe_dispose_restart_marker(root: &std::path::Path, nonce: &str) {
    let disposed = format!(".restart_pending.dispose.{}", uuid::Uuid::new_v4());
    if std::fs::rename(root.join("restart_pending"), root.join(&disposed)).is_ok() {
        restore_foreign_disposed_marker(root, &disposed, nonce);
    }
    if let Some(identity) = restart_request_artifact_path(root, "restart_pending", nonce) {
        let _ = std::fs::remove_file(identity);
    }
}

/// The CAS half of D11-4 step (1). Removing the disposed file has to be earned:
/// the body is ours, or the canonical name was restored from it. Neither holds
/// once a third request re-created the canonical name — `hard_link` fails with
/// `EEXIST` — so the lower bound keeps that inode for forensics only. It buys
/// no reachability: `.dispose.*` has no reader and no automatic successor
/// (§12-7). The identity name is what makes the request in the middle reachable,
/// and what will name it `restart-lease-lost` once S3a/S4 land.
pub(super) fn restore_foreign_disposed_marker(root: &std::path::Path, disposed: &str, nonce: &str) {
    let path = root.join(disposed);
    if restart_request_matches(root, disposed, nonce)
        || std::fs::hard_link(&path, root.join("restart_pending")).is_ok()
    {
        let _ = std::fs::remove_file(&path);
        return;
    }
    tracing::warn!(
        disposed = %path.display(),
        expected = nonce,
        found = ?restart_file_nonce(root, disposed),
        "restart-dispose-restore-eexist: keeping the disposed marker as recovery residue"
    );
}

fn release_deferred_restart_ownership(shared: &SharedData) {
    shared
        .restart
        .shutdown_counted
        .store(false, Ordering::Release);
    if shared
        .restart
        .shutdown_slot_consumed
        .swap(false, Ordering::AcqRel)
    {
        shared
            .restart
            .shutdown_remaining
            .fetch_add(1, Ordering::AcqRel);
    }
}

pub(super) fn rollback_deferred_restart(shared: &SharedData) {
    shared.restart.intake_worker_lifecycle.unfence_admission();
    shared.restart.shutting_down.store(false, Ordering::SeqCst);
    shared
        .restart
        .restart_pending
        .store(false, Ordering::SeqCst);
    release_deferred_restart_ownership(shared);
}

/// Release only the stale poller's per-provider barrier ownership. A newer
/// restart nonce inherits the process-wide admission fence and restart flags;
/// clearing those here would reopen intake underneath the new owner.
pub(super) fn handoff_superseded_restart(shared: &SharedData) {
    release_deferred_restart_ownership(shared);
}

pub(super) fn restart_request_is_superseded(root: &std::path::Path, nonce: &str) -> bool {
    let marker = root.join("restart_pending");
    marker.exists() && !restart_request_matches(root, "restart_pending", nonce)
}
