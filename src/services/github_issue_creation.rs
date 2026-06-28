use serde::Serialize;
use sqlx::PgPool;
use std::future::Future;
use std::pin::Pin;

use crate::db::kanban::{IssueCardUpsert, upsert_card_from_issue_pg};
use crate::github::{self, CreatedIssue};
use crate::services::issue_announcements::{
    IssueAnnouncementCreate, IssueAnnouncementCreated, IssueCompletionEvent, IssueCompletionKind,
    complete_issue_announcement_pg, create_issue_announcement_pg,
};

type IssueCreatorFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CreatedIssue, String>> + Send + 'a>>;

trait IssueCreator: Send + Sync {
    fn gh_available(&self) -> bool;

    fn create_issue<'a>(&'a self, request: &'a GitHubIssueCreateRequest) -> IssueCreatorFuture<'a>;
}

#[derive(Debug, Default)]
struct GhCliIssueCreator;

impl IssueCreator for GhCliIssueCreator {
    fn gh_available(&self) -> bool {
        github::gh_available()
    }

    fn create_issue<'a>(&'a self, request: &'a GitHubIssueCreateRequest) -> IssueCreatorFuture<'a> {
        Box::pin(async move {
            if request.labels.is_empty() {
                github::create_issue(&request.repo, &request.title, &request.body).await
            } else {
                github::create_issue_with_labels(
                    &request.repo,
                    &request.title,
                    &request.body,
                    &request.labels,
                )
                .await
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueCreationMode {
    Standard,
    GithubOnly,
}

impl IssueCreationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::GithubOnly => "github_only",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubIssueCreateRequest {
    pub origin: String,
    pub mode: IssueCreationMode,
    pub repo: String,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub kanban: KanbanCardSync,
    pub announcement: IssueAnnouncementSync,
}

impl GitHubIssueCreateRequest {
    pub fn new(
        origin: impl Into<String>,
        repo: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            origin: origin.into(),
            mode: IssueCreationMode::Standard,
            repo: repo.into(),
            title: title.into(),
            body: body.into(),
            labels: Vec::new(),
            kanban: KanbanCardSync::disabled("kanban sync not configured"),
            announcement: IssueAnnouncementSync::disabled("announcement sync not configured"),
        }
    }

    pub fn github_only(
        origin: impl Into<String>,
        repo: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        Self {
            origin: origin.into(),
            mode: IssueCreationMode::GithubOnly,
            repo: repo.into(),
            title: title.into(),
            body: body.into(),
            labels: Vec::new(),
            kanban: KanbanCardSync::disabled(reason.clone()),
            announcement: IssueAnnouncementSync::disabled(reason),
        }
    }

    pub fn with_labels(mut self, labels: Vec<String>) -> Self {
        self.labels = normalize_labels(&labels);
        self
    }

    pub fn with_kanban(mut self, kanban: KanbanCardSync) -> Self {
        self.kanban = kanban;
        self
    }

    pub fn with_announcement(mut self, announcement: IssueAnnouncementSync) -> Self {
        self.announcement = announcement;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KanbanCardSync {
    Enabled(KanbanCardSyncOptions),
    Disabled { reason: String },
}

impl KanbanCardSync {
    pub fn enabled(options: KanbanCardSyncOptions) -> Self {
        Self::Enabled(options)
    }

    pub fn disabled(reason: impl Into<String>) -> Self {
        Self::Disabled {
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KanbanCardSyncOptions {
    pub agent_id: Option<String>,
    pub metadata_json: Option<String>,
    pub status_on_create: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueAnnouncementSync {
    Enabled(IssueAnnouncementSyncOptions),
    Disabled { reason: String },
}

impl IssueAnnouncementSync {
    pub fn enabled(options: IssueAnnouncementSyncOptions) -> Self {
        Self::Enabled(options)
    }

    pub fn disabled(reason: impl Into<String>) -> Self {
        Self::Disabled {
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueAnnouncementSyncOptions {
    pub agent_id: Option<String>,
    pub announcement_channel_id: Option<String>,
    pub complete_if_closed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct KanbanCardSyncOutcome {
    pub enabled: bool,
    pub card_id: Option<String>,
    pub created: Option<bool>,
    pub error: Option<String>,
    pub skipped_reason: Option<String>,
}

impl KanbanCardSyncOutcome {
    fn synced(created: crate::db::kanban::IssueCardUpsertResult) -> Self {
        Self {
            enabled: true,
            card_id: Some(created.card_id),
            created: Some(created.created),
            error: None,
            skipped_reason: None,
        }
    }

    fn failed(error: impl Into<String>) -> Self {
        Self {
            enabled: true,
            card_id: None,
            created: None,
            error: Some(error.into()),
            skipped_reason: None,
        }
    }

    fn skipped(reason: impl Into<String>) -> Self {
        Self {
            enabled: false,
            card_id: None,
            created: None,
            error: None,
            skipped_reason: Some(reason.into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IssueAnnouncementSyncOutcome {
    pub enabled: bool,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    pub error: Option<String>,
    pub skipped_reason: Option<String>,
}

impl IssueAnnouncementSyncOutcome {
    fn synced(created: IssueAnnouncementCreated) -> Self {
        Self {
            enabled: true,
            channel_id: Some(created.channel_id),
            message_id: Some(created.message_id),
            error: None,
            skipped_reason: None,
        }
    }

    fn failed(error: impl Into<String>) -> Self {
        Self {
            enabled: true,
            channel_id: None,
            message_id: None,
            error: Some(error.into()),
            skipped_reason: None,
        }
    }

    fn skipped(reason: impl Into<String>) -> Self {
        Self {
            enabled: false,
            channel_id: None,
            message_id: None,
            error: None,
            skipped_reason: Some(reason.into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubIssueCreationResult {
    pub origin: String,
    pub mode: IssueCreationMode,
    pub issue: CreatedIssue,
    pub applied_labels: Vec<String>,
    pub kanban: KanbanCardSyncOutcome,
    pub announcement: IssueAnnouncementSyncOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueCreationError {
    GhUnavailable,
    GitHub(String),
}

impl std::fmt::Display for IssueCreationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GhUnavailable => f.write_str("gh CLI is not available on this system"),
            Self::GitHub(error) => write!(f, "gh issue create failed: {error}"),
        }
    }
}

impl std::error::Error for IssueCreationError {}

pub async fn create_github_issue_with_side_effects(
    pool: Option<&PgPool>,
    request: GitHubIssueCreateRequest,
) -> Result<GitHubIssueCreationResult, IssueCreationError> {
    let creator = GhCliIssueCreator;
    create_github_issue_with_creator(pool, request, &creator).await
}

async fn create_github_issue_with_creator(
    pool: Option<&PgPool>,
    mut request: GitHubIssueCreateRequest,
    creator: &dyn IssueCreator,
) -> Result<GitHubIssueCreationResult, IssueCreationError> {
    if !creator.gh_available() {
        return Err(IssueCreationError::GhUnavailable);
    }

    request.labels = normalize_labels(&request.labels);
    let issue = creator
        .create_issue(&request)
        .await
        .map_err(IssueCreationError::GitHub)?;

    let kanban = sync_kanban_card(pool, &request, &issue).await;
    let announcement = sync_issue_announcement(pool, &request, &issue).await;

    Ok(GitHubIssueCreationResult {
        origin: request.origin,
        mode: request.mode,
        issue,
        applied_labels: request.labels,
        kanban,
        announcement,
    })
}

async fn sync_kanban_card(
    pool: Option<&PgPool>,
    request: &GitHubIssueCreateRequest,
    issue: &CreatedIssue,
) -> KanbanCardSyncOutcome {
    let options = match &request.kanban {
        KanbanCardSync::Enabled(options) => options,
        KanbanCardSync::Disabled { reason } => {
            tracing::info!(
                "[issues] created GitHub-only issue {}#{} from {}: kanban sync disabled: {}",
                request.repo,
                issue.number,
                request.origin,
                reason
            );
            return KanbanCardSyncOutcome::skipped(reason.clone());
        }
    };

    let Some(pool) = pool else {
        return KanbanCardSyncOutcome::failed("postgres pool unavailable");
    };

    let assigned_agent_id = match resolve_known_agent_id_pg(pool, options.agent_id.as_deref()).await
    {
        Ok(agent_id) => agent_id,
        Err(error) => {
            tracing::error!(
                "[issues] created GitHub issue {}#{} but failed to resolve assignee: {}",
                request.repo,
                issue.number,
                error
            );
            return KanbanCardSyncOutcome::failed(error);
        }
    };

    match upsert_card_from_issue_pg(
        pool,
        IssueCardUpsert {
            repo_id: request.repo.clone(),
            issue_number: issue.number,
            issue_url: Some(issue.url.clone()),
            title: request.title.clone(),
            description: Some(request.body.clone()),
            priority: None,
            assigned_agent_id,
            metadata_json: options.metadata_json.clone(),
            status_on_create: options.status_on_create.clone(),
        },
    )
    .await
    {
        Ok(upserted) => KanbanCardSyncOutcome::synced(upserted),
        Err(error) => {
            tracing::error!(
                "[issues] created GitHub issue {}#{} but failed to sync kanban card: {}",
                request.repo,
                issue.number,
                error
            );
            KanbanCardSyncOutcome::failed(error)
        }
    }
}

async fn sync_issue_announcement(
    pool: Option<&PgPool>,
    request: &GitHubIssueCreateRequest,
    issue: &CreatedIssue,
) -> IssueAnnouncementSyncOutcome {
    let options = match &request.announcement {
        IssueAnnouncementSync::Enabled(options) => options,
        IssueAnnouncementSync::Disabled { reason } => {
            tracing::info!(
                "[issues] created GitHub-only issue {}#{} from {}: announcement sync disabled: {}",
                request.repo,
                issue.number,
                request.origin,
                reason
            );
            return IssueAnnouncementSyncOutcome::skipped(reason.clone());
        }
    };

    let Some(pool) = pool else {
        if options
            .agent_id
            .as_deref()
            .and_then(trim_non_empty)
            .is_some()
            || options
                .announcement_channel_id
                .as_deref()
                .and_then(trim_non_empty)
                .is_some()
        {
            return IssueAnnouncementSyncOutcome::failed(
                "postgres pool unavailable for issue announcement",
            );
        }
        return IssueAnnouncementSyncOutcome::skipped("no announcement target configured");
    };

    match create_issue_announcement_pg(
        pool,
        IssueAnnouncementCreate {
            repo: request.repo.clone(),
            issue_number: issue.number,
            issue_url: issue.url.clone(),
            title: request.title.clone(),
            agent_id: options.agent_id.as_deref().and_then(trim_non_empty),
            announcement_channel_id: options
                .announcement_channel_id
                .as_deref()
                .and_then(trim_non_empty),
        },
    )
    .await
    {
        Ok(Some(announcement)) => {
            if options.complete_if_closed
                && matches!(
                    github::issue_state(&request.repo, issue.number).as_deref(),
                    Ok("CLOSED")
                )
            {
                if let Err(error) = complete_issue_announcement_pg(
                    pool,
                    IssueCompletionEvent {
                        repo: request.repo.clone(),
                        issue_number: issue.number,
                        title: Some(request.title.clone()),
                        kind: IssueCompletionKind::Closed,
                        pr_number: None,
                        pr_url: None,
                    },
                )
                .await
                {
                    tracing::warn!(
                        "[issues] immediate completion announcement edit failed for {}#{}: {}",
                        request.repo,
                        issue.number,
                        error
                    );
                }
            }
            IssueAnnouncementSyncOutcome::synced(announcement)
        }
        Ok(None) => IssueAnnouncementSyncOutcome::skipped("no announcement channel resolved"),
        Err(error) => {
            tracing::warn!(
                "[issues] created GitHub issue {}#{} but failed to announce: {}",
                request.repo,
                issue.number,
                error
            );
            IssueAnnouncementSyncOutcome::failed(error)
        }
    }
}

pub(crate) async fn resolve_known_agent_id_pg(
    pool: &PgPool,
    agent_id: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(agent_id) = agent_id.and_then(trim_non_empty) else {
        return Ok(None);
    };

    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM agents WHERE id = $1 LIMIT 1")
        .bind(&agent_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("resolve agent {agent_id}: {error}"))?;

    if exists.is_none() {
        tracing::warn!("[issues] ignoring unknown assignee '{agent_id}' for linked kanban card");
    }

    Ok(exists)
}

fn normalize_labels(labels: &[String]) -> Vec<String> {
    labels
        .iter()
        .filter_map(|label| trim_non_empty(label))
        .collect()
}

fn trim_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeIssueCreator {
        available: bool,
        issue: CreatedIssue,
    }

    impl IssueCreator for FakeIssueCreator {
        fn gh_available(&self) -> bool {
            self.available
        }

        fn create_issue<'a>(
            &'a self,
            _request: &'a GitHubIssueCreateRequest,
        ) -> IssueCreatorFuture<'a> {
            Box::pin(async move { Ok(self.issue.clone()) })
        }
    }

    fn fake_creator() -> FakeIssueCreator {
        FakeIssueCreator {
            available: true,
            issue: CreatedIssue {
                number: 3742,
                url: "https://github.com/itismyfield/AgentDesk/issues/3742".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn standard_creation_reports_enabled_sync_outcomes_without_live_github() {
        let request = GitHubIssueCreateRequest::new(
            "api_github_issues_create",
            "itismyfield/AgentDesk",
            "Centralize issue creation",
            "body",
        )
        .with_labels(vec![" agent:td ".to_string(), String::new()])
        .with_kanban(KanbanCardSync::enabled(KanbanCardSyncOptions {
            agent_id: Some("td".to_string()),
            metadata_json: Some(r#"{"depends_on":[1]}"#.to_string()),
            status_on_create: Some("backlog".to_string()),
        }))
        .with_announcement(IssueAnnouncementSync::enabled(
            IssueAnnouncementSyncOptions {
                agent_id: Some("td".to_string()),
                announcement_channel_id: Some("123".to_string()),
                complete_if_closed: true,
            },
        ));

        let result = create_github_issue_with_creator(None, request, &fake_creator())
            .await
            .expect("fake issue creation should succeed");

        assert_eq!(result.mode, IssueCreationMode::Standard);
        assert_eq!(result.applied_labels, vec!["agent:td"]);
        assert_eq!(result.issue.number, 3742);
        assert_eq!(result.kanban.enabled, true);
        assert_eq!(
            result.kanban.error.as_deref(),
            Some("postgres pool unavailable")
        );
        assert_eq!(result.announcement.enabled, true);
        assert_eq!(
            result.announcement.error.as_deref(),
            Some("postgres pool unavailable for issue announcement")
        );
    }

    #[tokio::test]
    async fn github_only_creation_skips_route_specific_side_effects_explicitly() {
        let reason = "meeting issue creation stores issue_url locally and intentionally skips sync";
        let request = GitHubIssueCreateRequest::github_only(
            "meeting_issue_creation",
            "itismyfield/AgentDesk",
            "Meeting action item",
            "body",
            reason,
        );

        let result = create_github_issue_with_creator(None, request, &fake_creator())
            .await
            .expect("fake issue creation should succeed");

        assert_eq!(result.mode, IssueCreationMode::GithubOnly);
        assert!(result.applied_labels.is_empty());
        assert_eq!(result.kanban.enabled, false);
        assert_eq!(result.kanban.skipped_reason.as_deref(), Some(reason));
        assert_eq!(result.announcement.enabled, false);
        assert_eq!(result.announcement.skipped_reason.as_deref(), Some(reason));
    }
}
