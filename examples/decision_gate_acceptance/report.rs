use super::{
    MODEL, Result, Sample, TEMPLATE, capture, issue_duplicate, json, services, transport,
    write_json, write_safe,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Serialize)]
pub(super) struct Row {
    sample_id: String,
    expected: String,
    key: String,
    candidate_count: usize,
    title_chars: usize,
    background_bytes: usize,
    input_bytes: usize,
    model: String,
    template: String,
    deadline_ms: Option<u64>,
    candidate_fetch_ms: f64,
    decision_elapsed_ms: u64,
    total_elapsed_ms: f64,
    input_tokens: Option<u64>,
    result: &'static str,
    reason: &'static str,
    choice: Option<String>,
    p_choice: Option<f64>,
    max_non_none: Option<f64>,
    confidence: Option<f64>,
    action: &'static str,
    returned_key: Option<String>,
    false_positive: bool,
    false_negative: bool,
    wrong_candidate: bool,
    evidence: String,
    pub(super) probabilities: BTreeMap<String, f64>,
}

pub(super) fn row(
    sample: &Sample,
    outcome: &issue_duplicate::DuplicateCheck,
    record: &Option<transport::Record>,
    fetch_ms: f64,
    observation: &services::decision_gate::types::DecisionObservation,
    deadline_ms: Option<u64>,
    total_elapsed_ms: f64,
) -> Result<Row> {
    let (result, reason, action, returned_key) = match outcome {
        issue_duplicate::DuplicateCheck::Candidate(candidate) => (
            "Answer",
            "at_threshold",
            "409",
            Some(format!("issue_{}", candidate.number)),
        ),
        issue_duplicate::DuplicateCheck::Proceed(meta) => match meta.outcome {
            "clear" => ("Answer", meta.reason, "create", None),
            "off" => ("Off", meta.reason, "fallback", None),
            "no_answer" => ("NoAnswer", meta.reason, "fallback", None),
            _ => return Err("unexpected_consumer_outcome"),
        },
    };
    let record = record.as_ref().ok_or("missing_transport_record")?;
    if record.request["model"] != MODEL || observation.template_version != TEMPLATE {
        return Err("model_or_template_drift");
    }
    let criteria = record.request["questions"]["q"]["criteria"]
        .as_object()
        .ok_or("missing_request_criteria")?;
    let mut probabilities = BTreeMap::new();
    let (mut choice, mut confidence) = (None, None);
    if result == "Answer" {
        let data = record.response.as_ref().ok_or("missing_answer_body")?;
        let answer = &data["answers"]["q"];
        choice = answer["choice"].as_str().map(str::to_owned);
        confidence = answer["confidence"].as_f64();
        for (key, value) in answer["probabilities"]
            .as_object()
            .ok_or("missing_probabilities")?
        {
            probabilities.insert(key.clone(), value.as_f64().ok_or("invalid_probability")?);
        }
    }
    let p_choice = choice
        .as_ref()
        .and_then(|key| probabilities.get(key))
        .copied();
    let max_non_none = probabilities
        .iter()
        .filter(|(key, _)| key.as_str() != "none")
        .map(|(_, value)| *value)
        .reduce(f64::max);
    let input = record.request["input"][0]["text"]
        .as_str()
        .ok_or("missing_input_text")?;
    Ok(Row {
        sample_id: sample.sample_id.clone(),
        expected: sample.expected.clone(),
        key: sample.key.clone(),
        candidate_count: criteria.len() - 1,
        title_chars: sample.title.chars().count(),
        background_bytes: sample.background.len(),
        input_bytes: input.len(),
        model: MODEL.into(),
        template: observation.template_version.clone(),
        deadline_ms,
        candidate_fetch_ms: fetch_ms,
        decision_elapsed_ms: observation.elapsed_ms,
        total_elapsed_ms,
        input_tokens: observation.input_tokens,
        result,
        reason,
        choice,
        p_choice,
        max_non_none,
        confidence,
        action,
        false_positive: sample.expected == "new" && action == "409",
        false_negative: sample.expected == "duplicate" && action == "create",
        wrong_candidate: sample.expected == "duplicate"
            && returned_key.as_ref().is_some_and(|key| key != &sample.key),
        returned_key,
        evidence: format!(
            "probabilities.json#{};request_sha256={}",
            sample.sample_id,
            capture::hash(&serde_json::to_vec(&record.request).map_err(|_| "request_hash_failed")?)
        ),
        probabilities,
    })
}

const COLUMNS: &[&str] = &[
    "sample_id",
    "expected",
    "key",
    "candidate_count",
    "title_chars",
    "background_bytes",
    "input_bytes",
    "model",
    "template",
    "deadline_ms",
    "candidate_fetch_ms",
    "decision_elapsed_ms",
    "total_elapsed_ms",
    "input_tokens",
    "result",
    "reason",
    "choice",
    "p_choice",
    "max_non_none",
    "confidence",
    "action",
    "returned_key",
    "false_positive",
    "false_negative",
    "wrong_candidate",
    "evidence",
];

fn cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}

fn csv_cell(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

pub(super) fn write(out: &Path, rows: &[Row], distributions: &[Value], fake: bool) -> Result<()> {
    let summary = summary(rows, fake);
    let mut csv = format!("{}\n", COLUMNS.join(","));
    let mut md = format!(
        "# Decision gate acceptance measurements\n\nmode={} · threshold=0.8 · samples={}\n\n",
        if fake {
            "loopback_fake_only"
        } else {
            "gemini_rest"
        },
        rows.len()
    );
    md.push_str("Candidate fetch measures cloning the fixed in-memory list; no GitHub request is made. Decision elapsed and tokens come from the unchanged common gate observation. Action is the consumer decision; no issue is created.\n\n");
    md.push_str("Off/NoAnswer are fallback and excluded from correct-new, false-positive, and false-negative Answer counts. Wrong duplicate targets are counted separately. Empty metrics mean unavailable.\n\n");
    md.push_str(&format!(
        "| {} |\n| {} |\n",
        COLUMNS.join(" | "),
        vec!["---"; COLUMNS.len()].join(" | ")
    ));
    for row in rows {
        let value = serde_json::to_value(row).map_err(|_| "row_serialization_failed")?;
        let cells: Vec<_> = COLUMNS.iter().map(|column| cell(&value[column])).collect();
        csv.push_str(
            &cells
                .iter()
                .map(|value| csv_cell(value))
                .collect::<Vec<_>>()
                .join(","),
        );
        csv.push('\n');
        md.push_str(&format!(
            "| {} |\n",
            cells
                .iter()
                .map(|value| value.replace('|', "\\|").replace('\n', " "))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    md.push_str("\n## Threshold 0.8 totals\n\n");
    md.push_str(&format!("Answer={} · fallback={} · false_positive={} · false_negative={} · wrong_candidate={} · correct_new={}\n\n",
        summary["answer"], summary["fallback"], summary["false_positive"], summary["false_negative"], summary["wrong_candidate"], summary["correct_new"]));
    md.push_str(&format!(
        "Duplicate fallback={} · duplicate creation including fallback={} · new fallback={}\n\n",
        summary["duplicate_fallback"],
        summary["duplicate_creation_including_fallback"],
        summary["new_fallback"]
    ));
    md.push_str(&format!("254+none: count={} · Answer={} · Timeout={} · InvalidResponse={} · elapsed median={}ms · max={}ms\n\n",
        summary["large"]["count"], summary["large"]["answer"], summary["large"]["timeout"], summary["large"]["invalid_response"], summary["large"]["elapsed_median_ms"], summary["large"]["elapsed_max_ms"]));
    md.push_str("All choice probabilities are in `probabilities.json`. These are measurements, with no automatic quality PASS. Loopback results verify plumbing only.\n");
    write_safe(out, "acceptance.csv", &csv)?;
    write_safe(out, "acceptance.md", &md)?;
    write_json(out, "probabilities.json", &json!(distributions))?;
    write_json(out, "summary.json", &summary)
}

fn summary(rows: &[Row], fake: bool) -> Value {
    let large: Vec<_> = rows
        .iter()
        .filter(|row| row.candidate_count == 254)
        .collect();
    let mut elapsed: Vec<_> = large.iter().map(|row| row.decision_elapsed_ms).collect();
    elapsed.sort_unstable();
    let median = if elapsed.is_empty() {
        None
    } else {
        Some((elapsed[(elapsed.len() - 1) / 2] as f64 + elapsed[elapsed.len() / 2] as f64) / 2.0)
    };
    json!({
        "mode": if fake {"loopback_fake_only"} else {"gemini_rest"}, "threshold":0.8,
        "count":rows.len(), "duplicate":rows.iter().filter(|r| r.expected == "duplicate").count(),
        "new":rows.iter().filter(|r| r.expected == "new").count(),
        "answer":rows.iter().filter(|r| r.result == "Answer").count(),
        "fallback":rows.iter().filter(|r| r.action == "fallback").count(),
        "duplicate_fallback":rows.iter().filter(|r| r.expected == "duplicate" && r.action == "fallback").count(),
        "new_fallback":rows.iter().filter(|r| r.expected == "new" && r.action == "fallback").count(),
        "duplicate_creation_including_fallback":rows.iter().filter(|r| r.expected == "duplicate" && r.action != "409").count(),
        "false_positive":rows.iter().filter(|r| r.false_positive).count(),
        "false_negative":rows.iter().filter(|r| r.false_negative).count(),
        "wrong_candidate":rows.iter().filter(|r| r.wrong_candidate).count(),
        "correct_new":rows.iter().filter(|r| r.expected == "new" && r.action == "create").count(),
        "large": {"count":large.len(), "answer":large.iter().filter(|r| r.result == "Answer").count(),
            "timeout":large.iter().filter(|r| r.reason == "timeout").count(),
            "invalid_response":large.iter().filter(|r| r.reason == "invalid_response").count(),
            "elapsed_median_ms":median, "elapsed_max_ms":elapsed.last()}
    })
}

pub(super) fn verify_self_test(out: &Path, rows: &[Row], requests: usize) -> Result<()> {
    for value in [
        super::SENTINEL,
        "Authorization: Bearer fake",
        "x-goog-api-key: fake",
    ] {
        if write_safe(out, "rejected-output.txt", value) != Err("unsafe_output_rejected")
            || out.join("rejected-output.txt").exists()
        {
            return Err("self_test_output_guard_failed");
        }
    }
    let count = rows.len();
    if count != 24
        || rows.iter().any(|row| row.deadline_ms != Some(5000))
        || requests != count + 1
        || rows
            .iter()
            .filter(|r| {
                r.max_non_none == Some(0.8) && r.action == "409" && r.confidence == Some(0.1)
            })
            .count()
            != 4
        || rows
            .iter()
            .filter(|r| {
                r.max_non_none == Some(0.799999)
                    && r.action == "create"
                    && r.confidence == Some(0.999)
            })
            .count()
            != 4
        || rows
            .iter()
            .filter(|r| {
                r.reason == "invalid_response" && r.action == "fallback" && r.input_tokens.is_none()
            })
            .count()
            != 4
        || rows
            .iter()
            .filter(|r| r.reason == "authentication" && r.action == "fallback")
            .count()
            != 4
    {
        return Err("self_test_consumer_or_boundary_failed");
    }
    for name in [
        "raw-rest-body.json",
        "comment_shape.json",
        "capture-manifest.json",
        "acceptance.csv",
        "acceptance.md",
        "probabilities.json",
        "summary.json",
    ] {
        let contents =
            std::fs::read_to_string(out.join(name)).map_err(|_| "self_test_missing_output")?;
        if contents.contains(super::SENTINEL) || contents.contains("provider_debug") {
            return Err("self_test_secret_leak");
        }
    }
    let fixture: Value = serde_json::from_slice(
        &std::fs::read(out.join("comment_shape.json")).map_err(|_| "self_test_missing_fixture")?,
    )
    .map_err(|_| "self_test_invalid_fixture")?;
    let original: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/decision_gate/comment_shape.json"
    ))
    .map_err(|_| "self_test_original_fixture")?;
    for name in ["choice", "noul", "score", "choice_255"] {
        if fixture[name]["answers"] != original[name]["answers"] {
            return Err("self_test_fixture_mapping_failed");
        }
    }
    Ok(())
}
