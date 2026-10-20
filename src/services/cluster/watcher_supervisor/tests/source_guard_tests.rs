//! Classification 2: shutdown observation sites, judged inside the real supervisor loop and
//! helper items with comments blanked. Lexical tripwire only: boot teardown is empty, and
//! lagged is not exercised by R1.

use crate::test_support::{
    CommentForgery, braced_item_span, comment_out_code, rust_code_mask, strip_rust_comments,
};

pub(super) const SUPERVISOR: &str = include_str!("../../watcher_supervisor.rs");

const LOOP: &str = "pub async fn run_watcher_supervisor_loop_with_registry_and_producers(";
const ANCHORS: [&str; 4] = [
    "for handle in initial_teardowns {",
    "if let Some(handle) = to_shutdown {",
    "for handle in teardowns {",
    "for (session, handle) in active.drain() {",
];
const SITES: [&str; 4] = [
    "boot_reconcile",
    "registry_change",
    "lagged_reconcile",
    "final_drain",
];
const DEREGISTER: &str = "producers.deregister(&session);";
const HELPER: &str = "async fn observe_relay_shutdown(";
const LOG: &str = "\"watcher-supervisor relay shutdown observed; not a delivery acknowledgement\"";

fn shutdown_site_pattern(site: &str) -> regex::Regex {
    regex::Regex::new(&format!(r#"let\s+_\s*=\s*observe_relay_shutdown\s*\(\s*handle\s*\.\s*shutdown_with_result\s*\(\s*\)\s*,\s*"{}"\s*,?\s*\)\s*\.\s*await\s*;"#, regex::escape(site))).unwrap()
}

/// Regex hits in comment-free `text` that start in code, so a quoted copy never counts.
fn coded_matches(pattern: &regex::Regex, text: &str, mask: &str) -> Vec<usize> {
    let starts = pattern.find_iter(text).map(|found| found.start());
    starts.filter(|at| mask[*at..].starts_with("let")).collect()
}

/// Each branch's interval holds exactly one awaited observation of its own site, and the
/// final drain deregisters the producer before observing the shutdown.
pub(super) fn shutdown_site_gaps(source: &str) -> Vec<String> {
    let text = strip_rust_comments(source).expect("supervisor source lexes");
    let mask = rust_code_mask(source).expect("supervisor source lexes");
    let Some(span) = braced_item_span(&mask, LOOP) else {
        return vec![format!("missing {LOOP}")];
    };
    let (text, mask) = (&text[span.clone()], &mask[span]);
    let mut gaps = Vec::new();
    let mut positions = Vec::new();
    for anchor in ANCHORS {
        match mask
            .match_indices(anchor)
            .map(|(at, _)| at)
            .collect::<Vec<_>>()[..]
        {
            [at] => positions.push(at),
            ref found => gaps.push(format!("`{anchor}` found {} times", found.len())),
        }
    }
    if !gaps.is_empty() || !positions.windows(2).all(|pair| pair[0] < pair[1]) {
        gaps.push(format!("branch anchors out of order: {positions:?}"));
        return gaps;
    }
    for (i, site) in SITES.iter().enumerate() {
        let interval = positions[i]..positions.get(i + 1).copied().unwrap_or(mask.len());
        let hits = coded_matches(
            &shutdown_site_pattern(site),
            &text[interval.clone()],
            &mask[interval.clone()],
        );
        let observed = mask[interval.clone()]
            .matches("observe_relay_shutdown")
            .count();
        if hits.len() != 1 || observed != 1 {
            gaps.push(format!(
                "{site}: {} coded sites, {observed} observations",
                hits.len()
            ));
        } else if i == 3
            && mask[interval]
                .find(DEREGISTER)
                .is_none_or(|at| at > hits[0])
        {
            gaps.push(format!("{site} must deregister before observing"));
        }
    }
    gaps
}

/// The helper logs once, with the site and outcome fields and the fixed message literal.
pub(super) fn shutdown_log_gaps(source: &str) -> Vec<String> {
    let text = strip_rust_comments(source).expect("supervisor source lexes");
    let mask = rust_code_mask(source).expect("supervisor source lexes");
    let Some(span) = braced_item_span(&mask, HELPER) else {
        return vec![format!("missing {HELPER}")];
    };
    let pattern = regex::Regex::new(&format!(
        r#"tracing::info!\s*\(\s*site\s*,\s*outcome\s*,\s*{}\s*,?\s*\)"#,
        regex::escape(LOG)
    ))
    .unwrap();
    let logs = mask[span.clone()].matches("tracing::info!").count();
    let hits = pattern.find_iter(&text[span.clone()]);
    let coded = hits
        .filter(|found| mask[span.start + found.start()..].starts_with("tracing"))
        .count();
    if (logs, coded) == (1, 1) {
        Vec::new()
    } else {
        vec![format!(
            "helper logs {logs} times, {coded} with the classification fields"
        )]
    }
}

#[test]
fn shutdown_site_pattern_matches_only_its_awaited_site() {
    for call in [
        r#"let _ = observe_relay_shutdown(handle.shutdown_with_result(), "registry_change").await;"#,
        "let _ =\n observe_relay_shutdown(\n handle.shutdown_with_result(),\n \"registry_change\",\n )\n .await;",
    ] {
        assert!(shutdown_site_pattern("registry_change").is_match(call));
        assert!(!shutdown_site_pattern("final_drain").is_match(call));
    }
    assert!(!shutdown_site_pattern("registry_change").is_match(r#"let _ = tokio::time::timeout(Duration::from_secs(1), observe_relay_shutdown(handle.shutdown_with_result(), "registry_change")).await;"#));
}

#[test]
fn shutdown_guards_reject_commented_sites_and_reordered_drain() {
    let quoted_sites = SITES.map(|site| format!("\"{site}\""));
    let needles = [
        LOOP,
        HELPER,
        DEREGISTER,
        LOG,
        "tracing::info!(\n        site,",
    ]
    .into_iter()
    .chain(ANCHORS)
    .chain(quoted_sites.iter().map(String::as_str));
    for needle in needles {
        for forgery in CommentForgery::ALL {
            let forged = comment_out_code(SUPERVISOR, needle, forgery);
            let gaps = [shutdown_site_gaps(&forged), shutdown_log_gaps(&forged)].concat();
            assert!(
                !gaps.is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let observe =
        "let _ = observe_relay_shutdown(handle.shutdown_with_result(), \"final_drain\").await;";
    let late = SUPERVISOR.replacen(
        &format!("{DEREGISTER}\n        {observe}"),
        &format!("{observe}\n        {DEREGISTER}"),
        1,
    );
    assert_ne!(late, SUPERVISOR);
    assert!(
        !shutdown_site_gaps(&late).is_empty(),
        "deregister after observe passes"
    );
    let unlabeled = SUPERVISOR.replacen("/// Full-control variant.", "///", 1);
    assert_eq!(shutdown_log_gaps(&unlabeled), Vec::<String>::new());
}
