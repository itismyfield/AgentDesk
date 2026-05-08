fn github_path_segments(raw: &str) -> Option<Vec<&str>> {
    let trimmed = raw.trim().trim_end_matches(".git").trim_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    let path = if let Some(path) = trimmed.strip_prefix("git@github.com:") {
        path
    } else if let Some(path) = trimmed.strip_prefix("ssh://git@github.com/") {
        path
    } else if trimmed.starts_with("https://github.com/")
        || trimmed.starts_with("http://github.com/")
    {
        let index = trimmed.rfind("github.com/")?;
        &trimmed[index + "github.com/".len()..]
    } else {
        trimmed
    };

    let segments = path
        .trim_matches('/')
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    (!segments.is_empty()).then_some(segments)
}

fn valid_github_owner_or_repo_segment(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.contains(':')
        && !segment.chars().any(char::is_whitespace)
        && segment != "."
        && segment != ".."
}

pub(crate) fn normalize_github_repo_id(raw: &str) -> Option<String> {
    let segments = github_path_segments(raw)?;
    let owner = segments.first()?.trim();
    let repo = segments.get(1)?.trim().trim_end_matches(".git");
    if !valid_github_owner_or_repo_segment(owner) || !valid_github_owner_or_repo_segment(repo) {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

pub(crate) fn normalize_optional_github_repo_id(raw: Option<String>) -> Option<String> {
    let trimmed = raw.map(|value| value.trim().to_string())?;
    if trimmed.is_empty() {
        return None;
    }
    normalize_github_repo_id(&trimmed).or(Some(trimmed))
}

fn issue_number_from_segments(segments: &[&str]) -> Option<i64> {
    segments.windows(2).find_map(|window| {
        if window[0] != "issues" {
            return None;
        }
        let issue_number = window[1].parse::<i64>().ok()?;
        (issue_number > 0).then_some(issue_number)
    })
}

pub(crate) fn build_github_issue_url(repo_id: &str, issue_number: i64) -> Option<String> {
    if issue_number <= 0 {
        return None;
    }
    let repo_id = normalize_github_repo_id(repo_id)?;
    Some(format!(
        "https://github.com/{repo_id}/issues/{issue_number}"
    ))
}

pub(crate) fn normalize_github_issue_url(raw: &str) -> Option<String> {
    let segments = github_path_segments(raw)?;
    let repo_id = normalize_github_repo_id(raw)?;
    let issue_number = issue_number_from_segments(&segments)?;
    Some(format!(
        "https://github.com/{repo_id}/issues/{issue_number}"
    ))
}

pub(crate) fn normalize_optional_github_issue_url(
    raw_issue_url: Option<String>,
    repo_id: Option<&str>,
    issue_number: Option<i64>,
) -> Option<String> {
    let trimmed_issue_url = raw_issue_url
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    if let Some(canonical) = trimmed_issue_url
        .as_deref()
        .and_then(normalize_github_issue_url)
    {
        return Some(canonical);
    }

    if let Some(canonical) = repo_id
        .zip(issue_number)
        .and_then(|(repo_id, issue_number)| build_github_issue_url(repo_id, issue_number))
    {
        return Some(canonical);
    }

    trimmed_issue_url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_github_repo_id_accepts_repo_and_url_forms() {
        assert_eq!(
            normalize_github_repo_id("itismyfield/AgentDesk").as_deref(),
            Some("itismyfield/AgentDesk")
        );
        assert_eq!(
            normalize_github_repo_id("https://github.com/itismyfield/AgentDesk/issues/1929")
                .as_deref(),
            Some("itismyfield/AgentDesk")
        );
        assert_eq!(
            normalize_github_repo_id("git@github.com:itismyfield/AgentDesk.git").as_deref(),
            Some("itismyfield/AgentDesk")
        );
    }

    #[test]
    fn normalize_github_issue_url_repairs_duplicated_github_prefix() {
        assert_eq!(
            normalize_github_issue_url(
                "https://github.com/https://github.com/itismyfield/AgentDesk/issues/1830/issues/1830",
            )
            .as_deref(),
            Some("https://github.com/itismyfield/AgentDesk/issues/1830")
        );
    }

    #[test]
    fn normalize_optional_github_issue_url_builds_from_repo_and_number() {
        assert_eq!(
            normalize_optional_github_issue_url(
                None,
                Some("https://github.com/itismyfield/AgentDesk"),
                Some(1929),
            )
            .as_deref(),
            Some("https://github.com/itismyfield/AgentDesk/issues/1929")
        );
    }
}
