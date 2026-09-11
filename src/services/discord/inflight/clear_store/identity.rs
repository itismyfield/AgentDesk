use super::*;

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
) -> GuardedClearOutcome {
    clear_inflight_state_if_matches_identity_turn_nonce_in_root(
        root, provider, channel_id, expected, None,
    )
}

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity_returning_row_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
) -> (GuardedClearOutcome, Option<InflightTurnState>) {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return (GuardedClearOutcome::IoError, None);
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return (GuardedClearOutcome::Missing, None);
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return (GuardedClearOutcome::Missing, None);
    };
    let outcome = guarded_identity_clear_outcome(&state, expected, None);
    if outcome != GuardedClearOutcome::Cleared {
        return (outcome, None);
    }
    remove_identity_matched_state(
        &path,
        provider,
        channel_id,
        expected,
        state,
        "clear_inflight_state_if_matches_identity",
    )
}

fn guarded_identity_clear_outcome(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
) -> GuardedClearOutcome {
    if state.restart_mode.is_some() {
        return GuardedClearOutcome::PlannedRestartSkipped;
    }
    if state.rebind_origin {
        return GuardedClearOutcome::RebindOriginSkipped;
    }
    // #5464 B3 — an UNNAMEABLE identity is never a match. When `user_msg_id` is
    // 0 AND the `turn_start_offset` disambiguator is gone, `matches_state` has
    // no axis left that RELIABLY separates two id-0 turns of one channel
    // (`started_at` collides at 1-second resolution, `tmux_session_name` is the
    // shared pane), so with those two axes equal it
    // folds one turn's identity onto another turn's row, and the clear deletes
    // a LIVE row mid-turn, after which that turn's terminal is suppressed as
    // `no_inflight_row`.
    //
    // Deliberately NOT a blanket id-0 refusal: #3161 established that an id-0
    // turn must still clean up its OWN row (the dedicated
    // `clear_inflight_state_if_matches_zero_owned` path), and the watcher
    // terminal-commit, TUI-direct and stall-exit paths all clear id-0 rows that
    // still carry their offset. Only the conjunction is refused — the same
    // shape every save_store identity gate uses.
    //
    // Cost of the refusal: a legacy id-0 row whose on-disk JSON predates
    // `turn_start_offset` (field added 2026-04-16; read back as `None` through
    // `#[serde(default)]`) is refused at all five identity-guarded entry
    // points. It is not stranded: `clear_inflight_state_if_matches_zero_owned`
    // in `clear_store/mod.rs` never enters this chokepoint and clears on the
    // on-disk `user_msg_id == 0` alone, so recovery self-cleanup still lands.
    if expected.is_unnameable() {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    if !expected.matches_state(state) || !turn_nonce_matches(expected_turn_nonce, state) {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    GuardedClearOutcome::Cleared
}

fn remove_identity_matched_state(
    path: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    state: InflightTurnState,
    reason: &'static str,
) -> (GuardedClearOutcome, Option<InflightTurnState>) {
    log_inflight_remove(provider, channel_id, state.user_msg_id, reason, path);
    match fs::remove_file(path) {
        Ok(()) => (GuardedClearOutcome::Cleared, Some(state)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (GuardedClearOutcome::Missing, None)
        }
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight identity-guarded clear remove_file failed; treating as IoError so sweeper retries"
            );
            (GuardedClearOutcome::IoError, None)
        }
    }
}

pub(in crate::services::discord) fn turn_nonce_matches(
    expected_turn_nonce: Option<&str>,
    state: &InflightTurnState,
) -> bool {
    match (
        expected_turn_nonce.filter(|value| !value.is_empty()),
        state
            .turn_nonce
            .as_deref()
            .filter(|value| !value.is_empty()),
    ) {
        (Some(expected), Some(actual)) => expected == actual,
        _ => true,
    }
}

fn clear_inflight_state_if_matches_identity_turn_nonce_impl_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
    reconcile_current_generation: Option<u64>,
    exact_nonce: bool,
) -> super::reconcile_gate::ReconcileClearOutcome {
    use super::reconcile_gate::ReconcileClearOutcome;

    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::IoError);
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::Missing);
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::Missing);
    };
    if exact_nonce
        && expected_turn_nonce.filter(|nonce| !nonce.is_empty())
            != state
                .turn_nonce
                .as_deref()
                .filter(|nonce| !nonce.is_empty())
    {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::UserMsgMismatch);
    }
    if reconcile_current_generation
        .is_some_and(|current| super::reconcile_gate::row_is_current_generation(&state, current))
    {
        // The refusal reports the generation of THIS row — the one just read
        // under the lock — not the caller's snapshot (#5462 S5 r2).
        return ReconcileClearOutcome::LiveGenerationSkipped {
            fresh_born_generation: state.born_generation,
        };
    }
    let outcome = guarded_identity_clear_outcome(&state, expected, expected_turn_nonce);
    if outcome != GuardedClearOutcome::Cleared {
        return ReconcileClearOutcome::Delegated(outcome);
    }
    ReconcileClearOutcome::Delegated(
        remove_identity_matched_state(
            &path,
            provider,
            channel_id,
            expected,
            state,
            "clear_inflight_state_if_matches_identity",
        )
        .0,
    )
}

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity_turn_nonce_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
) -> GuardedClearOutcome {
    match clear_inflight_state_if_matches_identity_turn_nonce_impl_in_root(
        root,
        provider,
        channel_id,
        expected,
        expected_turn_nonce,
        None,
        false,
    ) {
        super::reconcile_gate::ReconcileClearOutcome::Delegated(outcome) => outcome,
        super::reconcile_gate::ReconcileClearOutcome::LiveGenerationSkipped { .. } => {
            unreachable!("the ordinary identity clear never enables the reconcile gate")
        }
    }
}

pub(super) fn clear_inflight_state_if_matches_identity_turn_nonce_for_reconcile_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
    current_generation: u64,
) -> super::reconcile_gate::ReconcileClearOutcome {
    clear_inflight_state_if_matches_identity_turn_nonce_impl_in_root(
        root,
        provider,
        channel_id,
        expected,
        expected_turn_nonce,
        Some(current_generation),
        false,
    )
}

pub(in crate::services::discord) fn clear_inflight_state_for_captured_episode(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    captured_nonce: Option<&str>,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    match clear_inflight_state_if_matches_identity_turn_nonce_impl_in_root(
        &root,
        provider,
        channel_id,
        expected,
        captured_nonce,
        None,
        true,
    ) {
        super::reconcile_gate::ReconcileClearOutcome::Delegated(outcome) => outcome,
        _ => unreachable!("captured episode clear does not use reconcile generation policy"),
    }
}

fn clear_rebind_origin_inflight_state_if_matches_identity_impl_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
    reconcile_current_generation: Option<u64>,
) -> super::reconcile_gate::ReconcileClearOutcome {
    use super::reconcile_gate::ReconcileClearOutcome;

    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::IoError);
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::Missing);
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::Missing);
    };
    if reconcile_current_generation
        .is_some_and(|current| super::reconcile_gate::row_is_current_generation(&state, current))
    {
        // Same fresh-row reporting contract as the turn-nonce fence above.
        return ReconcileClearOutcome::LiveGenerationSkipped {
            fresh_born_generation: state.born_generation,
        };
    }
    // #5464 B3: this rebind path decides on `rebind_origin && matches_state &&
    // nonce` WITHOUT the chokepoint's `is_unnameable` refusal, deliberately.
    // Every production rebind-origin row is born through
    // `InflightTurnState::new`, which stamps `turn_start_offset:
    // Some(last_offset)` (`model.rs`); neither
    // `build_external_adopted_inflight_state` (`recovery_engine/manual_rebind`)
    // nor `build_monitor_triggered_inflight_state` (`tmux.rs`) touches that
    // field, and the monitor path re-stamps `Some(turn_start_offset)`
    // (`tmux/monitor_auto_turn_inflight.rs`). No production site assigns
    // `None`. The only `None` a rebind row can carry is a pre-2026-04-16
    // on-disk row read through `#[serde(default)]`, and `matches_state`
    // compares the offset by exact `Option` equality, so an unnameable
    // `expected` can only ever match THAT legacy row — the rollback owner's
    // OWN row, which refusing would strand (the reverted revision-1 shape).
    let outcome = if state.restart_mode.is_some() {
        GuardedClearOutcome::PlannedRestartSkipped
    } else if !state.rebind_origin
        || !expected.matches_state(&state)
        || !turn_nonce_matches(expected_turn_nonce, &state)
    {
        GuardedClearOutcome::UserMsgMismatch
    } else {
        GuardedClearOutcome::Cleared
    };
    if outcome != GuardedClearOutcome::Cleared {
        return ReconcileClearOutcome::Delegated(outcome);
    }
    ReconcileClearOutcome::Delegated(
        remove_identity_matched_state(
            &path,
            provider,
            channel_id,
            expected,
            state,
            "clear_rebind_origin_inflight_state_if_matches_identity",
        )
        .0,
    )
}

pub(in crate::services::discord) fn clear_rebind_origin_inflight_state_if_matches_identity_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
) -> GuardedClearOutcome {
    match clear_rebind_origin_inflight_state_if_matches_identity_impl_in_root(
        root,
        provider,
        channel_id,
        expected,
        expected_turn_nonce,
        None,
    ) {
        super::reconcile_gate::ReconcileClearOutcome::Delegated(outcome) => outcome,
        super::reconcile_gate::ReconcileClearOutcome::LiveGenerationSkipped { .. } => {
            unreachable!("the ordinary rebind clear never enables the reconcile gate")
        }
    }
}

pub(super) fn clear_rebind_origin_inflight_state_if_matches_identity_for_reconcile_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_nonce: Option<&str>,
    current_generation: u64,
) -> super::reconcile_gate::ReconcileClearOutcome {
    clear_rebind_origin_inflight_state_if_matches_identity_impl_in_root(
        root,
        provider,
        channel_id,
        expected,
        expected_turn_nonce,
        Some(current_generation),
    )
}

#[cfg(test)]
mod tests {
    use super::super::reconcile_gate::ReconcileClearOutcome;
    use super::*;

    struct Fixture {
        _env: crate::config::TestEnvVarGuard,
        _temp: tempfile::TempDir,
        root: std::path::PathBuf,
        path: std::path::PathBuf,
        row: InflightTurnState,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
            let root = temp.path().join("runtime").join("discord_inflight");
            assert_eq!(inflight_runtime_root().as_ref(), Some(&root));
            let mut row = InflightTurnState::new(
                ProviderKind::Claude,
                576_701,
                None,
                42,
                123,
                0,
                "strict clear fixture".into(),
                None,
                Some("strict-clear-test".into()),
                None,
                None,
                0,
            );
            row.started_at = "2026-09-07T00:00:00Z".into();
            row.turn_start_offset = Some(10);
            row.turn_nonce = Some("episode-a".into());
            row.born_generation = 9;
            let path = inflight_state_path(&root, &ProviderKind::Claude, row.channel_id);
            Self {
                _env: env,
                _temp: temp,
                root,
                path,
                row,
            }
        }

        fn seed(&self, row: &InflightTurnState) -> Vec<u8> {
            // Keep missing/empty legacy nonces exactly as serialized in the fixture.
            fs::create_dir_all(self.path.parent().unwrap()).unwrap();
            let _lock = lock_inflight_state_path(&self.path).unwrap();
            let bytes = serde_json::to_vec(row).unwrap();
            fs::write(&self.path, &bytes).unwrap();
            bytes
        }

        fn captured_clear(&self, captured_nonce: Option<&str>) -> GuardedClearOutcome {
            crate::services::discord::inflight::clear_inflight_state_for_captured_episode(
                &ProviderKind::Claude,
                self.row.channel_id,
                &InflightTurnIdentity::from_state(&self.row),
                captured_nonce,
            )
        }

        fn assert_preserved(&self, bytes: &[u8]) {
            assert_eq!(fs::read(&self.path).unwrap(), bytes);
        }
    }

    #[test]
    fn captured_episode_clears_its_matching_row() {
        let fixture = Fixture::new();
        fixture.seed(&fixture.row);
        assert_eq!(
            fixture.captured_clear(Some("episode-a")),
            GuardedClearOutcome::Cleared
        );
        assert!(!fixture.path.exists());
        assert_eq!(
            fixture.captured_clear(Some("episode-a")),
            GuardedClearOutcome::Missing
        );
    }

    #[test]
    fn captured_episode_preserves_a_foreign_nonce() {
        let fixture = Fixture::new();
        let mut successor = fixture.row.clone();
        successor.turn_nonce = Some("episode-b".into());
        let bytes = fixture.seed(&successor);
        assert_eq!(
            fixture.captured_clear(Some("episode-a")),
            GuardedClearOutcome::UserMsgMismatch
        );
        fixture.assert_preserved(&bytes);
    }

    #[test]
    fn captured_episode_strictly_separates_legacy_and_modern_nonces() {
        let fixture = Fixture::new();
        for (captured, stored, clears) in [
            (None, Some("episode-a"), false),
            (Some("episode-a"), None, false),
            (None, None, true),
            (Some(""), None, true),
            (None, Some(""), true),
            (Some(""), Some("episode-a"), false),
            (Some("episode-a"), Some(""), false),
        ] {
            let mut row = fixture.row.clone();
            row.turn_nonce = stored.map(str::to_owned);
            let bytes = fixture.seed(&row);
            let outcome = fixture.captured_clear(captured);
            assert_eq!(
                outcome,
                if clears {
                    GuardedClearOutcome::Cleared
                } else {
                    GuardedClearOutcome::UserMsgMismatch
                },
                "captured={captured:?}, stored={stored:?}"
            );
            if clears {
                assert!(!fixture.path.exists());
            } else {
                fixture.assert_preserved(&bytes);
            }
        }
    }

    #[test]
    fn captured_episode_preserves_every_changed_identity_field() {
        let fixture = Fixture::new();
        for field in [
            "user_msg_id",
            "started_at",
            "tmux_session_name",
            "turn_start_offset",
        ] {
            let mut successor = fixture.row.clone();
            match field {
                "user_msg_id" => successor.user_msg_id += 1,
                "started_at" => successor.started_at = "2026-09-07T00:00:01Z".into(),
                "tmux_session_name" => successor.tmux_session_name = Some("successor".into()),
                "turn_start_offset" => successor.turn_start_offset = Some(11),
                _ => unreachable!(),
            }
            let bytes = fixture.seed(&successor);
            assert_eq!(
                fixture.captured_clear(Some("episode-a")),
                GuardedClearOutcome::UserMsgMismatch,
                "changed {field}"
            );
            fixture.assert_preserved(&bytes);
        }
    }

    #[test]
    fn captured_episode_preserves_restart_and_rebind_rows() {
        let fixture = Fixture::new();
        for (restart, rebind, expected) in [
            (true, false, GuardedClearOutcome::PlannedRestartSkipped),
            (false, true, GuardedClearOutcome::RebindOriginSkipped),
            (true, true, GuardedClearOutcome::PlannedRestartSkipped),
        ] {
            let mut row = fixture.row.clone();
            row.restart_mode = restart.then_some(InflightRestartMode::DrainRestart);
            row.rebind_origin = rebind;
            let bytes = fixture.seed(&row);
            assert_eq!(fixture.captured_clear(Some("episode-a")), expected);
            fixture.assert_preserved(&bytes);
        }
    }

    #[test]
    fn ordinary_and_reconcile_clear_keep_legacy_nonce_wildcards() {
        let fixture = Fixture::new();
        let identity = InflightTurnIdentity::from_state(&fixture.row);
        for (expected_nonce, stored_nonce, expected) in [
            (None, Some("episode-a"), GuardedClearOutcome::Cleared),
            (Some("episode-a"), None, GuardedClearOutcome::Cleared),
            (Some("episode-a"), Some(""), GuardedClearOutcome::Cleared),
            (Some(""), Some("episode-a"), GuardedClearOutcome::Cleared),
            (
                Some("episode-a"),
                Some("episode-b"),
                GuardedClearOutcome::UserMsgMismatch,
            ),
        ] {
            let mut row = fixture.row.clone();
            row.turn_nonce = stored_nonce.map(str::to_owned);
            for reconcile in [false, true] {
                let bytes = fixture.seed(&row);
                let outcome = if reconcile {
                    clear_inflight_state_if_matches_identity_turn_nonce_for_reconcile_in_root(
                        &fixture.root,
                        &ProviderKind::Claude,
                        row.channel_id,
                        &identity,
                        expected_nonce,
                        10,
                    )
                } else {
                    ReconcileClearOutcome::Delegated(
                        clear_inflight_state_if_matches_identity_turn_nonce_in_root(
                            &fixture.root,
                            &ProviderKind::Claude,
                            row.channel_id,
                            &identity,
                            expected_nonce,
                        ),
                    )
                };
                assert_eq!(outcome, ReconcileClearOutcome::Delegated(expected));
                if expected == GuardedClearOutcome::Cleared {
                    assert!(!fixture.path.exists());
                } else {
                    fixture.assert_preserved(&bytes);
                }
            }
        }
    }

    /// #5464 B3 closing test. An UNNAMEABLE row — `user_msg_id == 0` with its
    /// `turn_start_offset` disambiguator gone — is live: the turn is running,
    /// but nothing in the identity distinguishes it from another id-0 turn of
    /// the same channel. Before the guard, one turn's identity folded onto a
    /// DIFFERENT turn's row and the clear deleted it mid-turn, after which that
    /// turn's terminal was suppressed as `no_inflight_row`.
    #[test]
    fn unnameable_identity_never_matches_a_foreign_row_mid_turn_5464() {
        let fixture = Fixture::new();

        // Two genuinely different turns. They differ in `turn_nonce` and
        // `user_text`, neither of which `InflightTurnIdentity` carries.
        let mut turn_a = fixture.row.clone();
        turn_a.user_msg_id = 0;
        turn_a.turn_start_offset = None;
        turn_a.turn_nonce = Some("episode-a".into());

        let mut turn_b = turn_a.clone();
        turn_b.turn_nonce = Some("episode-b".into());
        turn_b.user_text = "a later, still-running turn".into();

        let identity_a = InflightTurnIdentity::from_state(&turn_a);
        assert!(identity_a.is_unnameable());
        // The real degeneracy: turn A's identity folds onto turn B's row. This
        // is a CROSS-turn comparison, not a row against itself.
        assert!(
            identity_a.matches_state(&turn_b),
            "all four axes collapse once the id is 0 and the offset is gone"
        );

        for site in [
            "identity",
            "identity_turn_nonce",
            "returning_row",
            "captured_episode",
            "reconcile",
        ] {
            // Turn B's row is on disk; turn A tries to clear with its identity.
            let bytes = fixture.seed(&turn_b);
            let outcome = match site {
                "identity" => clear_inflight_state_if_matches_identity_in_root(
                    &fixture.root,
                    &ProviderKind::Claude,
                    turn_b.channel_id,
                    &identity_a,
                ),
                "identity_turn_nonce" => {
                    clear_inflight_state_if_matches_identity_turn_nonce_in_root(
                        &fixture.root,
                        &ProviderKind::Claude,
                        turn_b.channel_id,
                        &identity_a,
                        None,
                    )
                }
                "returning_row" => {
                    clear_inflight_state_if_matches_identity_returning_row_in_root(
                        &fixture.root,
                        &ProviderKind::Claude,
                        turn_b.channel_id,
                        &identity_a,
                    )
                    .0
                }
                "captured_episode" => {
                    crate::services::discord::inflight::clear_inflight_state_for_captured_episode(
                        &ProviderKind::Claude,
                        turn_b.channel_id,
                        &identity_a,
                        None,
                    )
                }
                "reconcile" => {
                    match clear_inflight_state_if_matches_identity_turn_nonce_for_reconcile_in_root(
                        &fixture.root,
                        &ProviderKind::Claude,
                        turn_b.channel_id,
                        &identity_a,
                        None,
                        10,
                    ) {
                        ReconcileClearOutcome::Delegated(outcome) => outcome,
                        other => panic!("{site}: unexpected {other:?}"),
                    }
                }
                _ => unreachable!(),
            };
            assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch, "{site}");
            fixture.assert_preserved(&bytes);
        }
    }

    /// #5464 B3 sufficiency proof for the two axes `is_unnameable` does NOT
    /// read. Neither names a turn: `started_at` collides at `now_string`'s
    /// 1-second resolution and `tmux_session_name` is the shared pane. They are
    /// not omitted from the decision, though — the guard stands in conjunction
    /// with `matches_state`, so wherever either axis DIFFERS the four-axis
    /// compare already refuses with the same outcome, and the guard can only
    /// decide where both agree, which is where they disambiguate nothing.
    #[test]
    fn unnameable_guard_only_decides_where_started_at_and_tmux_agree_5464() {
        let fixture = Fixture::new();
        let mut turn_a = fixture.row.clone();
        turn_a.user_msg_id = 0;
        turn_a.turn_start_offset = None;
        let identity_a = InflightTurnIdentity::from_state(&turn_a);
        assert!(identity_a.is_unnameable());

        for axis in ["started_at", "tmux_session_name"] {
            let mut other = turn_a.clone();
            match axis {
                "started_at" => other.started_at = "2026-09-07T00:00:01Z".into(),
                "tmux_session_name" => other.tmux_session_name = Some("other-pane".into()),
                _ => unreachable!(),
            }
            // The pre-existing four-axis compare already rejects this row, so
            // the new guard is not what refuses the clear here.
            assert!(!identity_a.matches_state(&other), "{axis}");
            let bytes = fixture.seed(&other);
            assert_eq!(
                clear_inflight_state_if_matches_identity_in_root(
                    &fixture.root,
                    &ProviderKind::Claude,
                    other.channel_id,
                    &identity_a,
                ),
                GuardedClearOutcome::UserMsgMismatch,
                "{axis}"
            );
            fixture.assert_preserved(&bytes);
        }

        // With both axes equal they separate nothing: a genuinely different
        // turn still folds, and only the new guard stands between it and a
        // mid-turn delete.
        let mut turn_b = turn_a.clone();
        turn_b.turn_nonce = Some("episode-b".into());
        assert!(identity_a.matches_state(&turn_b));
    }

    /// Narrowness control for #5464 B3. The guard closes ONLY the unnameable
    /// conjunction. An id-0 row that still carries its `turn_start_offset` is
    /// nameable and must keep clearing — #3161's "an id-0 turn still cleans up
    /// its own row", which the watcher terminal-commit, TUI-direct synthetic
    /// and stall-exit paths all depend on.
    #[test]
    fn id_zero_row_that_kept_its_offset_still_clears_5464() {
        let fixture = Fixture::new();
        let mut row = fixture.row.clone();
        row.user_msg_id = 0;
        row.turn_start_offset = Some(10);
        let identity = InflightTurnIdentity::from_state(&row);
        assert!(!identity.is_unnameable());

        fixture.seed(&row);
        assert_eq!(
            clear_inflight_state_if_matches_identity_in_root(
                &fixture.root,
                &ProviderKind::Claude,
                row.channel_id,
                &identity,
            ),
            GuardedClearOutcome::Cleared
        );
        assert!(!fixture.path.exists());
    }

    #[test]
    fn reconcile_clear_keeps_its_current_generation_fence() {
        let fixture = Fixture::new();
        let bytes = fixture.seed(&fixture.row);
        assert_eq!(
            clear_inflight_state_if_matches_identity_turn_nonce_for_reconcile_in_root(
                &fixture.root,
                &ProviderKind::Claude,
                fixture.row.channel_id,
                &InflightTurnIdentity::from_state(&fixture.row),
                None,
                9,
            ),
            ReconcileClearOutcome::LiveGenerationSkipped {
                fresh_born_generation: 9
            },
        );
        fixture.assert_preserved(&bytes);
    }
}
