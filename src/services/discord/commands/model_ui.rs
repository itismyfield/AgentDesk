use poise::serenity_prelude as serenity;

use crate::services::discord::model_catalog::{
    DEFAULT_PICKER_VALUE, ModelCatalogEntry, SOURCE_PROVIDER_DEFAULT, is_default_picker_value,
    resolved_default_model, resolved_models,
};
use crate::services::provider::ProviderKind;

const DISCORD_SELECT_MENU_OPTION_LIMIT: usize = 25;
const EXPLICIT_MODEL_OPTION_LIMIT: usize = DISCORD_SELECT_MENU_OPTION_LIMIT - 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ModelPickerOptionSpec {
    pub value: String,
    pub label: String,
    pub description: String,
    pub selected: bool,
}

pub(super) fn display_model_value(raw: &str) -> String {
    match raw {
        "(default)" | "system default" => "default".to_string(),
        other => other.to_string(),
    }
}

pub(super) fn has_pending_model_change(
    pending_model: Option<&str>,
    override_model: Option<&str>,
) -> bool {
    match pending_model {
        None => false,
        Some(value) if is_default_picker_value(value) => override_model.is_some(),
        Some(pending) => {
            !override_model.is_some_and(|current| current.eq_ignore_ascii_case(pending))
        }
    }
}

fn build_model_picker_runtime_status(
    pending_model: Option<&str>,
    override_model: Option<&str>,
    notice: Option<&str>,
) -> String {
    if let Some(notice) = notice {
        return notice.to_string();
    }

    match pending_model {
        Some(value) if is_default_picker_value(value) && override_model.is_some() => {
            "기본값 복귀 대기".to_string()
        }
        Some(value) if is_default_picker_value(value) => "기본 설정 유지".to_string(),
        Some(value)
            if override_model.is_some_and(|current| current.eq_ignore_ascii_case(value)) =>
        {
            "현재 오버라이드 유지".to_string()
        }
        Some(value) => format!("`{}` 저장 대기", display_model_value(value)),
        None if override_model.is_some() => "채널 오버라이드 적용 중".to_string(),
        None => "기본 설정 사용 중".to_string(),
    }
}

pub(super) fn build_model_picker_summary_lines(
    provider: &ProviderKind,
    effective_model: &str,
    pending_model: Option<&str>,
    override_model: Option<&str>,
    notice: Option<&str>,
) -> [String; 3] {
    [
        format!("Provider : `{}`", provider.as_str()),
        format!("Current Model : `{}`", display_model_value(effective_model)),
        format!(
            "현재 작업 상태 : {}",
            build_model_picker_runtime_status(pending_model, override_model, notice)
        ),
    ]
}

fn default_picker_option_label() -> String {
    "기본값".to_string()
}

fn default_picker_option_description(
    provider: &ProviderKind,
    default_model: &str,
    default_source: &str,
    working_dir: Option<&str>,
) -> String {
    match default_source {
        SOURCE_PROVIDER_DEFAULT => {
            if let Some(resolved_default) = resolved_default_model(provider, working_dir) {
                return format!(
                    "오버라이드 해제 -> {} (Qwen settings default)",
                    display_model_value(&resolved_default)
                );
            }
            match provider.default_model_behavior().runtime_model {
                Some(model) => format!(
                    "오버라이드 해제 -> {} ({})",
                    display_model_value(model),
                    provider.default_model_behavior().source_label
                ),
                None => format!(
                    "오버라이드 해제 -> {}",
                    provider.default_model_behavior().source_label
                ),
            }
        }
        other => format!("오버라이드 해제 -> {} ({})", default_model, other),
    }
}

fn capped_model_picker_explicit_entries<'a>(
    resolved_models: &'a [ModelCatalogEntry],
    selected_explicit_model: Option<&str>,
) -> Vec<&'a ModelCatalogEntry> {
    let mut entries: Vec<&ModelCatalogEntry> = resolved_models
        .iter()
        .take(EXPLICIT_MODEL_OPTION_LIMIT)
        .collect();

    if let Some(selected_value) = selected_explicit_model {
        if let Some(selected_entry) = resolved_models
            .iter()
            .find(|entry| selected_value.eq_ignore_ascii_case(entry.value))
        {
            if !entries
                .iter()
                .any(|entry| entry.value.eq_ignore_ascii_case(selected_entry.value))
            {
                if entries.len() == EXPLICIT_MODEL_OPTION_LIMIT {
                    entries.pop();
                }
                entries.push(selected_entry);
            }
        }
    }

    entries
}

pub(super) fn build_model_picker_option_specs(
    provider: &ProviderKind,
    pending_model: Option<&str>,
    override_model: Option<&str>,
    default_model: &str,
    default_source: &str,
    working_dir: Option<&str>,
) -> Vec<ModelPickerOptionSpec> {
    let default_selected = match pending_model {
        Some(value) => is_default_picker_value(value),
        None => override_model.is_none(),
    };
    let selected_explicit_model = match pending_model {
        Some(value) if !is_default_picker_value(value) => Some(value),
        _ => override_model,
    };

    let resolved_models = resolved_models(provider, working_dir);
    let mut options = Vec::with_capacity(resolved_models.len() + 1);
    options.push(ModelPickerOptionSpec {
        value: DEFAULT_PICKER_VALUE.to_string(),
        label: default_picker_option_label(),
        description: default_picker_option_description(
            provider,
            default_model,
            default_source,
            working_dir,
        ),
        selected: default_selected,
    });
    options.extend(
        capped_model_picker_explicit_entries(&resolved_models, selected_explicit_model)
            .iter()
            .map(|entry| ModelPickerOptionSpec {
                value: entry.value.to_string(),
                label: entry.label.to_string(),
                description: entry.picker_description(),
                selected: selected_explicit_model
                    .is_some_and(|active| active.eq_ignore_ascii_case(entry.value)),
            }),
    );
    options
}

pub(super) fn build_model_picker_options(
    provider: &ProviderKind,
    pending_model: Option<&str>,
    override_model: Option<&str>,
    default_model: &str,
    default_source: &str,
    working_dir: Option<&str>,
) -> Vec<serenity::CreateSelectMenuOption> {
    build_model_picker_option_specs(
        provider,
        pending_model,
        override_model,
        default_model,
        default_source,
        working_dir,
    )
    .iter()
    .map(|entry| {
        serenity::CreateSelectMenuOption::new(entry.label.clone(), entry.value.clone())
            .description(entry.description.clone())
            .default_selection(entry.selected)
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::{EXPLICIT_MODEL_OPTION_LIMIT, capped_model_picker_explicit_entries};
    use crate::services::discord::model_catalog::ModelCatalogEntry;

    fn leaked(value: &str) -> &'static str {
        Box::leak(value.to_string().into_boxed_str())
    }

    fn sample_entry(index: usize) -> ModelCatalogEntry {
        ModelCatalogEntry {
            value: leaked(&format!("model-{index}")),
            label: leaked(&format!("Model {index}")),
            primary_summary: "summary",
            secondary_summary: "catalog",
        }
    }

    #[test]
    fn capped_model_picker_explicit_entries_obeys_discord_limit() {
        let models: Vec<ModelCatalogEntry> = (0..40).map(sample_entry).collect();
        let capped = capped_model_picker_explicit_entries(&models, None);

        assert_eq!(capped.len(), EXPLICIT_MODEL_OPTION_LIMIT);
        assert_eq!(capped.first().unwrap().value, "model-0");
        assert_eq!(capped.last().unwrap().value, "model-23");
    }

    #[test]
    fn capped_model_picker_explicit_entries_keeps_selected_model_visible() {
        let models: Vec<ModelCatalogEntry> = (0..40).map(sample_entry).collect();
        let capped = capped_model_picker_explicit_entries(&models, Some("model-39"));

        assert_eq!(capped.len(), EXPLICIT_MODEL_OPTION_LIMIT);
        assert!(capped.iter().any(|entry| entry.value == "model-39"));
        assert!(!capped.iter().any(|entry| entry.value == "model-23"));
    }
}
