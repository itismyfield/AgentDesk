//! Effort choices share the same capability check as settings mutations.
use poise::serenity_prelude::ChannelId;

use crate::services::provider::ProviderKind;

const CLAUDE_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

pub(in crate::services::discord) struct Capability {
    levels: Vec<String>,
    unavailable: Option<String>,
    unknown: Option<String>,
    /// Whether Claude ultra can turn on. Asked only when ultra is at stake: asking runs the CLI.
    ultra: Option<Box<dyn Fn() -> Result<bool, String> + Send>>,
}

impl Capability {
    pub(in crate::services::discord) fn supports(&self, level: &str) -> Result<(), String> {
        if self.levels.iter().any(|supported| supported == level) {
            return Ok(());
        }
        if level == "ultra"
            && let Some(ask) = &self.ultra
        {
            return match ask()? {
                true => Ok(()),
                false => Err("현재 Claude 모델/워크플로에서 ultra를 지원하지 않아 변경하지 않았습니다. 모델은 그대로 유지됩니다.".into()),
            };
        }
        // A reason about ultra, or about a channel with no levels at all, answers only for those.
        let reason = (level == "ultra" || self.levels.is_empty())
            .then(|| self.unknown.as_ref().or(self.unavailable.as_ref()))
            .flatten();
        Err(reason.cloned().unwrap_or_else(|| {
            format!(
                "현재 채널 모델은 effort `{level}`를 지원하지 않습니다. 모델은 그대로 유지됩니다."
            )
        }))
    }

    /// Whether the model is known to lack `level`: its levels are listed and `level` is not one,
    /// and no answer about ultra is still to be asked. Unknown support is not a refusal.
    pub(in crate::services::discord) fn lacks(&self, level: &str) -> bool {
        !self.levels.is_empty()
            && !self.levels.iter().any(|supported| supported == level)
            && !(level == "ultra" && self.ultra.is_some())
    }

    /// The supported levels that start with `prefix`, the only ones `/effort` suggests.
    pub(in crate::services::discord) fn choices(&self, prefix: &str) -> Vec<String> {
        let mut levels: Vec<String> = self
            .levels
            .iter()
            .filter(|level| level.starts_with(prefix))
            .cloned()
            .collect();
        if self.ultra.is_some() && "ultra".starts_with(prefix) && self.supports("ultra").is_ok() {
            levels.push("ultra".into());
        }
        levels
    }
}

/// A channel that takes no level at all, for `reason`.
pub(in crate::services::discord) fn refused(reason: String) -> Capability {
    Capability {
        levels: Vec::new(),
        unavailable: Some(reason),
        unknown: None,
        ultra: None,
    }
}

/// A live Herdr pane sets the five overlay levels; ultra waits for its restart path.
pub(in crate::services::discord) fn herdr_claude() -> Capability {
    Capability {
        levels: CLAUDE_LEVELS.into_iter().map(str::to_string).collect(),
        unavailable: Some(
            "Herdr 채널의 ultra는 아직 지원하지 않아 변경하지 않았습니다. 모델은 그대로 유지됩니다."
                .into(),
        ),
        unknown: None,
        ultra: None,
    }
}

/// The levels the next launch of `model` on `channel` takes, `cwd` being the folder it starts in.
pub(in crate::services::discord) fn for_model(
    provider: &ProviderKind,
    model: Option<&str>,
    channel: ChannelId,
    cwd: Option<&std::path::Path>,
) -> Capability {
    match provider {
        ProviderKind::Claude => {
            let (model, cwd) = (model.map(str::to_string), cwd.map(|cwd| cwd.to_path_buf()));
            claude(move || {
                let home = super::super::org_schema::predicted_launch_home(
                    ProviderKind::Claude,
                    Some(channel.get()),
                    "",
                )
                .map_err(|_| "Claude 실행 계정을 확정하지 못해 ultra 지원을 확인하지 못했습니다. 모델은 그대로 유지됩니다.".to_string())?;
                crate::services::claude::capability_probe::ultracode_available(
                    model.as_deref(),
                    home.as_deref(),
                    cwd.as_deref(),
                )
            })
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
        _ => refused("/effort는 Claude와 Codex TUI 채널에서만 지원합니다.".into()),
    }
}

fn claude(ultra: impl Fn() -> Result<bool, String> + Send + 'static) -> Capability {
    Capability {
        levels: CLAUDE_LEVELS.into_iter().map(str::to_string).collect(),
        unavailable: None,
        unknown: None,
        ultra: Some(Box::new(ultra)),
    }
}

fn codex(efforts: Result<Vec<String>, String>) -> Capability {
    match efforts {
        Ok(levels) => Capability {
            levels,
            unavailable: None,
            unknown: None,
            ultra: None,
        },
        Err(reason) => Capability {
            levels: Vec::new(),
            unavailable: None,
            unknown: Some(reason),
            ultra: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthful_effort_choices_match_provider_model_capabilities() {
        let all = claude(|| Ok(true));
        assert_eq!(
            all.choices(""),
            ["low", "medium", "high", "xhigh", "max", "ultra"]
        );
        assert_eq!(all.choices("u"), ["ultra"]);
        for capability in [claude(|| Ok(false)), claude(|| Err("unobserved".into()))] {
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
        // A reason about ultra is not the answer to another unsupported value.
        let off = claude(|| Ok(false));
        assert!(
            off.supports("ultra")
                .unwrap_err()
                .contains("ultra를 지원하지")
        );
        assert!(
            off.supports("ultracode")
                .unwrap_err()
                .contains("`ultracode`")
        );
        // Ultra is asked about only when ultra is at stake.
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = asked.clone();
        let lazy = claude(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(true)
        });
        assert!(lazy.supports("high").is_ok() && lazy.supports("bogus").is_err());
        assert_eq!(lazy.choices("h"), ["high"]);
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(lazy.supports("ultra").is_ok());
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
        let herdr = herdr_claude();
        assert_eq!(herdr.choices(""), CLAUDE_LEVELS);
        assert!(herdr.supports("ultra").unwrap_err().contains("Herdr"));
        let none = refused("이 채널은 안 됩니다".into());
        assert!(none.choices("").is_empty());
        assert_eq!(none.supports("high").unwrap_err(), "이 채널은 안 됩니다");
    }

    #[test]
    fn unknown_ultracode_does_not_regress_normal_claude_effort() {
        let capability = for_model(&ProviderKind::Claude, None, ChannelId::new(8109), None);
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
