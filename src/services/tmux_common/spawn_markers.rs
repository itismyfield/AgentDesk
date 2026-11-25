//! Writes a tmux session's per-spawn `.generation` and `.spawn_nonce` markers under its source authority.

use std::io::Write;

/// Read the `.generation` marker file mtime in nanoseconds since the unix
/// epoch. Returns 0 when the marker is missing in BOTH the canonical
/// persistent location (`runtime_root()/runtime/sessions/`) and the legacy
/// `/tmp/` fallback supported by `resolve_session_temp_path` (#892
/// migration window). All of those conditions are treated by callers as
/// "fresh wrapper".
///
/// `.generation` is atomically replaced once per successful provider spawn and
/// rewritten through a pinned fd when a surviving wrapper is adopted after a
/// dcserver restart. Spawn replacement changes the wrapper identity mtime;
/// adoption preserves it. NOTE: this mtime signal is the #1270 wrapper-identity
/// consumer ONLY. The status-panel session-instance key (#3087) reads the
/// dedicated `.spawn_nonce` marker content instead.
pub(crate) fn read_generation_file_mtime_ns(tmux_session_name: &str) -> i64 {
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
pub(crate) const SPAWN_NONCE_SUFFIX: &str = "spawn_nonce";

#[cfg(any(test, feature = "test-support"))]
pub(crate) type GenerationStampBeforeCreateHook = std::sync::Arc<dyn Fn() + Send + Sync + 'static>;
#[cfg(test)]
static GENERATION_STAMP_BEFORE_CREATE_HOOK: std::sync::LazyLock<
    std::sync::Mutex<Option<GenerationStampBeforeCreateHook>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
fn run_generation_stamp_before_create_hook_for_tests() {
    let hook = GENERATION_STAMP_BEFORE_CREATE_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Hook fired on the freshly written temp inode BEFORE its mtime is read, so a
/// test can give it any mtime the host filesystem would never produce on its own
/// — notably one that exactly equals the previous incarnation's (#5437), which an
/// APFS/ext4 host cannot reproduce because its timestamps advance every spawn.
#[cfg(any(test, feature = "test-support"))]
pub(crate) type GenerationStampAfterCreateHook =
    std::sync::Arc<dyn Fn(&str) + Send + Sync + 'static>;
#[cfg(test)]
static GENERATION_STAMP_AFTER_CREATE_HOOK: std::sync::LazyLock<
    std::sync::Mutex<Option<GenerationStampAfterCreateHook>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
fn run_generation_stamp_after_create_hook_for_tests(tmp_path: &str) {
    let hook = GENERATION_STAMP_AFTER_CREATE_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook(tmp_path);
    }
}

/// Escalating offsets (ns past the previous incarnation's mtime) tried when a
/// freshly stamped `.generation` inode does not land strictly after it (#5437).
/// The first steps cover nanosecond/microsecond-granularity filesystems; the
/// later ones cover coarse ones (HFS+/ext3 at 1s, FAT at 2s), where a `+1ns`
/// request truncates straight back onto the colliding value. The list length is
/// the re-check loop's upper bound.
const GENERATION_MTIME_BUMP_STEPS_NS: &[i64] = &[
    1,
    1_000,
    1_000_000,
    1_000_000_000,
    2_000_000_000,
    4_000_000_000,
    8_000_000_000,
];

#[cfg(test)]
static GENERATION_MTIME_BUMP_STEPS_OVERRIDE: std::sync::LazyLock<
    std::sync::Mutex<Option<Vec<i64>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

fn generation_mtime_bump_steps_ns() -> Vec<i64> {
    #[cfg(test)]
    if let Some(steps) = GENERATION_MTIME_BUMP_STEPS_OVERRIDE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
    {
        return steps;
    }
    GENERATION_MTIME_BUMP_STEPS_NS.to_vec()
}

/// Read an open marker inode's mtime as nanoseconds since the unix epoch, in the
/// same units `read_generation_file_mtime_ns` reports to consumers.
fn generation_marker_mtime_ns(file: &std::fs::File) -> std::io::Result<i64> {
    let modified = file.metadata()?.modified()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
        .ok_or_else(|| std::io::Error::other("generation marker mtime is out of range"))
}

/// Push a freshly created `.generation` inode's mtime strictly past the previous
/// incarnation's, so two incarnations of one tmux session name cannot share the
/// receipt-projection generation key when the filesystem's timestamp resolution
/// is coarser than the respawn interval (#5437).
///
/// What this GUARANTEES when it returns a value greater than `previous_ns`: that
/// value is what a later `stat` observes, because every step is verified by
/// RE-READING the mtime rather than trusting the request (a coarse filesystem
/// silently truncates it), and the escalation is bounded by
/// `generation_mtime_bump_steps_ns()`.
///
/// What it deliberately does NOT guarantee: success. Uniqueness here is a
/// best-effort hardening of the key, never a gate on the provider spawn. If
/// `set_times` is unsupported or refused, if the mtime cannot be re-read, or if
/// every escalation step is truncated back onto the collision, this warns once
/// and returns the last OBSERVED mtime; the caller still publishes the marker
/// and the spawn proceeds exactly as it did before this bump existed. A missing
/// marker (mtime 0, "fresh wrapper") is strictly worse for every consumer than a
/// marker whose uniqueness is merely unproven.
fn bump_generation_mtime_past_previous(
    file: &std::fs::File,
    previous_ns: i64,
    observed_ns: i64,
    tmux_session_name: &str,
) -> i64 {
    let steps_ns = generation_mtime_bump_steps_ns();
    let mut current_ns = observed_ns;
    let mut failure = None;

    for step_ns in steps_ns.iter().copied() {
        let Some(target_ns) = previous_ns
            .checked_add(step_ns)
            .and_then(|target_ns| u64::try_from(target_ns).ok())
        else {
            failure = Some(format!("bump target for +{step_ns}ns is out of range"));
            break;
        };
        let target = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(target_ns);
        if let Err(error) = file.set_times(std::fs::FileTimes::new().set_modified(target)) {
            failure = Some(format!("set_times(+{step_ns}ns) failed: {error}"));
            break;
        }
        match generation_marker_mtime_ns(file) {
            Ok(mtime_ns) => {
                current_ns = mtime_ns;
                if mtime_ns > previous_ns {
                    return mtime_ns;
                }
            }
            Err(error) => {
                failure = Some(format!("re-reading the +{step_ns}ns bump failed: {error}"));
                break;
            }
        }
    }

    let detail = failure.unwrap_or_else(|| {
        format!(
            "every bump step up to +{}ns was truncated back onto the previous mtime",
            steps_ns.last().copied().unwrap_or(0)
        )
    });
    tracing::warn!(
        "generation marker mtime uniqueness unproven for {tmux_session_name} \
         (previous={previous_ns}, publishing={current_ns}, {detail}); spawn continues"
    );
    current_ns
}

/// Stamp the `.generation` marker for one successful provider spawn.
///
/// A successful call creates a new sibling inode with `create_new` and atomically
/// renames that inode over the marker path. Adoption can therefore keep writing
/// an fd for the prior inode without changing the marker visible at this path.
/// Failures leave the destination untouched, emit one warning, and do not fail
/// the provider spawn. The write is not fsynced.
///
/// Before the rename publishes the new inode, its mtime is bumped past the
/// mtime the PREVIOUS incarnation published under this session name whenever the
/// fresh inode did not already land after it (#5437) — so the published marker
/// never carries a colliding generation key even transiently. That bump is
/// best-effort: see `bump_generation_mtime_past_previous` for what it does and
/// does not guarantee.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn stamp_session_generation_marker(tmux_session_name: &str) -> Option<i64> {
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        stamp_session_generation_marker_under_source_authority(authority, tmux_session_name)
    })
}

fn stamp_session_generation_marker_under_source_authority(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    tmux_session_name: &str,
) -> Option<i64> {
    let generation = crate::services::discord::runtime_store::process_generation();
    let path = crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
    let tmp_path = format!(
        "{path}.tmp.{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    );
    // The generation key the previous incarnation of this session name published,
    // read through the resolver consumers stat (canonical, then the legacy `/tmp`
    // fallback). 0 = no readable prior marker, so this spawn has nothing to
    // collide with and no comparison is needed. Read under source authority, which
    // serializes this session's marker writers.
    let previous_mtime_ns = read_generation_file_mtime_ns(tmux_session_name);

    #[cfg(test)]
    run_generation_stamp_before_create_hook_for_tests();

    let stamp = || -> std::io::Result<i64> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        file.write_all(generation.to_string().as_bytes())?;
        #[cfg(test)]
        run_generation_stamp_after_create_hook_for_tests(&tmp_path);
        let mut modified_ns = generation_marker_mtime_ns(&file)?;
        if previous_mtime_ns != 0 && modified_ns <= previous_mtime_ns {
            modified_ns = bump_generation_mtime_past_previous(
                &file,
                previous_mtime_ns,
                modified_ns,
                tmux_session_name,
            );
        }
        drop(file);
        authority.invalidate();
        std::fs::rename(&tmp_path, &path)?;
        Ok(modified_ns)
    };

    match stamp() {
        Ok(modified_ns) => Some(modified_ns),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp_path);
            tracing::warn!("failed to stamp generation marker for {tmux_session_name}: {error}");
            None
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) type SpawnMarkersAfterGenerationHook = std::sync::Arc<dyn Fn() + Send + Sync + 'static>;
#[cfg(test)]
static SPAWN_MARKERS_AFTER_GENERATION_HOOK: std::sync::LazyLock<
    std::sync::Mutex<Option<SpawnMarkersAfterGenerationHook>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
fn run_spawn_markers_after_generation_hook_for_tests() {
    let hook = SPAWN_MARKERS_AFTER_GENERATION_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Validate prepared evidence and publish both markers under the source authority; generation remains best-effort.
/// Nonce failures remove the stale destination; the v4 UUID is mtime-independent and never rewritten by live wrappers between turns.
pub(crate) fn stamp_spawn_markers(
    tmux_session_name: &str,
    prepared: Option<&crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation>,
) -> std::io::Result<String> {
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        if let Some(prepared) = prepared {
            prepared.validate(tmux_session_name)?;
        }
        let _ =
            stamp_session_generation_marker_under_source_authority(authority, tmux_session_name);
        #[cfg(test)]
        run_spawn_markers_after_generation_hook_for_tests();
        let nonce = prepared
            .map(|p| p.context.execution_nonce.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        write_spawn_nonce_value(authority, tmux_session_name, nonce)
    })
}

/// Create a fresh nonce for legacy callers; production spawns use the combined gate.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn write_spawn_nonce(tmux_session_name: &str) -> std::io::Result<String> {
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        write_spawn_nonce_value(
            authority,
            tmux_session_name,
            uuid::Uuid::new_v4().simple().to_string(),
        )
    })
}

fn write_spawn_nonce_value(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    tmux_session_name: &str,
    nonce: String,
) -> std::io::Result<String> {
    let path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, SPAWN_NONCE_SUFFIX);
    // Distinct per-process temp sibling to avoid clobbering across concurrent
    // spawns; replaced atomically into `path` on success.
    let tmp_path = format!("{path}.tmp.{}", std::process::id());

    let changed = std::fs::read(&path).ok().as_deref() != Some(nonce.as_bytes());
    let write_then_rename = || -> std::io::Result<()> {
        std::fs::write(&tmp_path, nonce.as_bytes())?;
        if changed {
            authority.invalidate();
        }
        std::fs::rename(&tmp_path, &path)
    };

    if let Err(e) = write_then_rename() {
        // Leave NO stale/torn nonce behind: remove both the temp sibling and any
        // pre-existing destination so the key resolves to `None`, never to a
        // prior spawn's nonce.
        let _ = std::fs::remove_file(&tmp_path);
        if std::fs::symlink_metadata(&path).is_ok() {
            authority.invalidate();
        }
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok(nonce)
}

/// Test seams of the spawn stamp; `None` clears one.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn set_generation_before_create_hook(hook: Option<GenerationStampBeforeCreateHook>) {
    *GENERATION_STAMP_BEFORE_CREATE_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = hook;
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn set_generation_after_create_hook(hook: Option<GenerationStampAfterCreateHook>) {
    *GENERATION_STAMP_AFTER_CREATE_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = hook;
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn set_generation_mtime_bump_steps(steps_ns: Option<Vec<i64>>) {
    *GENERATION_MTIME_BUMP_STEPS_OVERRIDE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = steps_ns;
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn set_spawn_after_generation_hook(hook: Option<SpawnMarkersAfterGenerationHook>) {
    *SPAWN_MARKERS_AFTER_GENERATION_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = hook;
}
