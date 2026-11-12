use super::super::types::{DecisionRequest, NoAnswerReason, Question};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) fn body(request: &DecisionRequest) -> Result<Value, NoAnswerReason> {
    if request.input_text.trim().is_empty() || request.template_version.trim().is_empty() {
        return Err(NoAnswerReason::InvalidRequest);
    }
    let question = match &request.question {
        Question::Noul {
            instructions,
            true_description,
            false_description,
        } => {
            let mut criteria = serde_json::Map::new();
            if let Some(description) = true_description {
                criteria.insert("true_description".into(), json!(description));
            }
            if let Some(description) = false_description {
                criteria.insert("false_description".into(), json!(description));
            }
            let mut question = json!({"type": "noul", "instructions": instructions});
            if !criteria.is_empty() {
                question["criteria"] = Value::Object(criteria);
            }
            question
        }
        Question::Choice {
            instructions,
            options,
        } => {
            let unique: BTreeSet<_> = options.iter().map(|(key, _)| key).collect();
            if !(2..=255).contains(&options.len())
                || unique.len() != options.len()
                || options.iter().any(|(key, description)| {
                    key.trim().is_empty() || description.trim().is_empty()
                })
            {
                return Err(NoAnswerReason::InvalidRequest);
            }
            let criteria: serde_json::Map<_, _> = options
                .iter()
                .map(|(key, description)| (key.clone(), json!(description)))
                .collect();
            json!({"type": "choice", "instructions": instructions, "criteria": criteria})
        }
        Question::Score {
            instructions,
            levels,
        } => {
            if !(2..=255).contains(&levels.len())
                || levels.iter().any(|level| level.trim().is_empty())
            {
                return Err(NoAnswerReason::InvalidRequest);
            }
            json!({"type": "score", "instructions": instructions, "criteria": levels})
        }
    };
    if question["instructions"]
        .as_str()
        .is_none_or(|value| value.trim().is_empty())
    {
        return Err(NoAnswerReason::InvalidRequest);
    }
    Ok(
        json!({"model": "gemini-decision-1", "input": [{"type": "text", "text": request.input_text}], "questions": {"q": question}}),
    )
}
