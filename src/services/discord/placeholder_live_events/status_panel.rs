use crate::services::agent_protocol::{StatusEvent, StatusTodoItem, SubagentSummary};
use crate::services::provider::ProviderKind;

use super::common::{
    STATUS_PANEL_MAX_CHARS, STATUS_PANEL_SUBAGENT_LIMIT, STATUS_PANEL_TODO_LIMIT,
    STATUS_PANEL_WORKFLOW_LIMIT, escape_status_panel_markdown, normalize_summary, truncate_chars,
};
use super::context_panel::{ContextPanelSnapshot, render_context_panel_line};
use super::session_banner_claim::SessionBannerClaims;
use super::session_panel::SessionPanelSnapshot;
use super::status_events::{is_schedule_wakeup_tool, parse_eta_secs};
use super::subagent_panel::{
    SubagentKeyTombstones, clean_match_key, log_idless_terminal_fallback,
    match_subagent_end_fallback, render_live_subagents_section,
};
use super::task_panel::{
    STUCK_BACKGROUND_TASK_TTL, TaskPanelSnapshot, TaskToolSlot, finish_background_task_tool_slot,
    force_abort_stuck_background_task_slots, render_live_tasks_section, render_task_panel_line,
    take_slot_ordinal, task_tool_slot_is_unfinished_background, upsert_background_task_tool_slot,
    upsert_task_tool_slot,
};
use super::workflow_panel::{
    WorkflowAgentSlot, WorkflowSlot, apply_workflow_end, render_workflow_slot, trim_workflow_slot,
    trim_workflows, upsert_workflow_agent, upsert_workflow_phase, workflow_status_label,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SubagentSlot {
    // #4367: read by the extracted `subagent_panel::render_subagent_slot`.
    pub(super) subagent_type: String,
    pub(super) desc: String,
    // #4367: read by the extracted `subagent_panel::render_subagent_slot`.
    pub(super) recent: Option<String>,
    pub(super) finished: Option<bool>,
    /// #3084: Task tool-use id that opened this slot, so `SubagentEnd` closes the
    /// exact slot among parallels instead of the first unfinished one.
    pub(super) tool_use_id: Option<String>,
    // #4177/#4396: read by `subagent_panel::match_subagent_end_fallback`.
    pub(super) agent_id: Option<String>,
    /// #3086: TUI-parity accounting from the finishing `SubagentEnd`; drives the
    /// `Done (N tools · M tokens · Xs)` summary on the render line.
    // #4367: read by the extracted `subagent_panel::render_subagent_slot`.
    pub(super) summary: Option<SubagentSummary>,
    /// `true` when launched with `run_in_background`: an ack-only `SubagentEnd`
    /// must NOT mark it ✓ (only a genuine completion finalizes it).
    background: bool,
    /// #3391: monotonic, never-reused per-entry slot id (mirrors
    /// `TaskToolSlot::ordinal`) backing slot-identity subagent eviction.
    // #4396: read by `subagent_panel::log_idless_terminal_fallback`.
    pub(super) ordinal: u64,
    /// #4177: monotonic creation instant, refreshed on observed slot activity
    /// (#4396 — so the TTL measures SILENCE, not lifetime). The stuck-slot sweep
    /// (turn boundary + periodic render tick) force-aborts a background subagent
    /// silent longer than `STUCK_BACKGROUND_TASK_TTL`.
    pub(super) started_at: std::time::Instant,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) enum DerivedStatus {
    #[default]
    Running,
    MonitorWait,
    ScheduleWakeup(Option<u64>),
    Completed {
        kind: CompletedKind,
    },
    ToolRunning {
        name: String,
        summary: Option<String>,
    },
    SubagentRunning {
        desc: String,
    },
    WorkflowRunning {
        label: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompletedKind {
    Foreground,
    Background,
}

impl CompletedKind {
    fn from_background(background: bool) -> Self {
        if background {
            Self::Background
        } else {
            Self::Foreground
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct StatusPanelState {
    pub(super) status: DerivedStatus,
    pub(super) session: Option<SessionPanelSnapshot>,
    pub(super) task: Option<TaskPanelSnapshot>,
    pub(super) context: Option<ContextPanelSnapshot>,
    todos: Vec<StatusTodoItem>,
    pub(super) tasks: Vec<TaskToolSlot>,
    pub(super) subagents: Vec<SubagentSlot>,
    // #4396 r3: ownership-conflict tombstones for subagent slots that LEFT this
    // state (footer eviction / trim / resets) — consulted by the fallback
    // matcher, carried across the turn reset. See `SubagentKeyTombstones`.
    pub(super) recently_evicted_subagent_keys: SubagentKeyTombstones,
    pub(super) workflows: Vec<WorkflowSlot>,
    next_slot_ordinal: u64, // #3391: advancing, never-reused task/subagent ordinals.
    // #3477 item 3: instant the turn entered `Completed` (None until then); vs the
    // store's `last_recent_event_at` it gates the late-batch 🖥️ Recent freshness.
    pub(super) completed_at: Option<std::time::Instant>,
    pub(super) background_agent_pending: bool,
    // #3811: intake-set original-request user_msg_id; drives the `요청:` deeplink
    // (`None` for headless/synthetic/voice/id-0 — no real Discord message).
    pub(super) request_user_msg_id: Option<u64>,
    // #3983/#4147/#4451: session one-shot + sticky winning-turn prefix ledger.
    // Bookkeeping only; excluded from session snapshot equality.
    session_banner_claims: SessionBannerClaims,
}

impl StatusPanelState {
    /// #3087: on a true session boundary (provider session id delta), clears the
    /// per-session content slots and resets status to `Running`, PRESERVING
    /// context/token usage + session snapshots and the ordinal counter.
    pub(super) fn reset_session_content(&mut self) {
        self.status = DerivedStatus::Running;
        self.todos.clear();
        self.tasks.clear();
        // #4396 r3: the cleared subagents leave the state — tombstone their keys
        // so a late end cannot close a same-key slot of the NEXT session.
        let now = std::time::Instant::now();
        for slot in &self.subagents {
            self.recently_evicted_subagent_keys
                .push_slot_keys(slot, now);
        }
        self.subagents.clear();
        self.workflows.clear();
        self.completed_at = None; // #3477 item 3: drop the stale freshness gate.
        self.background_agent_pending = false;
        self.request_user_msg_id = None; // #3811: new session = new request context.
    }

    /// #3983 item4: atomically claim the one-shot session banner for the CURRENT
    /// session snapshot, returning the rendered session line EXACTLY ONCE per
    /// session identity. The identity is the stable `session_instance_key` (the
    /// per-spawn nonce marker), falling back to the provider session id, then the
    /// rendered line itself when neither id is available. A repeat call for the
    /// same identity — the sibling refresh path arriving second, or a later
    /// status tick — returns `None`; a NEW session identity (new spawn / provider
    /// session) makes the stored key stale so the next boundary re-emits. `None`
    /// when there is no session snapshot to banner.
    ///
    /// Callers hold the per-channel `StatusPanelState` mutex across this call, so
    /// the read-current-identity + compare-and-record is a single atomic step:
    /// whichever of the sink/watcher refresh paths reaches it FIRST for a given
    /// session wins the banner, and the other observes the recorded key and skips
    /// (no double emit, no omission).
    #[cfg(test)]
    pub(super) fn claim_session_banner(&mut self, provider: &ProviderKind) -> Option<String> {
        self.session_banner_claims
            .claim_once(self.session.as_ref(), provider)
    }

    /// #4147: claim the current session banner for the first answer message of
    /// `turn_id`. Unlike the legacy one-shot POST claim, the winning turn gets
    /// the same rendered line on every re-compose so a terminal replace cannot
    /// wipe a banner shown during streaming. A later turn in the same session
    /// gets `None`; a new session identity re-arms the prefix.
    ///
    /// When ordinary turn cleanup has temporarily cleared the session snapshot,
    /// the already-winning turn may still re-compose from its stored line. This
    /// preserves #4451's redrive guarantee without letting a different turn
    /// inherit stale session chrome.
    pub(super) fn claim_session_banner_prefix(
        &mut self,
        provider: &ProviderKind,
        turn_id: &str,
    ) -> Option<String> {
        self.session_banner_claims
            .claim_prefix(self.session.as_ref(), provider, turn_id)
    }

    pub(super) fn reset_turn_content_preserving_unfinished_footer_residuals(&mut self) -> bool {
        // #3473: turn-boundary reconciliation — force a TTL-expired stuck
        // background task to `aborted` BEFORE the retain filter so it is dropped
        // here instead of sitting ⏳ forever.
        let now = std::time::Instant::now();
        force_abort_stuck_background_task_slots(&mut self.tasks, now);
        force_abort_stuck_subagent_slots(&mut self.subagents, now);
        let tasks = self
            .tasks
            .iter()
            .filter(|slot| task_tool_slot_is_unfinished_background(slot))
            .cloned()
            .collect::<Vec<_>>();
        let subagents = self
            .subagents
            .iter()
            .filter(|slot| slot.is_unfinished_background())
            .cloned()
            .collect::<Vec<_>>();
        let has_residuals = !tasks.is_empty() || !subagents.is_empty();
        // #4396 r3: every slot NOT kept as a residual leaves the state here —
        // tombstone the departing keys before the state is rebuilt.
        for slot in self
            .subagents
            .iter()
            .filter(|s| !s.is_unfinished_background())
        {
            self.recently_evicted_subagent_keys
                .push_slot_keys(slot, now);
        }
        *self = StatusPanelState {
            tasks,
            subagents,
            // #4396 r3: the tombstones outlive the slots they guard.
            recently_evicted_subagent_keys: std::mem::take(
                &mut self.recently_evicted_subagent_keys,
            ),
            background_agent_pending: self.background_agent_pending,
            // #3391: carry the counter so a residual ordinal is never reissued.
            next_slot_ordinal: self.next_slot_ordinal,
            request_user_msg_id: self.request_user_msg_id, // #3811: survive turn reset
            // #4451: this claim is session-scoped, not turn-scoped. The health
            // redrive path can run ordinary turn cleanup every 30 seconds while
            // the same tmux/provider session remains alive. Dropping the claim
            // here re-posted the same one-shot banner on every redrive.
            session_banner_claims: self.session_banner_claims.clone(),
            ..StatusPanelState::default()
        };
        has_residuals
    }

    /// #4451: a claimed session banner is durable turn bookkeeping. Keep the
    /// channel state entry across ordinary turn cleanup even when it carries no
    /// footer residuals or request anchor, otherwise removing the entry would
    /// discard the preserved claim immediately.
    pub(super) fn has_session_banner_claim(&self) -> bool {
        self.session_banner_claims.has_claim()
    }

    pub(super) fn apply(&mut self, event: StatusEvent) {
        match event {
            StatusEvent::ToolStart { name, args_summary } => {
                if is_schedule_wakeup_tool(&name) {
                    self.status =
                        DerivedStatus::ScheduleWakeup(parse_eta_secs(args_summary.as_deref()));
                } else {
                    self.status = DerivedStatus::ToolRunning {
                        name,
                        summary: args_summary,
                    };
                }
            }
            StatusEvent::ToolEnd { success: _ } => {
                self.status = DerivedStatus::Running;
            }
            StatusEvent::SubagentStart {
                subagent_type,
                desc,
                agent_id,
                tool_use_id,
                background,
            } => {
                // #3920: keep "was a real value provided?" BEFORE defaulting, so a
                // background-promotion re-affirmation (an async launch ack carries
                // no desc/type) never overwrites the launching slot's real
                // description with the `subagent`/`Task` placeholders.
                let provided_desc = desc.filter(|value| !value.trim().is_empty());
                let provided_type = subagent_type.filter(|value| !value.trim().is_empty());
                let provided_agent_id = agent_id.filter(|value| !value.trim().is_empty());
                // A background `SubagentStart` re-affirms (and #3920: PROMOTES) the
                // still-running slot for this tool-use id. Matching ANY unfinished
                // slot — not only an already-background one — lets an async/
                // `run_in_background` Agent launch (whose async-ness is known only
                // from the launch-ack `toolUseResult`, not the tool INPUT) flip its
                // foreground-looking slot to a background subagent. That keeps it
                // alive across turn-boundary resets like a Bash `run_in_background`
                // task, instead of being dropped a turn later (#3920).
                if background
                    && let Some(id) = tool_use_id.as_deref().filter(|id| !id.trim().is_empty())
                    && let Some(slot) = self.subagents.iter_mut().rev().find(|slot| {
                        slot.finished.is_none() && slot.tool_use_id.as_deref() == Some(id)
                    })
                {
                    slot.background = true;
                    if let Some(subagent_type) = provided_type {
                        slot.subagent_type = subagent_type;
                    }
                    if let Some(desc) = provided_desc {
                        slot.desc = desc;
                    }
                    if let Some(agent_id) = provided_agent_id {
                        slot.agent_id = Some(agent_id);
                    }
                    let running_desc = slot.desc.clone();
                    self.status = DerivedStatus::SubagentRunning { desc: running_desc };
                    return;
                }
                let desc = provided_desc.unwrap_or_else(|| "subagent".to_string());
                let subagent_type = provided_type.unwrap_or_else(|| "Task".to_string());
                let ordinal = take_slot_ordinal(&mut self.next_slot_ordinal);
                self.subagents.push(SubagentSlot {
                    subagent_type,
                    desc: desc.clone(),
                    recent: None,
                    finished: None,
                    tool_use_id,
                    summary: None,
                    agent_id: provided_agent_id,
                    background,
                    ordinal,
                    started_at: std::time::Instant::now(),
                });
                self.status = DerivedStatus::SubagentRunning { desc };
                trim_subagents(
                    &mut self.subagents,
                    &mut self.recently_evicted_subagent_keys,
                );
            }
            StatusEvent::SubagentEvent { summary } => {
                if let Some(slot) = self
                    .subagents
                    .iter_mut()
                    .rev()
                    .find(|slot| slot.finished.is_none())
                {
                    slot.recent = Some(normalize_summary(&summary));
                    slot.started_at = std::time::Instant::now(); // #4396: alive — reset the TTL clock.
                    self.status = DerivedStatus::SubagentRunning {
                        desc: slot.desc.clone(),
                    };
                }
            }
            StatusEvent::SubagentActivity {
                tool_use_id,
                summary,
            } => self.set_subagent_activity(tool_use_id, summary),
            StatusEvent::SubagentEnd {
                success,
                agent_id,
                desc,
                tool_use_id,
                summary,
                ack_only,
            } => {
                // #3084/#4177: real subagent lifecycle records pair start->finish by
                // exact Task `tool_use_id`; if an id-bearing completion misses, do
                // not guess another unfinished slot.
                let id = tool_use_id.as_deref();
                let matched = id.and_then(|id| {
                    self.subagents.iter().rposition(|slot| {
                        slot.finished.is_none() && slot.tool_use_id.as_deref() == Some(id)
                    })
                });
                // #3086 P1 / #3359: ack-only id-bearing ends are safe only
                // on an exact id match; genuine id-bearing completions may use
                // a unique agent-id/description fallback (#4177).
                let fallback = (!ack_only && id.is_some())
                    .then(|| {
                        match_subagent_end_fallback(
                            &self.subagents,
                            &self.recently_evicted_subagent_keys,
                            agent_id.as_deref(),
                            desc.as_deref(),
                        )
                    })
                    .flatten();
                let target = match matched {
                    Some(index) => Some(index),
                    None if id.is_some() => fallback,
                    None if ack_only => self
                        .subagents
                        .iter()
                        .rposition(|slot| slot.finished.is_none() && slot.tool_use_id.is_none()),
                    // #4396 point 2: an id-less GENUINE end that carries a match
                    // key (async task-notification without `<tool-use-id>`) closes
                    // ONLY a uniquely agent_id/desc-matched slot — zero/ambiguous
                    // matches drop the event (a stale running slot beats
                    // finalizing/evicting the wrong one). Key-LESS id-less ends
                    // below keep the legacy last-unfinished fallback (`system`
                    // stream path / failed id-less launches).
                    None if clean_match_key(agent_id.as_deref()).is_some()
                        || clean_match_key(desc.as_deref()).is_some() =>
                    {
                        let matched = match_subagent_end_fallback(
                            &self.subagents,
                            &self.recently_evicted_subagent_keys,
                            agent_id.as_deref(),
                            desc.as_deref(),
                        );
                        log_idless_terminal_fallback(
                            &self.subagents,
                            matched,
                            agent_id.as_deref(),
                            desc.as_deref(),
                        );
                        matched
                    }
                    None => self
                        .subagents
                        .iter()
                        .rposition(|slot| slot.finished.is_none()),
                };
                let slot = target.map(|index| &mut self.subagents[index]);
                if let Some(slot) = slot {
                    // A background ack-only end is just a launch ack (slot keeps
                    // running); a genuine/foreground end still closes it.
                    let finalize = !(ack_only && slot.background);
                    if finalize {
                        slot.finished = Some(success);
                    }
                    // #3086: attach Done summary only when accounting present.
                    if let Some(summary) = summary.filter(|summary| !summary.is_empty()) {
                        slot.summary = Some(summary);
                    }
                }
                self.status = DerivedStatus::Running;
            }
            StatusEvent::TaskToolUpdate {
                name,
                task_id,
                summary,
                status,
            } => {
                upsert_task_tool_slot(
                    &mut self.tasks,
                    &mut self.next_slot_ordinal,
                    name,
                    task_id,
                    summary,
                    status,
                );
            }
            StatusEvent::BackgroundTaskStart {
                name,
                summary,
                tool_use_id,
            } => {
                upsert_background_task_tool_slot(
                    &mut self.tasks,
                    &mut self.next_slot_ordinal,
                    name,
                    summary,
                    tool_use_id,
                );
            }
            StatusEvent::BackgroundTaskEnd {
                tool_use_id,
                success,
            } => {
                finish_background_task_tool_slot(&mut self.tasks, &tool_use_id, success);
            }
            StatusEvent::TodoUpdate { items } => {
                self.todos = items
                    .into_iter()
                    .filter(|item| !item.content.trim().is_empty())
                    .take(STATUS_PANEL_TODO_LIMIT)
                    .collect();
            }
            StatusEvent::MonitorWait => {
                self.status = DerivedStatus::MonitorWait;
            }
            StatusEvent::ScheduleWakeup { eta_secs } => {
                self.status = DerivedStatus::ScheduleWakeup(eta_secs);
            }
            StatusEvent::WorkflowStart { task_id, name } => {
                let label = {
                    let slot = self.workflow_slot_mut(task_id.clone());
                    if let Some(name) = name.filter(|value| !value.trim().is_empty()) {
                        slot.name = Some(normalize_summary(&name));
                    }
                    trim_workflow_slot(slot);
                    workflow_status_label(slot)
                };
                self.status = DerivedStatus::WorkflowRunning { label };
                trim_workflows(&mut self.workflows);
            }
            StatusEvent::WorkflowPhase {
                task_id,
                index,
                title,
            } => {
                let label = {
                    let slot = self.workflow_slot_mut(task_id);
                    upsert_workflow_phase(&mut slot.phases, index, title);
                    trim_workflow_slot(slot);
                    workflow_status_label(slot)
                };
                self.status = DerivedStatus::WorkflowRunning { label };
                trim_workflows(&mut self.workflows);
            }
            StatusEvent::WorkflowAgent {
                task_id,
                index,
                label,
                phase_index,
                phase_title,
                state,
            } => {
                let label = {
                    let slot = self.workflow_slot_mut(task_id);
                    upsert_workflow_agent(
                        &mut slot.agents,
                        WorkflowAgentSlot {
                            index,
                            label,
                            phase_index,
                            phase_title,
                            state,
                        },
                    );
                    trim_workflow_slot(slot);
                    workflow_status_label(slot)
                };
                self.status = DerivedStatus::WorkflowRunning { label };
                trim_workflows(&mut self.workflows);
            }
            StatusEvent::WorkflowLog { task_id, summary } => {
                {
                    let slot = self.workflow_slot_mut(task_id);
                    let summary = normalize_summary(&summary);
                    if !summary.is_empty() {
                        slot.recent = Some(summary);
                    }
                    trim_workflow_slot(slot);
                }
                trim_workflows(&mut self.workflows);
            }
            StatusEvent::WorkflowEnd {
                task_id,
                success,
                summary,
            } => {
                if apply_workflow_end(&mut self.workflows, task_id, success, summary) {
                    trim_workflows(&mut self.workflows);
                    if matches!(self.status, DerivedStatus::WorkflowRunning { .. }) {
                        self.status = DerivedStatus::Running;
                    }
                }
            }
            StatusEvent::TurnCompleted {
                background,
                background_agent_pending,
            } => {
                self.status = DerivedStatus::Completed {
                    kind: CompletedKind::from_background(background),
                };
                self.background_agent_pending = background_agent_pending;
                self.completed_at = Some(std::time::Instant::now()); // #3477 item 3
            }
            StatusEvent::Heartbeat => {
                if matches!(self.status, DerivedStatus::Running) {
                    self.status = DerivedStatus::Running;
                }
            }
        }
    }

    /// #3204/#3198: routes a running subagent's live step onto its slot's recent
    /// line (UNFINISHED id-matching slot; id-bearing no-match dropped).
    fn set_subagent_activity(&mut self, tool_use_id: Option<String>, summary: String) {
        let id = tool_use_id.as_deref();
        let target = self.subagents.iter_mut().rev().find(|slot| {
            slot.finished.is_none() && (id.is_none() || slot.tool_use_id.as_deref() == id)
        });
        if let Some(slot) = target {
            let summary = normalize_summary(&summary);
            if !summary.trim().is_empty() {
                slot.recent = Some(summary);
            }
            slot.started_at = std::time::Instant::now(); // #4396: alive — reset the TTL clock.
        }
    }

    fn workflow_slot_mut(&mut self, task_id: Option<String>) -> &mut WorkflowSlot {
        let index = task_id
            .as_deref()
            .and_then(|task_id| {
                self.workflows
                    .iter()
                    .position(|slot| slot.task_id.as_deref() == Some(task_id))
            })
            .or_else(|| (task_id.is_none() && self.workflows.len() == 1).then_some(0));
        if let Some(index) = index {
            return &mut self.workflows[index];
        }
        self.workflows.push(WorkflowSlot {
            task_id,
            name: None,
            phases: Vec::new(),
            agents: Vec::new(),
            recent: None,
            finished: None,
        });
        self.workflows.last_mut().expect("workflow just pushed")
    }
}

pub(super) fn render_status_panel(
    snapshot: StatusPanelState,
    provider: &ProviderKind,
    // #4601: precomputed `턴 시작 : …\n마지막 업데이트 : …` time lines.
    time_line: String,
    // #4601: precomputed `턴 트리거:` deeplink, rendered immediately after the
    // activity line (or `None` for headless/synthetic/id-0 turns).
    turn_trigger_line: Option<String>,
) -> String {
    let header_status = if matches!(provider, ProviderKind::Codex)
        && matches!(snapshot.status, DerivedStatus::SubagentRunning { .. })
    {
        DerivedStatus::Running
    } else {
        snapshot.status.clone()
    };
    // #4601: the header opens with the derived-status ACTIVITY label, followed by
    // the request anchor when present, then the start/update TIME fields. Keep the
    // entire header in one section so each field occupies the immediately following
    // physical line and section-wise truncation preserves the header atomically.
    let mut header_lines = vec![super::freshness::render_activity_line(&header_status)];
    if let Some(trigger) = turn_trigger_line.filter(|line| !line.trim().is_empty()) {
        header_lines.push(trigger);
    }
    header_lines.push(time_line);
    let mut sections = vec![header_lines.join("\n")];

    // #3983 item4: the session line is NO LONGER rendered in the every-tick
    // footer. It is composed once at the top of the first answer message via
    // `session_banner.rs`, so the
    // repeated per-tick footer echo of `🆕 새 세션 시작 · provider session … · tmux …`
    // is retired. Track A's header is unaffected.

    if let Some(task) = snapshot.task.as_ref() {
        sections.push(render_task_panel_line(task));
    }

    if let Some(context_line) = snapshot
        .context
        .as_ref()
        .and_then(|context| render_context_panel_line(context, provider))
    {
        sections.push(context_line);
    }

    if !snapshot.todos.is_empty() {
        let lines = snapshot
            .todos
            .iter()
            .take(STATUS_PANEL_TODO_LIMIT)
            .map(|item| {
                let content = escape_status_panel_markdown(&normalize_summary(&item.content));
                format!(
                    "- {} {}",
                    item.status.checkbox_marker(),
                    truncate_chars(&content, 110)
                )
            })
            .collect::<Vec<_>>();
        sections.push(format!("Plan\n{}", lines.join("\n")));
    }

    // #3983 item 5a: the compact 🖥️ Recent + host block is removed from the footer
    // (the terminal echo is retired from the status panel entirely).
    // #4093: the in-progress-only Tasks section (filter + #3404 compaction +
    // empty-section guard) lives in `task_panel::render_live_tasks_section`.
    if let Some(section) = render_live_tasks_section(&snapshot.tasks) {
        sections.push(section);
    }

    // #4367: the in-progress-only Subagents section (filter + #3404 compaction +
    // empty-section guard) lives in `subagent_panel::render_live_subagents_section`,
    // mirroring #4093's Tasks extraction. The Codex suppression stays here.
    if !matches!(provider, ProviderKind::Codex)
        && let Some(section) = render_live_subagents_section(&snapshot.subagents)
    {
        sections.push(section);
    }

    if !matches!(provider, ProviderKind::Codex) && !snapshot.workflows.is_empty() {
        let lines = snapshot
            .workflows
            .iter()
            .rev()
            .take(STATUS_PANEL_WORKFLOW_LIMIT)
            .flat_map(render_workflow_slot)
            .collect::<Vec<_>>();
        if !lines.is_empty() {
            sections.push(format!("Workflow\n{}", lines.join("\n")));
        }
    }

    truncate_status_panel_sections(sections)
}

fn join_status_panel_sections(sections: &[String]) -> String {
    sections.join("\n\n")
}

/// #3394: section-wise degradation. A char cut of the JOINED panel chops a
/// trailing fenced section's ``` (rendered as literal text), so on overflow DROP
/// whole trailing sections; a lone overflowing section is fence-safe-truncated and
/// `repair_fence_parity` re-balances every return path.
pub(super) fn truncate_status_panel_sections(mut sections: Vec<String>) -> String {
    use crate::services::discord::single_message_panel::repair_fence_parity;
    while sections.len() > 1
        && join_status_panel_sections(&sections).chars().count() > STATUS_PANEL_MAX_CHARS
    {
        sections.pop();
    }
    let joined = join_status_panel_sections(&sections);
    if joined.chars().count() <= STATUS_PANEL_MAX_CHARS {
        return repair_fence_parity(&joined);
    }
    repair_fence_parity(&truncate_chars(&joined, STATUS_PANEL_MAX_CHARS))
}

impl SubagentSlot {
    fn is_unfinished_background(&self) -> bool {
        self.background && self.finished.is_none()
    }

    pub(super) fn is_terminal(&self) -> bool {
        self.finished.is_some() // #3391: terminal (✓/✗) once `finished` is set.
    }

    /// #3391: the ✓/✗ this slot renders (`None` while unfinished); single source for both render and the footer honesty gate.
    pub(super) fn terminal_marker(&self) -> Option<&'static str> {
        self.finished.map(|ok| if ok { "✓" } else { "✗" })
    }

    // #3391: eviction identity — launching `tool_use_id`, else `ordinal`.
    pub(super) fn identity(&self) -> super::completion_footer::SlotKey {
        use super::completion_footer::SlotKey;
        match self.tool_use_id.as_deref() {
            Some(id) => SlotKey::ToolUseId(id.to_string()),
            None => SlotKey::Ordinal(self.ordinal),
        }
    }
}

/// #4177: force any background subagent slot that is still unfinished AND silent
/// longer than `STUCK_BACKGROUND_TASK_TTL` to terminal failed. Its terminal
/// notification never arrived, so it would otherwise survive every
/// residual-preserving reset as a ghost running entry. Runs at turn boundaries
/// and (#4396 point 2) on the periodic panel render tick, so a long single turn
/// bounds a stuck slot by the TTL instead of by turn length. Returns the number
/// swept.
pub(super) fn force_abort_stuck_subagent_slots(
    slots: &mut [SubagentSlot],
    now: std::time::Instant,
) -> usize {
    let mut swept = 0usize;
    for slot in slots.iter_mut() {
        if slot.is_unfinished_background()
            && now.saturating_duration_since(slot.started_at) >= STUCK_BACKGROUND_TASK_TTL
        {
            slot.finished = Some(false);
            swept += 1;
        }
    }
    if swept != 0 {
        tracing::info!(
            target: "agentdesk::discord::live_panel",
            swept_subagents = swept,
            ttl_secs = STUCK_BACKGROUND_TASK_TTL.as_secs(),
            "#4177: swept stuck background subagent slots"
        );
    }
    swept
}

fn trim_subagents(slots: &mut Vec<SubagentSlot>, tombstones: &mut SubagentKeyTombstones) {
    while slots.len() > STATUS_PANEL_SUBAGENT_LIMIT {
        let remove_index = slots
            .iter()
            .position(|slot| slot.finished.is_some())
            .unwrap_or(0);
        // #4396 r3: the trimmed slot leaves the state — tombstone its keys.
        let removed = slots.remove(remove_index);
        tombstones.push_slot_keys(&removed, std::time::Instant::now());
    }
}
