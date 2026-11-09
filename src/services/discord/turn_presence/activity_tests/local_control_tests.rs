use super::*;

fn user(id: String, content: String, meta: bool) -> serde_json::Value {
    json!({"type":"user","isMeta":meta,"uuid":id,"message":{"role":"user","content":content}})
}

/// The rows Claude Code writes once an `/effort` selector closes, whether applied or cancelled.
fn effort(id: &str, args: &str, stdout: &str) -> [serde_json::Value; 3] {
    let caveat = "<local-command-caveat>The command below was run directly in Claude Code, not sent \
        to you as a request, and its output goes straight to the user.</local-command-caveat>";
    let command = format!(
        "<command-name>/effort</command-name>\n            <command-message>effort</command-message>\n            <command-args>{args}</command-args>"
    );
    [
        user(format!("{id}-caveat"), caveat.into(), true),
        user(format!("{id}-command"), command, false),
        user(
            format!("{id}-stdout"),
            format!("<local-command-stdout>{stdout}</local-command-stdout>"),
            false,
        ),
    ]
}

/// An `/effort` echo after a closed turn stays idle and never ages into `open_without_progress`;
/// a real prompt still opens, and inside a real turn the echo changes neither busy nor stale.
#[test]
fn an_effort_echo_reads_idle_while_real_prompts_keep_their_turn() {
    let root = tempfile::tempdir().unwrap();
    let stale = STALE_OPEN_AFTER + Duration::from_secs(1);
    for (id, args, stdout) in [
        ("cancelled", "", "Cancelled"),
        ("applied", "high", "Set effort level to high"),
    ] {
        let (probe, path) = claude(&root.path().join(id), &[prompt("a"), end()]);
        assert_eq!(probe.settle(), (Activity::Idle, "closed"));
        write(&path, &effort(id, args, stdout));
        assert_eq!(probe.settle(), (Activity::Idle, "local_only"), "{id}");
        probe.age(stale);
        assert_eq!(probe.ask_presence(), (Activity::Idle, "local_only"), "{id}");
        write(&path, &effort("again", args, stdout));
        assert_eq!(probe.settle(), (Activity::Idle, "local_only"), "{id}");
        write(&path, &[prompt("b")]);
        assert_eq!(probe.settle(), (Activity::Busy, "open"), "{id}");
        write(&path, &effort("inside", "low", "Set effort level to low"));
        assert_eq!(probe.settle(), (Activity::Busy, "open"), "{id}");
        probe.age(stale);
        let unknown = (Activity::Unknown, "open_without_progress");
        assert_eq!(probe.ask(), unknown, "{id}");
        write(&path, &[end()]);
        assert_eq!(probe.settle(), (Activity::Idle, "closed"), "{id}");
    }
}

fn fast_rows(on: bool) -> Vec<serde_json::Value> {
    include_str!("fixtures/fast_transcript_rows.jsonl")
        .lines()
        .skip(if on { 0 } else { 4 })
        .take(if on { 4 } else { 5 })
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assert_fast_echo_stays_idle(on: bool) {
    let root = tempfile::tempdir().unwrap();
    let (probe, path) = claude(root.path(), &[prompt("a"), end()]);
    assert_eq!(probe.settle(), (Activity::Idle, "closed"));
    let rows = fast_rows(on);
    let stdout = rows[3]["message"]["content"].as_str().unwrap();
    assert_eq!(
        crate::services::tui_prompt_control::classify_local_only_slash_control(stdout)
            .unwrap()
            .kind,
        "local-command-stdout"
    );
    write(&path, &rows);
    assert_eq!(probe.settle(), (Activity::Idle, "local_only"));
    probe.age(STALE_OPEN_AFTER + Duration::from_secs(1));
    assert_eq!(probe.ask_presence(), (Activity::Idle, "local_only"));
    write(&path, &[prompt("b")]);
    assert_eq!(probe.settle(), (Activity::Busy, "open"));
    write(&path, &fast_rows(!on));
    assert_eq!(probe.settle(), (Activity::Busy, "open"));
    probe.age(STALE_OPEN_AFTER + Duration::from_secs(1));
    assert_eq!(probe.ask(), (Activity::Unknown, "open_without_progress"));
}

#[test]
fn fast_on_ansi_echo_stays_idle_while_real_prompts_keep_their_turn() {
    assert_fast_echo_stays_idle(true);
}

#[test]
fn fast_off_plain_echo_stays_idle_while_real_prompts_keep_their_turn() {
    assert_fast_echo_stays_idle(false);
}

#[test]
fn an_assistant_after_fast_echo_immediately_makes_the_turn_busy() {
    for on in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let (probe, path) = claude(root.path(), &fast_rows(on));
        assert_eq!(probe.settle(), (Activity::Idle, "local_only"));
        write(
            &path,
            &[
                json!({"type":"assistant","uuid":"answer","apiBlockIndex":0,"message":{
                    "id":"answer","role":"assistant","content":[{"type":"text","text":"Working"}]
                }}),
            ],
        );
        assert_eq!(probe.settle(), (Activity::Busy, "open"));
    }
}

#[test]
fn legacy_system_local_command_fast_rows_do_not_open_a_turn() {
    let root = tempfile::tempdir().unwrap();
    let (probe, path) = claude(root.path(), &[prompt("a"), end()]);
    assert_eq!(probe.settle(), (Activity::Idle, "closed"));
    for on in [true, false] {
        write(
            &path,
            &[json!({"type":"system","subtype":"local_command",
                "content":if on { "Fast mode ON" } else { "Fast mode OFF" }
            })],
        );
        assert_eq!(probe.settle(), (Activity::Idle, "closed"));
    }
}

#[test]
fn loop_command_envelope_still_opens_a_model_turn() {
    let root = tempfile::tempdir().unwrap();
    let command = user(
        "loop".into(),
        "<command-name>/loop</command-name><command-args>5m</command-args>".into(),
        false,
    );
    let (probe, _) = claude(root.path(), &[command]);
    assert_eq!(probe.settle(), (Activity::Busy, "open"));
    probe.age(STALE_OPEN_AFTER + Duration::from_secs(1));
    assert_eq!(probe.ask(), (Activity::Unknown, "open_without_progress"));
}
