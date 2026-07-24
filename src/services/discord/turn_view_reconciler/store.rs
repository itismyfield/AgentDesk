use super::*;

impl TurnViewReconciler {
    pub(super) fn target_lock(&self, target: TurnViewTarget) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .target_locks
            .lock()
            .expect("turn view target lock registry");
        locks
            .entry(target)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub(super) fn discard_target_locked(
        &self,
        target: TurnViewTarget,
        source: &'static str,
        target_lock: &Arc<tokio::sync::Mutex<()>>,
    ) {
        self.finish_target_locked(target, source, target_lock, true);
    }

    pub(super) fn finalize_target_locked(
        &self,
        target: TurnViewTarget,
        finalized_generation: u64,
        source: &'static str,
        target_lock: &Arc<tokio::sync::Mutex<()>>,
    ) {
        self.remember_recently_finalized(target, finalized_generation);
        self.finish_target_locked(target, source, target_lock, false);
    }

    pub(super) fn recently_finalized_blocks_queued(
        &self,
        target: TurnViewTarget,
        generation: u64,
    ) -> bool {
        self.recently_finalized
            .lock()
            .expect("turn view recently finalized guard")
            .blocks_queued(target, generation)
    }

    pub(super) fn remember_recently_finalized(&self, target: TurnViewTarget, generation: u64) {
        self.recently_finalized
            .lock()
            .expect("turn view recently finalized guard")
            .remember(target, generation);
    }

    pub(super) fn finish_target_locked(
        &self,
        target: TurnViewTarget,
        source: &'static str,
        target_lock: &Arc<tokio::sync::Mutex<()>>,
        force_remove_target: bool,
    ) {
        self.delete_persisted_target(target, source);
        let mut locks = self
            .target_locks
            .lock()
            .expect("turn view target lock registry");
        let prune_lock = locks.get(&target).is_some_and(|registered| {
            Arc::ptr_eq(registered, target_lock) && Arc::strong_count(registered) == 2
        });
        if force_remove_target || prune_lock {
            self.targets.remove(&target);
        }
        if prune_lock {
            locks.remove(&target);
        }
    }

    pub(super) fn prune_target_lock_if_idle(&self, target: TurnViewTarget) {
        let mut locks = self
            .target_locks
            .lock()
            .expect("turn view target lock registry");
        let remove = locks
            .get(&target)
            .is_some_and(|registered| Arc::strong_count(registered) == 1);
        if remove {
            locks.remove(&target);
        }
    }

    pub(super) fn resolve_identity(
        &self,
        shared: &SharedData,
        target_kind: TurnViewTargetKind,
        identity: TurnViewIdentity,
        source: &'static str,
    ) -> Option<ResolvedIdentity> {
        #[cfg(test)]
        let _ = (shared, target_kind, source);
        match identity {
            TurnViewIdentity::IntakeHttp(http) => {
                #[cfg(test)]
                let _ = &http;
                Some(ResolvedIdentity {
                    label: TurnViewTargetKind::IntakeUserMessage
                        .identity_label()
                        .to_string(),
                    token_hash: Some(shared.token_hash.clone()),
                    #[cfg(not(test))]
                    http,
                })
            }
            TurnViewIdentity::IntakeShared => {
                #[cfg(not(test))]
                {
                    let Some(http) = shared.serenity_http_or_token_fallback() else {
                        tracing::warn!(
                            target_kind = ?target_kind,
                            source,
                            "turn view reaction skipped; intake serenity http unavailable"
                        );
                        return None;
                    };
                    Some(ResolvedIdentity {
                        label: TurnViewTargetKind::IntakeUserMessage
                            .identity_label()
                            .to_string(),
                        token_hash: Some(shared.token_hash.clone()),
                        http,
                    })
                }
                #[cfg(test)]
                {
                    let _ = shared;
                    Some(ResolvedIdentity {
                        label: TurnViewTargetKind::IntakeUserMessage
                            .identity_label()
                            .to_string(),
                        token_hash: Some(shared.token_hash.clone()),
                    })
                }
            }
            TurnViewIdentity::ProviderBot => {
                #[cfg(not(test))]
                {
                    let Some(http) = shared.serenity_http_or_token_fallback() else {
                        tracing::warn!(
                            target_kind = ?target_kind,
                            source,
                            "turn view reaction skipped; provider serenity http unavailable"
                        );
                        return None;
                    };
                    Some(ResolvedIdentity {
                        label: TurnViewTargetKind::TuiDirectBotAnchor
                            .identity_label()
                            .to_string(),
                        token_hash: Some(shared.token_hash.clone()),
                        http,
                    })
                }
                #[cfg(test)]
                {
                    let _ = shared;
                    Some(ResolvedIdentity {
                        label: TurnViewTargetKind::TuiDirectBotAnchor
                            .identity_label()
                            .to_string(),
                        token_hash: Some(shared.token_hash.clone()),
                    })
                }
            }
            #[cfg(test)]
            TurnViewIdentity::Test(label) => {
                let _ = (shared, target_kind, source);
                Some(ResolvedIdentity {
                    label: label.to_string(),
                    token_hash: None,
                })
            }
        }
    }

    pub(super) fn resolve_persisted_identity(
        &self,
        record: &PersistedTargetState,
        shared: &SharedData,
        source: &'static str,
    ) -> Option<ResolvedIdentity> {
        #[cfg(not(test))]
        {
            let http = match record.token_hash.as_deref() {
                Some(token_hash) if token_hash != shared.token_hash => {
                    match settings::resolve_discord_token_by_hash(token_hash) {
                        Some(token) => Arc::new(serenity::http::Http::new(&token)),
                        None => {
                            tracing::warn!(
                                token_hash,
                                source,
                                "turn view persisted reaction identity token hash could not be resolved; falling back to current runtime identity"
                            );
                            shared.serenity_http_or_token_fallback()?
                        }
                    }
                }
                _ => shared.serenity_http_or_token_fallback()?,
            };
            Some(ResolvedIdentity {
                label: record.identity_label.clone(),
                token_hash: record.token_hash.clone(),
                http,
            })
        }
        #[cfg(test)]
        {
            let _ = (shared, source);
            Some(ResolvedIdentity {
                label: record.identity_label.clone(),
                token_hash: record.token_hash.clone(),
            })
        }
    }

    pub(super) fn persisted_target_path(target: TurnViewTarget) -> Option<PathBuf> {
        runtime_store::discord_turn_view_reconciler_root().map(|root| {
            root.join(target.kind.as_str()).join(format!(
                "{}-{}.json",
                target.channel_id.get(),
                target.message_id.get()
            ))
        })
    }

    pub(super) fn load_persisted_target(
        &self,
        target: TurnViewTarget,
        shared: &SharedData,
        source: &'static str,
    ) -> Option<AppliedTarget> {
        let path = Self::persisted_target_path(target)?;
        let text = fs::read_to_string(&path).ok()?;
        let record = match serde_json::from_str::<PersistedTargetState>(&text) {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    source,
                    "turn view persisted reaction state was malformed; deleting"
                );
                let _ = fs::remove_file(&path);
                return None;
            }
        };
        if !matches!(
            record.version,
            PERSISTED_STATE_VERSION
                | LEGACY_QUEUED_HOURGLASS_STATE_VERSION
                | QUEUED_MARKER_ONLY_STATE_VERSION
        ) || record.provider != shared.provider.as_str()
            || TurnViewTargetKind::from_str(&record.kind) != Some(target.kind)
            || record.channel_id != target.channel_id.get()
            || record.message_id != target.message_id.get()
        {
            tracing::warn!(
                path = %path.display(),
                version = record.version,
                provider = %record.provider,
                kind = %record.kind,
                channel_id = record.channel_id,
                message = record.message_id,
                source,
                "turn view persisted reaction state did not match target; deleting"
            );
            let _ = fs::remove_file(&path);
            return None;
        }
        let Some(recorded_applied) = TurnViewState::from_str(&record.applied) else {
            tracing::warn!(
                path = %path.display(),
                applied = %record.applied,
                source,
                "turn view persisted reaction state had unknown applied value; deleting"
            );
            let _ = fs::remove_file(&path);
            return None;
        };
        if recorded_applied == TurnViewState::None {
            let _ = fs::remove_file(&path);
            return None;
        }
        let identity = self.resolve_persisted_identity(&record, shared, source)?;
        let legacy_queue_reactions = match record.version {
            PERSISTED_STATE_VERSION if recorded_applied.is_queue_marker() => {
                vec![reaction_set::for_state(recorded_applied)[0]]
            }
            LEGACY_QUEUED_HOURGLASS_STATE_VERSION if recorded_applied.is_queue_marker() => vec![
                reaction_set::for_state(recorded_applied)[0],
                reaction_set::for_state(TurnViewState::Pending)[0],
            ],
            _ => Vec::new(),
        };
        let applied = if legacy_queue_reactions.is_empty() {
            recorded_applied
        } else {
            TurnViewState::None
        };
        let mut target = Self::applied_target(
            TurnViewOwner::new(record.owner_generation, record.owner_turn_id),
            applied,
            identity,
            record.start_attempt_id.map(TurnStartAttempt),
        );
        target.legacy_queue_reactions = legacy_queue_reactions;
        Some(target)
    }

    pub(super) fn persist_target(
        &self,
        target: TurnViewTarget,
        applied: &AppliedTarget,
        shared: &SharedData,
        source: &'static str,
    ) {
        if applied.applied == TurnViewState::None && applied.legacy_queue_reactions.is_empty() {
            self.delete_persisted_target(target, source);
            return;
        }
        let Some(path) = Self::persisted_target_path(target) else {
            return;
        };
        let applied_state = applied
            .legacy_queue_reactions
            .iter()
            .find_map(|emoji| TurnViewState::from_queue_marker_emoji(*emoji))
            .unwrap_or(applied.applied);
        let record = PersistedTargetState {
            version: if applied.applied.is_queue_marker()
                || !applied.legacy_queue_reactions.is_empty()
            {
                QUEUED_MARKER_ONLY_STATE_VERSION
            } else {
                PERSISTED_STATE_VERSION
            },
            provider: shared.provider.as_str().to_string(),
            kind: target.kind.as_str().to_string(),
            channel_id: target.channel_id.get(),
            message_id: target.message_id.get(),
            owner_generation: applied.owner.generation,
            owner_turn_id: applied.owner.turn_id.clone(),
            applied: applied_state.as_str().to_string(),
            identity_label: applied.identity.label.clone(),
            token_hash: applied.identity.token_hash.clone(),
            start_attempt_id: applied.start_attempt.map(TurnStartAttempt::get),
        };
        let Ok(json) = serde_json::to_string_pretty(&record) else {
            return;
        };
        if let Err(error) = runtime_store::atomic_write(&path, &json) {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                source,
                "turn view persisted reaction state write failed"
            );
        }
    }

    pub(super) fn delete_persisted_target(&self, target: TurnViewTarget, source: &'static str) {
        let Some(path) = Self::persisted_target_path(target) else {
            return;
        };
        if let Err(error) = fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                source,
                "turn view persisted reaction state delete failed"
            );
        }
    }
}
