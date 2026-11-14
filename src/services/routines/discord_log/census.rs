//! Census runs publish their native verdict once; all diagnostic detail stays in result_json.
use super::*;

fn render(outcome: &RoutineRunOutcome) -> String {
    let report = outcome.result_json.as_ref().unwrap_or(&Value::Null);
    let verdict = if outcome.status == "succeeded" {
        match report["stage1"].as_str() {
            Some(value @ ("PASS" | "FAIL" | "INCONCLUSIVE" | "NO_TRAFFIC" | "NO_TUI_BINDING")) => {
                value
            }
            _ => "INCONCLUSIVE",
        }
    } else {
        "INCONCLUSIVE"
    };
    let count = |host: &str, field: &str| {
        if report["hosts"][host]["status"] == "NO_TUI_BINDING" {
            return "N/A".to_owned();
        }
        let value = &report["hosts"][host][field];
        value
            .as_u64()
            .or_else(|| {
                // QuickJS returns floats; keep only exactly representable whole counts.
                let number = value.as_f64()?;
                ((0.0..=9_007_199_254_740_991.0).contains(&number) && number.fract() == 0.0)
                    .then_some(number as u64)
            })
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into())
    };
    format!(
        "Legacy body census {verdict} | TUI turns mini={}/10 book={}/10 | restarts mini={}/1 book={}/1 | O ACK mini={} book={} | complete={}",
        count("mac-mini-release", "tui_turns"),
        count("mac-book-release", "tui_turns"),
        count("mac-mini-release", "restarts"),
        count("mac-book-release", "restarts"),
        count("mac-mini-release", "o_ack"),
        count("mac-book-release", "o_ack"),
        report["complete"] == true
    )
}

impl RoutineDiscordLogger {
    /// `thread` is the store that may create a missing routine thread and the saved thread id;
    /// both are `None` for a run its thread failed, whose verdict goes to the primary channel.
    pub(super) async fn log_census_outcome(
        &self,
        store: &RoutineStore,
        (thread_store, saved_thread): (Option<&RoutineStore>, Option<&str>),
        routine: &RoutineRecord,
        outcome: &RoutineRunOutcome,
    ) -> RoutineDiscordLogStatus {
        let content = render(outcome);
        let state = match store
            .record_run_discord_log_section(&outcome.run_id, "public_result", &content)
            .await
        {
            Ok(state) => state,
            Err(error) => return RoutineDiscordLogStatus::failed(error),
        };
        let target = match self
            .resolve_routine_log_target(
                thread_store,
                Some(&routine.id),
                Some(&routine.name),
                routine.agent_id.as_deref(),
                saved_thread,
            )
            .await
        {
            Ok(Some(target)) => target,
            Ok(None) => return RoutineDiscordLogStatus::skipped(),
            Err(error) => return RoutineDiscordLogStatus::failed(error),
        };
        match self
            .send_or_edit_run_log(&target, state.message_id.as_deref(), &content)
            .await
        {
            Ok(id) => match store
                .record_run_discord_message_id(&outcome.run_id, &id)
                .await
            {
                Ok(_) => RoutineDiscordLogStatus::ok(),
                Err(error) => RoutineDiscordLogStatus::failed(error),
            },
            Err(error) => RoutineDiscordLogStatus::failed(error),
        }
    }
}
