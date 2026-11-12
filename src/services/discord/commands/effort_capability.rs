//! Effort choices share the same capability check as settings mutations.
use poise::serenity_prelude::ChannelId;

use crate::services::provider::ProviderKind;

const CLAUDE_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

pub(in crate::services::discord) struct Capability {
    levels: Vec<String>,
    unavailable: Option<String>,
    unknown: Option<String>,
}

impl Capability {
    pub(in crate::services::discord) fn supports(&self, level: &str) -> Result<(), String> {
        if self.levels.iter().any(|supported| supported == level) {
            return Ok(());
        }
        Err(self
            .unknown
            .as_ref()
            .or(self.unavailable.as_ref())
            .cloned()
            .unwrap_or_else(|| format!("현재 채널 모델은 effort `{level}`를 지원하지 않습니다. 모델은 그대로 유지됩니다.")))
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn choices(&self, prefix: &str) -> Vec<String> {
        self.levels
            .iter()
            .filter(|level| level.starts_with(prefix))
            .cloned()
            .collect()
    }
}

pub(in crate::services::discord) fn for_model(
    provider: &ProviderKind,
    model: Option<&str>,
    channel: ChannelId,
) -> Capability {
    match provider {
        ProviderKind::Claude => {
            claude(crate::services::claude::capability_probe::ultracode_available(model))
        }
        ProviderKind::Codex => {
            let home = super::super::org_schema::predicted_launch_home(
                ProviderKind::Codex,
                Some(channel.get()),
                "",
            );
            match home {
                Ok(home) => codex(
                    super::super::model_catalog::effort::codex_supported_efforts(
                        model,
                        home.as_deref(),
                    ),
                ),
                Err(_) => codex(Err(
                    "Codex 실행 계정을 확정하지 못해 effort 지원을 확인하지 못했습니다.".into(),
                )),
            }
        }
        _ => Capability {
            levels: Vec::new(),
            unavailable: Some("/effort는 Claude와 Codex TUI 채널에서만 지원합니다.".into()),
            unknown: None,
        },
    }
}

fn claude(ultra: Result<bool, String>) -> Capability {
    let mut capability = Capability {
        levels: CLAUDE_LEVELS.into_iter().map(str::to_string).collect(),
        unavailable: None,
        unknown: None,
    };
    match ultra {
        Ok(true) => capability.levels.push("ultra".into()),
        Ok(false) => capability.unavailable = Some("현재 Claude 모델/워크플로에서 ultra를 지원하지 않아 변경하지 않았습니다. 모델은 그대로 유지됩니다.".into()),
        Err(reason) => capability.unknown = Some(reason),
    }
    capability
}

fn codex(efforts: Result<Vec<String>, String>) -> Capability {
    match efforts {
        Ok(levels) => Capability {
            levels,
            unavailable: None,
            unknown: None,
        },
        Err(reason) => Capability {
            levels: Vec::new(),
            unavailable: None,
            unknown: Some(reason),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthful_effort_choices_match_provider_model_capabilities() {
        let all = claude(Ok(true));
        assert_eq!(
            all.choices(""),
            ["low", "medium", "high", "xhigh", "max", "ultra"]
        );
        assert_eq!(all.choices("u"), ["ultra"]);
        for capability in [claude(Ok(false)), claude(Err("unobserved".into()))] {
            assert_eq!(capability.choices(""), CLAUDE_LEVELS);
            assert!(capability.supports("ultra").is_err());
            for level in CLAUDE_LEVELS {
                assert!(capability.supports(level).is_ok());
            }
        }
        let luna = codex(Ok(CLAUDE_LEVELS.into_iter().map(str::to_string).collect()));
        assert_eq!(luna.choices(""), CLAUDE_LEVELS);
        assert!(luna.supports("ultra").is_err());
        assert!(luna.supports("xhigh").is_ok());
    }

    #[test]
    fn unknown_ultracode_does_not_regress_normal_claude_effort() {
        let capability = for_model(&ProviderKind::Claude, None, ChannelId::new(8109));
        for level in CLAUDE_LEVELS {
            assert!(capability.supports(level).is_ok());
        }
        assert!(
            capability
                .supports("ultra")
                .unwrap_err()
                .contains("확인하지 못했습니다")
        );
        let capability = codex(Err("unobserved model metadata".into()));
        assert!(capability.choices("").is_empty());
        assert_eq!(
            capability.supports("high").unwrap_err(),
            "unobserved model metadata"
        );
    }
}
