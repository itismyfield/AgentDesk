//! Classification 2: provider spawn-marker calls counted in code, comments excluded. A
//! lexical tripwire; aliases, macros and reachability of each spawn are not proven.

use crate::services::discord::router::source_guard_support::{Lexed, forge};
use crate::test_support::CommentForgery;

const CLAUDE: &str = include_str!("../../claude.rs");
const CODEX: &str = include_str!("../../codex.rs");
const CODEX_ABSENCE: &str = include_str!("../../codex/absence_launch.rs");
const QWEN: &str = include_str!("../../qwen.rs");

/// Each provider's sources with its expected prepared-spawn count; Codex spans two files.
const PROVIDERS: [(&str, &[&str], usize); 3] = [
    ("claude", &[CLAUDE], 1),
    ("codex", &[CODEX, CODEX_ABSENCE], 1),
    ("qwen", &[QWEN], 0),
];

const COMBINED: &str = "crate::services::discord::stamp_spawn_markers(tmux_session_name, None)";
const PREPARED: &str = "stamp_spawn_markers(tmux_session_name,Some(";
const NONCE_ONLY: &str = "crate::services::discord::write_spawn_nonce(tmux_session_name)";
const INLINE_GENERATION: &str = "session_temp_path(tmux_session_name, \"generation\")";
const PREPARED_WRITTEN: &str =
    "stamp_spawn_markers(\n        tmux_session_name,\n        Some(&prepared),\n    )";
const PREPARED_ONE_LINE: &str = "stamp_spawn_markers(tmux_session_name, Some(&prepared))";

/// One combined marker call and `prepared` prepared calls in code across the provider's
/// sources; their comment-free text (literals kept) never writes a nonce or generation inline.
fn spawn_marker_gaps(provider: &str, sources: &[&str], prepared: usize) -> Vec<String> {
    let lexed: Vec<Lexed> = sources.iter().map(|source| Lexed::new(source)).collect();
    let combined: usize = lexed.iter().map(|lexed| lexed.count(COMBINED)).sum();
    let prepared_calls: usize = lexed
        .iter()
        .map(|lexed| lexed.compact().count(PREPARED))
        .sum();
    let mut gaps = Vec::new();
    if combined != 1 {
        gaps.push(format!(
            "{provider}: {combined} combined marker calls, expected 1"
        ));
    }
    if prepared_calls != prepared {
        gaps.push(format!(
            "{provider}: {prepared_calls} prepared marker calls, expected {prepared}"
        ));
    }
    for banned in [NONCE_ONLY, INLINE_GENERATION] {
        if lexed.iter().any(|lexed| lexed.text().contains(banned)) {
            gaps.push(format!("{provider}: writes `{banned}`"));
        }
    }
    gaps
}

pub(super) fn assert_combined_marker_gate() {
    for (provider, sources, prepared) in PROVIDERS {
        assert_eq!(
            spawn_marker_gaps(provider, sources, prepared),
            Vec::<String>::new()
        );
    }
}

/// The provider's sources with the one source holding `needle` rewritten by `edit`.
fn edited(sources: &[&str], needle: &str, edit: impl Fn(&str) -> String) -> Vec<String> {
    let holder = sources
        .iter()
        .position(|source| Lexed::new(source).contains(needle))
        .expect("a source holds the needle in code");
    let mut out: Vec<String> = sources.iter().map(|source| source.to_string()).collect();
    out[holder] = edit(sources[holder]);
    out
}

fn gaps_of(provider: &str, sources: &[String], prepared: usize) -> Vec<String> {
    let sources: Vec<&str> = sources.iter().map(String::as_str).collect();
    spawn_marker_gaps(provider, &sources, prepared)
}

#[test]
fn spawn_marker_guard_rejects_commented_combined_and_prepared_calls() {
    for (provider, sources, prepared) in PROVIDERS {
        for forgery in CommentForgery::ALL {
            let forged = edited(sources, COMBINED, |source| forge(source, COMBINED, forgery));
            assert_eq!(
                gaps_of(provider, &forged, prepared).len(),
                1,
                "{provider} {forgery:?}"
            );
            if prepared == 0 {
                continue;
            }
            let one_line = edited(sources, PREPARED_WRITTEN, |source| {
                source.replacen(PREPARED_WRITTEN, PREPARED_ONE_LINE, 1)
            });
            assert_eq!(gaps_of(provider, &one_line, prepared), Vec::<String>::new());
            let forged: Vec<String> = one_line
                .iter()
                .map(
                    |source| match Lexed::new(source).contains(PREPARED_ONE_LINE) {
                        true => forge(source, PREPARED_ONE_LINE, forgery),
                        false => source.clone(),
                    },
                )
                .collect();
            assert_eq!(
                gaps_of(provider, &forged, prepared).len(),
                1,
                "{provider} {forgery:?}"
            );
        }
    }
}

#[test]
fn spawn_marker_guard_counts_coded_calls_and_writes_but_not_comments() {
    for (provider, sources, prepared) in PROVIDERS {
        let appended = |line: &str| {
            let mut out: Vec<String> = sources.iter().map(|source| source.to_string()).collect();
            out[0].push_str(&format!(
                "\nfn injected(tmux_session_name: &str) {{\n    {line}\n}}\n"
            ));
            out
        };
        for call in [
            format!("let _ = {COMBINED};"),
            format!("let _ = {NONCE_ONLY};"),
            format!("let _ = crate::services::tmux_common::{INLINE_GENERATION};"),
        ] {
            let commented = appended(&format!("// {call}"));
            assert_eq!(
                gaps_of(provider, &commented, prepared),
                Vec::<String>::new()
            );
            let coded = appended(&call);
            assert_eq!(
                gaps_of(provider, &coded, prepared).len(),
                1,
                "{provider}: {call}"
            );
        }
    }
}
