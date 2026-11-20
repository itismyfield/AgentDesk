use std::collections::BTreeMap;

use axum::extract::rejection::PathRejection;
use serde::Deserialize;

use super::{AppState, Command, CommandEnvelope, Json, JsonRejection, Path, Query, QueryRejection};
use super::{
    Response, State, Utc, Value, body, commands, id, json, pool, protocol, store, store_error,
};
use crate::{
    domain::campaign::{
        model::CampaignState,
        pipeline::{PipelineDefinition, PipelineExecutor, PipelineKind},
        pipeline_fields::PipelineField,
        pipeline_template::{TEMPLATE_ID, TEMPLATE_VERSION, development_template},
        pipeline_validate::PipelineDiagnostic,
    },
    error::{AppError, AppResult},
    services::campaign_execution::{control, pipeline_editor},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server::routes) struct FieldQuery {
    kind: PipelineKind,
    executor: Option<String>,
    executor_version: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server::routes) struct Validate {
    pipeline: PipelineDefinition,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server::routes) struct Save {
    idempotency_key: String,
    expected_definition_revision: u64,
    pipeline: Box<PipelineDefinition>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server::routes) struct Migrate {
    idempotency_key: String,
    expected_definition_revision: u64,
}

pub(in crate::server::routes) async fn condition_fields(
    query: Result<Query<FieldQuery>, QueryRejection>,
) -> AppResult<Json<Value>> {
    let Query(query) = query.map_err(|error| AppError::bad_request(error.body_text()))?;
    if query.executor.is_none() && query.executor_version.is_some() {
        return Err(AppError::bad_request("executor_version requires executor"));
    }
    let executor = query.executor.map(|id| PipelineExecutor {
        id,
        version: query.executor_version.unwrap_or(1),
        parameters: Default::default(),
    });
    let fields: Vec<_> = PipelineField::ALL
        .iter()
        .map(|field| field.spec(query.kind, executor.as_ref()))
        .collect();
    Ok(Json(json!({"vocabulary_version":1,"fields":fields})))
}

fn template_metadata() -> Value {
    json!({"id":TEMPLATE_ID,"version":TEMPLATE_VERSION,"design_enabled":false,"execution_readiness":"unavailable"})
}

pub(in crate::server::routes) async fn templates() -> Json<Value> {
    Json(json!({"templates":[template_metadata()]}))
}

pub(in crate::server::routes) async fn template(
    path: Result<Path<(String, u32)>, PathRejection>,
) -> AppResult<Json<Value>> {
    let Path((id, version)) = path.map_err(|error| AppError::bad_request(error.body_text()))?;
    if id != TEMPLATE_ID || version != TEMPLATE_VERSION {
        return Err(AppError::not_found("pipeline template not found"));
    }
    let pipeline = development_template();
    Ok(Json(json!({
        "template": template_metadata(),
        "execution_hash": execution_hash(&pipeline)?,
        "pipeline": pipeline
    })))
}

async fn campaign(state: &AppState, campaign_id: &str) -> AppResult<CampaignState> {
    id(campaign_id)?;
    store::get(pool(state)?, campaign_id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| AppError::not_found("server campaign not found"))
}

fn execution_hash(pipeline: &PipelineDefinition) -> AppResult<String> {
    pipeline
        .execution_hash()
        .map_err(|_| AppError::internal("could not hash campaign pipeline"))
}

pub(in crate::server::routes) async fn detail(
    State(state): State<AppState>,
    Path(campaign_id): Path<String>,
) -> AppResult<Json<Value>> {
    let state = campaign(&state, &campaign_id).await?;
    let persisted = state.definition.input().pipeline.as_deref();
    let fallback;
    let (pipeline, source) = if let Some(pipeline) = persisted {
        (pipeline, "saved")
    } else {
        fallback = development_template();
        (&fallback, "legacy_projection")
    };
    let legacy_state_ids: BTreeMap<_, _> = state
        .nodes
        .iter()
        .map(|(id, node)| {
            (
                id,
                state.run.as_ref().map(|_| node.stage.pipeline_state_id()),
            )
        })
        .collect();
    Ok(Json(json!({
        "pipeline":pipeline,
        "definition_revision":state.definition_revision,
        "execution_hash":execution_hash(pipeline)?,
        "source":source,
        "editable":control::edit_guard(&state).is_ok(),
        "migration_available":pipeline_editor::migration_available(&state),
        "execution":{
            "mode":"legacy_stage",
            "run_id":state.run.as_ref().map(|run| &run.id),
            "definition_revision":state.run.as_ref().map(|_| state.legacy_execution_revision()),
            "state_ids":legacy_state_ids,
        },
    })))
}

fn diagnostics(errors: Vec<PipelineDiagnostic>) -> Json<Value> {
    Json(json!({"errors":errors,"warnings":[]}))
}

pub(in crate::server::routes) async fn validate(
    input: Result<Json<Validate>, JsonRejection>,
) -> AppResult<Json<Value>> {
    Ok(diagnostics(body(input)?.pipeline.validate()))
}

pub(in crate::server::routes) async fn validate_for_campaign(
    State(state): State<AppState>,
    Path(campaign_id): Path<String>,
    input: Result<Json<Validate>, JsonRejection>,
) -> AppResult<Json<Value>> {
    let candidate = body(input)?.pipeline;
    let campaign = campaign(&state, &campaign_id).await?;
    let mut errors = candidate.validate();
    errors.extend(pipeline_editor::check_references(&campaign, &candidate));
    Ok(diagnostics(errors))
}

pub(in crate::server::routes) async fn save(
    State(state): State<AppState>,
    Path(campaign_id): Path<String>,
    input: Result<Json<Save>, JsonRejection>,
) -> AppResult<Response> {
    let input = body(input)?;
    persist(
        &state,
        &campaign_id,
        input.idempotency_key,
        input.expected_definition_revision,
        Some(input.pipeline),
    )
    .await
}

pub(in crate::server::routes) async fn migrate(
    State(state): State<AppState>,
    Path(campaign_id): Path<String>,
    input: Result<Json<Migrate>, JsonRejection>,
) -> AppResult<Response> {
    let input = body(input)?;
    persist(
        &state,
        &campaign_id,
        input.idempotency_key,
        input.expected_definition_revision,
        None,
    )
    .await
}

async fn persist(
    state: &AppState,
    campaign_id: &str,
    key: String,
    expected_revision: u64,
    pipeline: Option<Box<PipelineDefinition>>,
) -> AppResult<Response> {
    id(campaign_id)?;
    protocol::key(&key)?;
    protocol::revision(expected_revision)?;
    let envelope = CommandEnvelope {
        idempotency_key: key,
        observed_at: Utc::now(),
        command: Command::EditPipeline {
            expected_revision,
            pipeline,
        },
    };
    protocol::saved(
        commands::execute(pool(state)?, campaign_id, &envelope, protocol::edited)
            .await
            .map_err(store_error)?,
    )
}
