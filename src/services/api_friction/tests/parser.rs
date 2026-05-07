use super::super::*;

#[test]
fn extract_api_friction_reports_strips_valid_markers() {
    let input = "검증 완료\nAPI_FRICTION: {\"endpoint\":\"/api/docs/kanban\",\"friction_type\":\"docs-bypass\",\"summary\":\"카테고리를 모르고 시행착오\",\"workaround\":\"sqlite3\",\"keywords\":[\"kanban\"]}\n후속 작업 없음";
    let extracted = extract_api_friction_reports(input);

    assert_eq!(extracted.reports.len(), 1);
    assert!(extracted.cleaned_response.contains("검증 완료"));
    assert!(extracted.cleaned_response.contains("후속 작업 없음"));
    assert!(!extracted.cleaned_response.contains("API_FRICTION"));
    assert_eq!(extracted.reports[0].endpoint, "/api/docs/kanban");
    assert_eq!(extracted.reports[0].friction_type, "docs-bypass");
    assert!(
        extracted.reports[0]
            .keywords
            .iter()
            .any(|value| value == "sqlite3")
    );
}

#[test]
fn extract_api_friction_reports_keeps_invalid_markers_visible() {
    let input = "API_FRICTION: {not-json}";
    let extracted = extract_api_friction_reports(input);

    assert!(extracted.reports.is_empty());
    assert_eq!(extracted.cleaned_response, input);
    assert_eq!(extracted.parse_errors.len(), 1);
}

#[test]
fn extract_api_friction_reports_preserves_aliases_truncation_and_keyword_cleanup() {
    let long_endpoint = format!("/api/docs/{}", "x".repeat(260));
    let input = format!(
        "done\nAPI_FRICTION: {{\"surface\":\"{long_endpoint}\",\"frictionType\":\" docs gap \",\"summary\":\"  missing   facade docs  \",\"workaround_method\":\" sqlite   fallback \",\"suggestedFix\":\" publish docs \",\"docsCategory\":\" dispatches \",\"keywords\":[\" dispatches \",\"sqlite fallback\",\"dispatches\"]}}\n\n\nnext"
    );
    let extracted = extract_api_friction_reports(&input);

    assert!(extracted.parse_errors.is_empty());
    assert_eq!(extracted.cleaned_response, "done\n\nnext");
    assert_eq!(extracted.reports.len(), 1);

    let report = &extracted.reports[0];
    assert_eq!(report.endpoint.chars().count(), 243);
    assert!(report.endpoint.ends_with("..."));
    assert_eq!(report.friction_type, "docs gap");
    assert_eq!(report.summary, "missing facade docs");
    assert_eq!(report.workaround.as_deref(), Some("sqlite fallback"));
    assert_eq!(report.suggested_fix.as_deref(), Some("publish docs"));
    assert_eq!(report.docs_category.as_deref(), Some("dispatches"));
    let expected_endpoint_keyword = format!(
        "{}...",
        report.endpoint.chars().take(80).collect::<String>()
    );
    assert_eq!(
        report.keywords,
        vec![
            expected_endpoint_keyword,
            "dispatches".to_string(),
            "docs gap".to_string(),
            "sqlite fallback".to_string(),
        ]
    );
}
