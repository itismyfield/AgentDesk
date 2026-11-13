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
    let auth = crate::services::provider_auth_profile::overlay_shell_env_lines(&overlay);
    let requested = if turn.force_fresh {
        None
    } else {
        current_session(turn.channel_id, logical, turn.session_id)
            .map_err(|e| launch_error("current_session", e))?
    };
    let prior = resumed_session(turn, &overlay, requested.as_deref())
        .map_err(|e| launch_error("resume_source", e))?;
    if prior.is_some() {
        let mut earlier = ports.earlier_tmux(logical);
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
        current_session(turn.channel_id, logical, turn.session_id)
            .map_err(|e| launch_error("current_session", e))?
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
    let (source, history) =
        events::claude_history(channel, logical, nonce).map_err(|e| e.to_string())?;
    if history.awaiting.is_some() {
        return Err("handoff native session is still Pending".into());
    }
    let legacy = source.is_none();
    let source = source
        .or_else(|| match &latest.new {
            events::BindingTarget::Source(source)
            | events::BindingTarget::Resolved { source, .. }
                if latest.evidence.hook_event.is_none() =>
            {
                Some(source.clone())
            }
            _ => None,
        })
        .ok_or("handoff native session has no verified durable binding")?;
    if legacy {
        use crate::services::tui_prompt_dedupe::binding_context::{
            SpawnNonceMarker, observe_spawn_nonce_marker,
        };
        // A failed create can leave a marker without logging a native execution.
        match (nonce, observe_spawn_nonce_marker(logical)) {
            (Some(recorded), SpawnNonceMarker::Known(current)) if recorded != current => {
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
    if (metadata.dev(), metadata.ino()) != (source.dev, source.ino) {
        return Err("handoff transcript identity changed".into());
    }
    Ok(if legacy {
        original.map(str::to_string)
    } else {
        Some(source.session_id)
    })
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
        assert!(current_session(log.channel, log.logical, Some(&argv)).is_err());
        log.push(events::BindingTarget::Resolved {
            pending_seq: 2,
            source,
        });
        assert_eq!(
            current_session(log.channel, log.logical, Some(&argv)).unwrap(),
            Some(continued.clone())
        );
        log.push(events::BindingTarget::Rejected {
            payload_session_id: argv.clone(),
            payload_transcript_path: None,
            reason: "audit only".into(),
        });
        assert_eq!(
            current_session(log.channel, log.logical, Some(&argv)).unwrap(),
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
                current_session(log.channel, log.logical, Some(&original)).unwrap(),
                Some(original.clone())
            );
            if let Some(nonce) = &nonce {
                std::fs::write(&marker, "b".repeat(32)).unwrap();
                assert!(current_session(log.channel, log.logical, Some(&original)).is_err());
                std::fs::write(&marker, nonce).unwrap();
            } else {
                std::fs::write(&marker, "b".repeat(32)).unwrap();
                assert_eq!(
                    current_session(log.channel, log.logical, Some(&original)).unwrap(),
                    Some(original.clone())
                );
                std::fs::remove_file(&marker).unwrap();
            }
            std::fs::rename(&source.path, source.path.with_extension("old")).unwrap();
            std::fs::write(&source.path, b"{}\n").unwrap();
            assert!(current_session(log.channel, log.logical, Some(&original)).is_err());
        }
    }
    #[test]
    fn missing_log_keeps_legacy_id_but_corrupt_or_replaced_source_never_uses_it() {
        let mut log = Log::new(8077012);
        let original = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            current_session(log.channel, log.logical, Some(&original)).unwrap(),
            Some(original.clone())
        );
        let source = log.source(&original);
        log.push(events::BindingTarget::Source(source.clone()));
        std::fs::remove_file(&source.path).unwrap();
        assert!(current_session(log.channel, log.logical, Some(&original)).is_err());
        std::fs::write(log.path(), b"not-json\n").unwrap();
        assert!(current_session(log.channel, log.logical, Some(&original)).is_err());
    }
}
