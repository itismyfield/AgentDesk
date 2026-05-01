use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedIssueSpec {
    pub acceptance_criteria: Vec<String>,
    pub test_plan: Vec<String>,
    pub definition_of_done: Vec<String>,
    pub required_phases: Vec<String>,
    pub validation_errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IssueSpec {
    pub issue_id: String,
    pub card_id: Option<String>,
    pub repo_id: Option<String>,
    pub issue_number: Option<i32>,
    pub head_sha: Option<String>,
    pub acceptance_criteria: Value,
    pub test_plan: Value,
    pub definition_of_done: Value,
    pub required_phases: Value,
    pub validation_errors: Value,
    pub source_body_sha: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueSpecUpsertRequest {
    pub issue_id: String,
    pub body: String,
    #[serde(default)]
    pub card_id: Option<String>,
    #[serde(default)]
    pub repo_id: Option<String>,
    #[serde(default)]
    pub issue_number: Option<i32>,
    #[serde(default)]
    pub head_sha: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueSpecListQuery {
    #[serde(default)]
    pub issue_id: Option<String>,
    #[serde(default)]
    pub card_id: Option<String>,
    #[serde(default)]
    pub repo_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn upsert_issue_spec(
    pool: &PgPool,
    request: &IssueSpecUpsertRequest,
) -> Result<IssueSpec, String> {
    let issue_id = normalize_required("issue_id", &request.issue_id)?;
    let parsed = parse_issue_spec_markdown(&request.body);
    let source_body_sha = sha256_hex(&request.body);
    let row = sqlx::query(
        r#"
        INSERT INTO issue_specs (
            issue_id, card_id, repo_id, issue_number, head_sha, acceptance_criteria,
            test_plan, definition_of_done, required_phases, validation_errors,
            source_body_sha, created_at, updated_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NOW(), NOW())
        ON CONFLICT (issue_id) DO UPDATE SET
            card_id = COALESCE(EXCLUDED.card_id, issue_specs.card_id),
            repo_id = COALESCE(EXCLUDED.repo_id, issue_specs.repo_id),
            issue_number = COALESCE(EXCLUDED.issue_number, issue_specs.issue_number),
            head_sha = COALESCE(EXCLUDED.head_sha, issue_specs.head_sha),
            acceptance_criteria = EXCLUDED.acceptance_criteria,
            test_plan = EXCLUDED.test_plan,
            definition_of_done = EXCLUDED.definition_of_done,
            required_phases = EXCLUDED.required_phases,
            validation_errors = EXCLUDED.validation_errors,
            source_body_sha = EXCLUDED.source_body_sha,
            updated_at = NOW()
        RETURNING issue_id, card_id, repo_id, issue_number, head_sha,
                  acceptance_criteria, test_plan, definition_of_done,
                  required_phases, validation_errors, source_body_sha
        "#,
    )
    .bind(issue_id)
    .bind(clean_optional(request.card_id.as_deref()))
    .bind(clean_optional(request.repo_id.as_deref()))
    .bind(request.issue_number)
    .bind(clean_optional(request.head_sha.as_deref()))
    .bind(json!(parsed.acceptance_criteria))
    .bind(json!(parsed.test_plan))
    .bind(json!(parsed.definition_of_done))
    .bind(json!(parsed.required_phases))
    .bind(json!(parsed.validation_errors))
    .bind(source_body_sha)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("upsert issue spec: {error}"))?;

    issue_spec_from_row(row)
}

pub async fn list_issue_specs(
    pool: &PgPool,
    query: &IssueSpecListQuery,
) -> Result<Vec<IssueSpec>, String> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let rows = sqlx::query(
        r#"
        SELECT issue_id, card_id, repo_id, issue_number, head_sha,
               acceptance_criteria, test_plan, definition_of_done,
               required_phases, validation_errors, source_body_sha
          FROM issue_specs
         WHERE ($1::TEXT IS NULL OR issue_id = $1)
           AND ($2::TEXT IS NULL OR card_id = $2)
           AND ($3::TEXT IS NULL OR repo_id = $3)
         ORDER BY updated_at DESC
         LIMIT $4
        "#,
    )
    .bind(clean_optional(query.issue_id.as_deref()))
    .bind(clean_optional(query.card_id.as_deref()))
    .bind(clean_optional(query.repo_id.as_deref()))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("list issue specs: {error}"))?;

    rows.into_iter().map(issue_spec_from_row).collect()
}

pub fn parse_issue_spec_markdown(body: &str) -> ParsedIssueSpec {
    let mut section = IssueSpecSection::Other;
    let mut acceptance_criteria = Vec::new();
    let mut test_plan = Vec::new();
    let mut definition_of_done = Vec::new();
    let mut required_phases = Vec::new();

    for line in body.lines() {
        if let Some(next) = IssueSpecSection::from_heading(line) {
            section = next;
            continue;
        }
        let Some(item) = clean_markdown_list_item(line) else {
            continue;
        };
        match section {
            IssueSpecSection::AcceptanceCriteria => acceptance_criteria.push(item),
            IssueSpecSection::TestPlan => test_plan.push(item),
            IssueSpecSection::DefinitionOfDone => definition_of_done.push(item),
            IssueSpecSection::RequiredPhases => required_phases.push(normalize_phase_key(&item)),
            IssueSpecSection::Other => {}
        }
    }

    required_phases.retain(|phase| !phase.is_empty());
    required_phases.sort();
    required_phases.dedup();

    let mut validation_errors = Vec::new();
    if acceptance_criteria.is_empty() {
        validation_errors.push("missing acceptance criteria".to_string());
    }
    if test_plan.is_empty() {
        validation_errors.push("missing test plan".to_string());
    }
    if definition_of_done.is_empty() {
        validation_errors.push("missing definition of done".to_string());
    }

    ParsedIssueSpec {
        acceptance_criteria,
        test_plan,
        definition_of_done,
        required_phases,
        validation_errors,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IssueSpecSection {
    AcceptanceCriteria,
    TestPlan,
    DefinitionOfDone,
    RequiredPhases,
    Other,
}

impl IssueSpecSection {
    fn from_heading(line: &str) -> Option<Self> {
        let normalized = line
            .trim()
            .trim_start_matches('#')
            .trim()
            .to_ascii_lowercase();
        match normalized.as_str() {
            "acceptance criteria" | "acceptance" | "ac" | "수용 기준" | "완료 기준" => {
                Some(Self::AcceptanceCriteria)
            }
            "test plan" | "tests" | "검증 계획" | "테스트 계획" => Some(Self::TestPlan),
            "definition of done" | "dod" | "done" | "완료 정의" => Some(Self::DefinitionOfDone),
            "required phases" | "phase plan" | "phases" | "필수 phase" | "필수 단계" => {
                Some(Self::RequiredPhases)
            }
            _ => None,
        }
    }
}

fn clean_markdown_list_item(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let item = trimmed
        .strip_prefix("- [ ] ")
        .or_else(|| trimmed.strip_prefix("- [x] "))
        .or_else(|| trimmed.strip_prefix("- [X] "))
        .or_else(|| trimmed.strip_prefix("- "))
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))?
        .trim();
    (!item.is_empty()).then(|| item.to_string())
}

fn normalize_phase_key(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn issue_spec_from_row(row: sqlx::postgres::PgRow) -> Result<IssueSpec, String> {
    Ok(IssueSpec {
        issue_id: row.get("issue_id"),
        card_id: row.get("card_id"),
        repo_id: row.get("repo_id"),
        issue_number: row.get("issue_number"),
        head_sha: row.get("head_sha"),
        acceptance_criteria: row.get("acceptance_criteria"),
        test_plan: row.get("test_plan"),
        definition_of_done: row.get("definition_of_done"),
        required_phases: row.get("required_phases"),
        validation_errors: row.get("validation_errors"),
        source_body_sha: row.get("source_body_sha"),
    })
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn normalize_required(field: &str, value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{field} is required"));
    }
    if trimmed.len() > 256 {
        return Err(format!("{field} is too long"));
    }
    Ok(trimmed.to_string())
}

fn clean_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_structured_issue_sections() {
        let spec = parse_issue_spec_markdown(
            r#"
## Acceptance Criteria
- Dispatch routes to a capable worker
- [ ] Expired workers are rejected

## Test Plan
- Run heartbeat expiry regression

## Definition of Done
- Evidence is recorded

## Required Phases
- Unreal Smoke
- unreal-smoke
"#,
        );

        assert_eq!(spec.acceptance_criteria.len(), 2);
        assert_eq!(spec.test_plan, vec!["Run heartbeat expiry regression"]);
        assert_eq!(spec.definition_of_done, vec!["Evidence is recorded"]);
        assert_eq!(spec.required_phases, vec!["unreal-smoke"]);
        assert!(spec.validation_errors.is_empty());
    }

    #[test]
    fn reports_missing_required_sections() {
        let spec = parse_issue_spec_markdown("## Acceptance Criteria\n- One thing");
        assert_eq!(spec.acceptance_criteria, vec!["One thing"]);
        assert_eq!(
            spec.validation_errors,
            vec![
                "missing test plan".to_string(),
                "missing definition of done".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn upsert_issue_spec_persists_parsed_contract() {
        let base = postgres_base_database_url();
        let database_name = format!("agentdesk_issue_specs_{}", uuid::Uuid::new_v4().simple());
        let admin_url = format!("{base}/postgres");
        crate::db::postgres::create_test_database(&admin_url, &database_name, "issue_specs tests")
            .await
            .expect("create issue_specs test database");
        let database_url = format!("{base}/{database_name}");
        let pool =
            crate::db::postgres::connect_test_pool_and_migrate(&database_url, "issue_specs tests")
                .await
                .expect("connect + migrate issue_specs test database");

        let spec = upsert_issue_spec(
            &pool,
            &IssueSpecUpsertRequest {
                issue_id: "881".to_string(),
                card_id: Some("card-881".to_string()),
                repo_id: Some("itismyfield/AgentDesk".to_string()),
                issue_number: Some(881),
                head_sha: Some("abc123".to_string()),
                body: r#"
## Acceptance Criteria
- Phase evidence is persisted

## Test Plan
- Run phase evidence regression

## Definition of Done
- Merge gate consumes evidence

## Required Phases
- Unreal Smoke
"#
                .to_string(),
            },
        )
        .await
        .unwrap();

        assert_eq!(spec.issue_id, "881");
        assert_eq!(spec.required_phases, json!(["unreal-smoke"]));
        assert_eq!(spec.validation_errors, json!([]));
        assert!(spec.source_body_sha.is_some());

        pool.close().await;
        crate::db::postgres::drop_test_database(&admin_url, &database_name, "issue_specs tests")
            .await
            .expect("drop issue_specs test database");
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "postgres".to_string());
        format!("postgres://{user}@127.0.0.1:5432")
    }
}
