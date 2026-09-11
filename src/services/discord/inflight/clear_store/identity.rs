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
    // #5464 B3 — an UNRESOLVED identity is never a match. `matches_state` folds
    // `user_msg_id == 0` onto any id-0 row (`0 == 0`), so a caller holding an
    // identity whose dispatch had not resolved could clear the channel's LIVE
    // row mid-turn; the terminal that followed was then suppressed as
    // `no_inflight_row`. Same judgment the after-delivery clear and the
    // finalizer's mailbox-release guard already make — `is_resolved` is the one
    // shape all four read. This does NOT relax exact-episode fencing: the nonce,
    // generation and 4-axis checks below still run for resolved identities.
    if !expected.is_resolved() {
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
    let outcome = if state.restart_mode.is_some() {
        GuardedClearOutcome::PlannedRestartSkipped
    } else if !state.rebind_origin
        || !expected.is_resolved()
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

    /// #5464 B3 closing test. A row whose dispatch has NOT resolved
    /// (`user_msg_id == 0`, ledger identity carried by `finalizer_turn_id`) is
    /// live: the turn is running, the Discord anchor just is not known yet.
    /// Before the `is_resolved` guard every clear below folded the expected
    /// identity onto it on `0 == 0` — the remaining three axes come from the
    /// same row — and deleted it MID-TURN, after which the turn's terminal was
    /// suppressed as `no_inflight_row` (the C1 loss, reached from upstream).
    #[test]
    fn unresolved_identity_row_is_never_matched_by_a_mid_turn_clear_5464() {
        let fixture = Fixture::new();
        let mut row = fixture.row.clone();
        row.user_msg_id = 0;
        let identity = InflightTurnIdentity::from_state(&row);
        assert!(
            !identity.is_resolved(),
            "an id-0 dispatch is unresolved however many disambiguators it carries"
        );
        assert!(
            identity.matches_state(&row),
            "the 4-axis compare still folds it — that is exactly what the guard must gate"
        );

        for site in [
            "identity",
            "identity_turn_nonce",
            "returning_row",
            "captured_episode",
            "reconcile",
        ] {
            let bytes = fixture.seed(&row);
            let outcome = match site {
                "identity" => clear_inflight_state_if_matches_identity_in_root(
                    &fixture.root,
                    &ProviderKind::Claude,
                    row.channel_id,
                    &identity,
                ),
                "identity_turn_nonce" => {
                    clear_inflight_state_if_matches_identity_turn_nonce_in_root(
                        &fixture.root,
                        &ProviderKind::Claude,
                        row.channel_id,
                        &identity,
                        Some("episode-a"),
                    )
                }
                "returning_row" => {
                    clear_inflight_state_if_matches_identity_returning_row_in_root(
                        &fixture.root,
                        &ProviderKind::Claude,
                        row.channel_id,
                        &identity,
                    )
                    .0
                }
                "captured_episode" => {
                    crate::services::discord::inflight::clear_inflight_state_for_captured_episode(
                        &ProviderKind::Claude,
                        row.channel_id,
                        &identity,
                        Some("episode-a"),
                    )
                }
                "reconcile" => {
                    match clear_inflight_state_if_matches_identity_turn_nonce_for_reconcile_in_root(
                        &fixture.root,
                        &ProviderKind::Claude,
                        row.channel_id,
                        &identity,
                        Some("episode-a"),
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

        // The rebind-origin clear keeps its own inline condition; it reads the
        // same judgment, so an unresolved rebind row survives too.
        let mut rebind = row.clone();
        rebind.rebind_origin = true;
        let rebind_identity = InflightTurnIdentity::from_state(&rebind);
        let bytes = fixture.seed(&rebind);
        assert_eq!(
            clear_rebind_origin_inflight_state_if_matches_identity_in_root(
                &fixture.root,
                &ProviderKind::Claude,
                rebind.channel_id,
                &rebind_identity,
                Some("episode-a"),
            ),
            GuardedClearOutcome::UserMsgMismatch
        );
        fixture.assert_preserved(&bytes);

        // Narrowness control: the guard closes ONLY the unresolved fold. The
        // same row with its dispatch resolved still clears on an exact episode.
        fixture.seed(&fixture.row);
        assert_eq!(
            fixture.captured_clear(Some("episode-a")),
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
