use super::*;

#[path = "session_runtime/channel_routing.rs"]
mod channel_routing;
#[path = "session_runtime/restore_cwd.rs"]
mod restore_cwd;
#[path = "session_runtime/worktree.rs"]
mod worktree;

use self::channel_routing::choose_restore_channel_name;
pub(super) use self::channel_routing::{
    RuntimeChannelBindingStatus, provider_handles_channel, resolve_channel_category,
    resolve_is_dm_channel, resolve_runtime_channel_binding_status, resolve_thread_parent,
    synthetic_thread_channel_name, validate_live_channel_routing,
    validate_live_channel_routing_with_dm_hint,
};
#[cfg(test)]
use self::restore_cwd::restore_thread_worktree_path_from_db;
use self::restore_cwd::{
    correct_session_cwd_to_tmux, reconcile_recovery_cwd, restore_session_cwd_from_db,
};
pub(super) use self::restore_cwd::{
    db_cwd_is_reusable_worktree, select_restored_session_path, session_path_is_usable,
};
pub(super) use self::worktree::{
    WorktreeInfo, cleanup_git_worktree, create_git_worktree, detect_worktree_conflict,
    reconstruct_managed_worktree_metadata, resolve_reusable_worktree,
};
use self::worktree::{is_managed_worktree_path, sync_inflight_worktree_context};

/// Per-channel session state
#[derive(Clone)]
pub(super) struct DiscordSession {
    pub(super) session_id: Option<String>,
    pub(super) memento_context_loaded: bool,
    pub(super) memento_reflected: bool,
    pub(super) current_path: Option<String>,
    pub(super) history: Vec<HistoryItem>,
    pub(super) pending_uploads: Vec<String>,
    pub(super) cleared: bool,
    /// Legacy remote profile name slot.
    ///
    /// Remote SSH is disabled by policy (#2193), so restored/persisted profile
    /// names must not influence routing, path validation, or dispatch.
    pub(super) remote_profile_name: Option<String>,
    pub(super) channel_id: Option<u64>,
    pub(super) channel_name: Option<String>,
    pub(super) category_name: Option<String>,
    /// Last time this session was actively used (for TTL cleanup)
    pub(super) last_active: tokio::time::Instant,
    /// If this session runs in a git worktree, store the info here
    pub(super) worktree: Option<WorktreeInfo>,
    /// Restart generation at which this session was created/restored.
    #[allow(dead_code)]
    pub(super) born_generation: u64,
}

impl DiscordSession {
    pub(super) fn clear_provider_session(&mut self) {
        self.session_id = None;
        self.memento_context_loaded = false;
        self.memento_reflected = false;
    }

    pub(super) fn restore_provider_session(&mut self, session_id: Option<String>) {
        self.memento_context_loaded = restored_memento_context_loaded(
            self.memento_context_loaded,
            self.session_id.as_deref(),
            session_id.as_deref(),
        );
        self.session_id = session_id;
        self.memento_reflected = false;
    }

    pub(super) fn note_memento_context_loaded(&mut self) {
        self.memento_context_loaded = true;
        self.memento_reflected = false;
    }

    /// Validate `current_path` and return it if it exists on disk.
    /// If the path is stale (deleted), clear `current_path` and `worktree`, log, and return `None`.
    pub(super) fn validated_path(&mut self, channel_id: impl std::fmt::Display) -> Option<String> {
        let current_path = self.current_path.as_ref()?;
        if session_path_is_usable(current_path) {
            return Some(current_path.clone());
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⚠ Ignoring stale local session path for channel {}: {}",
            channel_id,
            current_path
        );
        self.current_path = None;
        self.worktree = None;
        None
    }
}

pub(super) fn restored_memento_context_loaded(
    previous_loaded: bool,
    previous_session_id: Option<&str>,
    next_session_id: Option<&str>,
) -> bool {
    previous_loaded && previous_session_id == next_session_id && next_session_id.is_some()
}

/// Auto-restore session from bot_settings.json if not in memory
pub(super) async fn auto_restore_session(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    serenity_ctx: &serenity::prelude::Context,
) {
    auto_restore_session_with_dm_hint(shared, channel_id, serenity_ctx, None).await;
}

pub(super) async fn auto_restore_session_with_dm_hint(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    serenity_ctx: &serenity::prelude::Context,
    dm_hint: Option<bool>,
) {
    if matches!(
        resolve_runtime_channel_binding_status(&serenity_ctx.http, channel_id).await,
        RuntimeChannelBindingStatus::Unowned
    ) {
        return;
    }

    auto_restore_session_force(shared, channel_id, serenity_ctx, dm_hint).await;
}

/// Same as [`auto_restore_session_with_dm_hint`] but skips the
/// `RuntimeChannelBindingStatus::Unowned` early-return. Intended for callers
/// that have already decided an unbound channel deserves restoration —
/// e.g. the BINDING-GUARD's `can_route_unbound_direct_session` path which
/// only proceeds when persistent state already names a workspace for that
/// channel. Without this escape hatch the BINDING-GUARD's restoration step
/// silently no-ops on unowned channels and the channel stops responding
/// after a dcserver restart drops the in-memory session map (#1190 followup,
/// agentless direct sessions regression observed 2026-04-26).
pub(super) async fn auto_restore_session_force(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    serenity_ctx: &serenity::prelude::Context,
    dm_hint: Option<bool>,
) {
    // Resolve channel/category before taking the lock for mutation
    let (live_ch_name, cat_name) =
        resolve_channel_category(&serenity_ctx.http, Some(&serenity_ctx.cache), channel_id).await;
    let existing_channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let restore_ch_name = choose_restore_channel_name(
        existing_channel_name.as_deref(),
        live_ch_name.as_deref(),
        resolve_thread_parent(&serenity_ctx.http, channel_id).await,
        channel_id,
    );
    let is_dm = matches!(
        channel_id.to_channel(&serenity_ctx.http).await.ok(),
        Some(serenity::Channel::Private(_))
    );
    let is_dm = resolve_is_dm_channel(dm_hint, is_dm);

    // Read settings first to get provider and runtime restore metadata.
    let (last_path, provider) = {
        let settings = shared.settings.read().await;
        let provider = settings.provider.clone();
        let configured_path = settings::resolve_workspace(channel_id, restore_ch_name.as_deref())
            .or_else(|| {
                if is_dm {
                    super::agentdesk_config::resolve_dm_default_agent(&provider)
                        .map(|resolved| resolved.workspace)
                } else {
                    None
                }
            });
        // Use the effective tmux channel name here so restart recovery keeps
        // looking up the same session key for thread sessions that intentionally
        // use a synthetic "{parent}-t{thread_id}" channel name.
        // #3207 (part 2) P0-a: the DB cwd resolve is channel-scoped (see
        // `restore_session_cwd_from_db`).
        let restored_cwd = restore_ch_name.as_ref().and_then(|ch| {
            restore_session_cwd_from_db(
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                &provider,
                ch,
                channel_id.get(),
            )
        });
        // #3219: capture ownership from the resolver's exact `channel_id = $2`
        // match BEFORE the path-usability filter — a stale/unusable persisted cwd
        // does not change WHO owns the row. Only a channel-scoped cwd may outrank
        // the configured base; a NULL-channel_id legacy fallback returns false and
        // is never elevated. This flag is preserved across the tmux reconcile
        // below (reconcile changes a row's cwd value, not its ownership), so an
        // owned channel whose persisted cwd went stale can still elevate the valid
        // live worktree the reconcile supplies (the #3216 GAP2 phantom-rotation
        // case). The final db_cwd is still re-validated by `db_cwd_is_reusable
        // _worktree` before it can win.
        let db_cwd_channel_scoped = restored_cwd
            .as_ref()
            .map(|r| r.channel_scoped)
            .unwrap_or(false);
        // Only a usable path is honored for install into `session.current_path`.
        let mut db_cwd: Option<String> = restored_cwd
            .map(|r| r.path)
            .filter(|p| !p.is_empty() && session_path_is_usable(p));

        // #3216 GAP 2: reconcile the DB cwd against the live tmux pane. The live
        // pane is the source of truth for where the session actually runs; if the
        // DB cwd diverged (e.g. a phantom worktree rotation), adopting it would
        // relaunch `--resume` against a transcript-less path. When a live tmux
        // session exists and its pane cwd is a real managed/usable worktree that
        // DIFFERS from the DB cwd, adopt the tmux cwd AND correct the DB row.
        if let Some(ch) = restore_ch_name.as_ref() {
            let tmux_name = provider.build_tmux_session_name(ch);
            let tmux_cwd = crate::services::platform::tmux::pane_current_path(&tmux_name);
            let tmux_cwd_is_managed = tmux_cwd
                .as_deref()
                .map(is_managed_worktree_path)
                .unwrap_or(false);
            let tmux_cwd_is_usable = tmux_cwd
                .as_deref()
                .map(session_path_is_usable)
                .unwrap_or(false);
            if let Some(reconciled) = reconcile_recovery_cwd(
                db_cwd.as_deref(),
                tmux_cwd.as_deref(),
                tmux_cwd_is_managed,
                tmux_cwd_is_usable,
            ) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ #3216 reconciling recovery cwd for channel {}: DB {:?} → live tmux {}",
                    channel_id,
                    db_cwd.as_deref(),
                    reconciled
                );
                correct_session_cwd_to_tmux(
                    shared.pg_pool.as_ref(),
                    &shared.token_hash,
                    &provider,
                    ch,
                    channel_id.get(),
                    &reconciled,
                );
                db_cwd = Some(reconciled);
                // #3219: do NOT recompute channel-ownership here. Reconcile only
                // changes a row's cwd VALUE, not WHO owns the row, so we preserve
                // `db_cwd_channel_scoped` from the resolver's bulletproof exact
                // `channel_id = $2` match. `session_key` is globally unique, so
                // under a tmux-name collision only the TRUE owner ever gets
                // `channel_scoped = true` from the resolver; the intruder gets
                // false and is never elevated into the owner's worktree. (A row
                // freshly stamped from a NULL legacy row stays non-scoped — its
                // ownership is ambiguous under collision — so it is not elevated;
                // it self-heals to an exact match on the next heartbeat.)
            }
        }
        let persisted_path = load_last_session_path(
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            channel_id.get(),
        );

        // #3219: validate whether the restored DB cwd is the channel's own
        // reusable managed worktree BEFORE selecting, so the log below reflects
        // the actual decision (we only "ignore" it when it is NOT reused). Only a
        // channel-scoped cwd is eligible — never a NULL-fallback cwd.
        let reusable_worktree = db_cwd_channel_scoped
            && db_cwd_is_reusable_worktree(configured_path.as_deref(), db_cwd.as_deref());
        if let (Some(configured), Some(restored)) = (configured_path.as_ref(), db_cwd.as_ref())
            && configured != restored
            && !reusable_worktree
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ Ignoring restored DB cwd for channel {}: {} (configured workspace: {})",
                channel_id,
                restored,
                configured
            );
        }

        let last_path = select_restored_session_path(
            configured_path,
            db_cwd,
            persisted_path,
            reusable_worktree,
        );

        (last_path, provider)
    };

    let mut data = shared.core.lock().await;
    if let Some(session) = data.sessions.get_mut(&channel_id) {
        session.channel_id = Some(channel_id.get());
        session.last_active = tokio::time::Instant::now();
        session.channel_name = restore_ch_name.clone();
        session.category_name = cat_name.clone();
        session.remote_profile_name = None;
        if session.current_path.is_some() || last_path.is_none() {
            // A pre-existing session (e.g. inserted by restart watcher
            // registration with `current_path` from `sessions.cwd` but
            // `worktree: None`) hits this early return before the insertion
            // block below. Reconstruct the managed-worktree metadata here too so
            // restarted thread sessions regain `WorktreeInfo` / inflight worktree
            // context and a correct cleanup root (#3011).
            if let Some(current_path) = session.current_path.clone() {
                reconstruct_managed_worktree_metadata(
                    session,
                    &provider,
                    channel_id,
                    &current_path,
                );
            }
            return;
        }
    }

    if let Some(last_path) = last_path
        && session_path_is_usable(&last_path)
    {
        // Session ID is restored from DB (sessions.claude_session_id column)
        // which is already loaded into DiscordSession.session_id at startup.
        let session = data
            .sessions
            .entry(channel_id)
            .or_insert_with(|| DiscordSession {
                session_id: None,
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                channel_id: Some(channel_id.get()),
                channel_name: restore_ch_name.clone(),
                category_name: cat_name.clone(),
                remote_profile_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: runtime_store::load_generation(),
            });
        session.channel_id = Some(channel_id.get());
        session.last_active = tokio::time::Instant::now();
        session.channel_name = restore_ch_name.clone();
        session.category_name = cat_name.clone();
        session.remote_profile_name = None;
        session.current_path = Some(last_path.clone());
        reconstruct_managed_worktree_metadata(session, &provider, channel_id, &last_path);
        drop(data);

        // Rescan skills with project path
        let new_skills = scan_skills(&provider, Some(&last_path));
        *shared.skills_cache.write().await = new_skills;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] ↻ Auto-restored session: {last_path}");
    }
}

/// Create a lightweight session for a thread, bootstrapped from the parent channel's path.
/// The session's `channel_name` uses `{parent_channel}-t{thread_id}` so the derived
/// tmux session name stays short and unique instead of using the full thread title.
pub(super) async fn bootstrap_thread_session(
    shared: &Arc<SharedData>,
    thread_channel_id: ChannelId,
    parent_path: &str,
    http: &Arc<serenity::http::Http>,
    cache: Option<&Arc<serenity::cache::Cache>>,
) -> bool {
    let (thread_title, cat_name) = resolve_channel_category(http, cache, thread_channel_id).await;
    let provider_kind = shared.settings.read().await.provider.clone();
    // Build a short, stable channel_name: "{parent_channel}-t{thread_id}"
    let parent_info = resolve_thread_parent(http, thread_channel_id).await;
    let ch_name = if let Some((parent_id, parent_name)) = parent_info {
        let parent = parent_name.unwrap_or_else(|| format!("{parent_id}"));
        Some(synthetic_thread_channel_name(&parent, thread_channel_id))
    } else {
        // Not a thread (shouldn't happen here) — fall back to resolved name
        thread_title
    };
    let mut data = shared.core.lock().await;
    if data.sessions.contains_key(&thread_channel_id) {
        return false;
    }

    // Session ID comes from DB (sessions.claude_session_id), not from file.
    let session = data
        .sessions
        .entry(thread_channel_id)
        .or_insert_with(|| DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            channel_id: Some(thread_channel_id.get()),
            channel_name: ch_name,
            category_name: cat_name,
            remote_profile_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: runtime_store::load_generation(),
        });
    // Prefer restoring the worktree persisted for this thread session across a
    // dcserver restart. The in-memory `sessions` map is cleared on restart, so
    // without this lookup a new thread message would create a brand-new worktree
    // and drop the provider session fingerprint / recovery context tied to the
    // previous worktree path (#3011). Mirror the DB cwd lookup used by
    // `auto_restore_session_force`, and only create a fresh worktree when the
    // stored path is absent or no longer a usable git worktree on disk.
    let ch = session
        .channel_name
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let restored_worktree = resolve_reusable_worktree(
        shared.pg_pool.as_ref(),
        &shared.token_hash,
        &provider_kind,
        &ch,
        thread_channel_id.get(),
        parent_path,
    );
    // Only honor the restore when a branch is recoverable. A detached / unknown
    // branch would yield no `WorktreeInfo`, so in that case fall through to
    // create a fresh, well-formed worktree instead.
    if let Some(wt_info) = restored_worktree {
        let base_commit = crate::services::platform::git_head_commit(&wt_info.original_path);
        let restored_path = wt_info.worktree_path.clone();
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ Restored thread worktree: {} (branch: {})",
            wt_info.worktree_path,
            wt_info.branch_name
        );
        sync_inflight_worktree_context(
            &provider_kind,
            thread_channel_id.get(),
            Some(wt_info.worktree_path.clone()),
            Some(wt_info.branch_name.clone()),
            base_commit,
        );
        session.worktree = Some(wt_info);
        session.current_path = Some(restored_path.clone());
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] ↻ Bootstrapped thread session: {restored_path}");
        return true;
    }

    // Always create a worktree for thread sessions to isolate concurrent work.
    let effective_path = {
        let provider_str = shared.settings.read().await.provider.as_str().to_string();
        match create_git_worktree(parent_path, &ch, &provider_str) {
            Ok((wt_path, branch)) => {
                let base_commit = crate::services::platform::git_head_commit(parent_path);
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🌿 Thread worktree created: {} (branch: {})",
                    wt_path,
                    branch
                );
                session.worktree = Some(WorktreeInfo {
                    original_path: parent_path.to_string(),
                    worktree_path: wt_path.clone(),
                    branch_name: branch.clone(),
                });
                sync_inflight_worktree_context(
                    &provider_kind,
                    thread_channel_id.get(),
                    Some(wt_path.clone()),
                    Some(branch),
                    base_commit,
                );
                wt_path
            }
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ Thread worktree creation failed: {e}, falling back to parent path"
                );
                parent_path.to_string()
            }
        }
    };
    session.current_path = Some(effective_path.clone());
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ↻ Bootstrapped thread session: {effective_path}");
    true
}

#[cfg(test)]
mod select_restored_session_path_tests {
    //! #3219: `select_restored_session_path` must prefer the channel's own
    //! reusable managed worktree (the channel-scoped DB cwd) over the configured
    //! *base* workspace, or crash/kill recovery re-derives a fresh worktree and
    //! `--resume` breaks. The reusable-worktree decision is made by the caller
    //! (`db_cwd_is_reusable_worktree`, git-validated against the configured parent
    //! repo); here we verify the selector honors that decision and otherwise
    //! preserves the configured→db_cwd→yaml fallback order unchanged.
    use super::{
        db_cwd_is_reusable_worktree, select_restored_session_path, session_path_is_usable,
    };

    struct FixturePaths {
        _root: tempfile::TempDir,
        base: String,
        wt: String,
        yaml: String,
    }

    fn fixture_paths() -> FixturePaths {
        let root = tempfile::tempdir().expect("temp root");
        let base = root.path().join("workspaces").join("agentdesk");
        let wt = root.path().join("worktrees").join("claude-adk-cc-113822");
        let yaml = root.path().join("yaml").join("path");
        std::fs::create_dir_all(&base).expect("base dir");
        std::fs::create_dir_all(&wt).expect("worktree dir");
        std::fs::create_dir_all(&yaml).expect("yaml dir");
        FixturePaths {
            _root: root,
            base: base.display().to_string(),
            wt: wt.display().to_string(),
            yaml: yaml.display().to_string(),
        }
    }

    #[test]
    fn reusable_worktree_outranks_configured_base() {
        let paths = fixture_paths();
        let selected = select_restored_session_path(
            Some(paths.base.clone()),
            Some(paths.wt.clone()),
            None,
            true,
        );
        assert_eq!(selected.as_deref(), Some(paths.wt.as_str()));
    }

    #[test]
    fn non_reusable_db_cwd_keeps_configured_priority() {
        let paths = fixture_paths();
        // When the caller's git validation says the worktree is NOT reusable
        // (stale/foreign/relocated/remote), configured base wins as before.
        let selected = select_restored_session_path(
            Some(paths.base.clone()),
            Some(paths.wt.clone()),
            None,
            false,
        );
        assert_eq!(selected.as_deref(), Some(paths.base.as_str()));
    }

    #[test]
    fn falls_back_to_db_cwd_then_yaml_when_no_configured() {
        let paths = fixture_paths();
        // No configured path: existing fallback order is unchanged regardless of
        // the reusable flag.
        let selected = select_restored_session_path(
            None,
            Some(paths.wt.clone()),
            Some(paths.yaml.clone()),
            false,
        );
        assert_eq!(selected.as_deref(), Some(paths.wt.as_str()));

        let selected = select_restored_session_path(None, None, Some(paths.yaml.clone()), true);
        assert_eq!(selected.as_deref(), Some(paths.yaml.as_str()));
    }

    #[test]
    fn legacy_remote_profile_names_do_not_bypass_local_path_validation() {
        let missing = "/agentdesk/remote-only/path";
        assert!(
            !session_path_is_usable(missing),
            "remote-profile compatibility names must not make remote-only paths usable"
        );
        let selected = select_restored_session_path(
            Some(missing.into()),
            Some("~/remote-shell-cwd".into()),
            None,
            true,
        );
        assert_eq!(
            selected, None,
            "restored paths must be real local directories while remote SSH is disabled"
        );
    }

    #[test]
    fn reusable_predicate_requires_configured_and_db_cwd() {
        // No git work here: a missing configured parent or missing db_cwd can
        // never be "reusable" (the full git validation only runs when both exist).
        let paths = fixture_paths();
        assert!(!db_cwd_is_reusable_worktree(None, Some(paths.wt.as_str())));
        assert!(!db_cwd_is_reusable_worktree(
            Some(paths.base.as_str()),
            None
        ));
        assert!(!db_cwd_is_reusable_worktree(None, None));
    }
}

#[cfg(test)]
mod worktree_reuse_channel_isolation_tests {
    //! #3207 (part 2) P0: the worktree-reuse DB lookup
    //! (`restore_thread_worktree_path_from_db`) must be scoped by the unique
    //! channel id. Two channels whose sanitized/truncated names collide produce
    //! the SAME `session_key`; without the channel-id predicate the second
    //! channel would resolve the first channel's persisted cwd and resume into
    //! its working tree (silent corruption). These tests are RED before the
    //! `channel_id = $2` predicate was added and GREEN after.
    //!
    //! #3216 extends this with the safe legacy NULL-channel_id fallback (GAP 1)
    //! and the live-tmux recovery-cwd reconcile policy (GAP 2).
    use super::{restore_session_cwd_from_db, restore_thread_worktree_path_from_db};
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::discord::adk_session::build_namespaced_session_key;
    use crate::services::provider::ProviderKind;

    async fn seed_session(
        pool: &sqlx::PgPool,
        session_key: &str,
        channel_id: Option<&str>,
        cwd: &str,
    ) {
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, cwd, channel_id, last_heartbeat)
             VALUES ($1, 'claude', 'idle', $2, $3, NOW())",
        )
        .bind(session_key)
        .bind(cwd)
        .bind(channel_id)
        .execute(pool)
        .await
        .expect("seed sessions row");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn name_collision_does_not_cross_channels() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3207";
        // Two distinct channels that sanitize/truncate to the SAME channel name
        // therefore share one session_key. Channel A is the persisted owner.
        let collide_name = "shared-name";
        let tmux_name = provider.build_tmux_session_name(collide_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let channel_a: u64 = 111_111_111_111_111_111;
        let channel_b: u64 = 222_222_222_222_222_222;
        let owner_cwd = "/home/u/.adk/release/worktrees/claude-shared-name-20260101-000000";

        seed_session(&pool, &session_key, Some(&channel_a.to_string()), owner_cwd).await;

        // Owner channel resolves its own persisted worktree.
        let owner = restore_thread_worktree_path_from_db(
            Some(&pool),
            token_hash,
            &provider,
            collide_name,
            channel_a,
        );
        assert_eq!(
            owner.as_deref(),
            Some(owner_cwd),
            "the owning channel must resolve its own persisted worktree"
        );

        // The colliding (different-id) channel must NOT resolve channel A's cwd.
        // This is the cross-channel corruption guard (RED before the P0 fix).
        let cross = restore_thread_worktree_path_from_db(
            Some(&pool),
            token_hash,
            &provider,
            collide_name,
            channel_b,
        );
        assert_eq!(
            cross, None,
            "a different channel sharing the same session_key must NOT resolve \
             another channel's persisted worktree"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // #3216 GAP 1: a SINGLE legacy NULL-channel_id row IS now reused via the
    // safe fallback (it cannot be cross-channel because it is the only row for
    // that session_key). Previously (#3207) this returned None and forced a
    // worktree rotation that divorced the live session from its transcript.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn legacy_null_channel_id_single_row_is_reused() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3216-legacy";
        let channel_name = "legacy-chan";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let channel_id: u64 = 333_333_333_333_333_333;
        let cwd = "/home/u/.adk/release/worktrees/claude-legacy-chan-20260101-000000";

        // A row written before the channel_id column existed has NULL channel_id.
        seed_session(&pool, &session_key, None, cwd).await;

        let resolved = restore_thread_worktree_path_from_db(
            Some(&pool),
            token_hash,
            &provider,
            channel_name,
            channel_id,
        );
        assert_eq!(
            resolved.as_deref(),
            Some(cwd),
            "a single legacy NULL-channel_id row must be reused via the #3216 \
             safe fallback so the legacy session keeps its transcript worktree"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // #3216 GAP 1 guard: a SINGLE row that carries a DIFFERENT non-null
    // channel_id must NOT be reused by another channel (cross-channel hazard).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fallback_refused_when_single_row_has_different_channel() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3216-wrongchan";
        let channel_name = "wrong-chan";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let owner_channel: u64 = 121_212_121_212_121_212;
        let requester_channel: u64 = 343_434_343_434_343_434;

        // Single row, but stamped with a different (non-null) channel.
        seed_session(
            &pool,
            &session_key,
            Some(&owner_channel.to_string()),
            "/home/u/.adk/release/worktrees/claude-wrong-chan-owner",
        )
        .await;

        let resolved = restore_thread_worktree_path_from_db(
            Some(&pool),
            token_hash,
            &provider,
            channel_name,
            requester_channel,
        );
        assert_eq!(
            resolved, None,
            "a single row with a different non-null channel_id must not be \
             reused by another channel"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // ---- P0-a: `auto_restore_session_force` restart restore (db_cwd) ----
    //
    // `restore_session_cwd_from_db` backs the restart-restore `db_cwd` lookup
    // that installs into `session.current_path`. It must be channel-scoped for
    // the same reason as the thread worktree reuse: a colliding session_key
    // must not let one channel install another channel's persisted cwd.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_restore_cwd_does_not_cross_channels() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3207-restore";
        let collide_name = "shared-restore-name";
        let tmux_name = provider.build_tmux_session_name(collide_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let channel_a: u64 = 444_444_444_444_444_444;
        let channel_b: u64 = 555_555_555_555_555_555;
        let owner_cwd = "/home/u/.adk/release/worktrees/claude-shared-restore-20260101-000000";

        seed_session(&pool, &session_key, Some(&channel_a.to_string()), owner_cwd).await;

        // Owner channel resolves its own persisted cwd for restart restore.
        let owner = restore_session_cwd_from_db(
            Some(&pool),
            token_hash,
            &provider,
            collide_name,
            channel_a,
        );
        assert_eq!(
            owner.as_ref().map(|r| r.path.as_str()),
            Some(owner_cwd),
            "the owning channel must resolve its own restart-restore cwd"
        );
        assert!(
            owner.as_ref().map(|r| r.channel_scoped).unwrap_or(false),
            "an exact channel-id match must be marked channel_scoped (#3219)"
        );

        // The colliding (different-id) channel must NOT install channel A's cwd
        // into its restored runtime (RED before the P0-a `channel_id = $2` fix).
        let cross = restore_session_cwd_from_db(
            Some(&pool),
            token_hash,
            &provider,
            collide_name,
            channel_b,
        );
        assert!(
            cross.is_none(),
            "a different channel sharing the same session_key must NOT resolve \
             another channel's restart-restore cwd"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // #3216 GAP 1: restart-restore resolves a single legacy NULL row via the
    // safe fallback too (mirrors the worktree-reuse path).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_restore_cwd_legacy_null_single_row_is_reused() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3216-restore-legacy";
        let channel_name = "legacy-restore-chan";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let channel_id: u64 = 666_666_666_666_666_666;
        let cwd = "/home/u/.adk/release/worktrees/claude-legacy-restore-20260101-000000";

        seed_session(&pool, &session_key, None, cwd).await;

        let resolved = restore_session_cwd_from_db(
            Some(&pool),
            token_hash,
            &provider,
            channel_name,
            channel_id,
        );
        assert_eq!(
            resolved.as_ref().map(|r| r.path.as_str()),
            Some(cwd),
            "a single legacy NULL-channel_id row must be reused for restart \
             restore via the #3216 safe fallback"
        );
        assert!(
            !resolved.as_ref().map(|r| r.channel_scoped).unwrap_or(true),
            "a NULL-channel_id legacy fallback must NOT be channel_scoped, so it \
             is never elevated over the configured base (#3219)"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // #3219: an EXACT channel-owned row whose `cwd` is NULL must still report
    // ownership (channel_scoped=true) with an empty path, so an owned channel
    // whose persisted cwd went stale/missing can still elevate the valid worktree
    // the tmux reconcile later supplies. Ownership comes from row EXISTENCE, not
    // the cwd value.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exact_owned_row_with_null_cwd_is_channel_scoped_empty_path() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3219-null-cwd";
        let channel_name = "null-cwd-owner";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let channel_id: u64 = 777_777_777_777_777_777;

        // Seed an exact channel-owned row with a NULL cwd.
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, cwd, channel_id, last_heartbeat)
             VALUES ($1, 'claude', 'idle', NULL, $2, NOW())",
        )
        .bind(&session_key)
        .bind(channel_id.to_string())
        .execute(&pool)
        .await
        .expect("seed NULL-cwd owned row");

        let resolved = restore_session_cwd_from_db(
            Some(&pool),
            token_hash,
            &provider,
            channel_name,
            channel_id,
        );
        assert!(
            resolved.as_ref().map(|r| r.channel_scoped).unwrap_or(false),
            "an exact channel-owned row must be channel_scoped even when its cwd \
             is NULL (#3219 ownership from row existence)"
        );
        assert_eq!(
            resolved.as_ref().map(|r| r.path.as_str()),
            Some(""),
            "a NULL cwd resolves to an empty path; the caller's usability filter \
             then drops it from db_cwd while preserving ownership"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // #3216 GAP 2: reconcile policy is a pure decision function; verify it picks
    // the live tmux cwd over a divergent DB cwd only when the tmux cwd is a real
    // managed/usable worktree, and is a no-op otherwise.
    #[test]
    fn reconcile_prefers_live_tmux_over_divergent_db_cwd() {
        let db = "/home/u/.adk/release/worktrees/claude-chan-PHANTOM-212437";
        let live = "/home/u/.adk/release/worktrees/claude-chan-LIVE-113822";
        // Divergent DB cwd, live tmux is managed + usable → adopt live.
        assert_eq!(
            super::reconcile_recovery_cwd(Some(db), Some(live), true, true).as_deref(),
            Some(live),
            "a divergent live tmux managed/usable cwd must be adopted as authoritative"
        );
    }

    #[test]
    fn reconcile_noop_when_db_matches_tmux() {
        let same = "/home/u/.adk/release/worktrees/claude-chan-SAME";
        assert_eq!(
            super::reconcile_recovery_cwd(Some(same), Some(same), true, true),
            None,
            "no reconcile when DB cwd already equals the live tmux cwd"
        );
    }

    #[test]
    fn reconcile_refused_when_tmux_cwd_not_managed_or_unusable() {
        let db = "/home/u/.adk/release/worktrees/claude-chan-DB";
        let transient = "/tmp/some-transient-path";
        // Not managed → refuse even though it differs.
        assert_eq!(
            super::reconcile_recovery_cwd(Some(db), Some(transient), false, true),
            None,
            "an unmanaged tmux cwd must never override the DB cwd"
        );
        // Managed but not usable on disk → refuse.
        assert_eq!(
            super::reconcile_recovery_cwd(Some(db), Some(transient), true, false),
            None,
            "an unusable tmux cwd must never override the DB cwd"
        );
        // No live tmux pane → refuse.
        assert_eq!(
            super::reconcile_recovery_cwd(Some(db), None, false, false),
            None,
            "absent live tmux must keep existing behavior"
        );
    }

    // #3216 GAP 2: end-to-end DB correction — after reconcile, the DB row's cwd
    // is rewritten to the live tmux path and the channel_id is stamped (so a
    // legacy NULL row both adopts the right cwd and self-heals onto this channel).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_corrects_db_cwd_and_stamps_channel() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let token_hash = "tok-3216-reconcile";
        let channel_name = "reconcile-chan";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key = build_namespaced_session_key(token_hash, &provider, &tmux_name);
        let channel_id: u64 = 999_999_999_999_999_999;
        let phantom = "/home/u/.adk/release/worktrees/claude-reconcile-PHANTOM";
        let live = "/home/u/.adk/release/worktrees/claude-reconcile-LIVE";

        // Legacy NULL-channel_id row pointing at the phantom (transcript-less) cwd.
        seed_session(&pool, &session_key, None, phantom).await;

        super::correct_session_cwd_to_tmux(
            Some(&pool),
            token_hash,
            &provider,
            channel_name,
            channel_id,
            live,
        );

        let (cwd, stamped): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT cwd, channel_id FROM sessions WHERE session_key = $1")
                .bind(&session_key)
                .fetch_one(&pool)
                .await
                .expect("fetch reconciled row");
        assert_eq!(
            cwd.as_deref(),
            Some(live),
            "DB cwd must be corrected to live tmux cwd"
        );
        assert_eq!(
            stamped.as_deref(),
            Some(channel_id.to_string().as_str()),
            "the reconciled row must be stamped with this channel_id (self-heal)"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
