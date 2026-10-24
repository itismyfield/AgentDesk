//! Classification 2: every session forwarder builds its request through `trusted_request`,
//! judged inside each real forwarder item. Bans count in code and literals, not comments.

use crate::test_support::{
    CommentForgery, braced_item_span, comment_out_code, rust_code_mask, strip_rust_comments,
};
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

#[test]
fn trusted_request_guard_rejects_commented_calls_and_counts_coded_bypasses() {
    let mask = rust_code_mask(SESSION_FORWARDING).expect("forwarding source lexes");
    let edit = |name: &str, change: &dyn Fn(&str) -> String| {
        let span = forwarder_span(&mask, name).expect("forwarder item");
        let body = change(&SESSION_FORWARDING[span.clone()]);
        [
            &SESSION_FORWARDING[..span.start],
            &body,
            &SESSION_FORWARDING[span.end..],
        ]
        .concat()
    };
    for name in FORWARDERS {
        for forgery in CommentForgery::ALL {
            let forged = comment_out_code(SESSION_FORWARDING, &format!("fn {name}("), forgery);
            assert_eq!(
                trusted_request_gaps(&forged),
                [format!("{name} is missing")]
            );
            let forged = edit(name, &|body| comment_out_code(body, TRUSTED, forgery));
            assert!(forged.contains(TRUSTED));
            let gaps = trusted_request_gaps(&forged);
            assert_eq!(gaps, [format!("{name} bypassed shared trusted_request")]);
        }
        let open = |line: &'static str| {
            move |body: &str| body.replacen("{\n", &format!("{{\n{line}\n"), 1)
        };
        let noted = edit(
            name,
            &open("    // reqwest::Client::builder() /* format!(\"http://x\") */"),
        );
        assert_eq!(trusted_request_gaps(&noted), Vec::<String>::new(), "{name}");
        let coded = edit(
            name,
            &open("    let _ = (reqwest::Client::builder(), format!(\"http://x\"));"),
        );
        assert_eq!(trusted_request_gaps(&coded).len(), 2, "{name}");
    }
}
