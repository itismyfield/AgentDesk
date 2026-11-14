use super::*;

#[cfg(unix)]
#[path = "native_policy.rs"]
pub(super) mod native_policy;
#[path = "policy_commit.rs"]
mod policy_commit;
pub(super) use policy_commit::PolicyCommit;
pub(super) async fn handle_terminal(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    pending_admission: &mut HashMap<LedgerKey, PendingCompletionAdmission>,
    key: TurnKey,
    provider: ProviderKind,
    event: TerminalEvent,
    ctx: FinalizeContext,
    evidence: TerminalEvidence,
    shared: &Arc<SharedData>,
) -> FinalizeOutcome {
    // Monitor producers carry an existing per-turn generation. Resolve only
    // its registered episode; a late/collected generation owns no current slot.
    let key = if key.episode.is_none() && key.generation != shared.restart.current_generation {
        let mut matches = ledger.values().filter(|entry| {
            entry.turn_key.channel_id == key.channel_id
                && entry.turn_key.user_msg_id == key.user_msg_id
                && entry.turn_key.generation == key.generation
        });
        let Some(entry) = matches.next() else {
            return FinalizeOutcome::AlreadyFinalized;
        };
        if matches.next().is_some() || entry.phase == Phase::Finalized {
            return FinalizeOutcome::AlreadyFinalized;
        }
        entry.turn_key
    } else {
        key
    };
    let released = if let TerminalEvent::OperatorRelease(release) = &event {
        let Some(finish) = release.claim(shared, &provider, key).await else {
            return FinalizeOutcome::AlreadyFinalized;
        };
        Some(finish)
    } else {
        None
    };
    // A producer lacking episode evidence cannot borrow a known successor's
    // ledger, nor enter the legacy AlreadyFinalized repair path.
    if key.episode.is_none()
        && ledger.keys().any(|known| {
            known.channel_id == key.channel_id
                && known.generation == key.generation
                && known.user_msg_id == key.user_msg_id
                && known.episode.is_some()
        })
    {
        return FinalizeOutcome::Deferred;
    }
    let claim_snapshot = evidence.claim_snapshot;
    let probed_tmux = evidence.probed_tmux_session;
    // #3866: test-only injection point — lets a test drive a real finalize
    // side-effect panic through the live actor loop to prove the catch_unwind
    // guard keeps the loop alive. No effect in production builds.
    #[cfg(test)]
    test_panic_hook::maybe_panic();

    // Resolve to the entry this terminal acts on: a real id keys exactly; a
    // channel-only id-0 collapses onto the channel's single live entry
    // (recovery/orphan). An unregistered turn (post-restart inflight, no live
    // `Start`) still finalizes below — idempotent, exactly-once.
    let ledger_key = resolve_ledger_key(ledger, key);
    #[cfg(unix)]
    if matches!(
        event,
        TerminalEvent::Complete | TerminalEvent::GateTimeout { .. }
    ) && ledger
        .get(&ledger_key)
        .is_some_and(|entry| entry.completion_admission.native_input.is_some())
    {
        return FinalizeOutcome::Deferred;
    }

    // Codex P1 — ambiguous channel-only terminal: an id-0 submission that fell
    // back to the literal orphan key (a recently-`Finalized` entry exists) is
    // most likely that turn's STALE terminal. With a DIFFERENT live entry on
    // this channel, the channel-scoped finish would release the follow-up's
    // token / decrement `global_active` — treat as no-op. (A genuine orphan
    // with NO live entry still finalizes below; idempotent + counter-gated.)
    if key.user_msg_id == 0 && !ledger.contains_key(&ledger_key) {
        let channel_has_live_turn = ledger.iter().any(|(lk, e)| {
            lk.channel_id == key.channel_id
                && lk.generation == key.generation
                && e.phase != Phase::Finalized
        });
        if channel_has_live_turn {
            return FinalizeOutcome::AlreadyFinalized;
        }
    }

    let pending = take_exact_pending_completion_admission(pending_admission, ledger_key);
    let entry = ledger
        .entry(ledger_key)
        .or_insert_with(|| LedgerEntry::orphan(key, provider));
    apply_pending_completion_admission(entry, pending);

    // A newly CAS-released lease may share a previously finalized episode.
    // Reopen only its phase; retain its delivery evidence and admission plan.
    if released.is_some() {
        entry.phase = Phase::Pending;
    }

    match entry.phase {
        Phase::Finalizing | Phase::Finalized => {
            return FinalizeOutcome::AlreadyFinalized;
        }
        Phase::Pending => {}
    }

    // #3646 OBSERVATION-ONLY (finalizer_ledger_owner companion): emit the
    // actor-owned ledger entry's `relay_owner` — the SECOND owner signal the
    // watcher-side `terminal_body_commit` event cannot read (the ledger lives on
    // this actor task; a synchronous cross-task query from the watcher would be
    // new behaviour). Keyed on the same `discord:<channel>:<user_msg_id>` turn id
    // so the two owner signals JOIN in PG and the #3607 "None-ledger vs
    // Watcher-finalize" ambiguity resolves. Read-only: it neither inspects nor
    // changes the clear_inflight / defer / finalize decision that follows.
    //
    // codex review #3678: key on the RESOLVED entry identity (`entry.turn_key`),
    // NOT the submitted `key`. A channel-only id-0 terminal collapses onto the
    // channel's real registered turn via `resolve_ledger_key`; that entry's
    // `turn_key` carries the real `user_msg_id`. Keying on the submitted `key`
    // would emit `user_msg_id=0` for exactly those collapsed terminals, dropping
    // the turn_id and breaking the JOIN against the watcher event — the #3607
    // cases this signal exists to disambiguate. Genuine orphans (id-0 with no
    // live entry) still carry id-0 here, which is correct (no real turn exists).
    super::super::relay_owner_observability::emit_finalizer_ledger_owner(
        entry.provider.as_str(),
        entry.turn_key.channel_id.get(),
        entry.turn_key.user_msg_id,
        entry.relay_owner.as_str(),
        terminal_event_kind_str(&event),
        ctx.clear_inflight,
    );

    // Gate-timeout with a still-busy pane AND a live relay owner is the only
    // event that defers instead of finalizing now.
    let mut effective_ctx = ctx;
    if let TerminalEvent::GateTimeout {
        pane_quiescent: Some(false),
    } = event
    {
        if entry.relay_owner != RelayOwnerKind::None {
            // Arm the backstop deadline ONCE. The watcher submits a
            // GateTimeout on every pass while the pane stays busy; if each
            // submission pushed the deadline forward by GATE_BACKSTOP the
            // backstop would never fire on a persistently busy pane (exactly
            // the never-finalizing bug). So only set it if not already armed.
            entry
                .terminal_deadline
                .get_or_insert_with(|| Instant::now() + GATE_BACKSTOP);
            return FinalizeOutcome::Deferred;
        }
        // No live relay owner → nothing will drive the pane to quiescence;
        // finalize now. This recovered/orphan watcher case (post-restart
        // inflight, no `register_start`) has no later watcher block to clear
        // inflight — the caller's `watcher()` submit SKIPS its cleanup block and
        // discards this outcome, so reproduce the deadline-armed
        // `gate_backstop()` context shape: clear inflight here (else the file
        // keeps blocking the channel after the mailbox release) and preserve the
        // queue-admission bit; actual drain is the #4048 `do_finalize` event.
        effective_ctx.clear_inflight = true;
        effective_ctx.kickoff_queue = true;
    }

    // Flip Pending → Finalizing, run the side-effects, flip → Finalized.
    entry.phase = Phase::Finalizing;
    let provider = entry.provider.clone();
    // Codex P1 — finalize on the RESOLVED identity: an id-0 terminal that
    // collapsed onto an entry registered with the real `user_msg_id` finalizes
    // under THAT identity, so `do_finalize` takes the guarded if-matches paths
    // instead of the unguarded channel-scoped finish (which could release a
    // newer turn's token when the entry is stale). Otherwise the same key.
    let finalize_key = if key.user_msg_id == 0 && entry.turn_key.user_msg_id != 0 {
        entry.turn_key
    } else {
        key
    };
    // #3866/#4048: `do_finalize` is the single chokepoint for finalize
    // side-effects (inflight clear, mailbox token release, `global_active`
    // decrement, completion-event publish). Contain a panic HERE
    // rather than only at the actor loop so the Finalizing->Finalized flip below
    // STILL runs on a caught panic.
    // That matters: the entry was just flipped to `Finalizing`, and reconcile GC
    // reaps only `Finalized` while every backstop/probe gates on `Pending`, so an
    // entry left stuck in `Finalizing` after a panic would leak FOREVER and
    // poison `ledger_has_live_watcher_pending` / `resolve_channel_only` for this
    // channel+generation. Resetting it to `Finalized` (the normal post-finalize
    // flip) lets GC reap it and frees the channel for the next turn.
    let exact_normal = released.is_none() && finalize_key.episode.is_some();
    let witness = super::episode::RuntimeWitness {
        probed_tmux: probed_tmux.as_deref(),
        claim_snapshot: claim_snapshot.as_ref(),
    };
    let outcome = match AssertUnwindSafe(super::finalize::do_finalize_with_probe(
        finalize_key,
        provider,
        &event,
        effective_ctx,
        witness,
        shared,
        (released, false),
    ))
    .catch_unwind()
    .await
    {
        Ok(outcome) => {
            if event.is_mute_settlement() {
                entry.completion_admission.operator_released = true;
                entry.completion_admission.queue_eligible_published = false;
            }
            note_mailbox_release_after_finalize(&outcome, entry, shared);
            // Only an exact finish that removed this episode's token proves its release.
            let removed = matches!(
                &outcome,
                FinalizeOutcome::Finalized {
                    removed_token: Some(_),
                    ..
                }
            );
            entry.exact_release_confirmed |= exact_normal && removed;
            outcome
        }
        Err(payload) => {
            tracing::error!(
                panic = %panic_payload_summary(payload.as_ref()),
                channel_id = ledger_key.channel_id.get(),
                user_msg_id = ledger_key.user_msg_id,
                "TurnFinalizer do_finalize panicked on the terminal path; contained, the \
                 ledger entry is reset Finalizing->Finalized below so it is never stuck (#3866)"
            );
            FinalizeOutcome::AlreadyFinalized
        }
    };
    if let Some(entry) = ledger.get_mut(&ledger_key) {
        entry.phase = Phase::Finalized;
        entry.finalized_at = Some(Instant::now());
        entry.terminal_deadline = None;
    }
    outcome
}

impl LedgerEntry {
    /// A terminal-first entry with no prior `register_start`: no borrowed takeover lease, and it
    /// finalizes right away, so it never needs the watcher far-backstop.
    fn orphan(key: TurnKey, provider: ProviderKind) -> Self {
        let immediate = CompletionAdmissionPlan::Immediate;
        Self::registered(key, provider, RelayOwnerKind::None, None, immediate, None)
    }
}

/// The finalizer half of a policy close: the exact entry owns its commit until the tail settles,
/// and anything short of that restores the entry so the close stays retryable.
pub(super) async fn handle_policy_close(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    mut intent: super::episode::PolicyCloseIntent,
    shared: &Arc<SharedData>,
) -> PolicyCloseOutcome {
    let key = intent.key;
    if key.episode.is_none() || key.generation != shared.restart.current_generation {
        return PolicyCloseOutcome::Superseded;
    }
    let exact = key.exact_key();
    let entry = ledger
        .entry(exact)
        .or_insert_with(|| LedgerEntry::orphan(key, intent.provider.clone()));
    let prior = std::mem::replace(&mut entry.phase, Phase::Finalizing);
    // The fence only refuses episodes older than the latest release, so the receipt-only grant
    // also needs this exact key and a release of its own token.
    let request = &intent.request;
    let (message, nonce) = (request.user_message_id, request.turn_nonce.as_str());
    let named = key.user_msg_id == message.get() && key.matches_episode_nonce(Some(nonce));
    let reused = (entry.pending_policy_commit.as_ref())
        .filter(|commit| commit.ticket().names(message, nonce))
        .cloned();
    intent.request.release_reconcile_allowed =
        named && prior == Phase::Finalized && entry.exact_release_confirmed;
    let commit = match reused {
        Some(commit) => commit,
        _ => entry
            .pending_policy_commit
            .insert(PolicyCommit::new(&intent))
            .clone(),
    };
    intent.request.ticket = Some(commit.ticket());
    let settled = AssertUnwindSafe(policy_commit::settle(intent, &commit, shared))
        .catch_unwind()
        .await;
    let (outcome, released) = settled.unwrap_or((
        PolicyCloseOutcome::Unconfirmed(PolicyStage::FinalizerPanic),
        None,
    ));
    if let Some(entry) = ledger.get_mut(&exact) {
        if matches!(
            outcome,
            PolicyCloseOutcome::Settled | PolicyCloseOutcome::DuplicateConfirmed
        ) {
            // A finalized entry keeps its age, so a confirmation never widens the reconcile window.
            if prior != Phase::Finalized {
                entry.finalized_at = Some(Instant::now());
            }
            entry.phase = Phase::Finalized;
            entry.terminal_deadline = None;
            entry.exact_release_confirmed |= matches!(outcome, PolicyCloseOutcome::Settled);
            if let Some(commit) = entry.pending_policy_commit.take() {
                commit.release();
            }
        } else {
            entry.phase = prior;
        }
        if let Some(released) = released.as_ref() {
            note_mailbox_release_after_finalize(released, entry, shared);
        }
    }
    outcome
}

/// An inactive native admission carries separate result and cleanup authority.
#[cfg(all(test, unix))]
#[derive(Clone)]
struct NativeAdmission {
    evidence: crate::services::tui_o::writer::input_facts::native::Evidence,
    registrations_fingerprint: String,
    cleanup: Option<(TurnKey, Arc<CancelToken>)>,
}

#[cfg(all(test, unix))]
impl NativeAdmission {
    fn capture(
        observed: &crate::services::tui_o::writer::input_facts::native::Observation,
        current: &crate::services::tui_o::writer::input_facts::native::Observation,
        index: usize,
        current_target: &serde_json::Value,
        key: TurnKey,
        captured_actor: Option<&Arc<CancelToken>>,
        current_actor: Option<&Arc<CancelToken>>,
    ) -> Result<Self, String> {
        use crate::services::tui_input::human_frame::history::v2::native::Disposition;
        if observed.binding != current.binding
            || observed.generation != current.generation
            || observed.binding_seq != current.binding_seq
            || observed.revision != current.revision
            || observed.through != current.through
            || observed.registrations_fingerprint != current.registrations_fingerprint
        {
            return Err("native observation superseded".into());
        }
        let evidence = observed
            .evidence
            .get(index)
            .ok_or("native candidate missing")?;
        let source = evidence
            .ticket
            .source
            .as_ref()
            .ok_or("native source missing")?;
        source.validate(&evidence.ticket.key)?;
        if observed.binding.provider != crate::services::tui_o::shadow::ShadowProvider::Claude
            || source.target != *current_target
            || evidence.namespace.is_empty()
            || evidence.ticket.channel_id != observed.binding.channel_id
            || source.provider != "claude"
            || source.session_id != observed.binding.source.session_id
            || source.transcript != observed.binding.source.path
            || source.file != [observed.binding.source.dev, observed.binding.source.ino]
            || evidence.range.0 < source.offset
            || evidence.range.0 >= evidence.range.1
            || evidence.range.1 > observed.through
        {
            return Err("native proof source/range mismatch".into());
        }
        match evidence.disposition {
            Disposition::Executed | Disposition::Cancelled => {
                let membership = evidence
                    .membership_range
                    .ok_or("native membership missing")?;
                let closer = evidence.closer_range.ok_or("native closer missing")?;
                if evidence.run.is_none()
                    || membership.0 < source.offset
                    || membership.0 >= membership.1
                    || membership.1 != evidence.range.0
                    || closer.0 < membership.1
                    || closer.0 >= closer.1
                    || closer.1 != evidence.range.1
                {
                    return Err("native run boundary mismatch".into());
                }
            }
            Disposition::WithdrawnNotExecuted | Disposition::TransferredUnverified => {
                if evidence.run.is_some()
                    || evidence.membership_range.is_some()
                    || evidence.closer_range.is_some()
                    || evidence.membership_row.is_some()
                {
                    return Err("native withdrawal carries run authority".into());
                }
            }
            Disposition::OperatorReleased => {
                return Err("native observation is not operator release".into());
            }
        }
        let cleanup = match &evidence.ticket.owning_episode {
            None => None,
            Some(episode) => {
                let actor = captured_actor.ok_or("native primary actor missing")?;
                if key.episode.is_none()
                    || key.channel_id.get() != evidence.ticket.channel_id
                    || key.user_msg_id != episode.user_message_id
                    || key.generation != episode.generation
                    || !key.matches_episode_nonce(Some(&episode.turn_nonce))
                    || actor.turn_nonce() != Some(episode.turn_nonce.as_str())
                    || !current_actor.is_some_and(|current| Arc::ptr_eq(current, actor))
                {
                    return Err("native primary episode/actor mismatch".into());
                }
                Some((key, actor.clone()))
            }
        };
        Ok(Self {
            evidence: evidence.clone(),
            registrations_fingerprint: observed.registrations_fingerprint.clone(),
            cleanup,
        })
    }
}

/// Explicit intent captures its own actor before keys; an empty lease never grants ownership.
#[cfg(unix)]
#[derive(Clone)]
pub(super) struct NativePrepared {
    history: crate::services::tui_input::human_frame::history::v2::Registry,
    namespace: String,
    ticket: crate::services::tui_input::human_frame::history::v2::native::Ticket,
    key: TurnKey,
    actor: Option<Arc<CancelToken>>,
    started_before: std::time::Instant,
}

#[cfg(unix)]
impl NativePrepared {
    pub(super) async fn capture(
        history: &crate::services::tui_input::human_frame::history::v2::Registry,
        namespace: &str,
        ticket: crate::services::tui_input::human_frame::history::v2::native::Ticket,
        key: TurnKey,
        actor: Option<Arc<CancelToken>>,
        shared: &Arc<SharedData>,
    ) -> Result<Self, String> {
        if history
            .native_settlement(namespace, &ticket)?
            .extra
            .get("not_sent")
            == Some(&serde_json::Value::Bool(true))
        {
            return Err("native preparation follows confirmed NotSent".into());
        }
        let started_before = std::time::Instant::now();
        if key.channel_id.get() != ticket.channel_id
            || ticket.key["identity"]["primary"] != key.user_msg_id
        {
            return Err("native pre-key identity mismatch".into());
        }
        match &ticket.owning_episode {
            None if actor.is_some() => return Err("native follower cannot borrow actor".into()),
            None => {}
            Some(episode) => {
                let expected = actor.as_ref().ok_or("native primary actor missing")?;
                let mailbox = shared
                    .mailbox_peek(key.channel_id)
                    .ok_or("native mailbox missing")?;
                let current = mailbox.snapshot().await;
                if key.generation != shared.restart.current_generation
                    || key.generation != episode.generation
                    || key.user_msg_id != episode.user_message_id
                    || !key.matches_episode_nonce(Some(&episode.turn_nonce))
                    || expected.turn_nonce() != Some(episode.turn_nonce.as_str())
                    || current.active_user_message_id.map(|id| id.get()) != Some(key.user_msg_id)
                    || !current
                        .cancel_token
                        .as_ref()
                        .is_some_and(|a| Arc::ptr_eq(a, expected))
                {
                    return Err("native pre-key actor mismatch".into());
                }
            }
        }
        Ok(Self {
            history: history.clone(),
            namespace: namespace.into(),
            ticket,
            key,
            actor,
            started_before,
        })
    }

    pub(super) fn owns_bridge(&self, key: TurnKey, actor: usize) -> bool {
        self.key.exact_key() == key.exact_key()
            && self
                .actor
                .as_ref()
                .is_some_and(|a| Arc::as_ptr(a) as usize == actor)
    }

    fn bound_ticket(
        &self,
        history: &crate::services::tui_input::human_frame::history::v2::Registry,
    ) -> Result<crate::services::tui_input::human_frame::history::v2::native::Ticket, String> {
        let current = history.native_ticket(&self.namespace, &self.ticket.key)?;
        if current.channel_id != self.ticket.channel_id
            || current.owning_episode != self.ticket.owning_episode
            || current.extra != self.ticket.extra
            || (self.ticket.source.is_some() && current.source != self.ticket.source)
        {
            return Err("native prepared ticket changed".into());
        }
        Ok(current)
    }

    pub(super) fn any_key(
        &self,
        history: &crate::services::tui_input::human_frame::history::v2::Registry,
    ) -> Result<(), String> {
        let ticket = self.bound_ticket(history)?;
        if history
            .native_settlement(&self.namespace, &ticket)?
            .extra
            .get("not_sent")
            == Some(&serde_json::Value::Bool(true))
        {
            return Err("native key follows confirmed NotSent".into());
        }
        history.note_native_any_key(&self.namespace, &ticket)?;
        tracing::debug!(
            channel_id = self.key.channel_id.get(),
            elapsed_ms = self.started_before.elapsed().as_millis(),
            "native key witness acknowledged"
        );
        Ok(())
    }

    #[cfg(all(test, unix))]
    async fn admit(
        &self,
        observed: &crate::services::tui_o::writer::input_facts::native::Observation,
        current: &crate::services::tui_o::writer::input_facts::native::Observation,
        index: usize,
        current_target: &serde_json::Value,
        history: &crate::services::tui_input::human_frame::history::v2::Registry,
        shared: &Arc<SharedData>,
    ) -> Result<NativeCleanup, String> {
        let registrations: Vec<_> = history
            .native_evidence_registrations()?
            .into_iter()
            .filter(|r| r.matches(&observed.binding))
            .collect();
        let fingerprint =
            crate::services::tui_o::writer::input_facts::native::registrations_fingerprint(
                &registrations,
            )?;
        if fingerprint != observed.registrations_fingerprint {
            return Err("native registrations superseded".into());
        }
        let ticket = self.bound_ticket(history)?;
        let settlement = history.native_settlement(&self.namespace, &ticket)?;
        if settlement.extra.get("any_key") != Some(&serde_json::Value::Bool(true)) {
            return Err("native original has no key receipt".into());
        }
        let current_actor = match shared.mailbox_peek(self.key.channel_id) {
            Some(mailbox) => mailbox.snapshot().await.cancel_token,
            None => None,
        };
        let admission = NativeAdmission::capture(
            observed,
            current,
            index,
            current_target,
            self.key,
            self.actor.as_ref(),
            current_actor.as_ref(),
        )?;
        if admission.evidence.namespace != self.namespace || admission.evidence.ticket != ticket {
            return Err("native candidate is not prepared input".into());
        }
        let row = self.actor.as_ref().and_then(|_| {
            super::super::inflight::load_inflight_state(
                &ProviderKind::Claude,
                self.key.channel_id.get(),
            )
        });
        NativeCleanup::capture(admission, row, self.started_before)
    }
}

/// A captured native result retains its row debt until the snapshot CAS succeeds.
#[cfg(all(test, unix))]
#[derive(Clone)]
pub(super) struct NativeCleanup {
    admission: NativeAdmission,
    row: Option<super::super::inflight::InflightTurnState>,
    started_before: std::time::Instant,
}

#[cfg(all(test, unix))]
impl NativeCleanup {
    pub(super) fn primary_key(&self) -> Option<TurnKey> {
        self.admission.cleanup.as_ref().map(|(key, _)| *key)
    }

    fn capture(
        admission: NativeAdmission,
        row: Option<super::super::inflight::InflightTurnState>,
        started_before: std::time::Instant,
    ) -> Result<Self, String> {
        let row = match &admission.cleanup {
            Some((key, _)) => {
                let row = row.ok_or("native primary row missing")?;
                let source = admission
                    .evidence
                    .ticket
                    .source
                    .as_ref()
                    .ok_or("native source missing")?;
                if !Self::matches_row(&row, *key, source) {
                    return Err("native row episode/runtime mismatch".into());
                }
                Some(row)
            }
            None => None,
        };
        Ok(Self {
            admission,
            row,
            started_before,
        })
    }

    fn matches_row(
        row: &super::super::inflight::InflightTurnState,
        key: TurnKey,
        source: &crate::services::tui_input::human_frame::history::v2::Source,
    ) -> bool {
        row.provider == "claude"
            && row.channel_id == key.channel_id.get()
            && row.effective_finalizer_turn_id() == key.user_msg_id
            && row.born_generation == key.generation
            && key.matches_episode_nonce(row.turn_nonce.as_deref())
            && row.session_id.as_deref() == Some(source.session_id.as_str())
            && row.output_path.as_deref() == source.transcript.to_str()
            && row.hosted_execution_nonce.as_deref() == source.target["execution_nonce"].as_str()
    }

    fn row_snapshot(
        &self,
        key: &TurnKey,
        missing_is_captured: bool,
    ) -> Result<Option<super::super::inflight::InflightTurnState>, String> {
        let evidence = &self.admission.evidence;
        let captured = self.row.as_ref().ok_or("native row capture missing")?;
        let source = evidence
            .ticket
            .source
            .as_ref()
            .ok_or("native source missing")?;
        let root =
            super::super::inflight::inflight_runtime_root().ok_or("native runtime root missing")?;
        let path = super::super::inflight::inflight_state_path(
            &root,
            &ProviderKind::Claude,
            key.channel_id.get(),
        );
        let row = match std::fs::read(&path) {
            Ok(raw) => {
                let current: super::super::inflight::InflightTurnState =
                    serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
                if !Self::matches_row(&current, *key, source)
                    || current.started_at != captured.started_at
                    || current.turn_start_offset != captured.turn_start_offset
                {
                    return Ok(None);
                }
                current
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !missing_is_captured {
                    return Ok(None);
                }
                captured.clone()
            }
            Err(error) => return Err(error.to_string()),
        };
        Ok(Some(row))
    }

    fn outcome(
        &self,
    ) -> Result<crate::services::tui_input::human_frame::history::v2::native::Outcome, String> {
        use crate::services::tui_input::human_frame::history::v2::native::Outcome;
        let evidence = &self.admission.evidence;
        Ok(Outcome {
            disposition: evidence.disposition.clone(),
            range: Some([evidence.range.0, evidence.range.1]),
            extra: serde_json::Map::from_iter([(
                "native_proof".into(),
                serde_json::to_value(evidence).map_err(|e| e.to_string())?,
            )]),
        })
    }

    pub(super) async fn prepare_notice(
        &self,
        history: &crate::services::tui_input::human_frame::history::v2::Registry,
        shared: &Arc<SharedData>,
    ) -> Result<(), String> {
        let evidence = &self.admission.evidence;
        let outcome = self.outcome()?;
        history.record_native_outcome_at_stamp(
            &evidence.namespace,
            &evidence.ticket,
            &outcome,
            &self.admission.registrations_fingerprint,
        )?;
        prepare_saved_notice(history, &evidence.namespace, &evidence.ticket, shared).await
    }

    pub(super) async fn drive(
        &self,
        history: &crate::services::tui_input::human_frame::history::v2::Registry,
        shared: &Arc<SharedData>,
    ) -> Result<bool, String> {
        use super::super::inflight::GuardedClearOutcome;
        use crate::services::tui_input::human_frame::history::v2::native::Disposition;
        let evidence = &self.admission.evidence;
        let outcome = self.outcome()?;
        let settlement = history.native_settlement(&evidence.namespace, &evidence.ticket)?;
        if settlement.outcome.as_ref() != Some(&outcome) {
            return Err("native durable result missing".into());
        }
        if settlement.cleanup_complete {
            return Ok(true);
        }
        if outcome.disposition == Disposition::WithdrawnNotExecuted && !settlement.withdrawal_notice
        {
            return Err("native withdrawal notice pending".into());
        }
        if let Some((key, actor)) = &self.admission.cleanup {
            let current = match shared.mailbox_peek(key.channel_id) {
                Some(mailbox) => Some(mailbox.snapshot().await),
                None => None,
            };
            let token = current
                .as_ref()
                .and_then(|current| current.cancel_token.as_ref());
            let owns_lease = current.as_ref().is_some_and(|current| {
                current.active_user_message_id.map(|id| id.get()) == Some(key.user_msg_id)
                    && key.matches_episode_nonce(current.active_turn_nonce.as_deref())
                    && token.is_some_and(|current| Arc::ptr_eq(current, actor))
            });
            if !owns_lease && self.row_snapshot(key, token.is_none())?.is_none() {
                history.ack_native_cleanup(&evidence.namespace, &evidence.ticket, &outcome)?;
                return Ok(true);
            }
            if let Some(mailbox) = shared.mailbox_peek(key.channel_id) {
                let current = mailbox.snapshot().await;
                if current
                    .cancel_token
                    .as_ref()
                    .is_some_and(|current| !Arc::ptr_eq(current, actor))
                {
                    return Ok(false);
                }
                if current.cancel_token.is_some() {
                    if self.row_snapshot(key, true)?.is_none() {
                        return Ok(false);
                    }
                    let finish = super::super::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion(
                        shared, &ProviderKind::Claude, key.channel_id,
                        serenity::model::id::MessageId::new(key.user_msg_id),
                        evidence.ticket.owning_episode.as_ref().map(|e| e.turn_nonce.clone()),
                        self.started_before, Some(actor.clone()),
                    ).await;
                    if let Some(removed) = finish.removed_token.as_ref() {
                        removed.mark_completion_cleanup();
                        super::super::saturating_decrement_global_active(shared);
                    } else {
                        return Ok(false);
                    }
                    if finish.persistence_error.is_some() {
                        return Ok(false);
                    }
                }
            }
            let Some(row) = self.row_snapshot(key, true)? else {
                return Ok(false);
            };
            let (cleared, detail) =
                GuardedClearOutcome::for_native_snapshot_with_detail(&ProviderKind::Claude, &row);
            if !matches!(
                (cleared, detail.reason),
                (GuardedClearOutcome::Cleared, _) | (GuardedClearOutcome::Missing, "not_found")
            ) {
                return Ok(false);
            }
        }
        let first = history.ack_native_cleanup(&evidence.namespace, &evidence.ticket, &outcome)?;
        if first && let Some((key, _)) = &self.admission.cleanup {
            super::super::turn_completion_events::publish_queue_eligible_completion_event(
                shared,
                key.channel_id,
                Some(key.user_msg_id),
            );
        }
        Ok(true)
    }
}

#[cfg(all(test, unix))]
#[path = "native_input_tests.rs"]
mod native_input_tests;

#[cfg(unix)]
pub(super) async fn prepare_saved_notice(
    history: &crate::services::tui_input::human_frame::history::v2::Registry,
    namespace: &str,
    ticket: &crate::services::tui_input::human_frame::history::v2::native::Ticket,
    shared: &Arc<SharedData>,
) -> Result<(), String> {
    use crate::services::tui_input::human_frame::history::v2::native::Disposition;
    let settlement = history.native_settlement(namespace, ticket)?;
    if settlement.cleanup_complete {
        return Ok(());
    }
    if settlement
        .outcome
        .as_ref()
        .is_some_and(|o| o.disposition == Disposition::WithdrawnNotExecuted)
        && !settlement.withdrawal_notice
    {
        let pool = shared
            .pg_pool
            .as_ref()
            .ok_or("native withdrawal outbox unavailable")?;
        let stable = serde_json::to_vec(&(namespace, ticket)).map_err(|e| e.to_string())?;
        let dedupe = format!("native-withdrawal:{}", blake3::hash(&stable));
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            crate::services::message_outbox::enqueue_outbox_pg_returning_id_with_persistent_dedupe(
                pool,
                crate::services::message_outbox::OutboxMessage {
                    target: &format!("channel:{}", ticket.channel_id),
                    content: "입력이 실행 전에 철회되었습니다.",
                    bot: "claude",
                    source: "retained_input_notice",
                    reason_code: None,
                    session_key: Some(&dedupe),
                },
            ),
        )
        .await
        .map_err(|_| "native withdrawal outbox timed out")?
        .map_err(|e| e.to_string())?;
        history.ack_native_withdrawal_notice(namespace, ticket)?;
    }
    Ok(())
}
