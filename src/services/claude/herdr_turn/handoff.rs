//! Forward launch preparation preserves the existing source/account and resume checks.
use super::*;

/// Preserve the wrapper's scheduling-refusal prefix while adding stages to launch errors.
fn launch_error(stage: &str, error: String) -> String {
    if error.starts_with("NotSent(") {
        error
    } else {
        format!("herdr launch {stage}: {error}")
    }
}

/// The session a launch resumes: the turn's own one while it last ran under the overlay's account,
/// its transcript exists under the overlay's home and O reads that file; otherwise a new one.
fn resumed_session(
    turn: &HerdrTurn<'_>,
    overlay: &ProviderAuthOverlay,
    requested: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(session) = requested.filter(|_| !turn.force_fresh) else {
        return Ok(None);
    };
    // As on tmux, a session of another account is never continued under this one.
    let logical = &turn.owner.logical_key;
    let profile = &overlay.profile_id;
    if !crate::services::tmux_common::claude_session_auth_matches(logical, session, profile)? {
        return Ok(None);
    }
    let working_dir = Path::new(turn.working_dir);
    let home = overlay.home.as_deref();
    let Some(source) = session_handoff::transcript_file(working_dir, session, home)? else {
        use crate::services::claude_tui::transcript_tail::claude_transcript_path;
        let missing = claude_transcript_path(working_dir, session, home)?;
        let channel = Some(turn.channel_id);
        super::super::fresh_notice::announce(Some(turn.pool), channel, session, &missing);
        return Ok(None);
    };
    if !session_handoff::o_reads(turn.channel_id, &source)? {
        return Err(format!(
            "herdr turn: O reads no cursor on {}, so a resume would post it again",
            source.path.display()
        ));
    }
    Ok(Some(session.to_owned()))
}

/// A new execution: create, evidence, SessionStart, then attach; no prompt before the attach.
/// A resume closes only its verified old pane, then reads the latest native binding again.
pub(super) fn launched_source(
    turn: &HerdrTurn<'_>,
    runtime: &Handle,
    ports: &dyn HerdrTurnPorts,
) -> Result<Attached, String> {
    let host = ports.launch_host().ok_or("herdr turn: no launch host")?;
    let working_dir = Path::new(turn.working_dir);
    let hook_endpoint = turn
        .hook_endpoint
        .clone()
        .ok_or("herdr turn: no hook endpoint")?;
    let logical = &turn.owner.logical_key;
    let overlay = ports
        .auth_overlay(logical)
        .map_err(|e| launch_error("auth_overlay", e))?;
    let auth = crate::services::provider_auth_profile::overlay_shell_env_lines(&overlay)
        .map_err(|e| launch_error("auth_overlay", e))?;
    let mut earlier = ports.earlier_tmux(logical);
    let judged = |earlier: &mut dyn EarlierExecution| {
        let (owner, row) = (&turn.owner, turn.row);
        let mut prior = PriorExecution {
            earlier,
            owner,
            row,
        };
        current_session(turn.channel_id, logical, turn.session_id, &mut prior)
    };
    let requested = if turn.force_fresh {
        None
    } else {
        judged(earlier.as_mut()).map_err(|e| launch_error("current_session", e))?
    };
    let prior = resumed_session(turn, &overlay, requested.as_deref())
        .map_err(|e| launch_error("resume_source", e))?;
    if prior.is_some() {
        if earlier.liveness() != session_handoff::Liveness::Ended {
            runtime
                .block_on(session_handoff::pinned_tmux::end(
                    logical,
                    turn.channel_id,
                    None,
                    |_| async { Ok(!cancel_requested(turn.cancel.as_deref())) },
                ))
                .map_err(|e| launch_error("close_legacy", e))?;
        }
    }
    let latest = if turn.force_fresh {
        None
    } else {
        judged(earlier.as_mut()).map_err(|e| launch_error("current_session", e))?
    };
    let resumed = resumed_session(turn, &overlay, latest.as_deref())
        .map_err(|e| launch_error("resume_source", e))?;
    let home = overlay.home.as_deref();
    let session_id = match &resumed {
        Some(session) => session.clone(),
        None => {
            super::super::fresh_claude_tui_session_resolution(working_dir, home)
                .map_err(|e| launch_error("fresh_session", e))?
                .session_id
        }
    };
    let prepare = |prepared: &PreparedIncarnation| {
        let (system, model) = (turn.system_prompt, turn.model);
        let (session, hooks) = (session_id.as_str(), hook_endpoint.clone());
        let command = super::super::tui_session_launch::prepare_claude_herdr_launch(
            prepared,
            working_dir,
            session,
            system,
            model,
            hooks,
            &auth,
        )
        .map_err(|e| launch_error("prepare_command", e))?;
        // The account this session runs under, which a later resume of it checks.
        let profile = &overlay.profile_id;
        crate::services::tmux_common::write_claude_session_auth(logical, session, profile)
            .map_err(|e| launch_error("write_auth", e))?;
        Ok(command)
    };
    // Subscribed before the create, so the launch's SessionStart cannot pass unseen.
    let mut hooks = ports.hook_events();
    let mut launch = launch(turn, Some(session_id.clone()), resumed.is_some());
    launch.provider_root = home.map(|home| home.join("projects"));
    let outcome = runtime.block_on(launch_herdr_session(turn.pool, launch, prepare, host));
    let nonce = launched(outcome).map_err(|e| launch_error("admit_create_evidence", e))?;
    let resume = resumed.is_some().then_some(nonce.as_str());
    let cancel = turn.cancel.as_deref();
    session_started(runtime, &mut hooks, (&session_id, resume), cancel)
        .map_err(|e| launch_error("session_start", e))?;
    attach(turn, runtime, ports, &nonce, &session_id).map_err(|e| launch_error("attach", e))
}

fn current_session(
    channel: u64,
    logical: &str,
    original: Option<&str>,
    prior: &mut PriorExecution<'_>,
) -> Result<Option<String>, String> {
    use crate::services::tui_prompt_dedupe::binding_events as events;
    use std::os::unix::fs::MetadataExt;
    let records = events::records_strict(channel)
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("handoff binding log corrupt: {e:?}"))?;
    let Some(latest) = records
        .iter()
        .rev()
        .find(|record| record.tmux_session == logical && record.provider == "claude")
    else {
        return Ok(original.map(str::to_string));
    };
    // A record without a nonce does not reset the writer's execution fold.
    let nonce = latest.execution_nonce.as_deref().or_else(|| {
        records
            .iter()
            .rev()
            .filter(|record| record.tmux_session == logical && record.provider == "claude")
            .find_map(|record| record.execution_nonce.as_deref())
    });
    let (pin, history) =
        events::claude_history_at(channel, logical, nonce).map_err(|e| e.to_string())?;
    if history.awaiting.is_some() {
        return Err("handoff native session is still Pending".into());
    }
    let pin = match pin {
        Some(pin) => Some(pin),
        None => pin_retired_resume(channel, logical, latest, prior)?,
    };
    let legacy = pin.is_none();
    let (source, pinned_at) = pin
        .or_else(|| match &latest.new {
            events::BindingTarget::Source(source)
            | events::BindingTarget::Resolved { source, .. }
                if latest.evidence.hook_event.is_none() =>
            {
                Some((source.clone(), Some(latest.committed_at)))
            }
            _ => None,
        })
        .ok_or("handoff native session has no verified durable binding")?;
    if legacy {
        use crate::services::tui_prompt_dedupe::binding_context::{
            SpawnNonceMarker, observe_spawn_nonce_marker,
        };
        // A failed create can leave a marker without logging a native execution. A tmux resume
        // of the same transcript logs nothing either, so its marker is no live owner once it ended.
        match (nonce, observe_spawn_nonce_marker(logical)) {
            (Some(recorded), SpawnNonceMarker::Known(current))
                if recorded != current && !ended_tmux_execution(logical, prior.earlier) =>
            {
                return Err("handoff binding belongs to another execution".into());
            }
            (_, SpawnNonceMarker::Unreadable) => {
                return Err("handoff execution marker unreadable".into());
            }
            _ => (),
        }
    } else {
        uuid::Uuid::parse_str(&source.session_id)
            .map_err(|_| "handoff native session ID is invalid")?;
    }
    let metadata = match std::fs::metadata(&source.path) {
        Ok(metadata) => metadata,
        // The existing resume check owns the missing-transcript fresh launch and notice.
        Err(error) if legacy && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(original.map(str::to_string));
        }
        Err(error) => return Err(error.to_string()),
    };
    if (metadata.dev(), metadata.ino()) != (source.dev, source.ino)
        && !renumbered_after_end(&source, pinned_at, logical, nonce, prior)
    {
        return Err("handoff transcript identity changed".into());
    }
    Ok(if legacy {
        original.map(str::to_string)
    } else {
        Some(source.session_id)
    })
}

/// Whether the marker names a tmux execution whose session is proven gone. Herdr writes its own
/// host marker before its nonce, so a Herdr create never reads as tmux here.
fn ended_tmux_execution(
    logical: &str,
    earlier: &mut dyn session_handoff::EarlierExecution,
) -> bool {
    crate::services::claude::host_gate::tmux_marked(logical)
        && earlier.liveness() == session_handoff::Liveness::Ended
}

/// What can prove that the execution which logged a binding has ended.
pub(super) struct PriorExecution<'a> {
    pub(super) earlier: &'a mut dyn session_handoff::EarlierExecution,
    pub(super) owner: &'a HostedOwner,
    pub(super) row: Option<&'a HostedRecord>,
}

impl PriorExecution<'_> {
    /// The tmux proof, or the channel's row naming execution `nonce` Retired.
    fn ended(&mut self, logical: &str, nonce: Option<&str>) -> bool {
        nonce.is_some_and(|nonce| self.retired(nonce))
            || ended_tmux_execution(logical, self.earlier)
    }

    /// Whether the channel's row names execution `nonce` of this owner Retired: that state is
    /// written only for a confirmed-ended execution of its owner and nonce.
    fn retired(&self, nonce: &str) -> bool {
        matches!(self.row, Some(HostedRecord::Known(record))
            if record.state == HostedState::Retired
                && record.owner == *self.owner
                && record.execution_nonce == nonce)
    }
}

/// A Herdr resume logs its own SessionStart as a stat; the pin stays unverified until a same-session
/// hook proves it or it re-pins a renumbered one. Once the row names it Retired, it is pinned now.
fn pin_retired_resume(
    channel: u64,
    logical: &str,
    latest: &crate::services::tui_prompt_dedupe::binding_events::BindingEvent,
    prior: &PriorExecution<'_>,
) -> Result<Option<crate::services::tui_prompt_dedupe::binding_events::PinnedAt>, String> {
    use crate::services::claude_tui::source_verify::{
        Observation, PinJudgment, judge_pin, observe_transcript,
    };
    use crate::services::tui_prompt_dedupe::binding_context::{
        SpawnNonceMarker, execution_context, observe_spawn_nonce_marker,
    };
    use crate::services::tui_prompt_dedupe::binding_events::{
        self as events, BindingCause, BindingTarget, CauseSource, Committed, Proposal,
    };
    let (BindingTarget::Source(source) | BindingTarget::Resolved { source, .. }) = &latest.new
    else {
        return Ok(None);
    };
    let (Some(nonce), Some(path)) = (latest.execution_nonce.as_deref(), source.path.to_str())
    else {
        return Ok(None);
    };
    let resumed = latest.cause == BindingCause::Resume
        && latest.evidence.hook_event.as_deref() == Some("session_start");
    // The start must be the one that execution's own launch context resumed, as its hook was.
    let own = execution_context("claude", nonce).is_ok_and(|context| {
        context.launch_mode == "resume"
            && context.tmux_session == logical
            && context.channel_id == Some(channel)
            && context.expected_native_session_id.as_deref() == Some(source.session_id.as_str())
    });
    let marked = observe_spawn_nonce_marker(logical) == SpawnNonceMarker::Known(nonce.to_owned());
    if !(resumed && own && marked && prior.retired(nonce)) {
        return Ok(None);
    }
    let opened = observe_transcript(&source.path);
    let seen = Observation::Opened(opened.as_ref());
    let pinned_at = Some(latest.committed_at);
    let judged = judge_pin(
        Some(source),
        pinned_at,
        &source.session_id,
        &source.path,
        seen,
    );
    let PinJudgment::Verified(verified) = judged else {
        return Ok(None);
    };
    if !events::codex::source_file_matches(&verified) {
        return Ok(None);
    }
    let proposal = Proposal {
        channel_id: channel,
        provider: "claude",
        tmux_session: logical,
        session_id: Some(&source.session_id),
        path,
        replaced: None,
        cause: CauseSource::Observed,
        hook: None,
    };
    let committed = crate::services::tmux_common::with_tmux_source_authority(logical, |_| {
        events::record_verified(&proposal, &verified)
    })
    .map_err(|e| e.to_string())?;
    if !matches!(committed, Committed::Appended | Committed::Unchanged) {
        return Ok(None);
    }
    let (pin, _) =
        events::claude_history_at(channel, logical, Some(nonce)).map_err(|e| e.to_string())?;
    Ok(pin)
}

/// Whether the transcript is the recorded file under the device number a host reboot gave its
/// volume, by the pin rule every Claude source goes through, and the binding's execution ended.
fn renumbered_after_end(
    source: &crate::services::tui_prompt_dedupe::binding_events::SourceId,
    pinned_at: Option<chrono::DateTime<chrono::Utc>>,
    logical: &str,
    nonce: Option<&str>,
    prior: &mut PriorExecution<'_>,
) -> bool {
    use crate::services::claude_tui::source_verify::{
        Observation, PinJudgment, judge_pin, observe_transcript,
    };
    let opened = observe_transcript(&source.path);
    let seen = Observation::Opened(opened.as_ref());
    let path = source.path.as_path();
    let judged = judge_pin(Some(source), pinned_at, &source.session_id, path, seen);
    matches!(judged, PinJudgment::Verified(_)) && prior.ended(logical, nonce)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_stage_preserves_scheduling_refusals() {
        let held = "NotSent(\"writer_held\")".to_string();
        assert_eq!(launch_error("auth_overlay", held.clone()), held);
        assert_eq!(
            launch_error("auth_overlay", "forced failure".into()),
            "herdr launch auth_overlay: forced failure"
        );
    }

    #[test]
    fn test_instance_claude_herdr_default_profile_refuses_before_launch_subscription() {
        crate::services::provider_auth_profile::instance_env_tests::with_instance(
            concat!(
                module_path!(),
                "::test_instance_claude_herdr_default_profile_refuses_before_launch_subscription"
            ),
            || {
                struct Ports;
                impl HerdrTurnPorts for Ports {
                    fn launch_host(&self) -> Option<Arc<dyn HerdrLaunchHost>> {
                        Some(Arc::new(SocketHerdrLaunchHost::new(
                            vec![],
                            Default::default(),
                        )))
                    }
                    fn hook_events(&self) -> broadcast::Receiver<HookEvent> {
                        panic!("default profile must refuse before launch subscription")
                    }
                    fn attach(&self, _: &AttachRequest<'_>) -> Result<bool, String> {
                        panic!("default profile must never attach")
                    }
                    fn confirm_bound(
                        &self,
                        _: &HostedOwner,
                        _: &HostedExecution,
                        _: &HerdrTarget,
                    ) -> Result<(), String> {
                        panic!("default profile must never bind")
                    }
                    fn auth_overlay(&self, _: &str) -> Result<ProviderAuthOverlay, String> {
                        Ok(ProviderAuthOverlay::default_for(ProviderKind::Claude))
                    }
                }
                let instance = crate::config::test_instance::current().unwrap().unwrap();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let _entered = runtime.enter();
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://unused@127.0.0.1:1/unused")
                    .unwrap();
                let turn = HerdrTurn {
                    pool: &pool,
                    owner: HostedOwner {
                        provider: "claude".into(),
                        discord_token_hash: "synthetic".into(),
                        channel_id: "41".into(),
                        logical_key: format!("{}-claude-herdr-env", instance.tmux.prefix),
                        owner_node: instance.instance_id.clone(),
                        runtime_root: instance.root.display().to_string(),
                    },
                    channel_id: 41,
                    endpoint: HerdrLaunchEndpoint {
                        execution_node: instance.instance_id.clone(),
                        config_key: instance.herdr.endpoint_key.clone(),
                        socket_addr: instance.herdr.socket.display().to_string(),
                        herdr_session: instance.herdr.session.clone(),
                    },
                    row: None,
                    prompt: "scope prompt",
                    working_dir: instance.root.to_str().unwrap(),
                    session_id: None,
                    force_fresh: true,
                    system_prompt: None,
                    model: None,
                    hook_endpoint: Some("http://127.0.0.1:9".into()),
                    cancel: None,
                };
                assert_eq!(
                    launched_source(&turn, runtime.handle(), &Ports)
                        .err()
                        .as_deref(),
                    Some("herdr launch auth_overlay: test instance host declaration rejected")
                );
            },
        );
    }

    use crate::services::tui_prompt_dedupe::binding_events as events;
    use std::os::unix::fs::MetadataExt;
    struct Log {
        root: tempfile::TempDir,
        old: Option<PathBuf>,
        channel: u64,
        logical: &'static str,
        rows: Vec<serde_json::Value>,
    }
    impl Log {
        fn new(channel: u64) -> Self {
            let root = tempfile::tempdir().unwrap();
            let old = events::test_root();
            events::set_test_root(Some(root.path()));
            std::fs::create_dir(root.path().join("binding_events")).unwrap();
            Self {
                root,
                old,
                channel,
                logical: "i8077-durable-native",
                rows: vec![],
            }
        }
        fn source(&self, id: &str) -> events::SourceId {
            let path = self.root.path().join(format!("{id}.jsonl"));
            std::fs::write(&path, b"{}\n").unwrap();
            let meta = std::fs::metadata(&path).unwrap();
            events::SourceId {
                session_id: id.into(),
                path,
                dev: meta.dev(),
                ino: meta.ino(),
            }
        }
        fn push(&mut self, target: events::BindingTarget) {
            let at = chrono::Utc::now();
            let event = events::BindingEvent {
                seq: self.rows.len() as u64 + 1,
                channel_id: self.channel,
                provider: "claude".into(),
                tmux_session: self.logical.into(),
                execution_nonce: Some("old-execution".into()),
                old: None,
                new: target,
                cause: events::BindingCause::Continuation,
                parent_hint: None,
                evidence: events::BindingEvidence {
                    hook_event: Some("session_start".into()),
                    received_at: at,
                },
                committed_at: at,
            };
            let mut row = serde_json::to_value(event).unwrap();
            row["verified"] = true.into();
            self.rows.push(row);
            events::forget_channel_for_tests(self.channel);
            let bytes = self
                .rows
                .iter()
                .map(|r| serde_json::to_string(r).unwrap() + "\n")
                .collect::<String>();
            std::fs::write(self.path(), bytes).unwrap();
        }
        fn path(&self) -> PathBuf {
            self.root
                .path()
                .join("binding_events")
                .join(format!("{}.log", self.channel))
        }
    }
    struct Earlier(Liveness);
    impl session_handoff::EarlierExecution for Earlier {
        fn liveness(&mut self) -> Liveness {
            self.0.clone()
        }
        fn send_exit(&mut self) -> session_handoff::ExitSend {
            panic!("the binding check never types into the old pane")
        }
    }
    use session_handoff::Liveness;
    fn owner() -> HostedOwner {
        HostedOwner {
            provider: "claude".into(),
            discord_token_hash: "synthetic".into(),
            channel_id: "8177".into(),
            logical_key: "i8077-durable-native".into(),
            owner_node: "node".into(),
            runtime_root: "/root".into(),
        }
    }
    /// The judgment with only the tmux proof available, as every channel without a hosted row.
    fn current_session(
        channel: u64,
        logical: &str,
        original: Option<&str>,
        earlier: &mut Earlier,
    ) -> Result<Option<String>, String> {
        let owner = owner();
        let mut prior = PriorExecution {
            earlier,
            owner: &owner,
            row: None,
        };
        super::current_session(channel, logical, original, &mut prior)
    }
    impl Drop for Log {
        fn drop(&mut self) {
            events::forget_channel_for_tests(self.channel);
            events::set_test_root(self.old.as_deref());
        }
    }
    #[test]
    fn resume_uses_durable_continuation_instead_of_the_original_argv_or_cache_uuid() {
        let root_guard = crate::config::TestRuntimeRootGuard::new();
        let mut log = Log::new(8077011);
        let argv = uuid::Uuid::new_v4().to_string();
        let cached = uuid::Uuid::new_v4().to_string();
        let previous = log.source(&cached);
        log.push(events::BindingTarget::Source(previous.clone()));
        let continued = uuid::Uuid::new_v4().to_string();
        let source = log.source(&continued);
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            log.logical,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: previous.path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(cached),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        log.push(events::BindingTarget::Pending {
            payload_session_id: continued.clone(),
            payload_transcript_path: Some(source.path.display().to_string()),
        });
        assert!(
            current_session(
                log.channel,
                log.logical,
                Some(&argv),
                &mut Earlier(Liveness::Running)
            )
            .is_err()
        );
        log.push(events::BindingTarget::Resolved {
            pending_seq: 2,
            source,
        });
        assert_eq!(
            current_session(
                log.channel,
                log.logical,
                Some(&argv),
                &mut Earlier(Liveness::Running)
            )
            .unwrap(),
            Some(continued.clone())
        );
        log.push(events::BindingTarget::Rejected {
            payload_session_id: argv.clone(),
            payload_transcript_path: None,
            reason: "audit only".into(),
        });
        assert_eq!(
            current_session(
                log.channel,
                log.logical,
                Some(&argv),
                &mut Earlier(Liveness::Running)
            )
            .unwrap(),
            Some(continued)
        );
        drop(root_guard);
    }
    #[test]
    fn tmux_stat_registration_resumes_without_a_verified_hook_pin() {
        let _root_guard = crate::config::TestRuntimeRootGuard::new();
        for nonce in [None, Some("a".repeat(32))] {
            let log = Log::new(8077013);
            let original = uuid::Uuid::new_v4().to_string();
            let source = log.source(&original);
            let marker =
                crate::services::tmux_common::session_temp_path(log.logical, "spawn_nonce");
            if let Some(nonce) = &nonce {
                std::fs::write(&marker, nonce).unwrap();
            }
            crate::services::tui_prompt_dedupe::register_tmux_channel(log.logical, log.channel);
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
                log.logical,
                TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    output_path: source.path.display().to_string(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: nonce.as_ref().map(|_| original.clone()),
                    last_offset: 0,
                    relay_last_offset: None,
                },
            );
            let records = events::records_strict(log.channel).unwrap().unwrap();
            assert!(!records.is_empty());
            assert_eq!(records.last().unwrap().execution_nonce, nonce);
            assert!(
                events::claude_history(log.channel, log.logical, nonce.as_deref())
                    .unwrap()
                    .0
                    .is_none()
            );
            assert_eq!(
                current_session(
                    log.channel,
                    log.logical,
                    Some(&original),
                    &mut Earlier(Liveness::Running)
                )
                .unwrap(),
                Some(original.clone())
            );
            if let Some(nonce) = &nonce {
                std::fs::write(&marker, "b".repeat(32)).unwrap();
                assert!(
                    current_session(
                        log.channel,
                        log.logical,
                        Some(&original),
                        &mut Earlier(Liveness::Running)
                    )
                    .is_err()
                );
                std::fs::write(&marker, nonce).unwrap();
            } else {
                std::fs::write(&marker, "b".repeat(32)).unwrap();
                assert_eq!(
                    current_session(
                        log.channel,
                        log.logical,
                        Some(&original),
                        &mut Earlier(Liveness::Running)
                    )
                    .unwrap(),
                    Some(original.clone())
                );
                std::fs::remove_file(&marker).unwrap();
            }
            std::fs::rename(&source.path, source.path.with_extension("old")).unwrap();
            std::fs::write(&source.path, b"{}\n").unwrap();
            assert!(
                current_session(
                    log.channel,
                    log.logical,
                    Some(&original),
                    &mut Earlier(Liveness::Running)
                )
                .is_err()
            );
        }
    }
    #[test]
    fn missing_log_keeps_legacy_id_but_corrupt_or_replaced_source_never_uses_it() {
        let mut log = Log::new(8077012);
        let original = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            current_session(
                log.channel,
                log.logical,
                Some(&original),
                &mut Earlier(Liveness::Running)
            )
            .unwrap(),
            Some(original.clone())
        );
        let source = log.source(&original);
        log.push(events::BindingTarget::Source(source.clone()));
        std::fs::remove_file(&source.path).unwrap();
        assert!(
            current_session(
                log.channel,
                log.logical,
                Some(&original),
                &mut Earlier(Liveness::Running)
            )
            .is_err()
        );
        std::fs::write(log.path(), b"not-json\n").unwrap();
        assert!(
            current_session(
                log.channel,
                log.logical,
                Some(&original),
                &mut Earlier(Liveness::Running)
            )
            .is_err()
        );
    }

    #[test]
    fn an_unlogged_tmux_resume_marker_yields_the_binding_only_once_its_tmux_ended() {
        let _root_guard = crate::config::TestRuntimeRootGuard::new();
        let log = Log::new(8177001);
        let original = uuid::Uuid::new_v4().to_string();
        let source = log.source(&original);
        let marker = crate::services::tmux_common::session_temp_path(log.logical, "spawn_nonce");
        let host = crate::services::tmux_common::session_temp_path(log.logical, "host_kind");
        std::fs::write(&marker, "a".repeat(32)).unwrap();
        crate::services::tui_prompt_dedupe::register_tmux_channel(log.logical, log.channel);
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            log.logical,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: source.path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(original.clone()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        // The later tmux resume of the same transcript replaced the marker without a record.
        std::fs::write(&marker, "b".repeat(32)).unwrap();
        let judged = |host_kind: &str, liveness: Liveness| {
            std::fs::write(&host, host_kind).unwrap();
            current_session(
                log.channel,
                log.logical,
                Some(&original),
                &mut Earlier(liveness),
            )
        };
        assert_eq!(judged("tmux", Liveness::Ended), Ok(Some(original.clone())));
        let refused = Err("handoff binding belongs to another execution".to_string());
        assert_eq!(judged("tmux", Liveness::Running), refused);
        assert_eq!(judged("tmux", Liveness::Unknown("probe".into())), refused);
        assert_eq!(judged("herdr", Liveness::Ended), refused);
    }

    /// The channel's hosted row naming execution `nonce` of `owner` in `state`.
    fn hosted_row(owner: HostedOwner, nonce: &str, state: HostedState) -> HostedRecord {
        use crate::db::dispatched_sessions::hosted_execution::{HostedExecution, SourceRef};
        let source = SourceRef {
            runtime_root: owner.runtime_root.clone(),
            channel: owner.channel_id.clone(),
            provider: owner.provider.clone(),
            logical_key: owner.logical_key.clone(),
            execution_nonce: nonce.into(),
            initial_source: None,
            baseline_event_seq: None,
        };
        let mut record = HostedExecution::pending(owner, nonce.into(), source);
        record.state = state;
        HostedRecord::Known(record)
    }

    /// Rewrites every logged source as if a host reboot after the record renumbered its volume.
    fn reboot_after_record(log: &Log, dev_shift: u64, ino_shift: u64) {
        let text = std::fs::read_to_string(log.path()).unwrap();
        let mut rows = String::new();
        for line in text.lines() {
            let mut row: serde_json::Value = serde_json::from_str(line).unwrap();
            let source = &mut row["new"]["source"];
            source["dev"] = (source["dev"].as_u64().unwrap() + dev_shift).into();
            source["ino"] = (source["ino"].as_u64().unwrap() + ino_shift).into();
            row["committed_at"] = "2000-01-01T00:00:00Z".into();
            rows.push_str(&format!("{row}\n"));
        }
        std::fs::write(log.path(), rows).unwrap();
        events::forget_channel_for_tests(log.channel);
    }

    #[test]
    fn a_reboot_renumbered_transcript_resumes_only_as_its_own_file_after_its_tmux_ended() {
        let _root_guard = crate::config::TestRuntimeRootGuard::new();
        let session = uuid::Uuid::new_v4().to_string();
        let marker_nonce = "c".repeat(32);
        let judged = |first: &str, dev_shift: u64, ino_shift: u64, liveness: Liveness| {
            let log = Log::new(8177002);
            let path = log.root.path().join(format!("{session}.jsonl"));
            std::fs::write(
                &path,
                format!("{}\n", serde_json::json!({"sessionId": first})),
            )
            .unwrap();
            let marker =
                crate::services::tmux_common::session_temp_path(log.logical, "spawn_nonce");
            let host = crate::services::tmux_common::session_temp_path(log.logical, "host_kind");
            std::fs::write(&marker, &marker_nonce).unwrap();
            std::fs::write(&host, "tmux").unwrap();
            crate::services::tui_prompt_dedupe::register_tmux_channel(log.logical, log.channel);
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
                log.logical,
                TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    output_path: path.display().to_string(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: Some(session.clone()),
                    last_offset: 0,
                    relay_last_offset: None,
                },
            );
            reboot_after_record(&log, dev_shift, ino_shift);
            current_session(
                log.channel,
                log.logical,
                Some(&session),
                &mut Earlier(liveness),
            )
        };
        let changed = Err("handoff transcript identity changed".to_string());
        assert_eq!(
            judged(&session, 1, 0, Liveness::Ended),
            Ok(Some(session.clone()))
        );
        assert_eq!(judged(&session, 1, 0, Liveness::Running), changed);
        assert_eq!(judged("another-session", 1, 0, Liveness::Ended), changed);
        assert_eq!(judged(&session, 1, 1, Liveness::Ended), changed);
    }

    #[test]
    fn a_reboot_renumbered_transcript_of_a_herdr_execution_resumes_only_once_its_own_row_is_retired()
     {
        let _root_guard = crate::config::TestRuntimeRootGuard::new();
        let session = uuid::Uuid::new_v4().to_string();
        let nonce = "d".repeat(32);
        let row = hosted_row;
        let judged = |first: &str, ino_shift: u64, row: Option<&HostedRecord>| {
            let log = Log::new(8177003);
            let path = log.root.path().join(format!("{session}.jsonl"));
            let first = serde_json::json!({"sessionId": first});
            std::fs::write(&path, format!("{first}\n")).unwrap();
            let temp = |ext| crate::services::tmux_common::session_temp_path(log.logical, ext);
            std::fs::write(temp("spawn_nonce"), &nonce).unwrap();
            std::fs::write(temp("host_kind"), "herdr").unwrap();
            crate::services::tui_prompt_dedupe::register_tmux_channel(log.logical, log.channel);
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
                log.logical,
                TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    output_path: path.display().to_string(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: Some(session.clone()),
                    last_offset: 0,
                    relay_last_offset: None,
                },
            );
            reboot_after_record(&log, 1, ino_shift);
            let owner = owner();
            let earlier = &mut Earlier(Liveness::Ended);
            let mut prior = PriorExecution {
                earlier,
                owner: &owner,
                row,
            };
            super::current_session(log.channel, log.logical, Some(&session), &mut prior)
        };
        let retired = row(owner(), &nonce, HostedState::Retired);
        let changed = Err("handoff transcript identity changed".to_string());
        assert_eq!(
            judged(&session, 0, Some(&retired)),
            Ok(Some(session.clone()))
        );
        assert_eq!(judged(&session, 0, None), changed);
        let bound = row(owner(), &nonce, HostedState::Bound);
        assert_eq!(judged(&session, 0, Some(&bound)), changed);
        let other_nonce = row(owner(), &"e".repeat(32), HostedState::Retired);
        assert_eq!(judged(&session, 0, Some(&other_nonce)), changed);
        let mut stranger = owner();
        stranger.discord_token_hash = "another-bot".into();
        let other_owner = row(stranger, &nonce, HostedState::Retired);
        assert_eq!(judged(&session, 0, Some(&other_owner)), changed);
        assert_eq!(judged("another-session", 0, Some(&retired)), changed);
        assert_eq!(judged(&session, 1, Some(&retired)), changed);
    }

    /// What a Herdr resume's own start leaves: its launch context, marker and a stat record of it.
    struct ResumeStart {
        session: String,
        nonce: String,
        first: String,
        cause: events::BindingCause,
        logged_nonce: Option<String>,
        marker: String,
        context_session: String,
        readable: bool,
    }

    impl ResumeStart {
        fn new(session: &str) -> Self {
            let nonce = "d".repeat(32);
            Self {
                session: session.into(),
                first: session.into(),
                cause: events::BindingCause::Resume,
                logged_nonce: Some(nonce.clone()),
                marker: nonce.clone(),
                context_session: session.into(),
                readable: true,
                nonce,
            }
        }

        /// Logs the start, then judges it after a reboot moving the recorded identity by `shift`.
        fn judged(
            &self,
            shift: (u64, u64),
            row: Option<&HostedRecord>,
        ) -> Result<Option<String>, String> {
            use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
            let log = Log::new(8177004);
            let path = log.root.path().join(format!("{}.jsonl", self.session));
            let first = serde_json::json!({"sessionId": self.first});
            std::fs::write(&path, format!("{first}\n")).unwrap();
            let temp = |ext| crate::services::tmux_common::session_temp_path(log.logical, ext);
            std::fs::write(temp("spawn_nonce"), &self.marker).unwrap();
            std::fs::write(temp("host_kind"), "herdr").unwrap();
            let context = BindingContext {
                schema: 1,
                provider: "claude".into(),
                created_at: chrono::Utc::now(),
                execution_nonce: self.nonce.clone(),
                tmux_session: log.logical.into(),
                channel_id: Some(log.channel),
                owner_runtime_root: "/root".into(),
                host: None,
                expected_native_session_id: Some(self.context_session.clone()),
                launch_mode: "resume".into(),
                provider_root: None,
                first_prompt_digest: None,
                source_policy: None,
                resume_start: None,
            };
            let own = crate::services::tui_prompt_dedupe::binding_context::context_path(
                "claude",
                &self.nonce,
            )
            .unwrap();
            std::fs::create_dir_all(own.parent().unwrap()).unwrap();
            std::fs::write(&own, serde_json::to_vec(&context).unwrap()).unwrap();
            let meta = std::fs::metadata(&path).unwrap();
            let at = chrono::Utc::now();
            let event = events::BindingEvent {
                seq: 1,
                channel_id: log.channel,
                provider: "claude".into(),
                tmux_session: log.logical.into(),
                execution_nonce: self.logged_nonce.clone(),
                old: None,
                new: events::BindingTarget::Source(events::SourceId {
                    session_id: self.session.clone(),
                    path: path.clone(),
                    dev: meta.dev(),
                    ino: meta.ino(),
                }),
                cause: self.cause,
                parent_hint: None,
                evidence: events::BindingEvidence {
                    hook_event: Some("session_start".into()),
                    received_at: at,
                },
                committed_at: at,
            };
            let row_json = serde_json::to_string(&event).unwrap();
            std::fs::write(log.path(), format!("{row_json}\n")).unwrap();
            events::forget_channel_for_tests(log.channel);
            if shift != (0, 0) {
                reboot_after_record(&log, shift.0, shift.1);
            }
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |bits| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(bits));
            if !self.readable {
                mode(0o000).unwrap();
            }
            let owner = owner();
            let earlier = &mut Earlier(Liveness::Ended);
            let mut prior = PriorExecution {
                earlier,
                owner: &owner,
                row,
            };
            let judged = super::current_session(log.channel, log.logical, None, &mut prior);
            mode(0o600).unwrap();
            std::fs::remove_file(own).unwrap();
            judged
        }
    }

    #[test]
    fn a_retired_herdr_resume_whose_start_was_logged_as_a_stat_resumes_its_own_proven_file() {
        let _root_guard = crate::config::TestRuntimeRootGuard::new();
        let session = uuid::Uuid::new_v4().to_string();
        let start = ResumeStart::new(&session);
        let nonce = start.nonce.clone();
        let retired = hosted_row(owner(), &nonce, HostedState::Retired);
        let resumed = Ok(Some(session.clone()));
        assert_eq!(start.judged((0, 0), Some(&retired)), resumed);
        assert_eq!(start.judged((1, 0), Some(&retired)), resumed);
        let refused = Err("handoff native session has no verified durable binding".to_string());
        for state in [HostedState::Bound, HostedState::Pending] {
            let live = hosted_row(owner(), &nonce, state);
            assert_eq!(start.judged((0, 0), Some(&live)), refused);
        }
        assert_eq!(start.judged((0, 0), None), refused);
        let other_nonce = hosted_row(owner(), &"e".repeat(32), HostedState::Retired);
        assert_eq!(start.judged((0, 0), Some(&other_nonce)), refused);
        let mut stranger = owner();
        stranger.discord_token_hash = "another-bot".into();
        let other_owner = hosted_row(stranger, &nonce, HostedState::Retired);
        assert_eq!(start.judged((0, 0), Some(&other_owner)), refused);
        assert_eq!(start.judged((0, 1), Some(&retired)), refused);
        let variants: [fn(&mut ResumeStart); 6] = [
            |s| s.first = "another-session".into(),
            |s| s.readable = false,
            |s| s.logged_nonce = None,
            |s| s.marker = "e".repeat(32),
            |s| s.context_session = uuid::Uuid::new_v4().to_string(),
            |s| s.cause = events::BindingCause::Startup,
        ];
        for vary in variants {
            let mut other = ResumeStart::new(&session);
            vary(&mut other);
            assert_eq!(other.judged((0, 0), Some(&retired)), refused);
        }
    }
}
