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
