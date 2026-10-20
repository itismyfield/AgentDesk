//! Classification 2: generation publication guards read real runtime-store items, comments
//! and formatting ignored. A lexical tripwire; aliases, macros or moved readers evade it.

use crate::services::discord::router::source_guard_support::{Lexed, forge};
use crate::test_support::CommentForgery;

pub(super) const RUNTIME_STORE: &str = include_str!("../../runtime_store.rs");

const ONCE_CELL: &str = "static PROCESS_GENERATION: std::sync::OnceLock<";
const LOCK_FILE: &str = "struct GenerationFileLock";
const NOT_TEST: &str = "#[cfg(not(test))]";
const TEST: &str = "#[cfg(test)]";
const BINDING: &str = "pub(in crate::services::discord) fn process_generation_binding() -> ProcessGenerationAllocation {";
const BINDING_READ: &str = "#[cfg(not(test))]\n    if let Some(bound) = PROCESS_GENERATION.get() {\n        return *bound;\n    }";
/// Items whose `cfg(not(test))` branch must be exactly the given block.
const BRANCHES: [(&str, &str); 2] = [
    (
        "pub fn process_generation() -> u64 {",
        "{\n        process_generation_binding().generation\n    }",
    ),
    (
        "pub(in crate::services::discord) fn allocate_process_generation_binding()",
        "{\n        allocate_once_with(\n            &PROCESS_GENERATION,\n            &PROCESS_GENERATION_ALLOCATION_TRANSACTION,\n            allocate_generation_epoch,\n        )\n    }",
    ),
];
const COMPOSER: &str = "fn allocate_generation_epoch() -> ProcessGenerationAllocation {";
const COMPOSER_BODY: &str = "allocate_generation_epoch_with_io(GenerationIo {\n        path: generation_path(),\n        lock: lock_generation_path,\n        read: read_generation_counter,\n        write: atomic_write,\n        fsync: fsync_parent_dir,\n        flushes: PARENT_DIR_FSYNC_FLUSHES,\n    })\n}";
const SURFACE_FIRST: &str = "enum GenerationAllocationRoute {";
const SURFACE_LAST: &str = "use test_generation_publication::{";
const SURVIVAL_WORDS: [&str; 5] = ["durable", "persistent", "persisted", "survive", "crash"];

fn same_code(found: Option<Lexed>, expected: &str) -> bool {
    found.is_some_and(|found| found.compact().text() == Lexed::new(expected).compact().text())
}

/// The production read cell is a process `OnceLock` declared before the lock-file item.
pub(super) fn once_cell_gaps(source: &str) -> Vec<String> {
    let lexed = Lexed::new(source);
    match (lexed.find(ONCE_CELL), lexed.find(LOCK_FILE)) {
        (Some(cell), Some(end)) if cell < end => Vec::new(),
        found => vec![format!("{ONCE_CELL} must precede {LOCK_FILE}: {found:?}")],
    }
}

/// The binding returns the stored allocation whole, the accessor and allocator read only
/// that binding, and the production composer is exactly the canonical allocation tail.
pub(super) fn binding_gaps(source: &str) -> Vec<String> {
    let lexed = Lexed::new(source);
    let mut gaps = Vec::new();
    let read = lexed.block(BINDING).and_then(|item| {
        let end = item.find(TEST)?;
        Some(item.slice(BINDING.len()..end))
    });
    if !same_code(read, BINDING_READ) {
        gaps.push("the production binding reader must return the stored allocation".into());
    }
    for (head, branch) in BRANCHES {
        let found = lexed.block(head).and_then(|item| {
            let start = item.find(NOT_TEST)? + NOT_TEST.len();
            let end = item.find_after(start, TEST)?;
            Some(item.slice(start..end))
        });
        if !same_code(found, branch) {
            gaps.push(format!(
                "{head} production branch is not exactly {branch:?}"
            ));
        }
    }
    if !same_code(lexed.block(COMPOSER), &format!("{COMPOSER}{COMPOSER_BODY}")) {
        gaps.push("the production composer is not the canonical allocation tail".into());
    }
    gaps
}

/// From the docs above the route enum through the test publication re-export: code,
/// literals and doc comments (the provenance prose this bans), ordinary comments excluded.
pub(super) fn provenance_claim_gaps(source: &str) -> Vec<String> {
    let lexed = Lexed::new(source);
    let range = lexed.find(SURFACE_FIRST).and_then(|first| {
        let last = lexed.find_after(first, SURFACE_LAST)?;
        Some((first, lexed.find_after(last, "};")? + 2))
    });
    let Some((first, end)) = range else {
        return vec![format!("missing surface {SURFACE_FIRST}..{SURFACE_LAST}")];
    };
    let mut start = source[..first].rfind('\n').map_or(0, |at| at + 1);
    while start > 0 {
        let above = source[..start - 1].rfind('\n').map_or(0, |at| at + 1);
        let line = source[above..start - 1].trim_start();
        if !(line.starts_with("///") || line.starts_with("#[")) {
            break;
        }
        start = above;
    }
    let is_doc = |line: &str| {
        let line = line.trim_start();
        (line.starts_with("///") && !line.starts_with("////"))
            || ["//!", "/**", "/*!", "*"]
                .iter()
                .any(|doc| line.starts_with(doc))
    };
    let surface = source[start..end]
        .lines()
        .zip(lexed.text()[start..end].lines())
        .map(|(raw, code)| {
            if code.trim().is_empty() && is_doc(raw) {
                raw
            } else {
                code
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    SURVIVAL_WORDS
        .into_iter()
        .filter(|word| surface.contains(word))
        .map(|word| format!("provenance surface claims survival: {word}"))
        .collect()
}

#[test]
fn generation_publication_guards_reject_commented_items() {
    type Judge = fn(&str) -> Vec<String>;
    let cases: [(Judge, Vec<&str>); 3] = [
        (once_cell_gaps, vec![ONCE_CELL, LOCK_FILE]),
        (
            binding_gaps,
            vec![
                BINDING,
                "return *bound;",
                BRANCHES[0].0,
                "process_generation_binding().generation",
                BRANCHES[1].0,
                "allocate_generation_epoch,",
                COMPOSER,
                "write: atomic_write,",
            ],
        ),
        (provenance_claim_gaps, vec![SURFACE_FIRST, SURFACE_LAST]),
    ];
    for (judge, needles) in cases {
        for needle in needles {
            for forgery in CommentForgery::ALL {
                let forged = forge(RUNTIME_STORE, needle, forgery);
                assert!(
                    !judge(&forged).is_empty(),
                    "{forgery:?} commented `{needle}` still passes"
                );
            }
        }
    }
    let read = "PROCESS_GENERATION.get() {\n        return *bound;";
    let noted = RUNTIME_STORE.replacen(
        read,
        "PROCESS_GENERATION.get() {\n        // whole\n        return *bound; /* no rebuild */",
        1,
    );
    assert_ne!(noted, RUNTIME_STORE);
    assert_eq!(binding_gaps(&noted), Vec::<String>::new());
    let rebuilt = RUNTIME_STORE.replacen(
        read,
        "PROCESS_GENERATION.get() {\n        return ProcessGenerationAllocation { ..*bound };",
        1,
    );
    assert_eq!(binding_gaps(&rebuilt).len(), 1);
}

#[test]
fn provenance_guard_bans_doc_and_code_claims_but_not_plain_comments() {
    let markers = RUNTIME_STORE
        .replacen("// <epoch-provenance-surface>", "", 1)
        .replacen("// </epoch-provenance-surface>", "", 1);
    assert_ne!(markers, RUNTIME_STORE);
    assert_eq!(provenance_claim_gaps(&markers), Vec::<String>::new());
    let variant = "    ParentSyncFailed,";
    let ways = [
        (format!("    // a crash may survive this\n{variant}"), 0),
        (format!("    /* outer /* durable */ */\n{variant}"), 0),
        (format!("    /// Survives a crash.\n{variant}"), 2),
        (format!("{variant}\n    PersistedDurable,"), 2),
    ];
    for (rewrite, gaps) in ways {
        let forged = RUNTIME_STORE.replacen(variant, &rewrite, 1);
        assert_ne!(forged, RUNTIME_STORE);
        assert_eq!(provenance_claim_gaps(&forged).len(), gaps, "{rewrite}");
    }
}
