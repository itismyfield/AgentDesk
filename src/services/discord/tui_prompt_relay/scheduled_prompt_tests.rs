//! Rows and hook payloads are the ones Claude Code 2.1.325 wrote for a real one-shot `CronCreate`
//! fire (2026-11-14, session 708eaac7), with only the prompt id and transcript path replaced.

use super::*;
use crate::services::tui_prompt_dedupe::{
    extract_prompt_from_hook_payload, extract_prompt_id_from_hook_payload,
};

const REAL_PROMPT_ID: &str = "3fc992ba-8e68-43c2-a178-6ed88df586d3";
const HOOK_PAYLOAD: &str = r#"{"session_id":"708eaac7-d46f-4c29-837a-84e79c2d811f","transcript_path":"TRANSCRIPT","cwd":"/tmp/i8142-probe2/proj","prompt_id":"3fc992ba-8e68-43c2-a178-6ed88df586d3","permission_mode":"auto","hook_event_name":"UserPromptSubmit","prompt":"i8142 probe line one\nsecond line with `backticks`"}"#;
const ENQUEUE_ROW: &str = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-11-13T23:24:00.328Z","sessionId":"708eaac7-d46f-4c29-837a-84e79c2d811f","content":"i8142 probe line one\nsecond line with `backticks`"}"#;
const DEQUEUE_ROW: &str = r#"{"type":"queue-operation","operation":"dequeue","timestamp":"2026-11-13T23:24:00.335Z","sessionId":"708eaac7-d46f-4c29-837a-84e79c2d811f"}"#;
const FIRE_ROW: &str = r#"{"parentUuid":"d56236d0-3c8f-41dc-8ec7-580cbb1f0d3b","isSidechain":false,"type":"system","subtype":"scheduled_task_fire","content":"Running scheduled task (Nov 14 8:24am)","isMeta":false,"timestamp":"2026-11-13T23:24:00.327Z","uuid":"8156566a-f661-406c-adff-025cfdde0230","taskId":"abbf09df","cron":"24 8 14 11 *","prompt":"i8142 probe line one second line with `backticks`","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/i8142-probe2/proj","sessionId":"708eaac7-d46f-4c29-837a-84e79c2d811f","version":"2.1.325","gitBranch":"HEAD"}"#;
const SCHEDULED_USER_ROW: &str = r#"{"parentUuid":"8156566a-f661-406c-adff-025cfdde0230","isSidechain":false,"promptId":"3fc992ba-8e68-43c2-a178-6ed88df586d3","type":"user","message":{"role":"user","content":"i8142 probe line one\nsecond line with `backticks`"},"isMeta":true,"uuid":"e240fedf-dea7-4efe-a5ba-120d919918f7","timestamp":"2026-11-13T23:24:00.344Z","permissionMode":"auto","promptSource":"system","queueSkipSlashCommands":true,"wakeupSource":"schedule_wakeup","scheduledTaskId":"abbf09df","scheduledFireId":"8156566a-f661-406c-adff-025cfdde0230","turnOrigin":"scheduled","turnPosition":{"promptIndex":1,"turnIndex":2},"queuePriority":"later","queueSkipAttachments":true,"userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/i8142-probe2/proj","sessionId":"708eaac7-d46f-4c29-837a-84e79c2d811f","version":"2.1.325","gitBranch":"HEAD"}"#;
/// The same session's typed input, as Claude wrote it (session 64c63eeb row 6), text swapped.
const TYPED_USER_ROW: &str = r#"{"parentUuid":null,"isSidechain":false,"promptId":"3fc992ba-8e68-43c2-a178-6ed88df586d3","type":"user","message":{"role":"user","content":"i8142 probe line one\nsecond line with `backticks`"},"uuid":"5657c950-53bd-480f-9907-74dcd4001c17","timestamp":"2026-11-13T23:17:21.096Z","permissionMode":"auto","origin":{"kind":"human"},"promptSource":"typed","turnOrigin":"human","turnPosition":{"promptIndex":1,"turnIndex":1},"userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/i8142-probe/proj","sessionId":"64c63eeb-8075-4df5-906a-679028ef264d","version":"2.1.325","gitBranch":"HEAD"}"#;

const SCHEDULED_NOTICE: &str =
    "-# ⏰ scheduled prompt\n```\ni8142 probe line one\nsecond line with `backticks`\n```";
const TYPED_NOTICE: &str =
    "-# ⌨️ tui direct input\n```\ni8142 probe line one\nsecond line with `backticks`\n```";

/// One hook submission against its own transcript file, through the hook observer's own calls.
struct Fire {
    _dir: tempfile::TempDir,
    transcript: PathBuf,
    prompt_id: String,
    observed: ObservedTuiPrompt,
}

impl Fire {
    fn submitted(case: &str) -> Self {
        let dir = tempfile::tempdir().expect("transcript dir");
        let transcript = dir
            .path()
            .join("708eaac7-d46f-4c29-837a-84e79c2d811f.jsonl");
        let prompt_id = format!("i8142-{case}");
        let payload: Value = serde_json::from_str(
            &HOOK_PAYLOAD
                .replace("TRANSCRIPT", transcript.to_str().expect("utf-8 path"))
                .replace(REAL_PROMPT_ID, &prompt_id),
        )
        .expect("hook payload");
        let hook_prompt_id = extract_prompt_id_from_hook_payload(&payload);
        assert_eq!(hook_prompt_id.as_deref(), Some(prompt_id.as_str()));
        note_hook_transcript("claude", hook_prompt_id.as_deref(), &payload);
        let observed = ObservedTuiPrompt {
            hosted_source: None,
            provider: "claude".to_string(),
            tmux_session_name: format!("AgentDesk-claude-i8142-{case}"),
            prompt: extract_prompt_from_hook_payload(&payload).expect("hook prompt"),
            source_event_id: None,
            observed_at: chrono::Utc::now(),
            external_input_lease_generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
            ssh_direct_observation_generation:
                crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
            hook_prompt_id,
            row_prompt_id: None,
            native_turn_id: None,
            steer_echo: false,
        };
        Self {
            _dir: dir,
            transcript,
            prompt_id,
            observed,
        }
    }

    fn row(&self, row: &str) -> String {
        row.replace(REAL_PROMPT_ID, &self.prompt_id)
    }

    fn write(&self, rows: &[&str]) {
        append(
            &self.transcript,
            &rows.iter().map(|row| self.row(row)).collect::<Vec<_>>(),
        );
    }
}

fn append(path: &Path, rows: &[String]) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open transcript");
    for row in rows {
        writeln!(file, "{row}").expect("append row");
    }
}

#[tokio::test]
async fn scheduled_fire_notice_carries_the_scheduled_header() {
    let fire = Fire::submitted("fire");
    fire.write(&[ENQUEUE_ROW, DEQUEUE_ROW, FIRE_ROW, SCHEDULED_USER_ROW]);

    assert!(is_scheduled_fire(&fire.observed).await);
    assert_eq!(direct_prompt_notice(&fire.observed).await, SCHEDULED_NOTICE);
}

#[tokio::test]
async fn typed_input_keeps_the_direct_input_header() {
    let fire = Fire::submitted("typed");
    fire.write(&[TYPED_USER_ROW]);

    assert_eq!(direct_prompt_notice(&fire.observed).await, TYPED_NOTICE);
}

#[tokio::test]
async fn typed_input_whose_row_is_not_written_yet_waits_only_the_short_grace() {
    let fire = Fire::submitted("idle");
    // Before its first row a transcript does not exist at all.
    let started = std::time::Instant::now();
    assert_eq!(direct_prompt_notice(&fire.observed).await, TYPED_NOTICE);
    assert!(started.elapsed() >= ROW_WAIT_STEP * QUEUE_WAIT_STEPS as u32);

    let rows = std::fs::read_to_string(&fire.transcript).unwrap_or_default();
    assert_eq!(
        origin_in_rows(&rows, &fire.prompt_id, &fire.observed.prompt).wait_steps(),
        QUEUE_WAIT_STEPS
    );
}

#[tokio::test]
async fn input_absorbed_into_a_running_turn_waits_only_the_short_grace() {
    // Real rows: the hook of an input typed into a running turn carried that turn's prompt id,
    // and no `user` row was ever written for the input.
    let mut fire = Fire::submitted("absorbed");
    fire.observed.prompt = "queued human input i8142".to_string();
    fire.write(&[
        TYPED_USER_ROW,
        r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-11-13T23:29:27.640Z","sessionId":"s","content":"queued human input i8142"}"#,
    ]);
    let rows = std::fs::read_to_string(&fire.transcript).expect("transcript");
    let still_queued = origin_in_rows(&rows, &fire.prompt_id, &fire.observed.prompt);
    assert_eq!(still_queued, Origin::Unwritten { dequeued: false });

    fire.write(&[
        r#"{"type":"queue-operation","operation":"remove","timestamp":"2026-11-13T23:29:36.688Z","sessionId":"s","content":"queued human input i8142","reason":"absorbed_mid_turn","commandUuid":"f97b0864-98b4-4c3d-b797-bab9960256b9","deliveryId":"c10a7c9e-21b3-49bd-950a-985b0eed57c1"}"#,
        DEQUEUE_ROW,
    ]);
    let rows = std::fs::read_to_string(&fire.transcript).expect("transcript");
    let absorbed = origin_in_rows(&rows, &fire.prompt_id, &fire.observed.prompt);
    assert_eq!(absorbed, Origin::Unwritten { dequeued: false });
    assert_eq!(absorbed.wait_steps(), QUEUE_WAIT_STEPS);

    assert_eq!(
        direct_prompt_notice(&fire.observed).await,
        "-# ⌨️ tui direct input\n```\nqueued human input i8142\n```"
    );
}

#[tokio::test]
async fn scheduled_row_that_lands_after_the_hook_is_waited_for() {
    let fire = Fire::submitted("late");
    // Nothing of the fire is flushed when the hook fires: queue rows land inside the grace,
    // the `user` row after it.
    let (path, queue_rows, fire_rows) = (
        fire.transcript.clone(),
        vec![
            fire.row(ENQUEUE_ROW),
            fire.row(DEQUEUE_ROW),
            fire.row(FIRE_ROW),
        ],
        vec![fire.row(SCHEDULED_USER_ROW)],
    );
    let writer = tokio::spawn(async move {
        tokio::time::sleep(ROW_WAIT_STEP * 2).await;
        append(&path, &queue_rows);
        tokio::time::sleep(ROW_WAIT_STEP * QUEUE_WAIT_STEPS as u32).await;
        append(&path, &fire_rows);
    });

    assert_eq!(direct_prompt_notice(&fire.observed).await, SCHEDULED_NOTICE);
    writer.await.expect("late writer");
}

#[tokio::test]
async fn dequeued_input_whose_row_never_lands_falls_back_to_direct_input() {
    let fire = Fire::submitted("never");
    fire.write(&[ENQUEUE_ROW, DEQUEUE_ROW, FIRE_ROW]);

    let started = std::time::Instant::now();
    assert_eq!(direct_prompt_notice(&fire.observed).await, TYPED_NOTICE);
    assert!(started.elapsed() >= ROW_WAIT_STEP * ROW_WAIT_STEPS as u32);
}

#[tokio::test]
async fn transcript_without_scheduled_marks_keeps_the_direct_input_header() {
    // An older Claude wrote the fire's row with no origin fields at all.
    let fire = Fire::submitted("old");
    let unmarked = r#"{"parentUuid":null,"isSidechain":false,"promptId":"3fc992ba-8e68-43c2-a178-6ed88df586d3","type":"user","message":{"role":"user","content":"i8142 probe line one\nsecond line with `backticks`"},"uuid":"e240fedf-dea7-4efe-a5ba-120d919918f7","timestamp":"2026-11-13T23:24:00.344Z"}"#;
    fire.write(&[ENQUEUE_ROW, DEQUEUE_ROW, unmarked]);

    assert_eq!(direct_prompt_notice(&fire.observed).await, TYPED_NOTICE);
}

#[tokio::test]
async fn input_queued_under_a_running_scheduled_prompt_id_is_not_scheduled() {
    // A person's input queued during the scheduled turn may be submitted under that turn's id.
    let mut fire = Fire::submitted("shared");
    fire.write(&[ENQUEUE_ROW, DEQUEUE_ROW, FIRE_ROW, SCHEDULED_USER_ROW]);
    fire.observed.prompt = "stop that and report".to_string();
    let typed = TYPED_USER_ROW.replace(
        r#"i8142 probe line one\nsecond line with `backticks`"#,
        "stop that and report",
    );
    fire.write(&[typed.as_str()]);

    assert_eq!(
        direct_prompt_notice(&fire.observed).await,
        "-# ⌨️ tui direct input\n```\nstop that and report\n```"
    );
}

#[tokio::test]
async fn prompt_without_a_known_hook_transcript_is_not_scheduled() {
    let mut fire = Fire::submitted("unknown");
    fire.write(&[ENQUEUE_ROW, DEQUEUE_ROW, FIRE_ROW, SCHEDULED_USER_ROW]);

    let mut scanner_row = fire.observed.clone();
    scanner_row.hook_prompt_id = None;
    assert_eq!(direct_prompt_notice(&scanner_row).await, TYPED_NOTICE);

    fire.observed.provider = "codex".to_string();
    assert_eq!(direct_prompt_notice(&fire.observed).await, TYPED_NOTICE);
}

#[test]
fn origin_is_read_only_from_the_prompts_own_row() {
    let id = REAL_PROMPT_ID;
    let text = "i8142 probe line one\nsecond line with `backticks`";
    let rows = |rows: &[&str]| rows.join("\n");

    assert_eq!(
        origin_in_rows(
            &rows(&[ENQUEUE_ROW, DEQUEUE_ROW, FIRE_ROW, SCHEDULED_USER_ROW]),
            id,
            text
        ),
        Origin::Scheduled
    );
    // The fire row alone names no prompt id, so it decides nothing.
    assert_eq!(
        origin_in_rows(&rows(&[ENQUEUE_ROW, DEQUEUE_ROW, FIRE_ROW]), id, text),
        Origin::Unwritten { dequeued: true }
    );
    assert_eq!(
        origin_in_rows(&rows(&[ENQUEUE_ROW, FIRE_ROW]), id, text),
        Origin::Unwritten { dequeued: false }
    );
    assert_eq!(
        origin_in_rows(&rows(&[DEQUEUE_ROW, FIRE_ROW]), id, text),
        Origin::Unwritten { dequeued: false }
    );
    assert_eq!(
        origin_in_rows(&rows(&[TYPED_USER_ROW]), id, text),
        Origin::Typed
    );
    // Another prompt's scheduled row, and a torn copy of this one, decide nothing.
    assert_eq!(
        origin_in_rows(
            &rows(&[SCHEDULED_USER_ROW, &SCHEDULED_USER_ROW[..200]]),
            "other-id",
            text
        ),
        Origin::Unwritten { dequeued: false }
    );
    assert_eq!(
        origin_in_rows(&SCHEDULED_USER_ROW[..200], id, text),
        Origin::Unwritten { dequeued: false }
    );
    // A person quoting the marks in a typed input does not carry them.
    let quoting = TYPED_USER_ROW.replace(
        r#"i8142 probe line one\nsecond line with `backticks`"#,
        r#"\"turnOrigin\":\"scheduled\""#,
    );
    assert_eq!(
        origin_in_rows(&quoting, id, r#""turnOrigin":"scheduled""#),
        Origin::Typed
    );
}

#[test]
fn a_turn_wide_scheduled_origin_alone_is_not_a_scheduled_fire() {
    let id = REAL_PROMPT_ID;
    let text = "i8142 probe line one\nsecond line with `backticks`";
    // An input typed into a scheduled turn carries that turn's origin and no fire mark.
    let typed_in_scheduled_turn =
        TYPED_USER_ROW.replace(r#""turnOrigin":"human""#, r#""turnOrigin":"scheduled""#);
    assert_ne!(typed_in_scheduled_turn, TYPED_USER_ROW);
    assert_eq!(
        origin_in_rows(&typed_in_scheduled_turn, id, text),
        Origin::Typed
    );
    // A scheduled turn's compact summary keeps every mark of the fire.
    let summary = SCHEDULED_USER_ROW.replace(
        r#""isMeta":true"#,
        r#""isMeta":true,"isCompactSummary":true"#,
    );
    assert_ne!(summary, SCHEDULED_USER_ROW);
    assert_eq!(origin_in_rows(&summary, id, text), Origin::Typed);
    // Either fire mark is enough: Claude 2.1.295 wrote no `wakeupSource`.
    for mark in [
        r#""wakeupSource":"schedule_wakeup","#,
        r#""scheduledTaskId":"abbf09df","#,
    ] {
        let row = SCHEDULED_USER_ROW.replace(mark, "");
        assert_ne!(row, SCHEDULED_USER_ROW);
        assert_eq!(origin_in_rows(&row, id, text), Origin::Scheduled, "{mark}");
    }
}

#[test]
fn scheduled_class_differs_from_a_typed_input_only_in_its_notice() {
    use super::super::injected_prompt_policy::{
        InjectedPromptClass, classify_injected_prompt,
        injected_prompt_suppresses_user_turn_lifecycle,
    };
    let text = "i8142 probe line one\nsecond line with `backticks`";
    let scheduled = classify_injected_prompt_with_origin(text, true);
    assert_eq!(scheduled, InjectedPromptClass::ScheduledPrompt);
    assert_eq!(
        classify_injected_prompt_with_origin(text, false),
        InjectedPromptClass::HumanTuiDirect
    );
    let typed = InjectedPromptClass::HumanTuiDirect;
    assert_eq!(
        scheduled.is_human_active_turn(),
        typed.is_human_active_turn()
    );
    assert_eq!(
        scheduled.suppresses_user_turn_lifecycle(),
        typed.suppresses_user_turn_lifecycle()
    );
    assert_eq!(
        scheduled.still_delivers_assistant_output(),
        typed.still_delivers_assistant_output()
    );
    assert_eq!(
        scheduled.is_subagent_notification_event(),
        typed.is_subagent_notification_event()
    );
    assert_eq!(
        injected_prompt_suppresses_user_turn_lifecycle(scheduled, None),
        injected_prompt_suppresses_user_turn_lifecycle(typed, None)
    );
    assert_eq!(
        format_direct_prompt_notification(scheduled, "claude", "", text),
        SCHEDULED_NOTICE
    );
    assert_eq!(
        format_direct_prompt_notification(typed, "claude", "", text),
        TYPED_NOTICE
    );

    // A scheduled mark never turns a machine event or a slash control into a scheduled prompt.
    for machine in [
        "<task-notification><task-id>t</task-id></task-notification>",
        "<command-name>/loop</command-name><command-args>5m</command-args>",
        "This session is being continued from a previous conversation",
    ] {
        assert_eq!(
            classify_injected_prompt_with_origin(machine, true),
            classify_injected_prompt(machine),
            "{machine}"
        );
    }
}
