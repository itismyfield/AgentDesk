use super::*;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

const ROTATION_CHECK_EVERY: u32 = 120; // ~30s at 250ms base cadence

/// Paths already refused this process lifetime, so the skip WARN is one line per
/// file instead of one every `ROTATION_CHECK_EVERY` tick for as long as the watcher
/// runs. The key is the resolved path, and what that buys is exactly one thing:
/// several spellings of one file collapse onto one entry and therefore one line.
///
/// It does not make the key non-UTF-8 safe, which an earlier version of this comment
/// claimed. `output_path` reaches this module as a `&str`, so a path that was not
/// UTF-8 was already put through a lossy conversion upstream and two distinct such
/// paths can arrive here as the same string. A `PathBuf` faithfully keeps whatever it
/// is handed; it cannot recover bytes the function boundary has dropped.
static REFUSED_ROTATION_PATHS: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Ceiling on that set, which nothing ever reads back: past it the further skip
/// WARNs are dropped rather than grown for the rest of the process's life.
const REFUSED_ROTATION_PATHS_CAP: usize = 1024;

#[allow(clippy::too_many_arguments)]
pub(super) async fn rotate_watcher_jsonl_if_due(
    rotation_tick: u32,
    output_path: &str,
    tmux_session_name: &str,
    mut current_offset: u64,
    mut last_relayed_offset: Option<u64>,
    mut last_observed_generation_mtime_ns: Option<i64>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
) -> (u64, Option<u64>, Option<i64>, bool) {
    // Periodic size-cap rotation for the session jsonl. Running this off
    // the watcher loop keeps the wrapper child process simple while
    // still enforcing a 20 MB soft cap (see issue #892).
    let mut rewrote_from_head = false;
    if rotation_tick % ROTATION_CHECK_EVERY == 0 {
        let path = output_path.to_string();
        let session = tmux_session_name.to_string();
        let prev_offset = current_offset;
        let owned_session = session.clone();
        let rotation = tokio::task::spawn_blocking(move || {
            rotate_owned_jsonl(
                &path,
                &owned_session,
                channel_id,
                crate::services::tmux_common::JSONL_SIZE_CAP_BYTES,
                crate::services::tmux_common::JSONL_TARGET_KEEP_BYTES,
            )
        })
        .await
        .unwrap_or_else(|e| Err(format!("join error: {e}")));
        match rotation {
            Ok(Some(new_size)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ✂ rotated jsonl for {} — new size {} bytes (was beyond cap)",
                    session,
                    new_size
                );
                // File was rewritten from the head: reset reader offset
                // so the watcher doesn't seek past the new EOF. Also
                // reset the duplicate-relay guard.
                if prev_offset > new_size {
                    rewrote_from_head = true;
                    current_offset = new_size;
                    last_relayed_offset = Some(new_size);
                    // #1270 codex P2: snapshot the current `.generation`
                    // mtime alongside the local offset so a later regression
                    // check has a real baseline. Without this, the local
                    // mtime would still be `None` after a normal relay path
                    // and any subsequent regression would misclassify
                    // same-wrapper rotation as fresh-respawn and clear the
                    // local offset to None — re-relaying surviving content.
                    last_observed_generation_mtime_ns =
                        Some(read_generation_file_mtime_ns(tmux_session_name));
                    reset_stale_relay_watermark_if_output_regressed(
                        shared,
                        channel_id,
                        tmux_session_name,
                        new_size,
                        "jsonl_rotation",
                    );
                }
            }
            Ok(None) => {}
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!("  [{ts}] ⚠ jsonl rotation failed for {}: {}", session, e);
            }
        }
    }

    (
        current_offset,
        last_relayed_offset,
        last_observed_generation_mtime_ns,
        rewrote_from_head,
    )
}

/// The blocking half of the rotation: refuse any jsonl AgentDesk does not own,
/// then apply the size cap to the ones it does — #5452 PR-A. Ownership, and why
/// only `Owned` may be rewritten, is
/// [`crate::services::tmux_common::classify_watcher_jsonl_owner`]'s contract.
///
/// A refused file reports `Ok(None)`, the same "nothing was rewritten" answer an
/// under-cap file gives, so the caller's offset bookkeeping is untouched: a refusal
/// must not look like a rotation that moved the reader's coordinates. The truncate
/// then runs on the resolved path the verdict came back carrying, never on
/// `output_path` resolved again — re-resolving the caller's spelling would let a
/// link swapped in after the verdict aim the rewrite at another file.
fn rotate_owned_jsonl(
    output_path: &str,
    tmux_session_name: &str,
    channel_id: ChannelId,
    size_cap_bytes: u64,
    target_keep_bytes: u64,
) -> Result<Option<u64>, String> {
    let owner =
        crate::services::tmux_common::classify_watcher_jsonl_owner(output_path, tmux_session_name);
    let Some(target) = owner.rotatable_path() else {
        // Resolving here serves two purposes: it is the WARN's dedupe key, and it is
        // the evidence for whether a line is owed at all. `NotFound` means the entry
        // is not on disk at this instant, which is what a wrapper recreating its own
        // relay jsonl looks like to a tick landing in that gap — the classifier
        // refuses any path it cannot resolve, so the verdict there says nothing about
        // whose file it is, and announcing this session's own jsonl as one AgentDesk
        // may not rewrite would be wrong. Ordering matters as much as the test: the
        // check precedes `first_rotation_refusal`, whose insert is the single warning
        // that path will ever get, so a suppressed line must not spend it. Every other
        // resolution failure still warns, keyed on the caller's spelling.
        let resolved = std::fs::canonicalize(output_path);
        let missing = resolved
            .as_ref()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        let key = resolved.unwrap_or_else(|_| PathBuf::from(output_path));
        if !missing && first_rotation_refusal(key) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                channel_id = channel_id.get(),
                output_path,
                owner = ?owner,
                "  [{ts}] ⏭ jsonl rotation skipped for {tmux_session_name} — this file is not AgentDesk's relay jsonl to rewrite"
            );
        }
        return Ok(None);
    };
    crate::services::tmux_common::truncate_jsonl_head_safe(
        target,
        size_cap_bytes,
        target_keep_bytes,
    )
    .map_err(|e| e.to_string())
}

/// Whether this is the first refusal recorded under `key`, which the caller has
/// already resolved. Best-effort by nature: the set only suppresses logs, so a
/// drifted key costs one duplicate WARN, and a poisoned lock — unreachable, nothing
/// panics holding it — drops the line instead of repeating it every tick.
fn first_rotation_refusal(key: PathBuf) -> bool {
    REFUSED_ROTATION_PATHS
        .lock()
        .map(|mut seen| seen.len() < REFUSED_ROTATION_PATHS_CAP && seen.insert(key))
        .unwrap_or(false)
}

#[cfg(test)]
mod foreign_rotation_ban_tests {
    use super::*;
    use std::path::Path;

    // Small enough to keep the fixtures cheap; the production call site passes
    // the real `JSONL_SIZE_CAP_BYTES` / `JSONL_TARGET_KEEP_BYTES`.
    const CAP: u64 = 200;
    const KEEP: u64 = 100;

    fn oversized_jsonl(path: &Path) -> Vec<u8> {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
        let body: String = (0..40)
            .map(|index| format!("{{\"type\":\"assistant\",\"i\":{index:03}}}\n"))
            .collect();
        assert!(body.len() as u64 > CAP, "fixture must exceed the cap");
        std::fs::write(path, &body).expect("write fixture");
        body.into_bytes()
    }

    fn rotate(path: &str, session: &str) -> Result<Option<u64>, String> {
        rotate_owned_jsonl(
            path,
            session,
            ChannelId::new(1_479_662_682_909_966_490),
            CAP,
            KEEP,
        )
    }

    /// Both directions in one call path, so the gate and the truncate are proven
    /// together: this runtime's own over-cap relay jsonl still shrinks on a line
    /// boundary, while an over-cap transcript under a provider home keeps every byte.
    /// `tmux_common::watcher_jsonl_owner_tests` carries which paths land in which
    /// verdict, both provider homes included; this adds that a refusal really does
    /// leave the bytes alone.
    #[test]
    fn only_this_runtimes_own_relay_jsonl_is_rewritten() {
        let host = crate::config::pin_runtime_host_for_test();

        let session = "AgentDesk-claude-rot-5452-gate";
        let relay = crate::services::tmux_common::session_temp_path(session, "jsonl");
        let relay_before = oversized_jsonl(Path::new(&relay));

        let rotated = rotate(&relay, session).expect("rotation must not error");
        let relay_after = std::fs::read(&relay).expect("read rotated");
        assert_eq!(rotated, Some(relay_after.len() as u64));
        assert!(
            relay_after.len() < relay_before.len(),
            "an over-cap relay jsonl must shrink"
        );
        assert!(
            relay_after.starts_with(b"{\"type\""),
            "rotation must keep the file starting on a line boundary"
        );

        let transcript = host
            .claude_home
            .path()
            .join("projects/-Users-me-repo/0f2b9d1e-0000-4000-8000-000000000000.jsonl");
        let transcript_before = oversized_jsonl(&transcript);
        let refused = rotate(&transcript.display().to_string(), session)
            .expect("a refused rotation is not an error");
        assert_eq!(
            refused, None,
            "a provider transcript must report no rewrite"
        );
        assert_eq!(
            std::fs::read(&transcript).expect("read transcript"),
            transcript_before,
            "not one byte of a provider-owned transcript may change"
        );
    }

    /// The window between the verdict and the rewrite it authorises: the entry
    /// classification judged is relinked at another file before the truncate opens it.
    /// Both halves of `rotate_owned_jsonl` are driven by hand because the swap has to
    /// land between them and the production function offers no seam. Asserted is what
    /// the checks buy: the rewrite refuses, and the entry is still the link somebody
    /// put there.
    ///
    /// What this pins is the pair of fd-identity checks, not either one of them. The
    /// refusal here is reached by the one after the open; revert only that and the
    /// swap is still caught immediately before the rename, so the verdict is
    /// unchanged and all that differs is a staging file written and then dropped.
    /// Reverting both is the mutation this fails on — the rename then resolves the
    /// link's own entry and publishes over it, replacing the link with a regular file
    /// and reporting a rewrite. Deliberately *not* asserted: the victim's own bytes,
    /// which survive either way because the rename lands on the link's entry and
    /// never on the victim's.
    #[cfg(unix)]
    #[test]
    fn a_target_relinked_after_the_verdict_is_refused() {
        let host = crate::config::pin_runtime_host_for_test();

        let session = "AgentDesk-claude-rot-5452-swap";
        let relay = crate::services::tmux_common::session_temp_path(session, "jsonl");
        let _ = std::fs::remove_file(&relay);
        oversized_jsonl(Path::new(&relay));

        // The verdict, reached on the real relay jsonl.
        let target = crate::services::tmux_common::classify_watcher_jsonl_owner(&relay, session)
            .rotatable_path()
            .expect("this session's own relay jsonl is rotatable")
            .to_path_buf();

        // The swap, inside the window that verdict opened.
        let victim = host.root.path().join("someone-elses.jsonl");
        oversized_jsonl(&victim);
        std::fs::remove_file(&target).expect("unlink the judged entry");
        std::os::unix::fs::symlink(&victim, &target).expect("symlink");

        // The rewrite the verdict authorised, now aimed elsewhere.
        let refused = crate::services::tmux_common::truncate_jsonl_head_safe(&target, CAP, KEEP)
            .expect("a refused rotation is not an error");
        assert_eq!(refused, None, "a swapped entry must report no rewrite");
        assert!(
            std::fs::symlink_metadata(&target)
                .expect("the swapped-in entry is still there")
                .is_symlink(),
            "refusing means writing nothing, so the rename must not have replaced the link"
        );
    }

    /// #3277 puts two watcher instances on one relay jsonl, so two rotation attempts
    /// can overlap. A single fixed staging name cleared with `remove_file` makes A
    /// unlink and reuse the sibling B is still filling, and A's rename then publishes
    /// B's half-written inode as the relay jsonl.
    ///
    /// A file parked at the name that scheme used stands in for the other instance's
    /// staging: the rotation must still succeed — residue cannot block a retry, since
    /// every attempt names its own sibling — and must leave that file byte-for-byte,
    /// where the fixed name would unlink it and reuse the name for this attempt's
    /// output. Its freshly written mtime is also what keeps it clear of the stale-
    /// staging sweep, whose whole safety argument is that age floor.
    #[test]
    fn a_concurrent_instances_staging_file_is_neither_reused_nor_removed() {
        let _host = crate::config::pin_runtime_host_for_test();

        let session = "AgentDesk-claude-rot-5452-staging";
        let relay = crate::services::tmux_common::session_temp_path(session, "jsonl");
        let _ = std::fs::remove_file(&relay);
        let relay_before = oversized_jsonl(Path::new(&relay));
        let resolved = std::fs::canonicalize(&relay).expect("relay resolves");

        let mut other_staging = resolved.clone().into_os_string();
        other_staging.push(".truncate.tmp");
        let other_staging = PathBuf::from(other_staging);
        let half_written = b"{\"type\":\"assistant\",\"i\":0".as_slice();
        std::fs::write(&other_staging, half_written).expect("park the other instance's staging");

        let rotated = rotate(&relay, session).expect("rotation must not error");
        assert!(
            rotated.is_some_and(|new_size| new_size < relay_before.len() as u64),
            "the owned relay jsonl must still rotate past parked residue"
        );
        assert_eq!(
            std::fs::read(&other_staging).expect("another instance's staging must survive"),
            half_written,
            "an overlapping instance's staging is neither reused nor unlinked"
        );
    }
}
