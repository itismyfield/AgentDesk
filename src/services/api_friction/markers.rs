use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const MAX_REPORT_FIELD_CHARS: usize = 240;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ApiFrictionReport {
    pub endpoint: String,
    pub friction_type: String,
    pub summary: String,
    pub workaround: Option<String>,
    pub suggested_fix: Option<String>,
    pub docs_category: Option<String>,
    pub keywords: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawApiFrictionReport {
    endpoint: Option<String>,
    #[serde(alias = "surface")]
    area: Option<String>,
    friction_type: Option<String>,
    #[serde(alias = "frictionType")]
    friction_type_camel: Option<String>,
    #[serde(alias = "type")]
    kind: Option<String>,
    summary: Option<String>,
    workaround: Option<String>,
    #[serde(alias = "workaround_method")]
    workaround_method: Option<String>,
    suggested_fix: Option<String>,
    #[serde(alias = "suggestedFix")]
    suggested_fix_camel: Option<String>,
    docs_category: Option<String>,
    #[serde(alias = "docsCategory")]
    docs_category_camel: Option<String>,
    keywords: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ApiFrictionExtraction {
    pub cleaned_response: String,
    pub reports: Vec<ApiFrictionReport>,
    pub parse_errors: Vec<String>,
}

pub(crate) fn extract_api_friction_reports(full_response: &str) -> ApiFrictionExtraction {
    let mut cleaned_lines = Vec::new();
    let mut reports = Vec::new();
    let mut parse_errors = Vec::new();

    for line in full_response.lines() {
        let trimmed = line.trim();
        let Some(payload) = trimmed.strip_prefix("API_FRICTION:") else {
            cleaned_lines.push(line.to_string());
            continue;
        };

        match serde_json::from_str::<RawApiFrictionReport>(payload.trim())
            .map_err(|err| err.to_string())
            .and_then(ApiFrictionReport::try_from_raw)
        {
            Ok(report) => reports.push(report),
            Err(error) => {
                parse_errors.push(format!("invalid API_FRICTION marker: {error}"));
                cleaned_lines.push(line.to_string());
            }
        }
    }

    ApiFrictionExtraction {
        cleaned_response: normalize_cleaned_response(&cleaned_lines.join("\n")),
        reports,
        parse_errors,
    }
}

impl ApiFrictionReport {
    fn try_from_raw(raw: RawApiFrictionReport) -> Result<Self, String> {
        let endpoint = raw
            .endpoint
            .or(raw.area)
            .map(|value| clean_text_field(&value, MAX_REPORT_FIELD_CHARS))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "endpoint is required".to_string())?;
        let friction_type = raw
            .friction_type
            .or(raw.friction_type_camel)
            .or(raw.kind)
            .map(|value| clean_text_field(&value, 80))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "friction_type is required".to_string())?;
        let summary = raw
            .summary
            .map(|value| clean_text_field(&value, MAX_REPORT_FIELD_CHARS))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "summary is required".to_string())?;
        let workaround = raw
            .workaround
            .or(raw.workaround_method)
            .map(|value| clean_text_field(&value, MAX_REPORT_FIELD_CHARS))
            .filter(|value| !value.is_empty());
        let suggested_fix = raw
            .suggested_fix
            .or(raw.suggested_fix_camel)
            .map(|value| clean_text_field(&value, MAX_REPORT_FIELD_CHARS))
            .filter(|value| !value.is_empty());
        let docs_category = raw
            .docs_category
            .or(raw.docs_category_camel)
            .map(|value| clean_text_field(&value, 80))
            .filter(|value| !value.is_empty());

        let keywords = collect_keywords(
            raw.keywords.unwrap_or_default(),
            &endpoint,
            &friction_type,
            workaround.as_deref(),
            docs_category.as_deref(),
        );

        Ok(Self {
            endpoint,
            friction_type,
            summary,
            workaround,
            suggested_fix,
            docs_category,
            keywords,
        })
    }
}

fn normalize_cleaned_response(text: &str) -> String {
    let collapsed = text
        .lines()
        .scan(false, |last_blank, line| {
            let is_blank = line.trim().is_empty();
            if is_blank && *last_blank {
                return Some(None);
            }
            *last_blank = is_blank;
            Some(Some(line))
        })
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
    collapsed.trim().to_string()
}

fn clean_text_field(value: &str, limit: usize) -> String {
    truncate_chars(
        &value
            .split_whitespace()
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        limit,
    )
}

fn collect_keywords(
    explicit: Vec<String>,
    endpoint: &str,
    friction_type: &str,
    workaround: Option<&str>,
    docs_category: Option<&str>,
) -> Vec<String> {
    let mut set = BTreeSet::new();
    set.insert(clean_text_field(endpoint, 80));
    set.insert(clean_text_field(friction_type, 80));
    if let Some(workaround) = workaround {
        set.insert(clean_text_field(workaround, 80));
    }
    if let Some(docs_category) = docs_category {
        set.insert(clean_text_field(docs_category, 80));
    }
    for keyword in explicit {
        let cleaned = clean_text_field(&keyword, 80);
        if !cleaned.is_empty() {
            set.insert(cleaned);
        }
    }
    set.into_iter().filter(|value| !value.is_empty()).collect()
}

pub(super) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{}...", truncated.trim_end())
    } else {
        truncated
    }
}
