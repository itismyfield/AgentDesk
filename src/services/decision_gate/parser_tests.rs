use super::*;
use serde_json::{Value, json};

#[test]
fn parser_rejects_invalid_fields_keys_probabilities_and_types() {
    let original = fixtures()["choice"].clone();
    for patch in [
        json!({"confidence": -0.01}),
        json!({"confidence": 1.01}),
        json!({"confidence": "0.9"}),
        json!({"type": "noul"}),
        json!({"choice": "unknown"}),
        json!({"probabilities": {"host-a": 0.9, "hold": 0.1}}),
        json!({"probabilities": {"host-a": 0.8, "host-b": 0.1, "hold": 0.1, "extra": 0.0}}),
        json!({"probabilities": {"host-a": 0.6, "host-b": 0.1, "hold": 0.1}}),
        json!({"probabilities": {"host-a": 0.2, "host-b": 0.4, "hold": 0.4}}),
        json!({"probabilities": {"host-a": 1.01, "host-b": -0.01, "hold": 0.0}}),
        json!({"probabilities": {"host-a": "0.8", "host-b": 0.1, "hold": 0.1}}),
        json!({"confidence": null}),
    ] {
        let mut data = original.clone();
        for (key, value) in patch.as_object().unwrap() {
            data["answers"]["q"][key] = value.clone();
        }
        assert_eq!(
            parse_response(&choice(), &data),
            Err(NoAnswerReason::InvalidResponse)
        );
    }
    for missing in ["type", "confidence", "choice", "probabilities"] {
        let mut data = original.clone();
        data["answers"]["q"]
            .as_object_mut()
            .unwrap()
            .remove(missing);
        assert_eq!(
            parse_response(&choice(), &data),
            Err(NoAnswerReason::InvalidResponse)
        );
    }
    for raw in ["NaN", "Infinity", "1e999"] {
        assert!(serde_json::from_str::<Value>(raw).is_err());
    }
}

#[test]
fn parser_allows_ties_only_within_epsilon_and_keeps_raw_values() {
    let question = Question::Choice {
        instructions: "Pick".into(),
        options: vec![("a".into(), "A".into()), ("b".into(), "B".into())],
    };
    for (a, b, allowed) in [
        (0.5, 0.5, true),
        (0.5 - 2e-10, 0.5 + 2e-10, true),
        (0.5 - 2e-9, 0.5 + 2e-9, false),
        (0.505, 0.505, true),
        (0.505001, 0.505001, false),
    ] {
        let data = json!({"answers":{"q":{"type":"choice", "choice":"a", "confidence":0.1,"probabilities":{"a":a,"b":b}}}});
        let result = parse_response(&question, &data);
        assert_eq!(result.is_ok(), allowed);
        if allowed {
            let DecisionResult::Answer {
                confidence,
                probabilities,
                ..
            } = result.unwrap()
            else {
                panic!("expected Answer")
            };
            assert_eq!(confidence, 0.1);
            assert_eq!(probabilities["a"], a);
        }
    }
}

#[test]
fn parser_noul_boundary_score_range_and_missing_answers() {
    let noul = Question::Noul {
        instructions: "Work?".into(),
        true_description: Some("Has work".into()),
        false_description: Some("No work".into()),
    };
    for (probability, yes) in [(0.0, false), (0.499999, false), (0.5, true), (1.0, true)] {
        let data = json!({"answers":{"q":{"type":"noul","noul":probability,"confidence":0.9}}});
        let DecisionResult::Answer {
            choice,
            probabilities,
            ..
        } = parse_response(&noul, &data).unwrap()
        else {
            panic!("expected Answer")
        };
        assert_eq!(
            choice,
            DecisionValue::Noul {
                answer: yes,
                p_yes: probability
            }
        );
        assert_eq!(probabilities["false"], 1.0 - probability);
    }
    for probability in [json!(-0.1), json!(1.1), json!(null), json!(true)] {
        assert_eq!(
            parse_response(
                &noul,
                &json!({"answers":{"q":{"type":"noul","noul":probability,"confidence":0.9}}})
            ),
            Err(NoAnswerReason::InvalidResponse)
        );
    }
    let score = Question::Score {
        instructions: "Urgency?".into(),
        levels: vec!["Low".into(), "Medium".into(), "High".into()],
    };
    for value in [json!(-0.01), json!(2.01), json!(null), json!("1.25")] {
        let mut data = fixtures()["score"].clone();
        data["answers"]["q"]["score"] = value;
        assert_eq!(
            parse_response(&score, &data),
            Err(NoAnswerReason::InvalidResponse)
        );
    }
    for data in [json!({}), json!({"answers":{"q":null}})] {
        assert_eq!(
            parse_response(&choice(), &data),
            Err(NoAnswerReason::MissingAnswer)
        );
    }
    assert_eq!(
        parse_response(
            &choice(),
            &json!({"prompt_feedback":{"block_reason":"SAFETY"}})
        ),
        Err(NoAnswerReason::Blocked)
    );
}
