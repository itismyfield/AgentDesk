//! Classification 2: every session forwarder builds its request through `trusted_request`,
//! judged inside each real forwarder item. Bans count in code and literals, not comments.

use crate::test_support::{braced_item_span, rust_code_mask, strip_rust_comments};
use std::ops::Range;

pub(super) const SESSION_FORWARDING: &str = include_str!("../../session_forwarding.rs");

const FORWARDERS: [&str; 6] = [
    "forward_tmux_output",
    "forward_force_kill",
    "forward_kill_tmux",
    "forward_resume_candidates",
    "forward_resume_previous",
    "forward_cancel_turn",
];
const TRUSTED: &str = "trusted_request(";
const BANNED: [&str; 2] = ["reqwest::Client::builder", "format!(\"http"];

fn forwarder_span(mask: &str, name: &str) -> Option<Range<usize>> {
    braced_item_span(mask, &format!("fn {name}("))
}

/// Forwarders that are missing, skip `trusted_request(`, or build a client or URL directly.
pub(super) fn trusted_request_gaps(source: &str) -> Vec<String> {
    let text = strip_rust_comments(source).expect("forwarding source lexes");
    let mask = rust_code_mask(source).expect("forwarding source lexes");
    let mut gaps = Vec::new();
    for name in FORWARDERS {
        let Some(span) = forwarder_span(&mask, name) else {
            gaps.push(format!("{name} is missing"));
            continue;
        };
        if !mask[span.clone()].contains(TRUSTED) {
            gaps.push(format!("{name} bypassed shared trusted_request"));
        }
        for banned in BANNED
            .into_iter()
            .filter(|banned| text[span.clone()].contains(banned))
        {
            gaps.push(format!("{name} builds its own request with {banned}"));
        }
    }
    gaps
}
