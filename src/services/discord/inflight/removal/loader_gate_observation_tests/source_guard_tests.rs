//! Classification 2: loader-gate checks read code item ranges, comments excluded, needles
//! as written. A lexical tripwire; the parent module's route tests are the semantic proof.

use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

pub(super) const REMOVAL: &str = include_str!("../../removal.rs");
pub(super) const SCOPED: &str = include_str!("../scoped_restore.rs");
pub(super) const REAPER: &str = include_str!("../boot_reaper.rs");

const WRAPPER: &str = "fn loader_gate_refuses_with_allocation(";
const WRAPPER_BODY: &str = "state: &InflightTurnState,\n    allocation: crate::services::discord::runtime_store::ProcessGenerationAllocation,\n) -> bool {\n    loader_gate_refuses(state, allocation.generation)\n}";
/// The test seam that follows the observation items; the old bound was its doc comment.
const SEAM: &str = "static TEST_TMUX_ALIVE_OVERRIDE";
const OBSERVED: [(&str, usize); 5] = [
    ("\"epoch_route\":", 2),
    ("\"epoch_advanced\":", 2),
    ("epoch_route,", 2),
    ("epoch_advanced,", 2),
    ("ProcessGenerationAllocation", 3),
];
const TEST_MODULE: &str = "mod loader_gate_observation_tests";
const SCOPED_MUTATIONS: [&str; 2] = [
    "async fn invalidate_stale_generation_scoped(",
    "fn backfill_scoped_restore(",
];
const CONNECTIONS: [(&str, &str); 2] = [
    (
        "use scoped_restore::invalidate_stale_generation_scoped;",
        "pub(super) fn load_inflight_states_from_root(",
    ),
    (
        "use scoped_restore::backfill_scoped_restore;",
        "fn load_inflight_probe_with_scope(",
    ),
];
const READS: [&str; 2] = ["fn classify_inflight_row(", "mod boot_reaper;"];
const EFFECTS: [&str; 7] = [
    "fs::remove_file",
    "fs::rename",
    "fs::write",
    "atomic_write",
    "persist_under_lock",
    "record_loader_generation_gate",
    "emit_loader_generation_gate_allowed",
];
const BINDING: &str = "runtime_store::process_generation_binding();";
const REAPER_FN: &str = "fn reap_inflight_rows_at_boot_in_root(";
const ARMS: [(&str, &str); 3] = [
    (
        "RowVerdict::HideStale(",
        "same_snapshot(&unlocked, &locked)",
    ),
    (
        "RowVerdict::HideForeign(",
        "same_snapshot(&unlocked, &locked)",
    ),
    ("RowVerdict::HideMalformed(", "unlocked == locked"),
];
/// Same-snapshot check, then the allow observation, then the unlink.
const MARKS: [&str; 3] = [
    "if !unchanged {",
    "emit_loader_generation_gate_allowed(",
    "fs::remove_file(&path)",
];
const UNLINK: &str = "fs::remove_file(";
const WITNESS: &str = "fn public_loader_observes_production_shaped_published_routes()";
const WITNESS_MUTEX: &str = "super::super::stall_recovery_tests::stale_override_test_mutex()";

pub(super) fn loader_gate_gaps(removal: &str, scoped: &str, reaper: &str) -> Vec<String> {
    let removal = Lexed::new(removal);
    let mut gaps = Vec::new();
    match (removal.find(WRAPPER), removal.find(SEAM)) {
        (Some(start), Some(end)) if start < end => {
            let observation = removal.slice(start..end);
            for (needle, want) in OBSERVED {
                if observation.count(needle) != want {
                    gaps.push(format!("observation must hold `{needle}` {want} times"));
                }
            }
        }
        found => gaps.push(format!(
            "observation range {WRAPPER}..{SEAM} broken: {found:?}"
        )),
    }
    let wrapper = Lexed::new(&format!("{WRAPPER}{WRAPPER_BODY}")).compact();
    if removal
        .block(WRAPPER)
        .map(|item| item.compact().text().to_string())
        != Some(wrapper.text().to_string())
    {
        gaps.push(format!(
            "{WRAPPER} is not exactly the allocation-only wrapper"
        ));
    }
    let Some(owned) = removal.find(TEST_MODULE).map(|end| removal.slice(0..end)) else {
        gaps.push(format!("missing {TEST_MODULE}"));
        return gaps;
    };
    gaps.extend(scoped_read_gaps(scoped));
    for (connection, next) in CONNECTIONS {
        let end = owned.find(connection).map(|at| at + connection.len());
        match end.and_then(|end| Some((end, owned.find_after(end, next)?))) {
            Some((end, next)) if owned.text()[end..next].trim().is_empty() => {}
            _ => gaps.push(format!("{connection} must be followed directly by {next}")),
        }
    }
    let reads = owned.find(READS[0]).and_then(|start| {
        let end = owned.find_after(start, READS[1])?;
        Some(owned.slice(start + READS[0].len()..end))
    });
    match reads {
        Some(reads) => {
            for effect in EFFECTS {
                if reads.text().contains(effect) {
                    gaps.push(format!("read path has {effect}"));
                }
            }
            if reads.count(BINDING) != 1 {
                gaps.push(format!("read path must take {BINDING} once"));
            }
            for banned in ["allocation.epoch_route()", "allocation.epoch_advanced()"] {
                if reads.text().contains(banned) {
                    gaps.push(format!("read path observes {banned}"));
                }
            }
        }
        None => gaps.push(format!("missing read path {}..{}", READS[0], READS[1])),
    }
    gaps.extend(reaper_gaps(&owned, reaper));
    match removal.block(WITNESS) {
        Some(witness) if witness.count(WITNESS_MUTEX) == 1 => {}
        _ => gaps.push(format!("{WITNESS} must take {WITNESS_MUTEX} once")),
    }
    gaps
}

/// The scoped restore file minus its two named mutation items holds no effect, and nothing
/// but whitespace or comments follows each excluded item before the next one or EOF.
fn scoped_read_gaps(scoped: &str) -> Vec<String> {
    let scoped = Lexed::new(scoped);
    let mut spans = Vec::new();
    for name in SCOPED_MUTATIONS {
        match (scoped.count(name), scoped.find(name), scoped.block(name)) {
            (1, Some(at), Some(item)) => spans.push(at..at + item.text().len()),
            _ => return vec![format!("scoped restore needs one item {name}")],
        }
    }
    let text = scoped.text();
    let second_line = text[..spans[1].start].rfind('\n').map_or(0, |at| at + 1);
    if spans[0].end > second_line
        || !text[spans[0].end..second_line].trim().is_empty()
        || !text[spans[1].end..].trim().is_empty()
    {
        return vec!["an extra item sits beside the excluded scoped mutations".to_string()];
    }
    let reads = [
        &text[..spans[0].start],
        &text[spans[0].end..spans[1].start],
        &text[spans[1].end..],
    ]
    .concat();
    EFFECTS
        .into_iter()
        .filter(|effect| reads.contains(effect))
        .map(|effect| format!("scoped read path has {effect}"))
        .collect()
}

/// Each hide arm re-checks the locked snapshot, then the reaper observes and unlinks in
/// that order; the loader and the reaper unlink twice in total.
fn reaper_gaps(owned: &Lexed, reaper: &str) -> Vec<String> {
    let Some(reaper) = Lexed::new(reaper).block(REAPER_FN) else {
        return vec![format!("missing {REAPER_FN}")];
    };
    let mut gaps = Vec::new();
    let mut starts = reaper.find_all("RowVerdict::");
    starts.push(reaper.text().len());
    for (arm, check) in ARMS {
        let checked = starts.windows(2).any(|pair| {
            let body = reaper.slice(pair[0]..pair[1]);
            body.text().starts_with(arm) && body.contains(check)
        });
        if !checked {
            gaps.push(format!("{arm} must check {check}"));
        }
    }
    let marks = MARKS.map(|mark| reaper.find(mark));
    if marks.iter().any(Option::is_none) || !marks.is_sorted() {
        gaps.push(format!("reaper order {MARKS:?} broken: {marks:?}"));
    }
    if owned.count(UNLINK) + reaper.count(UNLINK) != 2 {
        gaps.push(format!("loader and reaper must hold two {UNLINK}"));
    }
    gaps
}

#[test]
fn loader_gate_guard_rejects_commented_items_and_reordered_reaping() {
    let removal_needles = OBSERVED.map(|(needle, _)| needle).into_iter().chain([
        WRAPPER,
        "loader_gate_refuses(state, allocation.generation)",
        SEAM,
        TEST_MODULE,
        CONNECTIONS[0].0,
        CONNECTIONS[1].1,
        READS[0],
        READS[1],
        BINDING,
        WITNESS,
        WITNESS_MUTEX,
    ]);
    let scoped_needles = SCOPED_MUTATIONS;
    let reaper_needles = [REAPER_FN, ARMS[0].0, ARMS[0].1, ARMS[2].1]
        .into_iter()
        .chain(MARKS);
    let inputs = [(REMOVAL, 0), (SCOPED, 1), (REAPER, 2)];
    let judge = |forged: &str, slot: usize| {
        let mut sources = [REMOVAL, SCOPED, REAPER];
        sources[slot] = forged;
        loader_gate_gaps(sources[0], sources[1], sources[2])
    };
    let needles: [Vec<&str>; 3] = [
        removal_needles.collect(),
        scoped_needles.to_vec(),
        reaper_needles.collect(),
    ];
    for (source, slot) in inputs {
        for needle in &needles[slot] {
            for forgery in CommentForgery::ALL {
                let forged = forge(source, needle, forgery);
                assert!(
                    !judge(&forged, slot).is_empty(),
                    "{forgery:?} commented `{needle}` still passes"
                );
            }
        }
    }
    let unlink_first = swap(
        REAPER,
        "if !unchanged {",
        "if fs::remove_file(&path).is_ok() {",
    );
    assert!(!judge(&unlink_first, 2).is_empty());
    let observed_late = swap(
        REAPER,
        "emit_loader_generation_gate_allowed(provider, locked, allocation, &path);",
        "if fs::remove_file(&path).is_ok() {",
    );
    assert!(!judge(&observed_late, 2).is_empty());
}

#[test]
fn loader_gate_guard_counts_coded_effects_but_not_commented_ones() {
    let anchor = "mod scoped_restore;";
    let prose = REMOVAL.replacen(
        anchor,
        &format!("{anchor}\n// fs::rename\n/* /* persist_under_lock */ */"),
        1,
    );
    assert_ne!(prose, REMOVAL);
    assert_eq!(
        loader_gate_gaps(&prose, SCOPED, REAPER),
        Vec::<String>::new()
    );
    let coded = REMOVAL.replacen(anchor, &format!("{anchor}\nuse std::fs::rename as _;"), 1);
    assert_eq!(
        loader_gate_gaps(&coded, SCOPED, REAPER),
        ["read path has fs::rename"]
    );
    let preamble = "use super::*;\n";
    let scoped_prose = SCOPED.replacen(preamble, &format!("{preamble}// fs::write\n"), 1);
    assert_eq!(
        loader_gate_gaps(REMOVAL, &scoped_prose, REAPER),
        Vec::<String>::new()
    );
    let scoped_coded = SCOPED.replacen(
        preamble,
        &format!("{preamble}use std::fs::write as _;\n"),
        1,
    );
    assert_eq!(
        loader_gate_gaps(REMOVAL, &scoped_coded, REAPER),
        ["scoped read path has fs::write"]
    );
    let extra = format!("{SCOPED}\nfn extra() {{}}\n");
    assert_eq!(loader_gate_gaps(REMOVAL, &extra, REAPER).len(), 1);
}
