//! Platform startup resource limits.
/// Raise the process file-descriptor soft limit toward `desired`, clamped to
/// the inherited hard limit. Best-effort: logs and continues on failure so the
/// server never refuses to start over an rlimit tweak.
#[cfg(unix)]
pub(super) fn raise_fd_soft_limit(desired: u64) {
    // SAFETY: getrlimit/setrlimit with a valid resource id and a local rlimit
    // struct; no aliasing or lifetime concerns.
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            eprintln!("  ⚠ getrlimit(RLIMIT_NOFILE) failed; leaving fd limit unchanged");
            return;
        }
        let hard = lim.rlim_max;
        // rlim_max can be RLIM_INFINITY; cap the request at `desired` either way.
        let hard_capped = if hard == libc::RLIM_INFINITY {
            desired as libc::rlim_t
        } else {
            std::cmp::min(desired as libc::rlim_t, hard)
        };
        // macOS rejects setrlimit(RLIMIT_NOFILE) with EINVAL when the requested
        // soft limit exceeds the kernel per-process ceiling (OPEN_MAX, surfaced
        // via sysconf(_SC_OPEN_MAX), itself derived from kern.maxfilesperproc).
        // A tmux-inherited soft=256 with an "unlimited" hard limit would push
        // target=16384 past that ceiling and silently leave the limit at 256.
        // Clamp to the kernel cap so the raise actually takes effect.
        #[cfg(target_os = "macos")]
        let target = {
            let open_max = libc::sysconf(libc::_SC_OPEN_MAX);
            if open_max > 0 {
                std::cmp::min(hard_capped, open_max as libc::rlim_t)
            } else {
                hard_capped
            }
        };
        #[cfg(not(target_os = "macos"))]
        let target = hard_capped;
        if lim.rlim_cur >= target {
            return; // already at or above target
        }
        let new_lim = libc::rlimit {
            rlim_cur: target,
            rlim_max: hard,
        };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &new_lim) != 0 {
            eprintln!(
                "  ⚠ setrlimit(RLIMIT_NOFILE, {target}) failed; fd limit stays at {}",
                lim.rlim_cur
            );
        } else {
            eprintln!("  ✓ raised RLIMIT_NOFILE soft limit to {target}");
        }
    }
}

#[cfg(not(unix))]
pub(super) fn raise_fd_soft_limit(_desired: u64) {}

pub(super) fn spawn_skill_manifest_audit(config: &crate::config::Config, pool: &sqlx::PgPool) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let config = config.clone();
    let pool = pool.clone();
    spawn_skill_manifest_audit_once(
        &STARTED,
        async move {
            let request = crate::server::routes::skills_manifest_audit::manifest_audit_request(
                &config, &pool,
            )
            .await;
            tokio::task::spawn_blocking(move || {
                crate::server::routes::skills_manifest_audit::audit_skill_manifest_agents(request)
            })
            .await
            .map_or_else(
                |error| {
                    Some(format!(
                        "skill manifest audit inspection_unavailable: {error}"
                    ))
                },
                |report| report.warning(),
            )
        },
        |message| tracing::warn!("{message}"),
    );
}

fn spawn_skill_manifest_audit_once(
    once: &std::sync::Once,
    inspect: impl std::future::Future<Output = Option<String>> + Send + 'static,
    emit: impl FnOnce(String) + Send + 'static,
) {
    // Scheduling is process-once and never awaits disk or database work on the boot path.
    once.call_once(|| {
        tokio::spawn(async move {
            if let Some(message) = inspect.await {
                emit(message);
            }
        });
    });
}

#[cfg(test)]
#[path = "startup_audit_tests.rs"]
mod tests;
