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
