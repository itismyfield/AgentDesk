use super::*;

fn attempt(id: &str) -> RestartAttempt {
    RestartAttempt(id.into())
}
fn context() -> RowContext {
    RowContext {
        nonce: Some("nonce-a".into()),
        actor_cutoff: Some((7, 90)),
        turn_id: 0,
        turn_start_offset: Some(42),
    }
}
fn binding() -> RowBinding<[u64; 8]> {
    RowBinding::capture([1, 2, 3, 4, 5, 6, 7, 8], 9, context()).unwrap()
}
fn report(association: Association<[u64; 8]>) -> ReportPin<[u64; 8]> {
    ReportPin {
        provider: "claude".into(),
        channel_id: 42,
        attempt: attempt("a"),
        association,
        state: RestartReportState::Awaiting,
    }
}

#[test]
fn association_and_attempt_fence() {
    let before = decision_count();
    let expected = report(Association::RowEpisode(binding()));
    let mut replacements = Vec::new();
    let mut next = expected.clone();
    next.attempt = attempt("b");
    replacements.push(next);
    let mut next = expected.clone();
    next.provider = "codex".into();
    replacements.push(next);
    let mut next = expected.clone();
    next.channel_id += 1;
    replacements.push(next);
    let mut next = expected.clone();
    next.association = Association::RestartOnly;
    replacements.push(next);
    let mut next = expected.clone();
    next.state = RestartReportState::Succeeded;
    replacements.push(next);
    for current in replacements {
        assert_eq!(
            decide(&expected, &current, Evidence::RowCompleted(binding())),
            Decision::Preserve
        );
    }
    assert_eq!(
        decide(&expected, &expected, Evidence::RowCompleted(binding())),
        Decision::ConsumeRowReport
    );
    assert!(decision_count() > before);
}

#[test]
fn committed_row_requires_full_pin_revision_and_required_evidence() {
    let expected = report(Association::RowEpisode(binding()));
    // Every scalar axis represents an axis of the existing full episode pin, including id-0 offset.
    for axis in 0..8 {
        let mut changed = binding();
        changed.full_pin[axis] += 1;
        assert_eq!(
            decide(&expected, &expected, Evidence::RowCompleted(changed)),
            Decision::Preserve
        );
    }
    let mut changed = binding();
    changed.row_revision += 1;
    assert_eq!(
        decide(&expected, &expected, Evidence::RowCompleted(changed)),
        Decision::Preserve
    );
    let mut changed = binding();
    changed.context.actor_cutoff = Some((7, 91));
    assert_eq!(
        decide(&expected, &expected, Evidence::RowCompleted(changed)),
        Decision::Preserve
    );
    for bad in [
        RowContext {
            nonce: None,
            ..context()
        },
        RowContext {
            nonce: Some(" ".into()),
            ..context()
        },
        RowContext {
            actor_cutoff: None,
            ..context()
        },
        RowContext {
            actor_cutoff: Some((0, 90)),
            ..context()
        },
        RowContext {
            turn_start_offset: None,
            ..context()
        },
    ] {
        assert!(
            RowBinding::capture([1; 8], 1, bad).is_none(),
            "missing attribution must preserve"
        );
    }
    assert_eq!(
        decide(&expected, &expected, Evidence::Unverified),
        Decision::Preserve
    );
    assert_eq!(
        decide(
            &expected,
            &expected,
            Evidence::RestartDelivery {
                attempt: attempt("a"),
                accepted: Ok(())
            }
        ),
        Decision::Preserve
    );
}

#[test]
fn restart_only_requires_proof_and_acceptance() {
    let capacity = super::super::capacity::Capacity {
        canonical: 4096,
        archived: 0,
        invalid: 0,
        complete: true,
    };
    assert_eq!(
        capacity.new_attempt(0),
        super::super::capacity::Admission::CapacityDeferred
    );
    let awaiting = report(Association::RestartOnly);
    for state in [
        RestartReportState::Succeeded,
        RestartReportState::Failed(RestartFailure::Cancelled),
        RestartReportState::Failed(RestartFailure::RolledBack),
    ] {
        assert_eq!(
            decide(
                &awaiting,
                &awaiting,
                Evidence::ExplicitRestartSettle {
                    attempt: attempt("a"),
                    state: state.clone()
                }
            ),
            Decision::SettleRestart(state.clone())
        );
        let terminal = ReportPin {
            state,
            ..awaiting.clone()
        };
        assert_eq!(
            decide(
                &terminal,
                &terminal,
                Evidence::RestartDelivery {
                    attempt: attempt("a"),
                    accepted: Ok(())
                }
            ),
            Decision::ConsumeRestartReport
        );
        assert_eq!(
            decide(
                &terminal,
                &terminal,
                Evidence::RestartDelivery {
                    attempt: attempt("a"),
                    accepted: Err(())
                }
            ),
            Decision::Preserve
        );
        assert_eq!(
            decide(
                &terminal,
                &terminal,
                Evidence::RestartDelivery {
                    attempt: attempt("b"),
                    accepted: Ok(())
                }
            ),
            Decision::Preserve
        );
    }
    for state in [
        RestartReportState::Awaiting,
        RestartReportState::Failed(RestartFailure::Unverified),
    ] {
        assert_eq!(
            decide(
                &awaiting,
                &awaiting,
                Evidence::ExplicitRestartSettle {
                    attempt: attempt("a"),
                    state: state.clone()
                }
            ),
            Decision::Preserve
        );
        let unverified = ReportPin {
            state,
            ..awaiting.clone()
        };
        assert_eq!(
            decide(
                &unverified,
                &unverified,
                Evidence::RestartDelivery {
                    attempt: attempt("a"),
                    accepted: Ok(())
                }
            ),
            Decision::Preserve
        );
    }
    assert_eq!(
        decide(
            &awaiting,
            &awaiting,
            Evidence::ExplicitRestartSettle {
                attempt: attempt("b"),
                state: RestartReportState::Succeeded
            }
        ),
        Decision::Preserve
    );
    let legacy = report(Association::LegacyUnassociated);
    assert_eq!(
        decide(
            &legacy,
            &legacy,
            Evidence::ExplicitRestartSettle {
                attempt: attempt("a"),
                state: RestartReportState::Succeeded
            }
        ),
        Decision::Preserve
    );
    assert_eq!(
        decide(&legacy, &legacy, Evidence::RowCompleted(binding())),
        Decision::Preserve
    );
}
