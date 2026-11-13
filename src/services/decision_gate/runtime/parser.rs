use super::super::types::{DecisionResult, DecisionValue, NoAnswerReason, Question};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(in crate::services::decision_gate) fn parse_response(
    question: &Question,
    data: &Value,
) -> Result<DecisionResult, NoAnswerReason> {
    let answer = match data.get("answers").and_then(|answers| answers.get("q")) {
        Some(answer) if !answer.is_null() => answer,
        _ if data
            .get("prompt_feedback")
            .or_else(|| data.get("promptFeedback"))
            .is_some_and(|feedback| !feedback.is_null()) =>
        {
            return Err(NoAnswerReason::Blocked);
        }
        _ => return Err(NoAnswerReason::MissingAnswer),
    };
    let confidence = probability(&answer["confidence"])?;
    let (choice, probabilities) = match question {
        #[cfg(test)]
        Question::Noul { .. } if answer["type"] == "noul" => {
            let value = probability(&answer["noul"])?;
            (
                DecisionValue::Noul {
                    answer: value >= 0.5,
                    p_yes: value,
                },
                BTreeMap::from([("true".into(), value), ("false".into(), 1.0 - value)]),
            )
        }
        Question::Choice { options, .. } if answer["type"] == "choice" => {
            let expected = options.iter().map(|(key, _)| key.clone()).collect();
            let probabilities = distribution(&answer["probabilities"], &expected)?;
            let selected = answer["choice"]
                .as_str()
                .ok_or(NoAnswerReason::InvalidResponse)?;
            let selected_probability = probabilities
                .get(selected)
                .ok_or(NoAnswerReason::InvalidResponse)?;
            let maximum = probabilities.values().copied().fold(0.0, f64::max);
            if selected_probability + 1e-9 < maximum {
                return Err(NoAnswerReason::InvalidResponse);
            }
            (DecisionValue::Choice(selected.into()), probabilities)
        }
        #[cfg(test)]
        Question::Score { levels, .. } if answer["type"] == "score" => {
            let score = answer["score"]
                .as_f64()
                .ok_or(NoAnswerReason::InvalidResponse)?;
            if !score.is_finite() || !(0.0..=(levels.len() - 1) as f64).contains(&score) {
                return Err(NoAnswerReason::InvalidResponse);
            }
            let expected = (0..levels.len()).map(|i| i.to_string()).collect();
            (
                DecisionValue::Score(score),
                distribution(&answer["probabilities"], &expected)?,
            )
        }
        _ => return Err(NoAnswerReason::InvalidResponse),
    };
    Ok(DecisionResult::Answer {
        choice,
        confidence,
        probabilities,
    })
}

fn probability(value: &Value) -> Result<f64, NoAnswerReason> {
    value
        .as_f64()
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        .ok_or(NoAnswerReason::InvalidResponse)
}

fn distribution(
    value: &Value,
    expected: &BTreeSet<String>,
) -> Result<BTreeMap<String, f64>, NoAnswerReason> {
    let object = value.as_object().ok_or(NoAnswerReason::InvalidResponse)?;
    if object.keys().cloned().collect::<BTreeSet<_>>() != *expected {
        return Err(NoAnswerReason::InvalidResponse);
    }
    let probabilities: BTreeMap<_, _> = object
        .iter()
        .map(|(key, value)| Ok((key.clone(), probability(value)?)))
        .collect::<Result<_, NoAnswerReason>>()?;
    if !(0.99..=1.01).contains(&probabilities.values().sum::<f64>()) {
        return Err(NoAnswerReason::InvalidResponse);
    }
    Ok(probabilities)
}
