//! Loader-gate source guard, read from the removal files it pins.

pub(super) fn observation_labels_and_allocation_only_evaluation_api_are_exact() {
    let source = include_str!("../../removal.rs");
    let production = source
        .split_once("/// Thread-local test seam for `tmux_pane_alive_for_stale_check`.")
        .unwrap()
        .0;
    let observation = source
        .split_once("fn loader_gate_refuses_with_allocation(")
        .expect("loader allocation observation must remain present")
        .1
        .split_once("/// Thread-local test seam for `tmux_pane_alive_for_stale_check`.")
        .expect("loader allocation observation must remain bounded")
        .0;
    assert_eq!(observation.matches("\"epoch_route\"").count(), 2);
    assert_eq!(observation.matches("\"epoch_advanced\"").count(), 2);
    assert_eq!(observation.matches("epoch_route,").count(), 2);
    assert_eq!(observation.matches("epoch_advanced,").count(), 2);
    assert_eq!(
        observation.matches("ProcessGenerationAllocation").count(),
        3
    );
    let wrapper = production
        .split_once("fn loader_gate_refuses_with_allocation(")
        .unwrap()
        .1
        .split_once("/// §7.2-1's refusal payload")
        .unwrap()
        .0;
    assert_eq!(
        wrapper.trim(),
        "state: &InflightTurnState,\n    allocation: crate::services::discord::runtime_store::ProcessGenerationAllocation,\n) -> bool {\n    loader_gate_refuses(state, allocation.generation)\n}"
    );

    // Legacy reads decide without retirement effects; scoped restore has
    // separate checked mutation bodies outside that read-only contract.
    let owned = source
        .split_once("\nmod loader_gate_observation_tests")
        .unwrap()
        .0;
    let scoped = include_str!("../scoped_restore.rs");
    // Exclude only the two named items; extra items before the next item or EOF are refused.
    let item = |text: &str, name: &str, sentinel: &str| {
        assert_eq!(text.matches(name).count(), 1, "unique item {name}");
        let name_at = text.find(name).expect(name);
        let start = text[..name_at].rfind('\n').map_or(0, |index| index + 1);
        let end = name_at + text[name_at..].find("\n}").expect("item closing brace") + 2;
        let next = if sentinel.is_empty() {
            text.len()
        } else {
            end + text[end..].find(sentinel).expect(sentinel)
        };
        assert!(text[end..next].trim().is_empty(), "extra item after {name}");
        start..end
    };
    let ranges = [
        item(
            scoped,
            "async fn invalidate_stale_generation_scoped(",
            "pub(super) fn backfill_scoped_restore(",
        ),
        item(scoped, "fn backfill_scoped_restore(", ""),
    ];
    assert!(ranges[0].end < ranges[1].start);
    let scoped_reads = format!(
        "{}{}{}",
        &scoped[..ranges[0].start],
        &scoped[ranges[0].end..ranges[1].start],
        &scoped[ranges[1].end..]
    );
    for (connection, sentinel) in [
        (
            "use scoped_restore::invalidate_stale_generation_scoped;",
            "pub(super) fn load_inflight_states_from_root(",
        ),
        (
            "use scoped_restore::backfill_scoped_restore;",
            "fn load_inflight_probe_with_scope(",
        ),
    ] {
        let end = owned.find(connection).expect(connection) + connection.len();
        let next = end + owned[end..].find(sentinel).expect(sentinel);
        assert!(
            owned[end..next].trim().is_empty(),
            "extra item after {connection}"
        );
    }
    let reads = owned.split_once("fn classify_inflight_row(").unwrap().1;
    let reads = reads.split_once("\nmod boot_reaper;").unwrap().0;
    let effects = [
        "fs::remove_file",
        "fs::rename",
        "fs::write",
        "atomic_write",
        "persist_under_lock",
    ];
    let events = [
        "record_loader_generation_gate",
        "emit_loader_generation_gate_allowed",
    ];
    for effect in effects.iter().chain(&events) {
        assert!(!reads.contains(effect), "{effect}");
        assert!(!scoped_reads.contains(effect), "scoped {effect}");
    }
    assert_eq!(
        reads
            .matches("runtime_store::process_generation_binding();")
            .count(),
        1
    );
    assert!(
        !reads.contains("allocation.epoch_route()")
            && !reads.contains("allocation.epoch_advanced()")
    );
    let reaper = include_str!("../boot_reaper.rs");
    let reaper = reaper
        .split_once("fn reap_inflight_rows_at_boot_in_root(")
        .unwrap()
        .1;
    let arms: Vec<_> = reaper.split("RowVerdict::").collect();
    for (arm, check) in [
        ("HideStale(", "same_snapshot(&unlocked, &locked)"),
        ("HideForeign(", "same_snapshot(&unlocked, &locked)"),
        ("HideMalformed(", "unlocked == locked"),
    ] {
        assert!(
            arms.iter()
                .any(|arm_text| arm_text.starts_with(arm) && arm_text.contains(check)),
            "{arm}"
        );
    }
    let marks = [
        "if !unchanged {",
        "emit_loader_generation_gate_allowed(",
        "fs::remove_file(&path)",
    ];
    assert!(marks.map(|mark| reaper.find(mark).expect(mark)).is_sorted());
    assert_eq!(
        owned.matches("fs::remove_file(").count() + reaper.matches("fs::remove_file(").count(),
        2
    );

    let public_witness = source
        .split_once("fn public_loader_observes_production_shaped_published_routes()")
        .expect("public loader witness must remain present")
        .1
        .split_once("/// Lexical tripwire only")
        .expect("public loader witness must remain bounded")
        .0;
    assert_eq!(
        public_witness
            .matches("super::super::stall_recovery_tests::stale_override_test_mutex()")
            .count(),
        1,
        "the process-global tmux override witness must share its canonical test mutex",
    );
}
