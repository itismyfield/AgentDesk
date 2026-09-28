use super::*;

#[test]
fn oversized_observation_stays_incomplete_after_reaching_eof() {
    use super::super::super::ledger::read_ledger_at;
    use super::super::super::observation::{
        ReachabilityObservationState, capture_watcher_incarnation, observe_channel_at,
    };
    use super::super::super::tail::TAIL_READ_CAP_BYTES;
    use std::io::Write;

    let root = tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let provider = provider();
    let ledger_file = ledger_path(&provider, 6372).unwrap();
    let transcript = root.path().join("transcript.jsonl");
    std::fs::write(&transcript, b"").unwrap();
    let observe = || {
        let snapshot = capture_watcher_incarnation(
            &transcript,
            SESSION,
            GENERATION,
            Some("coverage-nonce".into()),
        )
        .unwrap();
        observe_channel_at(&ledger_file, snapshot.clone(), NOW_MS, || Some(snapshot))
    };
    let wire = || {
        let verdict = observe_relay_verdict(RelayVerdictProbe {
            provider: Some(&provider),
            channel_id: 6372,
            row_output_path: None,
            registry_output_path: Some(transcript.to_str().unwrap()),
            pane_idle_confirmed: true,
            rowless_turn: RowlessTurn::None,
            placeholder_present: false,
            executor: ExecutorWitness::Present,
            now_epoch_ms: NOW_MS,
            process_started_at_epoch_ms: PROCESS_STARTED_MS,
        });
        serde_json::to_value(RelayVerdictReport::of(&verdict, true)).unwrap()
    };
    assert_eq!(observe(), ReachabilityObservationState::Bootstrapped);
    let baseline = wire();
    assert_eq!(baseline["verdict"], "reachable");
    assert_eq!(baseline["coverage"]["observation_state"], "current");
    assert_eq!(baseline["coverage"]["pending_ranges"], 0);

    let assistant_line = |text: &str| {
        format!(
            "{}\n",
            serde_json::json!({"type": "assistant", "timestamp": "2020-01-01T00:00:00Z",
                "message": {"model": "claude", "content": [{"type": "text", "text": text}]
            }})
        )
    };
    let raw = assistant_line(&"x".repeat(TAIL_READ_CAP_BYTES as usize + 64));
    assert!(raw.len() as u64 > TAIL_READ_CAP_BYTES);
    std::fs::write(&transcript, &raw).unwrap();
    assert!(matches!(
        observe(),
        ReachabilityObservationState::Recorded { .. }
    ));
    let capped = read_ledger_at(&ledger_file).unwrap();
    assert_eq!(capped.cursor_offset, TAIL_READ_CAP_BYTES);
    assert_eq!(capped.counters.incomplete_observations, 1);
    assert!(capped.live_obligations().is_empty());

    for _ in 0..2 {
        assert!(matches!(
            observe(),
            ReachabilityObservationState::Recorded { .. }
        ));
        let ledger = read_ledger_at(&ledger_file).unwrap();
        assert_eq!(ledger.cursor_offset, raw.len() as u64);
        assert_eq!(ledger.last_observed_len, ledger.cursor_offset);
        assert_eq!(ledger.counters.incomplete_observations, 1);
        let observed = wire();
        println!("oversized EOF wire: {observed}");
        assert_eq!(observed["verdict"], baseline["verdict"]);
        assert!(observed.get("uncovered_ranges").is_none());
        for field in ["uncovered_ranges", "unproven_ranges", "pending_ranges"] {
            assert_eq!(observed["coverage"][field], 0);
        }
        assert_eq!(observed["coverage"]["observation_state"], "incomplete");
    }

    std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(assistant_line("known obligation").as_bytes())
        .unwrap();
    assert!(matches!(
        observe(),
        ReachabilityObservationState::Recorded { .. }
    ));
    let observed = wire();
    println!("incomplete with known obligation wire: {observed}");
    assert_eq!(observed["verdict"], baseline["verdict"]);
    assert_eq!(observed["coverage"]["observation_state"], "incomplete");
    assert_eq!(observed["coverage"]["uncovered_ranges"], 1);
    assert_eq!(observed["coverage"]["unproven_ranges"], 0);
    assert_eq!(observed["coverage"]["pending_ranges"], 1);
}

#[test]
fn reachable_grace_keeps_pending_on_the_wire() {
    let verdict = observe_rowless_channel(
        6300,
        vec![
            obligation(0, 100, 119),
            obligation(100, 200, 80),
            obligation(200, 300, 70),
        ],
        unproven_incarnation(),
        vec![receipt((100, 300), GENERATION)],
        RowlessTurn::None,
    );
    let wire = serde_json::to_value(RelayVerdictReport::of(&verdict, true)).unwrap();
    println!("coverage wire: {wire}");
    assert_eq!(wire["verdict"], "reachable");
    assert_eq!(wire["coverage"]["pending_ranges"], 3);
    assert_eq!(wire["coverage"]["uncovered_ranges"], 1);
    assert_eq!(wire["coverage"]["unproven_ranges"], 2);
}

fn project(case: &Case, now: u64) -> RelayVerdict {
    let (in_band, coverage) = evaluate_reachability(ReachabilityInputs {
        provider: &provider(),
        divergence: case.divergence,
        ledger: case.ledger.as_ref(),
        ledger_present: case.ledger_present,
        ledger_observed_at_epoch_ms: case.ledger_observed_at_epoch_ms,
        executor: case.executor,
        receipts: &case.receipts,
        transcript: case.transcript,
        read_truncated: case.read_truncated,
        rowless_turn: case.rowless_turn,
        placeholder_present: case.placeholder_present,
        now_epoch_ms: now,
        process_started_at_epoch_ms: PROCESS_STARTED_MS,
    });
    let mut verdict = compose_relay_verdict(in_band, ExternalRelayVerdict::Unknown);
    verdict.coverage = coverage;
    verdict
}

fn counts(verdict: &RelayVerdict) -> (Option<u32>, Option<u32>, Option<u32>) {
    let c = &verdict.coverage;
    (c.uncovered_ranges, c.unproven_ranges, c.pending_ranges)
}

#[test]
fn grace_boundary_changes_only_the_existing_ladder() {
    let (receipts, _dir) = receipts_covering(100, 300, GENERATION);
    let case = Case {
        ledger: Some(ledger_with(
            vec![
                obligation(0, 100, 119),
                obligation(100, 200, 80),
                obligation(200, 300, 70),
            ],
            unproven_incarnation(),
        )),
        receipts,
        ..Case::default()
    };
    for (elapsed, label) in [(0, "reachable"), (1, "degraded"), (481, "unreachable")] {
        let verdict = project(&case, NOW_MS + elapsed * 1000);
        let wire = serde_json::to_value(RelayVerdictReport::of(&verdict, true)).unwrap();
        println!("boundary wire: {wire}");
        assert_eq!(verdict.label(), label);
        assert_eq!(counts(&verdict), (Some(1), Some(2), Some(3)));
        assert_eq!(
            verdict.coverage.oldest_uncovered_age_secs,
            Some(119 + elapsed)
        );
        assert_eq!(
            verdict.coverage.oldest_unproven_age_secs,
            Some(80 + elapsed)
        );
        assert_eq!(
            verdict.coverage.oldest_pending_age_secs,
            Some(119 + elapsed)
        );
        assert_eq!(wire["coverage"]["age_basis"], "first_observed");
        if elapsed == 0 {
            assert!(wire.get("uncovered_ranges").is_none());
        } else {
            assert_eq!(wire["uncovered_ranges"], 3);
        }
    }
}

#[test]
fn all_covered_proven_keeps_provenance_and_empty_ages() {
    let (receipts, _dir) = receipts_covering(0, 300, GENERATION);
    let case = Case {
        ledger: Some(ledger_with(
            vec![obligation(0, 100, 900), obligation(100, 300, 800)],
            proven_incarnation(),
        )),
        receipts,
        transcript: TranscriptLiveness::Resolved {
            eof: 4000,
            alive: true,
        },
        ..Case::default()
    };
    let verdict = project(&case, NOW_MS);
    assert_eq!(verdict.label(), "reachable");
    assert_eq!(counts(&verdict), (Some(0), Some(0), Some(0)));
    assert_eq!(verdict.coverage.observation_state, "current");
    assert_eq!(verdict.coverage.oldest_uncovered_age_secs, None);
    assert_eq!(verdict.coverage.oldest_unproven_age_secs, None);
    assert_eq!(verdict.coverage.oldest_pending_age_secs, None);
    assert_eq!(verdict.coverage.provenance.unwrap().exact_receipt_ranges, 2);
}

#[test]
fn absent_store_is_uncovered_but_unreadable_is_not_zero() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("receipts.json");
    let mut case = Case {
        ledger: Some(ledger_with(
            vec![obligation(0, 100, 119)],
            proven_incarnation(),
        )),
        receipts: read_receipt_index_at(&path),
        ..Case::default()
    };
    assert_eq!(counts(&project(&case, NOW_MS)), (Some(1), Some(0), Some(1)));
    std::fs::write(&path, b"not-json").unwrap();
    case.receipts = read_receipt_index_at(&path);
    let verdict = project(&case, NOW_MS);
    assert_eq!(verdict.label(), "unknown");
    assert_eq!(verdict.coverage.observation_state, "unreadable");
    assert_eq!(counts(&verdict), (None, None, None));
    let wire = serde_json::to_value(RelayVerdictReport::of(&verdict, true)).unwrap();
    println!("unreadable wire: {wire}");
    assert!(wire["coverage"]["pending_ranges"].is_null());
    case.receipts = ReceiptIndexRead::Absent;
    case.ledger = None;
    assert_eq!(
        project(&case, NOW_MS).coverage.observation_state,
        "unreadable"
    );
}

#[test]
fn never_observed_and_unresolved_do_not_claim_empty_coverage() {
    let mut case = Case {
        ledger: None,
        ledger_present: false,
        ledger_observed_at_epoch_ms: None,
        ..Case::default()
    };
    let verdict = project(&case, NOW_MS);
    assert_eq!(verdict.coverage.observation_state, "never_observed");
    assert_eq!(counts(&verdict), (None, None, None));
    assert_eq!(verdict.coverage.cursor_offset, None);
    assert_eq!(verdict.coverage.observed_eof, None);
    assert_eq!(verdict.coverage.observation_committed_at_epoch_ms, None);
    case = Case {
        transcript: TranscriptLiveness::Unresolved,
        ..Case::default()
    };
    let verdict = project(&case, NOW_MS);
    assert_eq!(verdict.coverage.observation_state, "unresolved");
    assert_eq!(counts(&verdict), (None, None, None));
    case.divergence = RowCoordinateDivergence::Diverged;
    assert_eq!(
        project(&case, NOW_MS).coverage.observation_state,
        "unresolved"
    );
}

#[test]
fn cursor_lag_does_not_turn_zero_pending_into_complete_observation() {
    let mut case = Case::default();
    for observed_eof in [4000, 4500] {
        case.ledger.as_mut().unwrap().last_observed_len = observed_eof;
        let verdict = project(&case, NOW_MS);
        assert_eq!(verdict.label(), "reachable");
        assert_eq!(counts(&verdict), (Some(0), Some(0), Some(0)));
        assert_eq!(verdict.coverage.observation_state, "lagging");
        assert_eq!(verdict.coverage.cursor_offset, Some(4000));
        assert_eq!(verdict.coverage.observed_eof, Some(observed_eof));
        assert_eq!(
            verdict.coverage.observation_committed_at_epoch_ms,
            Some(NOW_MS)
        );
    }
    case.transcript = TranscriptLiveness::Resolved {
        eof: 2000,
        alive: true,
    };
    for cursor in [1000, 4000] {
        case.ledger.as_mut().unwrap().cursor_offset = cursor;
        let verdict = project(&case, NOW_MS);
        assert_eq!(verdict.label(), "reachable");
        assert_eq!(verdict.coverage.observation_state, "unresolved");
        assert_eq!(counts(&verdict), (Some(0), Some(0), Some(0)));
    }
    case.read_truncated = true;
    let verdict = project(&case, NOW_MS);
    assert_eq!(verdict.coverage.observation_state, "lagging");
    assert_eq!(counts(&verdict), (None, None, None));
}

#[test]
fn expired_keeps_its_existing_abstention_and_stale_observation() {
    let case = Case {
        executor: ExecutorWitness::Absent,
        transcript: TranscriptLiveness::Unresolved,
        ledger_observed_at_epoch_ms: Some(NOW_MS - (LEDGER_OBSERVATION_TTL_SECS + 1) * 1000),
        ..Case::default()
    };
    let verdict = project(&case, NOW_MS);
    assert_eq!(verdict.label(), "expired");
    assert!(verdict.abstains_from_health_polarity());
    assert_eq!(verdict.coverage.observation_state, "expired");
    assert_eq!(counts(&verdict), (None, None, None));
    assert_eq!(verdict.coverage.cursor_offset, Some(4000));
}

#[test]
fn provenance_distinguishes_exact_prefix_and_combined_coverage_without_a_witness() {
    for (ranges, frontier_end, expected) in [
        (vec![(0, 50), (50, 100)], None, (1, 0, 0)),
        (vec![], Some(100), (0, 1, 0)),
        (vec![(50, 100)], Some(50), (0, 0, 1)),
        (vec![(0, 100)], Some(100), (1, 0, 0)),
        (vec![(51, 100)], Some(50), (0, 0, 0)),
    ] {
        let (receipts, _dir) = read_index(&DeliveryRecord {
            confirmed_deliveries: ranges
                .into_iter()
                .map(|range| receipt(range, GENERATION))
                .collect(),
            delivered_frontier: frontier_end.map(|end| DeliveredCommit {
                range: (0, end),
                generation_mtime_ns: GENERATION,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            }),
            ..DeliveryRecord::default()
        });
        for incarnation in [proven_incarnation(), unproven_incarnation()] {
            let proven = incarnation.spawn_nonce.is_some();
            let case = Case {
                ledger: Some(ledger_with(vec![obligation(0, 100, 119)], incarnation)),
                receipts: receipts.clone(),
                ..Case::default()
            };
            let verdict = project(&case, NOW_MS);
            let p = verdict.coverage.provenance.as_ref().unwrap();
            assert_eq!(
                (
                    p.exact_receipt_ranges,
                    p.frontier_prefix_ranges,
                    p.mixed_ranges
                ),
                expected
            );
            let covered = expected != (0, 0, 0);
            assert_eq!(
                counts(&verdict),
                (
                    Some(u32::from(!covered)),
                    Some(u32::from(covered && !proven)),
                    Some(u32::from(!covered || !proven))
                )
            );
            println!(
                "provenance proven={proven}: {}",
                serde_json::to_value(RelayVerdictReport::of(&verdict, true)).unwrap()
            );
        }
    }
}

#[test]
fn structural_and_composite_switches_keep_coverage_when_external_wins() {
    let case = Case {
        ledger: Some(ledger_with(
            vec![obligation(0, 100, 119)],
            proven_incarnation(),
        )),
        ..Case::default()
    };
    let in_band = project(&case, NOW_MS);
    let mut verdict = compose_relay_verdict(
        in_band.in_band.clone(),
        ExternalRelayVerdict::Unreachable { lost_blocks: 2 },
    );
    verdict.coverage = in_band.coverage;
    for governs in [false, true] {
        let wire = serde_json::to_value(RelayVerdictReport::of(&verdict, governs)).unwrap();
        assert_eq!(wire["verdict"], "unreachable");
        assert_eq!(wire["decided_by"], "external");
        assert!(wire.get("uncovered_ranges").is_none());
        assert_eq!(wire["governs_health_polarity"], governs);
        assert_eq!(wire["coverage"]["pending_ranges"], 1);
        let mut status = HealthStatus::Healthy;
        apply_relay_verdict_polarity(
            governs,
            &verdict,
            "claude",
            6300,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut status,
        );
        assert_eq!(
            status,
            if governs {
                HealthStatus::Degraded
            } else {
                HealthStatus::Healthy
            }
        );
        println!("switch wire: {wire}");
    }
}

#[test]
fn ledger_snapshot_keeps_commit_time_with_bytes_across_atomic_replacement() {
    use super::super::super::ledger::{read_ledger_snapshot_at, read_ledger_snapshot_file};
    let dir = tempdir().unwrap();
    let path = dir.path().join("ledger.json");
    assert_eq!(read_ledger_snapshot_at(&path), (None, false, None));
    let old = ledger_with(vec![obligation(0, 100, 119)], proven_incarnation());
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    let file = std::fs::File::open(&path).unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100)),
    )
    .unwrap();
    let commit = ledger_committed_at_epoch_ms(&path).unwrap();
    let next = dir.path().join("next.json");
    std::fs::write(&next, b"corrupt replacement").unwrap();
    std::fs::rename(next, &path).unwrap();
    assert_eq!(
        read_ledger_snapshot_file(file),
        (Some(old), true, Some(commit))
    );
    let (ledger, present, _) = read_ledger_snapshot_at(&path);
    assert!(ledger.is_none() && present);
}

#[test]
fn canonical_a_b_obligations_remain_pending_inside_grace() {
    use super::super::super::obligation::scan_canonical;
    let (raw, base) = match std::env::var("REACHABILITY_INCIDENT_FIXTURE") {
        Ok(path) => (std::fs::read(path).unwrap(), 106224790),
        Err(_) => (concat!(
            "{\"type\":\"assistant\",\"timestamp\":\"2020-01-01T00:00:00Z\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"A tail\"}]}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2020-01-01T00:00:00Z\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"B body\"}]}}\n"
        ).as_bytes().to_vec(), 0),
    };
    let scan = scan_canonical(
        &raw,
        base,
        GENERATION,
        TranscriptFileId { dev: 7, ino: 11 },
        1_000_000,
    );
    let records: Vec<_> = scan
        .records
        .into_iter()
        .filter(|r| {
            r.reason.is_obligation()
                && (base == 0
                    || [(106286862, 106289479), (106294125, 106295863)].contains(&(r.start, r.end)))
        })
        .collect();
    assert_eq!(records.len(), 2);
    let mut ledger = ledger_with(Vec::new(), proven_incarnation());
    ledger.append_obligations(records, NOW_MS - 119_000);
    ledger.cursor_offset = scan.next_offset;
    ledger.last_observed_len = base + raw.len() as u64;
    let case = Case {
        ledger: Some(ledger),
        transcript: TranscriptLiveness::Resolved {
            eof: base + raw.len() as u64,
            alive: true,
        },
        ..Case::default()
    };
    let verdict = project(&case, NOW_MS);
    assert_eq!(verdict.label(), "reachable");
    assert_eq!(counts(&verdict), (Some(2), Some(0), Some(2)));
    assert_eq!(verdict.coverage.oldest_pending_age_secs, Some(119));
    println!(
        "A/B fixture base={base}: {}",
        serde_json::to_value(RelayVerdictReport::of(&verdict, true)).unwrap()
    );
}
