use super::*;

#[rustfmt::skip]
impl TmuxWatcherRegistry {
    pub(in crate::services::discord) fn new() -> Self {
        Self {
            by_tmux_session: dashmap::DashMap::new(),
            tmux_session_by_channel: dashmap::DashMap::new(),
            owner_channel_by_tmux_session: dashmap::DashMap::new(),
            restored_owner_by_tmux_session: dashmap::DashMap::new(),
        }
    }

    pub(in crate::services::discord) fn len(&self) -> usize {
        self.by_tmux_session.len()
    }

    pub(in crate::services::discord) fn contains_key(&self, channel_id: &ChannelId) -> bool {
        self.channel_binding(channel_id)
            .and_then(|binding| self.by_tmux_session.get(&binding.tmux_session_name))
            .is_some()
    }

    pub(in crate::services::discord) fn get(
        &self,
        channel_id: &ChannelId,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, TmuxWatcherHandle>> {
        let tmux_session_name = self.tmux_session_by_channel.get(channel_id)?.clone();
        self.by_tmux_session.get(&tmux_session_name)
    }

    // #3034: test-only convenience wrapper (prod code calls `insert_locked`
    // with an explicit registry guard). Used only by `#[cfg(test)]` setup.
    #[allow(dead_code)]
    pub(in crate::services::discord) fn insert(
        &self,
        channel_id: ChannelId,
        handle: TmuxWatcherHandle,
    ) -> Option<TmuxWatcherHandle> {
        let guard = lock_tmux_watcher_registry();
        self.insert_locked(&guard, channel_id, handle)
    }

    pub(in crate::services::discord) fn insert_locked(
        &self,
        _guard: &TmuxWatcherRegistryGuard,
        channel_id: ChannelId,
        handle: TmuxWatcherHandle,
    ) -> Option<TmuxWatcherHandle> {
        if let Some((_, old_tmux_session_name)) = self.tmux_session_by_channel.remove(&channel_id) {
            self.owner_channel_by_tmux_session
                .remove(&old_tmux_session_name);
            self.by_tmux_session.remove(&old_tmux_session_name);
        }

        let tmux_session_name = handle.tmux_session_name.clone();
        if let Some((_, old_owner_channel_id)) = self
            .owner_channel_by_tmux_session
            .remove(&tmux_session_name)
        {
            self.tmux_session_by_channel.remove(&old_owner_channel_id);
        }

        // #3105: a live watcher handle is now the authoritative owner for this
        // session — drop any restored owner-only binding so it can never shadow
        // or contradict live truth.
        self.restored_owner_by_tmux_session
            .remove(&tmux_session_name);

        self.tmux_session_by_channel
            .insert(channel_id, tmux_session_name.clone());
        self.owner_channel_by_tmux_session
            .insert(tmux_session_name.clone(), channel_id);
        self.by_tmux_session.insert(tmux_session_name, handle)
    }

    pub(in crate::services::discord) fn remove(&self, channel_id: &ChannelId) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let guard = lock_tmux_watcher_registry();
        self.remove_locked(&guard, channel_id)
    }

    pub(in crate::services::discord) fn remove_locked(
        &self,
        _guard: &TmuxWatcherRegistryGuard,
        channel_id: &ChannelId,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let (_, tmux_session_name) = self.tmux_session_by_channel.remove(channel_id)?;
        self.owner_channel_by_tmux_session
            .remove(&tmux_session_name);
        self.by_tmux_session
            .remove(&tmux_session_name)
            .map(|(_, handle)| (*channel_id, handle))
    }

    pub(in crate::services::discord) fn remove_tmux_session_locked(
        &self,
        _guard: &TmuxWatcherRegistryGuard,
        tmux_session_name: &str,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let (_, owner_channel_id) = self
            .owner_channel_by_tmux_session
            .remove(tmux_session_name)?;
        self.tmux_session_by_channel.remove(&owner_channel_id);
        self.by_tmux_session
            .remove(tmux_session_name)
            .map(|(_, handle)| (owner_channel_id, handle))
    }

    pub(in crate::services::discord) fn remove_tmux_session_if_current(
        &self,
        tmux_session_name: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let guard = lock_tmux_watcher_registry();
        let is_current = self
            .by_tmux_session
            .get(tmux_session_name)
            .is_some_and(|entry| Arc::ptr_eq(&entry.cancel, expected_cancel));
        if !is_current {
            return None;
        }
        self.remove_tmux_session_locked(&guard, tmux_session_name)
    }

    pub(in crate::services::discord) fn cancel_and_remove_channel_if_current(
        &self,
        channel_id: &ChannelId,
        expected_tmux_session_name: &str,
        expected_output_path: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> bool {
        let guard = lock_tmux_watcher_registry();
        let Some(tmux_session_name) = self
            .tmux_session_by_channel
            .get(channel_id)
            .map(|entry| entry.clone())
        else {
            return false;
        };
        if tmux_session_name != expected_tmux_session_name {
            return false;
        }
        let matches_current = self
            .by_tmux_session
            .get(&tmux_session_name)
            .is_some_and(|entry| {
                entry.output_path == expected_output_path
                    && Arc::ptr_eq(&entry.cancel, expected_cancel)
            });
        if !matches_current {
            return false;
        }
        let Some((_, handle)) = self.remove_locked(&guard, channel_id) else {
            return false;
        };
        handle
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        true
    }

    pub(in crate::services::discord) fn iter(&self) -> dashmap::iter::Iter<'_, String, TmuxWatcherHandle> {
        self.by_tmux_session.iter()
    }

    pub(in crate::services::discord) fn channel_binding(&self, channel_id: &ChannelId) -> Option<TmuxWatcherBinding> {
        let tmux_session_name = self.tmux_session_by_channel.get(channel_id)?.clone();
        let owner_channel_id = self
            .owner_channel_by_tmux_session
            .get(&tmux_session_name)
            .map(|entry| *entry.value())
            .unwrap_or(*channel_id);
        Some(TmuxWatcherBinding {
            owner_channel_id,
            tmux_session_name,
        })
    }

    pub(in crate::services::discord) fn owner_channel_for_tmux_session(
        &self,
        tmux_session_name: &str,
    ) -> Option<ChannelId> {
        // The live watcher-handle binding is the primary authority. When no live
        // watcher owns the session (e.g. a TUI-direct session whose slot was
        // evicted by compact/restart/rebind), fall back to the #3105 restored
        // owner map — still authoritative (settings-derived), unlike the dedupe
        // mirror, which is never consulted here.
        self.owner_channel_by_tmux_session
            .get(tmux_session_name)
            .map(|entry| *entry.value())
            .or_else(|| {
                self.restored_owner_by_tmux_session
                    .get(tmux_session_name)
                    .map(|entry| *entry.value())
            })
    }

    /// #3105: re-register the authoritative owner channel for a LIVE tmux
    /// session that currently has no live watcher handle.
    ///
    /// This is the self-heal path for the permanent relay drop described in
    /// #3105: the idle transcript relay resolves a session's owner channel
    /// deterministically from the configured channel→provider bindings (which
    /// handle both base and thread-suffixed tmux names) and promotes that
    /// evidence into the authoritative registry here, instead of routing from
    /// the `tui_prompt_dedupe` mirror (which #3018 forbids as a reverse
    /// authority).
    ///
    /// No-ops when a live watcher already owns the session (live truth wins) or
    /// when the binding is unchanged. Returns `true` only on the first/changed
    /// registration so callers can emit a single bounded incident instead of a
    /// per-poll log.
    pub(in crate::services::discord) fn restore_owner_channel_for_tmux_session(
        &self,
        tmux_session_name: &str,
        channel_id: ChannelId,
    ) -> bool {
        let _guard = lock_tmux_watcher_registry();
        if self
            .owner_channel_by_tmux_session
            .contains_key(tmux_session_name)
        {
            // A live watcher handle already owns this session authoritatively.
            self.restored_owner_by_tmux_session
                .remove(tmux_session_name);
            return false;
        }
        let changed = self
            .restored_owner_by_tmux_session
            .get(tmux_session_name)
            .map(|entry| *entry.value())
            != Some(channel_id);
        self.restored_owner_by_tmux_session
            .insert(tmux_session_name.to_string(), channel_id);
        changed
    }

    /// #3105: drop a restored owner-only binding (e.g. once the session is no
    /// longer live). Idempotent.
    pub(in crate::services::discord) fn clear_restored_owner_for_tmux_session(&self, tmux_session_name: &str) {
        self.restored_owner_by_tmux_session
            .remove(tmux_session_name);
    }

    /// Preserve authoritative routing for a live tmux pane after the channel's
    /// provider session is rebound. Existing watcher ownership already wins; an
    /// owner-only entry covers watcher teardown until pane death is confirmed.
    pub(in crate::services::discord) fn retain_owner_during_session_rebind(
        &self,
        tmux_session_name: &str,
        channel_id: ChannelId,
    ) -> bool {
        let _guard = lock_tmux_watcher_registry();
        let tmux_session_name = tmux_session_name.trim();
        if tmux_session_name.is_empty() {
            return false;
        }
        if let Some(current_owner) = self.owner_channel_by_tmux_session.get(tmux_session_name)
            && *current_owner.value() != channel_id
        {
            return false;
        }
        self.restored_owner_by_tmux_session
            .insert(tmux_session_name.to_string(), channel_id);
        true
    }

    /// #3105 (codex P1 sub-case B): true when a LIVE watcher handle currently
    /// owns this tmux session. Used to distinguish a genuinely dead/orphaned
    /// session (no live watcher) from a live session whose authoritative owner
    /// map entry was transiently evicted (which must self-heal, not be tombstoned).
    pub(in crate::services::discord) fn has_live_watcher_handle(&self, tmux_session_name: &str) -> bool {
        self.by_tmux_session.contains_key(tmux_session_name)
    }

    pub(in crate::services::discord) fn tmux_session_is_stale(&self, tmux_session_name: &str) -> Option<bool> {
        self.by_tmux_session
            .get(tmux_session_name)
            .map(|entry| entry.heartbeat_stale())
    }

    pub(in crate::services::discord) fn tmux_session_live_for_relay(&self, tmux_session_name: &str) -> Option<bool> {
        self.by_tmux_session.get(tmux_session_name).map(|entry| {
            !entry.cancel.load(std::sync::atomic::Ordering::Relaxed) && !entry.heartbeat_stale()
        })
    }

    /// #2843: the output path the live watcher (if any) is tailing for this tmux
    /// session. The Claude idle relay uses this to decide whether a non-stale
    /// watcher genuinely covers the freshest transcript (and thus already relays
    /// it). Comparing against the runtime *binding* is wrong: re-registering the
    /// binding does not retarget the already-running watcher, so the binding and
    /// the watcher can point at different files.
    pub(in crate::services::discord) fn watcher_output_path(&self, tmux_session_name: &str) -> Option<String> {
        self.by_tmux_session
            .get(tmux_session_name)
            .map(|entry| entry.output_path.clone())
    }
}
