//! Pure policy facts; callers obtain authentication and routing decisions from existing accessors.

use super::PreparedSource;
#[cfg(test)]
use crate::services::discord::input_runtime::supervisor::mutant;
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum Protection {
    Unprotected,
    Protected(ProviderKind),
    Unknown,
}

#[derive(Clone, Debug)]
pub(in crate::services::discord) struct TargetFacts {
    pub channel_id: u64,
    pub protection: Protection,
}

#[derive(Clone, Debug)]
pub(in crate::services::discord) enum VoiceTarget {
    NotApplicable,
    Unknown,
    Resolved(TargetFacts),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum InputDisposition {
    Prompt,
    Terminal,
    Unsupported,
}

pub(in crate::services::discord) struct PolicyFacts {
    pub provider: Option<ProviderKind>,
    pub original: TargetFacts,
    pub execution: TargetFacts,
    pub voice_target: VoiceTarget,
    pub authorized: Option<bool>,
    pub supported: Option<bool>,
    pub disposition: Option<InputDisposition>,
    pub client_ready: bool,
    pub admission_open: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum Retry {
    Unknown,
    ClientUnavailable,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum Refusal {
    Provenance,
    ProviderMismatch,
    Unauthorized,
    Unsupported,
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum Classification {
    Legacy,
    Target,
    NonTarget,
    Retry(Retry),
    Rejected(Refusal),
}

/// A target is receipt-eligible only; this grants neither settlement nor an actor offer.
pub(in crate::services::discord) fn classify(
    facts: &PolicyFacts,
    source: &PreparedSource,
) -> Classification {
    let identity = source.identity();
    let voice_target = match &facts.voice_target {
        VoiceTarget::Resolved(target) => Some(target),
        VoiceTarget::NotApplicable | VoiceTarget::Unknown => None,
    };
    if !identity.validates(source.key())
        || facts.original.channel_id != identity.original_channel_id
        || facts.execution.channel_id != identity.execution_channel_id
        || voice_target.is_some_and(|target| target.channel_id == 0)
    {
        return Classification::Rejected(Refusal::Provenance);
    }
    let voice_unknown = matches!(facts.voice_target, VoiceTarget::Unknown);
    #[cfg(test)]
    let voice_unknown = voice_unknown && !mutant("c2_voice_unknown_to_absent");
    if voice_unknown {
        return Classification::Retry(Retry::Unknown);
    }
    let targets = [&facts.original, &facts.execution]
        .into_iter()
        .chain(voice_target);
    let mut protected = false;
    let mut unknown = false;
    for target in targets {
        match &target.protection {
            Protection::Unprotected => {}
            Protection::Unknown => unknown = true,
            Protection::Protected(provider) => {
                protected = true;
                if facts
                    .provider
                    .as_ref()
                    .is_some_and(|selected| selected != provider)
                {
                    return Classification::Rejected(Refusal::ProviderMismatch);
                }
            }
        }
    }
    #[cfg(test)]
    let unknown = unknown
        && (!mutant("c2_unknown_terminal_only")
            || facts.disposition == Some(InputDisposition::Terminal));
    if unknown {
        return Classification::Retry(Retry::Unknown);
    }
    if !protected {
        return Classification::Legacy;
    }
    if facts.provider.is_none()
        || facts.authorized.is_none()
        || facts.supported.is_none()
        || facts.disposition.is_none()
    {
        return Classification::Retry(Retry::Unknown);
    }
    if facts.authorized == Some(false) {
        return Classification::Rejected(Refusal::Unauthorized);
    }
    if !matches!(
        facts.provider,
        Some(ProviderKind::Claude | ProviderKind::Codex)
    ) || facts.supported == Some(false)
        || facts.disposition == Some(InputDisposition::Unsupported)
    {
        return Classification::Rejected(Refusal::Unsupported);
    }
    if !facts.client_ready {
        return Classification::Retry(Retry::ClientUnavailable);
    }
    if facts.disposition == Some(InputDisposition::Terminal) {
        return Classification::NonTarget;
    }
    if !source.has_content() {
        return Classification::Rejected(Refusal::Empty);
    }
    if !facts.admission_open {
        return Classification::Retry(Retry::Closed);
    }
    Classification::Target
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::input_runtime::intake::{Origin, SourceDraft, prepare_source};

    fn source(text: &str) -> PreparedSource {
        prepare_source(SourceDraft {
            origin: Origin::Message {
                message_id: 10,
                source_ids: vec![10],
                author_id: 7,
                original_channel_id: 8,
                execution_channel_id: 9,
            },
            author_is_bot: false,
            original_text: text.into(),
            prompt: "transformed boilerplate".into(),
            reply_context: Some("reply is not a new prompt".into()),
            attachments: vec![],
            voice_announcement: None,
        })
        .unwrap()
    }

    fn facts() -> PolicyFacts {
        PolicyFacts {
            provider: Some(ProviderKind::Claude),
            original: TargetFacts {
                channel_id: 8,
                protection: Protection::Unprotected,
            },
            execution: TargetFacts {
                channel_id: 9,
                protection: Protection::Protected(ProviderKind::Claude),
            },
            voice_target: VoiceTarget::NotApplicable,
            authorized: Some(true),
            supported: Some(true),
            disposition: Some(InputDisposition::Prompt),
            client_ready: true,
            admission_open: true,
        }
    }

    #[test]
    fn c1_every_protected_target_blocks_legacy_and_foreign_provider_execution() {
        for disposition in [InputDisposition::Prompt, InputDisposition::Terminal] {
            for index in 0..3 {
                let mut f = facts();
                f.disposition = Some(disposition);
                f.execution.protection = Protection::Unprotected;
                f.voice_target = VoiceTarget::Resolved(TargetFacts {
                    channel_id: 12,
                    protection: Protection::Unprotected,
                });
                let target = match index {
                    0 => &mut f.original,
                    1 => &mut f.execution,
                    _ => match &mut f.voice_target {
                        VoiceTarget::Resolved(target) => target,
                        _ => unreachable!(),
                    },
                };
                target.protection = Protection::Protected(ProviderKind::Claude);
                assert_eq!(
                    classify(&f, &source("hello")),
                    match disposition {
                        InputDisposition::Prompt => Classification::Target,
                        _ => Classification::NonTarget,
                    }
                );
                f.provider = Some(ProviderKind::Codex);
                assert_eq!(
                    classify(&f, &source("hello")),
                    Classification::Rejected(Refusal::ProviderMismatch)
                );
                f.provider = Some(ProviderKind::Claude);
                f.client_ready = false;
                assert_eq!(
                    classify(&f, &source("hello")),
                    Classification::Retry(Retry::ClientUnavailable)
                );
            }
        }
    }

    #[test]
    fn c1_unknown_facts_never_approve_or_settle_and_unprotected_stays_legacy() {
        for disposition in [InputDisposition::Prompt, InputDisposition::Terminal] {
            for other_protected in [false, true] {
                for index in 0..3 {
                    let mut f = facts();
                    f.disposition = Some(disposition);
                    f.execution.protection = Protection::Unprotected;
                    f.voice_target = VoiceTarget::Resolved(TargetFacts {
                        channel_id: 12,
                        protection: Protection::Unprotected,
                    });
                    match index {
                        0 => {
                            f.original.protection = Protection::Unknown;
                            if other_protected {
                                f.execution.protection =
                                    Protection::Protected(ProviderKind::Claude);
                            }
                        }
                        1 => {
                            f.execution.protection = Protection::Unknown;
                            if other_protected {
                                f.original.protection = Protection::Protected(ProviderKind::Claude);
                            }
                        }
                        _ => {
                            f.voice_target = VoiceTarget::Resolved(TargetFacts {
                                channel_id: 12,
                                protection: Protection::Unknown,
                            });
                            if other_protected {
                                f.execution.protection =
                                    Protection::Protected(ProviderKind::Claude);
                            }
                        }
                    }
                    assert_eq!(
                        classify(&f, &source("hello")),
                        Classification::Retry(Retry::Unknown),
                        "unknown target {index}, {disposition:?}, other protected {other_protected}"
                    );
                }
            }
            let protections = [
                Protection::Unprotected,
                Protection::Protected(ProviderKind::Claude),
                Protection::Unknown,
                Protection::Protected(ProviderKind::Codex),
            ];
            for original in &protections {
                for execution in &protections {
                    let mut f = facts();
                    f.disposition = Some(disposition);
                    f.original.protection = original.clone();
                    f.execution.protection = execution.clone();
                    f.voice_target = VoiceTarget::Unknown;
                    assert_eq!(
                        classify(&f, &source("hello")),
                        Classification::Retry(Retry::Unknown),
                        "voice unknown, {disposition:?}, original {original:?}, execution {execution:?}"
                    );
                }
            }
            for index in 0..4 {
                let mut f = facts();
                f.disposition = Some(disposition);
                match index {
                    0 => f.provider = None,
                    1 => f.authorized = None,
                    2 => f.supported = None,
                    _ => f.disposition = None,
                }
                assert_eq!(
                    classify(&f, &source("hello")),
                    Classification::Retry(Retry::Unknown)
                );
            }
            for voice_target in [
                VoiceTarget::NotApplicable,
                VoiceTarget::Resolved(TargetFacts {
                    channel_id: 12,
                    protection: Protection::Unprotected,
                }),
            ] {
                let mut f = facts();
                f.disposition = Some(disposition);
                f.voice_target = voice_target;
                f.execution.protection = Protection::Unprotected;
                f.client_ready = false;
                f.admission_open = false;
                assert_eq!(classify(&f, &source("hello")), Classification::Legacy);
                f.execution.protection = Protection::Protected(ProviderKind::Claude);
                f.client_ready = true;
                f.admission_open = true;
                assert_eq!(
                    classify(&f, &source("hello")),
                    match disposition {
                        InputDisposition::Prompt => Classification::Target,
                        _ => Classification::NonTarget,
                    }
                );
            }
        }
    }

    #[test]
    fn c1_empty_unauthorized_unsupported_and_wrong_channels_are_explicit_refusals() {
        let mut f = facts();
        assert_eq!(
            classify(&f, &source(" \n")),
            Classification::Rejected(Refusal::Empty)
        );
        f.authorized = Some(false);
        assert_eq!(
            classify(&f, &source("hello")),
            Classification::Rejected(Refusal::Unauthorized)
        );
        f.authorized = Some(true);
        f.supported = Some(false);
        assert_eq!(
            classify(&f, &source("hello")),
            Classification::Rejected(Refusal::Unsupported)
        );
        f.supported = Some(true);
        f.disposition = Some(InputDisposition::Unsupported);
        assert_eq!(
            classify(&f, &source("hello")),
            Classification::Rejected(Refusal::Unsupported)
        );
        f.disposition = Some(InputDisposition::Prompt);
        f.provider = Some(ProviderKind::Unsupported("unverified".into()));
        f.execution.protection = Protection::Protected(f.provider.clone().unwrap());
        assert_eq!(
            classify(&f, &source("hello")),
            Classification::Rejected(Refusal::Unsupported)
        );
        for provider in [
            ProviderKind::Gemini,
            ProviderKind::OpenCode,
            ProviderKind::Qwen,
            ProviderKind::Grok,
            ProviderKind::Antigravity,
        ] {
            f.provider = Some(provider.clone());
            f.execution.protection = Protection::Protected(provider);
            assert_eq!(
                classify(&f, &source("hello")),
                Classification::Rejected(Refusal::Unsupported)
            );
        }
        f.execution.channel_id = 8;
        assert_eq!(
            classify(&f, &source("hello")),
            Classification::Rejected(Refusal::Provenance)
        );
        for protection in [Protection::Unprotected, Protection::Unknown] {
            let mut f = facts();
            f.voice_target = VoiceTarget::Resolved(TargetFacts {
                channel_id: 0,
                protection,
            });
            assert_eq!(
                classify(&f, &source("hello")),
                Classification::Rejected(Refusal::Provenance)
            );
        }
    }

    #[test]
    fn c1_terminal_is_only_a_scan_classification_and_receipt_close_is_explicit() {
        let mut f = facts();
        f.admission_open = false;
        assert_eq!(
            classify(&f, &source("hello")),
            Classification::Retry(Retry::Closed)
        );
        f.disposition = Some(InputDisposition::Terminal);
        assert_eq!(classify(&f, &source("hello")), Classification::NonTarget);
        f.client_ready = false;
        assert_eq!(
            classify(&f, &source("hello")),
            Classification::Retry(Retry::ClientUnavailable)
        );
    }
}
