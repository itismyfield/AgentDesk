use poise::serenity_prelude::ChannelId;

use super::SharedData;

/// Read the `.generation` marker file mtime in nanoseconds since the unix
/// epoch. Returns 0 when the marker is missing in BOTH the canonical
/// persistent location (`runtime_root()/runtime/sessions/`) and the legacy
/// `/tmp/` fallback supported by `resolve_session_temp_path` (#892
/// migration window). All of those conditions are treated by callers as
/// "fresh wrapper".
///
/// `.generation` is written exactly once per spawn by `claude.rs` after
/// `tmux::create_session` and never touched by the live wrapper, so its
/// mtime uniquely identifies the wrapper instance even when jsonl
/// rotation changes the jsonl inode (#1270). NOTE: this mtime signal is the
/// #1270 wrapper-identity consumer ONLY. The status-panel session-instance
/// key (#3087) no longer reads this mtime — it reads the dedicated
/// `.spawn_nonce` marker content instead (see `session_panel_instance_key`).
pub(in crate::services::discord) fn read_generation_file_mtime_ns(tmux_session_name: &str) -> i64 {
    let Some(path) =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "generation")
    else {
        return 0;
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        return 0;
    };
    let Ok(modified) = meta.modified() else {
        return 0;
    };
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

/// The per-spawn marker-file suffix carrying the status-panel session-INSTANCE
/// nonce (#3087). Distinct from `.generation`, whose mtime is the #1270
/// wrapper-identity signal and whose CONTENT is the runtime generation number
/// parsed by the adoption path (`watchers::lifecycle`). The nonce lives in its
/// own file so neither of those `.generation` consumers is perturbed.
const SPAWN_NONCE_SUFFIX: &str = "spawn_nonce";

/// Write a fresh, globally-unique per-spawn nonce to the `.spawn_nonce` marker.
///
/// Called once at each provider spawn site (claude/codex/qwen) right after
/// `tmux::create_session` stamps `.generation`. The nonce is the stability key
/// the status panel uses to detect a genuine new-session boundary (#3087): it
/// is guaranteed unique per spawn (a v4 UUID — no reliance on filesystem mtime
/// resolution or `fsync` ordering), invariant across every status tick and
/// every TURN of one session (the marker is never rewritten by the live
/// wrapper), and orthogonal to both the runtime generation number and the
/// provider-session id.
///
/// Errors are propagated to the caller (a missing/short write would degrade the
/// panel-reset boundary to best-effort, so it is logged rather than silently
/// swallowed — best-effort writes are exactly what made the earlier mtime key
/// fragile). The `.generation` marker and its mtime are left untouched.
///
/// The write is atomic and stale-safe (#3087 P2): the nonce is written to a
/// sibling temp file and `rename`d over `.spawn_nonce`, so a reader never sees a
/// torn/short nonce. If ANY step fails, the destination is removed before the
/// error returns, so a respawn whose write fails leaves NO readable nonce at all
/// — the instance key degrades to `None` (panel may redundantly reset) rather
/// than reading the PRIOR spawn's stale nonce (which would wrongly SUPPRESS the
/// reset on a genuinely new session). "Absent → None key" is always preferred
/// over "stale → colliding key".
pub(crate) fn write_spawn_nonce(tmux_session_name: &str) -> std::io::Result<String> {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, SPAWN_NONCE_SUFFIX);
    // Distinct per-process temp sibling to avoid clobbering across concurrent
    // spawns; replaced atomically into `path` on success.
    let tmp_path = format!("{path}.tmp.{}", std::process::id());

    let write_then_rename = || -> std::io::Result<()> {
        std::fs::write(&tmp_path, nonce.as_bytes())?;
        std::fs::rename(&tmp_path, &path)
    };

    if let Err(e) = write_then_rename() {
        // Leave NO stale/torn nonce behind: remove both the temp sibling and any
        // pre-existing destination so the key resolves to `None`, never to a
        // prior spawn's nonce.
        let _ = std::fs::remove_file(&tmp_path);
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok(nonce)
}

/// Read the per-spawn nonce from the `.spawn_nonce` marker CONTENT. Returns
/// `None` when the marker is missing/unreadable/empty in BOTH the canonical
/// persistent location and the legacy `/tmp/` fallback. Unlike a missing mtime
/// (which the prior design folded into a `#0` key that COLLIDED across
/// respawns of the same tmux session name), a missing nonce yields `None` so
/// the key is simply unavailable — never a colliding key that would suppress a
/// real reset (#3087 Edge 3).
pub(in crate::services::discord) fn read_spawn_nonce(tmux_session_name: &str) -> Option<String> {
    let path = crate::services::tmux_common::resolve_session_temp_path(
        tmux_session_name,
        SPAWN_NONCE_SUFFIX,
    )?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let nonce = raw.trim();
    if nonce.is_empty() {
        return None;
    }
    Some(nonce.to_string())
}

/// Build the stable session-INSTANCE key the status panel uses to detect a
/// genuine new-session boundary (#3087): `"{tmux_session_name}#{nonce}"`, where
/// `nonce` is the per-spawn `.spawn_nonce` marker CONTENT (a v4 UUID). The nonce
/// is written once per spawn and never touched by the live wrapper, so this key
/// is invariant across every status tick and every TURN of one session, and
/// across the `None`→`Some` provider-session-id assignment — yet it changes on a
/// real respawn (`/clear`, idle-timeout, cancel→respawn, …) because each spawn
/// mints a fresh UUID.
///
/// Returns `None` when `tmux_session_name` is blank OR no readable nonce exists
/// yet (headless / pre-spawn / unreadable marker). Crucially, a missing nonce
/// yields `None` rather than a `{name}#0` key: a `None` key is gated NOT to
/// reset on the `None`→`Some` availability transition (so no false reset) AND
/// does not collide across respawns of the same name (so no suppressed real
/// reset — the provider-session delta remains the secondary boundary). This is
/// the codex Edge-3/Edge-4 fix: switching from mtime (non-unique, missing→`#0`
/// collision) to a per-spawn nonce (unique, missing→`None`).
pub(in crate::services::discord) fn session_panel_instance_key(
    tmux_session_name: &str,
) -> Option<String> {
    let name = tmux_session_name.trim();
    if name.is_empty() {
        return None;
    }
    let nonce = read_spawn_nonce(name)?;
    Some(format!("{name}#{nonce}"))
}

/// Rewrite a file's contents while preserving its prior modified time. Used
/// by the adoption path to refresh the `.generation` marker payload (so the
/// generation number on disk matches the current dcserver runtime) without
/// changing the file's mtime — the mtime is the wrapper-identity signal that
/// the regression resolver uses to distinguish "same wrapper, mid-flight
/// rotation" from "fresh wrapper after cancel→respawn" (see
/// `watermark_after_output_regression`). Adoption changes the runtime that
/// owns the wrapper, but it does NOT respawn the wrapper itself, so the
/// identity signal must stay pinned.
///
/// Failures are logged and swallowed: the worst case is a redundant fresh-
/// wrapper reset on a restored offset, which is the same behaviour the
/// codebase had before #1271. Returning an error would not unblock the
/// adoption.
pub(super) fn preserve_mtime_after_write(path: &str, content: &[u8], context: &str) {
    let prior_mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
    if let Err(e) = std::fs::write(path, content) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ preserve_mtime_after_write: failed to write {} (context={}, error={})",
            path,
            context,
            e
        );
        return;
    }
    let Some(prior) = prior_mtime else {
        // No prior mtime to preserve (file did not exist or metadata unavailable).
        // The post-write mtime is the only baseline we have, which is the same
        // outcome as before this helper existed.
        return;
    };
    let times = std::fs::FileTimes::new().set_modified(prior);
    let file = match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ preserve_mtime_after_write: failed to reopen {} for set_times (context={}, error={})",
                path,
                context,
                e
            );
            return;
        }
    };
    if let Err(e) = file.set_times(times) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ preserve_mtime_after_write: set_times failed for {} (context={}, error={})",
            path,
            context,
            e
        );
    }
}

/// Decide what watermark a stale-output regression (current EOF lower than
/// `confirmed`) should land on, based on whether the wrapper instance is
/// the same one that advanced the watermark in the first place.
///
/// - Same wrapper (`.generation` mtime unchanged): mid-flight rotation
///   (`truncate_jsonl_head_safe` rename). The byte stream beyond the
///   surviving content is genuinely new, so we pin to `observed_output_end`
///   to avoid re-relaying surviving content (PR #1256 intent).
/// - Different wrapper (mtime changed, mtime missing, or first observation
///   with stored mtime == 0): cancel→respawn or any fresh spawn. The
///   current file is fully new content — reset to 0 so the watcher walks
///   it from the beginning (#1270 regression fix).
pub(super) fn watermark_after_output_regression(
    stored_generation_mtime_ns: i64,
    current_generation_mtime_ns: i64,
    observed_output_end: u64,
) -> u64 {
    let same_wrapper = stored_generation_mtime_ns != 0
        && stored_generation_mtime_ns == current_generation_mtime_ns;
    if same_wrapper { observed_output_end } else { 0 }
}

pub(super) fn reset_stale_relay_watermark_if_output_regressed(
    shared: &SharedData,
    channel_id: ChannelId,
    tmux_session_name: &str,
    observed_output_end: u64,
    context: &str,
) -> bool {
    let relay_coord = shared.tmux_relay_coord(channel_id);
    let mut confirmed = relay_coord
        .confirmed_end_offset
        .load(std::sync::atomic::Ordering::Acquire);

    while confirmed != 0 && observed_output_end < confirmed {
        let stored_gen_mtime_ns = relay_coord
            .confirmed_end_generation_mtime_ns
            .load(std::sync::atomic::Ordering::Acquire);
        let current_gen_mtime_ns = read_generation_file_mtime_ns(tmux_session_name);
        let new_watermark = watermark_after_output_regression(
            stored_gen_mtime_ns,
            current_gen_mtime_ns,
            observed_output_end,
        );

        match relay_coord.confirmed_end_offset.compare_exchange(
            confirmed,
            new_watermark,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => {
                relay_coord
                    .last_relay_ts_ms
                    .store(0, std::sync::atomic::Ordering::Release);
                relay_coord
                    .confirmed_end_generation_mtime_ns
                    .store(current_gen_mtime_ns, std::sync::atomic::Ordering::Release);
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 👁 Reset stale tmux relay watermark for {} (channel {}, context={}, observed_output_end={}, stale_confirmed_end={}, new_watermark={}, generation_mtime_changed={})",
                    tmux_session_name,
                    channel_id.get(),
                    context,
                    observed_output_end,
                    confirmed,
                    new_watermark,
                    stored_gen_mtime_ns != current_gen_mtime_ns
                );
                return true;
            }
            Err(observed) => confirmed = observed,
        }
    }

    false
}

pub(super) fn reset_stale_local_relay_offset_if_output_regressed(
    last_relayed_offset: &mut Option<u64>,
    last_observed_generation_mtime_ns: &mut Option<i64>,
    channel_id: ChannelId,
    tmux_session_name: &str,
    observed_output_end: u64,
    context: &str,
) -> bool {
    let Some(prev_offset) = *last_relayed_offset else {
        return false;
    };
    if observed_output_end >= prev_offset {
        return false;
    }

    let stored_gen_mtime_ns = last_observed_generation_mtime_ns.unwrap_or(0);
    let current_gen_mtime_ns = read_generation_file_mtime_ns(tmux_session_name);
    let new_offset = watermark_after_output_regression(
        stored_gen_mtime_ns,
        current_gen_mtime_ns,
        observed_output_end,
    );
    let new_local = if new_offset == 0 {
        // Fresh wrapper — clear the local watermark entirely so the next
        // tick walks the file from offset 0 (matches the global reset
        // semantics for cancel→respawn).
        None
    } else {
        Some(new_offset)
    };
    *last_relayed_offset = new_local;
    *last_observed_generation_mtime_ns = Some(current_gen_mtime_ns);

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] 👁 Reset stale tmux local relay offset for {} (channel {}, context={}, observed_output_end={}, stale_last_relayed={}, new_local_offset={:?}, generation_mtime_changed={})",
        tmux_session_name,
        channel_id.get(),
        context,
        observed_output_end,
        prev_offset,
        new_local,
        stored_gen_mtime_ns != current_gen_mtime_ns
    );
    true
}

/// Remove jsonl/input/prompt/owner/etc files in the persistent sessions
/// directory that no longer belong to a running tmux session. Conservative:
/// require an owner marker (or the jsonl) to be older than
/// `ORPHAN_MIN_AGE_SECS` and require the session to be absent from tmux
/// before deleting. Legacy `/tmp/` files are *never* swept at startup —
/// pre-migration wrappers may still be writing into them.
pub(super) async fn sweep_orphan_session_files() {
    const ORPHAN_MIN_AGE_SECS: u64 = 10 * 60; // 10 minutes

    let Some(dir) = crate::services::tmux_common::persistent_sessions_dir() else {
        return;
    };
    if !dir.exists() {
        return;
    }

    // List live tmux sessions.
    let live: std::collections::HashSet<String> = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names),
    )
    .await
    {
        Ok(Ok(Ok(names))) => names.into_iter().collect(),
        _ => return, // tmux unavailable — skip sweep rather than risk false positives
    };

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    // Group files under the sessions dir by the `agentdesk-<hash>-<host>-<session>`
    // prefix. Any prefix whose session name is not in `live` *and* whose
    // oldest file mtime is older than ORPHAN_MIN_AGE_SECS is swept.
    let mut groups: std::collections::HashMap<String, (String, std::time::SystemTime)> =
        std::collections::HashMap::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.starts_with("agentdesk-") {
            continue;
        }
        // Strip extension.
        let stem = match name.rsplit_once('.') {
            Some((s, _)) => s.to_string(),
            None => name.clone(),
        };
        // Session name is the last token after the fourth dash — but our
        // prefix format is `agentdesk-<12hex>-<host>-<session>` and host
        // may contain dashes. The simplest robust approach: split_once on
        // `agentdesk-<hash>-<host>-` is hard to reverse, so instead we use
        // the owner file's prefix as the grouping key directly — any file
        // whose stem matches some live session (ends with `-<live>`) is kept.
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or_else(|_| std::time::SystemTime::now());
        groups
            .entry(stem.clone())
            .and_modify(|slot| {
                if mtime < slot.1 {
                    *slot = (stem.clone(), mtime);
                }
            })
            .or_insert((stem, mtime));
    }

    let now = std::time::SystemTime::now();
    let mut swept = 0usize;
    for (stem, (_, oldest_mtime)) in groups {
        // Is this stem associated with any live tmux session? We check
        // whether ANY live session name appears as a suffix of the stem.
        // Since session names are distinctive (provider:channel shape), a
        // conservative suffix match keeps ambiguity low; we also require
        // that the match is preceded by a dash so we don't match e.g.
        // "claude:foo" against a stem ending with "-thisisnotclaude:foo".
        let is_live = live.iter().any(|live_name| {
            let needle = format!("-{}", live_name);
            stem.ends_with(&needle) || stem == *live_name
        });
        if is_live {
            continue;
        }
        // Conservative: require age threshold.
        let age = now
            .duration_since(oldest_mtime)
            .unwrap_or(std::time::Duration::ZERO);
        if age.as_secs() < ORPHAN_MIN_AGE_SECS {
            continue;
        }
        // Delete every file under this stem.
        let Ok(iter) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in iter.flatten() {
            if let Ok(fname) = entry.file_name().into_string() {
                if fname.starts_with(&format!("{}.", stem)) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        swept += 1;
    }
    if swept > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🧹 Swept {} orphan session file group(s) from {}",
            swept,
            dir.display()
        );
    }
}
