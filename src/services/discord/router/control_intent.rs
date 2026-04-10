#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ControlIntentKind {
    ReviewBypassDirectMerge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ControlIntentSurface {
    ExplicitCommand,
    NaturalLanguageFallback,
}

impl ControlIntentSurface {
    fn as_str(self) -> &'static str {
        match self {
            ControlIntentSurface::ExplicitCommand => "explicit_command",
            ControlIntentSurface::NaturalLanguageFallback => "natural_language_fallback",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhraseGroupKind {
    Activation,
    Meta,
    Negation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PhraseGroup {
    id: &'static str,
    kind: PhraseGroupKind,
    phrases: &'static [&'static str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ControlIntentRule {
    id: &'static str,
    kind: ControlIntentKind,
    surface: ControlIntentSurface,
    phrase_groups: &'static [PhraseGroup],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PhraseMatch {
    group_id: &'static str,
    phrase: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ControlIntentTrace {
    pub rule_id: &'static str,
    pub surface: ControlIntentSurface,
    pub activation: &'static str,
    pub activation_group_id: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DetectedControlIntent {
    pub kind: ControlIntentKind,
    pub pr_number: u64,
    pub trace: ControlIntentTrace,
}

const REVIEW_BYPASS_DIRECT_MERGE_GROUPS: &[PhraseGroup] = &[
    PhraseGroup {
        id: "review_bypass_direct_merge.meta",
        kind: PhraseGroupKind::Meta,
        phrases: &[
            "인식",
            "안먹",
            "안 먹",
            "왜",
            "원인",
            "버그",
            "로그",
            "테스트",
            "수정",
            "디버그",
            "debug",
            "parser",
            "파서",
            "잡아줘",
        ],
    },
    PhraseGroup {
        id: "review_bypass_direct_merge.negation",
        kind: PhraseGroupKind::Negation,
        phrases: &[
            "하지 마",
            "하지마",
            "하면 안",
            "안 돼",
            "안돼",
            "안 됩니다",
            "안됩니다",
            "안됨",
            "못 하게",
            "못하게",
            "막아",
            "막아줘",
            "보류",
            "금지",
            "불가",
            "불가능",
        ],
    },
    PhraseGroup {
        id: "review_bypass_direct_merge.activation",
        kind: PhraseGroupKind::Activation,
        phrases: &[
            "리뷰 우회",
            "리뷰 무시",
            "리뷰 스킵",
            "직접 머지",
            "직접 merge",
            "머지 가능하게",
            "머지가능하게",
            "merge 가능하게",
            "merge가능하게",
            "기여자가 직접 머지",
            "contributor can merge",
            "author can merge",
            "direct merge",
        ],
    },
];

const CONTROL_INTENT_RULES: &[ControlIntentRule] = &[ControlIntentRule {
    id: "review_bypass_direct_merge",
    kind: ControlIntentKind::ReviewBypassDirectMerge,
    surface: ControlIntentSurface::NaturalLanguageFallback,
    phrase_groups: REVIEW_BYPASS_DIRECT_MERGE_GROUPS,
}];

fn normalize_control_intent_text(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn find_phrase_match(
    groups: &[PhraseGroup],
    kind: PhraseGroupKind,
    normalized: &str,
) -> Option<PhraseMatch> {
    let mut best_match: Option<PhraseMatch> = None;

    for group in groups.iter().filter(|group| group.kind == kind) {
        for phrase in group.phrases {
            if !normalized.contains(phrase) {
                continue;
            }
            let candidate = PhraseMatch {
                group_id: group.id,
                phrase,
            };
            if best_match.is_none_or(|current| candidate.phrase.len() > current.phrase.len()) {
                best_match = Some(candidate);
            }
        }
    }

    best_match
}

fn extract_merge_target_pr_number(user_text: &str) -> Option<u64> {
    let explicit_re =
        regex::Regex::new(r"(?i)(?:^|[^\w])(?:pr\s*#?\s*|#)(\d{1,6})(?:\b|$)").ok()?;
    if let Some(caps) = explicit_re.captures(user_text) {
        return caps.get(1)?.as_str().parse::<u64>().ok();
    }

    let leading_re = regex::Regex::new(r"^\s*(\d{1,6})(?:은|는|이|가|\s|$)").ok()?;
    if let Some(caps) = leading_re.captures(user_text) {
        return caps.get(1)?.as_str().parse::<u64>().ok();
    }

    None
}

fn detect_control_intent_for_surface(
    user_text: &str,
    surface: ControlIntentSurface,
) -> Option<DetectedControlIntent> {
    let normalized = normalize_control_intent_text(user_text);
    if normalized.is_empty() {
        return None;
    }

    CONTROL_INTENT_RULES
        .iter()
        .filter(|rule| rule.surface == surface)
        .find_map(|rule| {
            if find_phrase_match(rule.phrase_groups, PhraseGroupKind::Meta, &normalized).is_some() {
                return None;
            }
            if find_phrase_match(rule.phrase_groups, PhraseGroupKind::Negation, &normalized)
                .is_some()
            {
                return None;
            }
            let activation =
                find_phrase_match(rule.phrase_groups, PhraseGroupKind::Activation, &normalized)?;
            let pr_number = extract_merge_target_pr_number(user_text)?;
            Some(DetectedControlIntent {
                kind: rule.kind,
                pr_number,
                trace: ControlIntentTrace {
                    rule_id: rule.id,
                    surface: rule.surface,
                    activation: activation.phrase,
                    activation_group_id: activation.group_id,
                },
            })
        })
}

// Explicit `!` command handling happens in intake_gate.rs before natural-language
// routing. This parser only covers fallback phrase detection for free-form text.
pub(super) fn detect_natural_language_control_intent(
    user_text: &str,
) -> Option<DetectedControlIntent> {
    let surface = if user_text.trim_start().starts_with('!') {
        ControlIntentSurface::ExplicitCommand
    } else {
        ControlIntentSurface::NaturalLanguageFallback
    };

    match surface {
        ControlIntentSurface::ExplicitCommand => None,
        ControlIntentSurface::NaturalLanguageFallback => {
            detect_control_intent_for_surface(user_text, surface)
        }
    }
}

pub(super) fn build_control_intent_system_reminder(intent: &DetectedControlIntent) -> String {
    match intent.kind {
        ControlIntentKind::ReviewBypassDirectMerge => format!(
            "<system-reminder>\n\
             Detected control intent from the latest user message:\n\
             - kind: review_bypass_direct_merge\n\
             - pr_number: {}\n\
             - review_decision: dismiss\n\
             - matched_rule: {}\n\
             - matched_phrase_group: {}\n\
             - matched_phrase: {}\n\
             - input_surface: {}\n\
             Treat this as an explicit approval to bypass or dismiss review blockers so PR #{} can proceed via the direct/manual merge path if the repository/runtime supports it.\n\
             This is not a passive status question.\n\
             </system-reminder>",
            intent.pr_number,
            intent.trace.rule_id,
            intent.trace.activation_group_id,
            intent.trace.activation,
            intent.trace.surface.as_str(),
            intent.pr_number,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ControlIntentSurface, build_control_intent_system_reminder,
        detect_natural_language_control_intent,
    };

    #[test]
    fn detects_korean_review_bypass_phrase_with_leading_pr_number() {
        let intent =
            detect_natural_language_control_intent("366은 기여자가 직접 머지가능하게 만들 것 같아")
                .expect("direct merge intent should be detected");

        assert_eq!(intent.pr_number, 366);
        assert_eq!(intent.trace.rule_id, "review_bypass_direct_merge");
        assert_eq!(
            intent.trace.activation_group_id,
            "review_bypass_direct_merge.activation"
        );
        assert_eq!(intent.trace.activation, "기여자가 직접 머지");
    }

    #[test]
    fn detects_english_review_bypass_phrase_with_explicit_pr_reference() {
        let intent = detect_natural_language_control_intent(
            "Please make PR #366 author can merge after this review.",
        )
        .expect("english phrase should be detected");

        assert_eq!(intent.pr_number, 366);
        assert_eq!(
            intent.trace.surface,
            ControlIntentSurface::NaturalLanguageFallback
        );
        assert_eq!(intent.trace.activation, "author can merge");
    }

    #[test]
    fn ignores_meta_discussion_about_parser_behavior() {
        assert_eq!(
            detect_natural_language_control_intent("366 리뷰 우회 인식이 왜 안먹었는지 잡아줘"),
            None
        );
        assert_eq!(
            detect_natural_language_control_intent("PR #366 direct merge parser debug"),
            None
        );
    }

    #[test]
    fn ignores_negative_review_bypass_requests() {
        assert_eq!(
            detect_natural_language_control_intent("#366 리뷰 우회하면 안 돼"),
            None
        );
        assert_eq!(
            detect_natural_language_control_intent("366은 직접 머지하지 마"),
            None
        );
    }

    #[test]
    fn ignores_stray_non_pr_numbers() {
        assert_eq!(
            detect_natural_language_control_intent("2명만 직접 머지 가능하게 해줘"),
            None
        );
    }

    #[test]
    fn natural_language_parser_ignores_explicit_command_surface() {
        assert_eq!(
            detect_natural_language_control_intent("!direct-merge #366"),
            None
        );
    }

    #[test]
    fn system_reminder_includes_trace_fields() {
        let intent = detect_natural_language_control_intent("#366 리뷰 우회하고 직접 머지해도 돼")
            .expect("explicit PR reference should be detected");
        let reminder = build_control_intent_system_reminder(&intent);

        assert!(reminder.contains("matched_rule: review_bypass_direct_merge"));
        assert!(reminder.contains("matched_phrase_group: review_bypass_direct_merge.activation"));
        assert!(reminder.contains("matched_phrase: 리뷰 우회"));
        assert!(reminder.contains("input_surface: natural_language_fallback"));
        assert!(reminder.contains("PR #366"));
    }
}
