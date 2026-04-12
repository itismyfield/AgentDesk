//! DoD (Definition of Done) checklist mirroring from/to GitHub issues.

#![allow(dead_code)]

/// A single checklist item parsed from markdown.
#[derive(Debug, Clone, PartialEq)]
pub struct DodItem {
    pub text: String,
    pub checked: bool,
}

/// Parse the DoD section from a GitHub issue body.
///
/// Looks for a section headed with `## DoD` or `## Definition of Done` and
/// extracts checkbox items (`- [ ] ...` / `- [x] ...`).
pub fn parse_dod_from_body(body: &str) -> Vec<DodItem> {
    let mut items = Vec::new();
    let mut in_dod_section = false;

    for line in body.lines() {
        let trimmed = line.trim();

        // Check for the DoD section header
        if trimmed.starts_with("## ") {
            let header = trimmed[3..].trim().to_lowercase();
            if header == "dod" || header == "definition of done" {
                in_dod_section = true;
                continue;
            } else if in_dod_section {
                // Hit a different ## section, stop
                break;
            }
        }

        if !in_dod_section {
            continue;
        }

        // Parse checkbox items
        if let Some(item) = parse_checkbox_line(trimmed) {
            items.push(item);
        }
    }

    items
}

/// Parse a single checkbox line. Supports:
/// - `- [ ] text`
/// - `- [x] text`
/// - `- [X] text`
/// - `* [ ] text`
/// - `* [x] text`
fn parse_checkbox_line(line: &str) -> Option<DodItem> {
    let line = line.trim();

    // Must start with - or *
    let rest = if line.starts_with("- ") {
        &line[2..]
    } else if line.starts_with("* ") {
        &line[2..]
    } else {
        return None;
    };

    let rest = rest.trim();

    if rest.starts_with("[ ] ") {
        Some(DodItem {
            text: rest[4..].trim().to_string(),
            checked: false,
        })
    } else if rest.starts_with("[x] ") || rest.starts_with("[X] ") {
        Some(DodItem {
            text: rest[4..].trim().to_string(),
            checked: true,
        })
    } else {
        None
    }
}

/// Render DoD items back to markdown checkbox format.
pub fn render_dod_markdown(items: &[DodItem]) -> String {
    items
        .iter()
        .map(|item| {
            let checkbox = if item.checked { "[x]" } else { "[ ]" };
            format!("- {checkbox} {}", item.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fetch an issue body via `gh` CLI and extract DoD items.
pub fn mirror_dod_from_issue(repo: &str, issue_number: i64) -> Result<Vec<DodItem>, String> {
    mirror_dod_from_issue_with(super::adapter(), repo, issue_number)
}

pub(crate) fn mirror_dod_from_issue_with(
    adapter: &dyn super::GitHubAdapter,
    repo: &str,
    issue_number: i64,
) -> Result<Vec<DodItem>, String> {
    let output = adapter.run(&[
        "issue",
        "view",
        &issue_number.to_string(),
        "--repo",
        repo,
        "--json",
        "body",
    ])?;

    let parsed: serde_json::Value =
        serde_json::from_str(&output).map_err(|e| format!("parse: {e}"))?;

    let body = parsed["body"].as_str().unwrap_or("");
    Ok(parse_dod_from_body(body))
}

/// Update the DoD section of an issue body on GitHub via `gh issue edit`.
/// This replaces the existing DoD section with the new checklist.
pub fn update_dod_on_github(
    repo: &str,
    issue_number: i64,
    checklist: &[DodItem],
) -> Result<(), String> {
    update_dod_on_github_with(super::adapter(), repo, issue_number, checklist)
}

pub(crate) fn update_dod_on_github_with(
    adapter: &dyn super::GitHubAdapter,
    repo: &str,
    issue_number: i64,
    checklist: &[DodItem],
) -> Result<(), String> {
    // First, fetch the current body
    let output = adapter.run(&[
        "issue",
        "view",
        &issue_number.to_string(),
        "--repo",
        repo,
        "--json",
        "body",
    ])?;

    let parsed: serde_json::Value =
        serde_json::from_str(&output).map_err(|e| format!("parse: {e}"))?;
    let current_body = parsed["body"].as_str().unwrap_or("");

    let new_body = replace_dod_section(current_body, checklist);

    adapter.run(&[
        "issue",
        "edit",
        &issue_number.to_string(),
        "--repo",
        repo,
        "--body",
        &new_body,
    ])?;

    Ok(())
}

/// Replace the DoD section in a body string with updated checklist items.
fn replace_dod_section(body: &str, items: &[DodItem]) -> String {
    let mut result = String::new();
    let mut in_dod_section = false;
    let mut dod_replaced = false;
    let mut had_dod = false;

    for line in body.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("## ") {
            let header = trimmed[3..].trim().to_lowercase();
            if header == "dod" || header == "definition of done" {
                in_dod_section = true;
                had_dod = true;
                // Write the header and new items
                result.push_str(line);
                result.push('\n');
                result.push_str(&render_dod_markdown(items));
                result.push('\n');
                dod_replaced = true;
                continue;
            } else if in_dod_section {
                in_dod_section = false;
            }
        }

        if in_dod_section {
            // Skip old DoD content
            continue;
        }

        result.push_str(line);
        result.push('\n');
    }

    // If no DoD section existed, append one
    if !had_dod && !items.is_empty() {
        if !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str("\n## DoD\n");
        result.push_str(&render_dod_markdown(items));
        result.push('\n');
    }

    let _ = dod_replaced; // suppress unused variable warning
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::test_utils::RecordingAdapter;

    #[test]
    fn parse_dod_simple() {
        let body = r#"
## Description
Some description here.

## DoD
- [ ] Unit tests pass
- [x] Code reviewed
- [ ] Deployed to staging

## Notes
Other notes
"#;
        let items = parse_dod_from_body(body);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].text, "Unit tests pass");
        assert!(!items[0].checked);
        assert_eq!(items[1].text, "Code reviewed");
        assert!(items[1].checked);
        assert_eq!(items[2].text, "Deployed to staging");
        assert!(!items[2].checked);
    }

    #[test]
    fn parse_dod_definition_of_done_header() {
        let body = r#"
## Definition of Done
- [ ] Build passes
- [X] Reviewed
"#;
        let items = parse_dod_from_body(body);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "Build passes");
        assert!(items[1].checked);
    }

    #[test]
    fn parse_dod_no_section() {
        let body = "## Description\nJust a description";
        let items = parse_dod_from_body(body);
        assert!(items.is_empty());
    }

    #[test]
    fn parse_dod_empty_body() {
        let items = parse_dod_from_body("");
        assert!(items.is_empty());
    }

    #[test]
    fn parse_dod_with_asterisk_bullets() {
        let body = "## DoD\n* [ ] Item one\n* [x] Item two\n";
        let items = parse_dod_from_body(body);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "Item one");
        assert!(!items[0].checked);
        assert_eq!(items[1].text, "Item two");
        assert!(items[1].checked);
    }

    #[test]
    fn render_dod_markdown_output() {
        let items = vec![
            DodItem {
                text: "Tests pass".to_string(),
                checked: false,
            },
            DodItem {
                text: "Reviewed".to_string(),
                checked: true,
            },
        ];

        let md = render_dod_markdown(&items);
        assert_eq!(md, "- [ ] Tests pass\n- [x] Reviewed");
    }

    #[test]
    fn replace_dod_section_updates_existing() {
        let body = "## Intro\nHello\n\n## DoD\n- [ ] Old item\n\n## End\nBye\n";
        let new_items = vec![DodItem {
            text: "New item".to_string(),
            checked: true,
        }];

        let result = replace_dod_section(body, &new_items);
        assert!(result.contains("- [x] New item"));
        assert!(!result.contains("Old item"));
        assert!(result.contains("## End"));
        assert!(result.contains("## Intro"));
    }

    #[test]
    fn replace_dod_section_appends_when_missing() {
        let body = "## Intro\nHello\n";
        let items = vec![DodItem {
            text: "New check".to_string(),
            checked: false,
        }];

        let result = replace_dod_section(body, &items);
        assert!(result.contains("## DoD"));
        assert!(result.contains("- [ ] New check"));
    }

    #[test]
    fn mirror_dod_routes_through_adapter_interface() {
        let adapter = RecordingAdapter::with_sync_responses(vec![Ok(
            "{\"body\":\"## DoD\\n- [ ] Test coverage\\n- [x] Reviewed\\n\"}".to_string(),
        )]);

        let items = mirror_dod_from_issue_with(&adapter, "owner/repo", 42).unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "Test coverage");
        assert!(!items[0].checked);
        assert_eq!(
            adapter.calls(),
            vec![vec![
                "issue".to_string(),
                "view".to_string(),
                "42".to_string(),
                "--repo".to_string(),
                "owner/repo".to_string(),
                "--json".to_string(),
                "body".to_string(),
            ]]
        );
    }

    #[test]
    fn update_dod_routes_through_adapter_interface() {
        let adapter = RecordingAdapter::with_sync_responses(vec![
            Ok("{\"body\":\"## DoD\\n- [ ] Old item\\n\\n## Notes\\nKeep me\\n\"}".to_string()),
            Ok(String::new()),
        ]);

        update_dod_on_github_with(
            &adapter,
            "owner/repo",
            7,
            &[DodItem {
                text: "New item".to_string(),
                checked: true,
            }],
        )
        .unwrap();

        let calls = adapter.calls();
        assert_eq!(
            calls[0],
            vec![
                "issue".to_string(),
                "view".to_string(),
                "7".to_string(),
                "--repo".to_string(),
                "owner/repo".to_string(),
                "--json".to_string(),
                "body".to_string(),
            ]
        );
        assert_eq!(
            calls[1][0..5],
            ["issue", "edit", "7", "--repo", "owner/repo"]
        );
        assert_eq!(calls[1][5], "--body");
        assert_eq!(
            calls[1][6],
            "## DoD\n- [x] New item\n## Notes\nKeep me\n".to_string()
        );
    }
}
