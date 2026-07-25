use poise::serenity_prelude as serenity;

use crate::services::discord::model_catalog::{
    DEFAULT_PICKER_VALUE, ModelCatalogEntry, SOURCE_PROVIDER_DEFAULT, is_default_picker_value,
    is_valid_discord_select_option_value, resolved_default_model, resolved_models,
};
use crate::services::provider::ProviderKind;

const DISCORD_SELECT_MENU_OPTION_LIMIT: usize = 25;
const EXPLICIT_MODEL_OPTION_LIMIT: usize = DISCORD_SELECT_MENU_OPTION_LIMIT - 1;
const DISCORD_SELECT_MENU_TEXT_LIMIT: usize = 100;

fn truncate_picker_text(raw: &str) -> String {
    raw.chars().take(DISCORD_SELECT_MENU_TEXT_LIMIT).collect()
}

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

fn effective_model_display(
    provider: &ProviderKind,
    effective_model: &str,
    effective_source: &str,
    working_dir: Option<&str>,
) -> String {
    if effective_source == SOURCE_PROVIDER_DEFAULT
        && effective_model.eq_ignore_ascii_case("default")
    {
        if let Some(resolved_default) = resolved_default_model(provider, working_dir) {
            return display_model_value(&resolved_default);
        }

        if let Some(runtime_model) = provider.default_model_behavior().runtime_model {
            return display_model_value(runtime_model);
        }
    }

    display_model_value(effective_model)
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
    effective_source: &str,
    pending_model: Option<&str>,
    override_model: Option<&str>,
    notice: Option<&str>,
    working_dir: Option<&str>,
) -> [String; 3] {
    [
        format!("Provider : `{}`", provider.as_str()),
        format!(
            "Current Model : `{}`",
            effective_model_display(provider, effective_model, effective_source, working_dir)
        ),
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
        .filter(|entry| is_valid_discord_select_option_value(entry.value.as_ref()))
        .take(EXPLICIT_MODEL_OPTION_LIMIT)
        .collect();

    if let Some(selected_value) = selected_explicit_model {
        if let Some(selected_entry) = resolved_models.iter().find(|entry| {
            is_valid_discord_select_option_value(entry.value.as_ref())
                && selected_value.eq_ignore_ascii_case(entry.value.as_ref())
        }) {
            if !entries.iter().any(|entry| {
                entry
                    .value
                    .eq_ignore_ascii_case(selected_entry.value.as_ref())
            }) {
                if entries.len() == EXPLICIT_MODEL_OPTION_LIMIT {
                    entries.pop();
                }
                entries.push(selected_entry);
            }
        }
    }

    entries
}

fn append_unavailable_selected_option(
    options: &mut Vec<ModelPickerOptionSpec>,
    selected_explicit_model: Option<&str>,
) {
    let Some(selected_value) =
        selected_explicit_model.filter(|value| is_valid_discord_select_option_value(value))
    else {
        return;
    };

    if options
        .iter()
        .any(|entry| entry.value.eq_ignore_ascii_case(selected_value))
    {
        return;
    }

    if options.len() == DISCORD_SELECT_MENU_OPTION_LIMIT {
        options.pop();
    }

    options.push(ModelPickerOptionSpec {
        value: selected_value.to_string(),
        label: truncate_picker_text(selected_value),
        description: truncate_picker_text("Current override | Not in current catalog"),
        selected: true,
    });
}

pub(super) fn build_model_picker_option_specs(
    provider: &ProviderKind,
    pending_model: Option<&str>,
    override_model: Option<&str>,
    default_model: &str,
    default_source: &str,
    working_dir: Option<&str>,
) -> Vec<ModelPickerOptionSpec> {
    let selected_explicit_model = match pending_model {
        Some(value) if is_default_picker_value(value) => None,
        Some(value) if is_valid_discord_select_option_value(value) => Some(value),
        Some(_) => override_model.filter(|value| is_valid_discord_select_option_value(value)),
        None => override_model.filter(|value| is_valid_discord_select_option_value(value)),
    };
    let resolved_models = resolved_models(provider, working_dir);
    let mut options = Vec::with_capacity(resolved_models.len());
    options.extend(
        capped_model_picker_explicit_entries(&resolved_models, selected_explicit_model)
            .iter()
            .map(|entry| ModelPickerOptionSpec {
                value: entry.value.to_string(),
                label: truncate_picker_text(&entry.label),
                description: truncate_picker_text(&entry.picker_description()),
                selected: selected_explicit_model
                    .is_some_and(|active| active.eq_ignore_ascii_case(entry.value.as_ref())),
            }),
    );
    if options.is_empty() {
        options.push(ModelPickerOptionSpec {
            value: DEFAULT_PICKER_VALUE.to_string(),
            label: truncate_picker_text(&default_picker_option_label()),
            description: truncate_picker_text(&default_picker_option_description(
                provider,
                default_model,
                default_source,
                working_dir,
            )),
            selected: false,
        });
    }
    append_unavailable_selected_option(&mut options, selected_explicit_model);
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
    use super::*;

    fn entry(index: usize) -> ModelCatalogEntry {
        ModelCatalogEntry::owned(
            format!("claude-test-{index}"),
            format!("Claude test {index}"),
            "Test catalog entry".to_string(),
            "Model picker invariant".to_string(),
        )
    }

    #[test]
    fn invariant_model_picker_caps_explicit_entries_and_keeps_selected_catalog_entry() {
        let entries = (0..40).map(entry).collect::<Vec<_>>();
        let selected = "claude-test-39";
        let capped = capped_model_picker_explicit_entries(&entries, Some(selected));

        assert_eq!(capped.len(), EXPLICIT_MODEL_OPTION_LIMIT);
        assert!(
            capped
                .iter()
                .any(|entry| entry.value.eq_ignore_ascii_case(selected))
        );
    }

    #[test]
    fn invariant_model_picker_keeps_valid_unavailable_selection_with_bounded_text() {
        let selected = "가".repeat(DISCORD_SELECT_MENU_TEXT_LIMIT);
        let mut options = (0..DISCORD_SELECT_MENU_OPTION_LIMIT)
            .map(|index| ModelPickerOptionSpec {
                value: format!("value-{index}"),
                label: format!("label-{index}"),
                description: format!("description-{index}"),
                selected: false,
            })
            .collect::<Vec<_>>();

        append_unavailable_selected_option(&mut options, Some(&selected));

        assert_eq!(options.len(), DISCORD_SELECT_MENU_OPTION_LIMIT);
        let retained = options.last().expect("selected option retained");
        assert_eq!(retained.value, selected);
        assert!(retained.selected);
        assert_eq!(
            retained.label.chars().count(),
            DISCORD_SELECT_MENU_TEXT_LIMIT
        );
        assert!(options.iter().all(|option| {
            option.label.chars().count() <= DISCORD_SELECT_MENU_TEXT_LIMIT
                && option.description.chars().count() <= DISCORD_SELECT_MENU_TEXT_LIMIT
        }));
    }

    #[test]
    fn invariant_model_picker_empty_catalog_keeps_default_before_valid_unavailable_override() {
        let selected = "custom-provider/custom-model";
        let options = build_model_picker_option_specs(
            &ProviderKind::OpenCode,
            None,
            Some(selected),
            "default",
            SOURCE_PROVIDER_DEFAULT,
            None,
        );

        assert_eq!(options.len(), 2);
        assert_eq!(options[0].value, DEFAULT_PICKER_VALUE);
        assert!(!options[0].selected);
        assert_eq!(options[1].value, selected);
        assert!(options[1].selected);
    }

    #[test]
    fn invariant_model_picker_skips_oversize_catalog_and_persisted_values_without_truncating() {
        let invalid = "x".repeat(DISCORD_SELECT_MENU_TEXT_LIMIT + 1);
        let entries = vec![ModelCatalogEntry::owned(
            invalid.clone(),
            "Invalid catalog value".to_string(),
            "Test catalog entry".to_string(),
            "Model picker invariant".to_string(),
        )];
        assert!(capped_model_picker_explicit_entries(&entries, Some(&invalid)).is_empty());

        let options = build_model_picker_option_specs(
            &ProviderKind::OpenCode,
            None,
            Some(&invalid),
            "default",
            SOURCE_PROVIDER_DEFAULT,
            None,
        );

        assert_eq!(options.len(), 1);
        assert_eq!(options[0].value, DEFAULT_PICKER_VALUE);
        assert!(options.iter().all(|option| option.value != invalid));
    }

    #[test]
    fn invariant_model_picker_truncation_is_unicode_character_safe() {
        let raw = "가".repeat(DISCORD_SELECT_MENU_TEXT_LIMIT + 5);
        let truncated = truncate_picker_text(&raw);

        assert_eq!(truncated.chars().count(), DISCORD_SELECT_MENU_TEXT_LIMIT);
        assert!(truncated.is_char_boundary(truncated.len()));
    }
}
