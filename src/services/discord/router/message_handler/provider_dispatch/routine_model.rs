//! The strict routine boundary precedes both an owned idle effect and a provider launch.
use super::*;
use crate::services::discord::commands::routine_model::{self as policy, Input};
use crate::services::tui_input::routine_model as proof;

pub(in crate::services::discord::router::message_handler) fn execute(
    turn: StreamingTurn<'_>,
    start: ReplayStart,
    sender: Sender<StreamMessage>,
    owner: Option<OwnerTurn>,
) -> Result<(), String> {
    super::start_failure::run_with(turn, start, sender, owner, execute_inner)
}

fn execute_inner(
    turn: StreamingTurn<'_>,
    start: ReplayStart,
    sender: Sender<StreamMessage>,
    owner: Option<OwnerTurn>,
) -> Result<(), String> {
    let (requirement, publish) = match prepare(&turn) {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Some(owner) = owner {
                owner.defer_before_submit(&error);
            }
            return Err(error);
        }
    };
    let turn = StreamingTurn {
        model: Some(&requirement.model),
        session_id: requirement.resume.as_deref(),
        ..turn
    };
    proof::scope_with_publisher(requirement.clone(), Some(publish), || {
        super::execute_inner(turn, start, sender, owner)
    })
}

fn prepare(turn: &StreamingTurn<'_>) -> Result<(proof::Requirement, proof::Publisher), String> {
    let shared = turn
        .shared
        .ok_or("NotSent(\"routine_model_runtime_unavailable\")")?;
    if !matches!(turn.host, TurnHost::Tmux) {
        return Err("NotSent(\"routine_model_host_unproved\")".into());
    }
    let tmux = turn
        .tmux_session_name
        .ok_or("NotSent(\"routine_model_session_missing\")")?;
    let input = Input {
        shared,
        provider: turn.provider,
        channel: poise::serenity_prelude::ChannelId::new(turn.channel_id),
        tmux,
        cwd: turn.working_dir,
        session: turn.session_id,
        model: turn.model,
    };
    let context =
        crate::services::platform::binary_resolver::active_provider_context(turn.provider.as_str());
    let key = context
        .as_ref()
        .and_then(|c| c.session_key.clone())
        .ok_or("NotSent(\"routine_model_session_key_unproved\")")?;
    let pool = turn
        .pool
        .cloned()
        .ok_or("NotSent(\"routine_model_store_unavailable\")")?;
    let provider = turn.provider.as_str().to_string();
    let publish: proof::Publisher = std::sync::Arc::new(move |applied| {
        tokio::runtime::Handle::current().block_on(async {
            let done = sqlx::query(
                "UPDATE sessions SET model=$1 WHERE session_key=$2 AND LOWER(provider)=LOWER($3)",
            )
            .bind(&applied.model)
            .bind(&key)
            .bind(&provider)
            .execute(&pool)
            .await
            .map_err(|e| e.to_string())?;
            if done.rows_affected() != 1 {
                return Err("NotSent(\"routine_model_session_record_changed\")".into());
            }
            Ok(())
        })
    });
    let prepared = tokio::runtime::Handle::current().block_on(policy::prepare(input))?;
    tracing::debug!(
        channel = turn.channel_id,
        reused = prepared.reused,
        "routine model boundary admitted"
    );
    let requirement = prepared.requirement;
    Ok((requirement, publish))
}

#[cfg(test)]
pub(in crate::services::discord) fn test_launch(
    input: Input<'_>,
    cancel: std::sync::Arc<crate::services::provider::CancelToken>,
    clearance: &crate::services::provider_teardown::TeardownClearance,
) -> Result<(), String> {
    let turn = StreamingTurn {
        pool: input.shared.pg_pool.as_ref(),
        registration_origin: None,
        shared: Some(input.shared),
        provider: input.provider,
        prompt: "hello",
        session_id: input.session,
        working_dir: input.cwd,
        system_prompt: None,
        allowed_tools: &[],
        cancel,
        codex_observation: None,
        tmux_session_name: Some(input.tmux),
        teardown: Some(clearance),
        host: &TurnHost::Tmux,
        channel_id: input.channel.get(),
        model: input.model,
        native_fast_mode: Some(true),
        effort: None,
        codex_goals: None,
        compact_percent: None,
        compact_lower_bound_tokens: 0,
        compact_token_limit: None,
        cache_ttl_minutes: None,
        dispatch_type: None,
        force_fresh: false,
        submission: None,
    };
    let start = crate::services::discord::router::intake_dispatch::replay_admission::dormant_start_for_driver(
        &input.channel.get().to_string(),
        crate::services::provider_hosting::ProviderSessionDriver::TuiHosting,
    ).unwrap();
    let (tx, _rx) = std::sync::mpsc::channel();
    let (owner, mut answer) = tokio::runtime::Handle::current().block_on(
        super::owner_input::tests::claimed_owner_turn(input.channel.get(), "hello"),
    );
    let result = execute(turn, start, tx, Some(owner));
    assert!(
        !matches!(
            answer.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ),
        "owner settlement was dropped: {result:?}"
    );
    result
}
