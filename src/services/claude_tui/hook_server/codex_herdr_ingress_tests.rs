use super::*;

struct ListedExecution {
    logical: String,
    nonce: String,
}

impl ListedExecution {
    fn new(h: &Harness, nonce: &str) -> Self {
        dedupe::install_herdr_execution(&h.context.tmux_session, nonce);
        Self {
            logical: h.context.tmux_session.clone(),
            nonce: nonce.to_owned(),
        }
    }
}

impl Drop for ListedExecution {
    fn drop(&mut self) {
        dedupe::release_herdr_execution(&self.logical, &self.nonce);
    }
}

fn native_start() -> Harness {
    let mut h = Harness::new();
    h.command = h.payload["session_id"].as_str().unwrap().to_owned();
    h.payload["source"] = json!("startup");
    write(&h.path, &h.header);
    h
}

fn assert_proceeds_unchanged(h: &Harness) {
    let before = h.snapshot();
    let (status, body) = h.hook();
    assert_eq!(status, 202);
    assert_eq!(body["binding_observation"], "Proceed(NoSessionSwitch)");
    assert_eq!(h.snapshot(), before);
}

#[test]
fn registered_herdr_native_start_records_startup_for_its_logical_key() {
    let h = native_start();
    let _listed = ListedExecution::new(&h, &h.context.execution_nonce);
    let before = h.events().len();
    let (status, body) = h.hook();
    assert_eq!(status, 202);
    assert_eq!(body["binding_observation"], "Durable(Adopted)");
    let events = h.events();
    assert_eq!(events.len(), before + 1);
    let event = events.last().unwrap();
    assert_eq!(event.tmux_session, h.context.tmux_session);
    assert_eq!(
        event.execution_nonce.as_deref(),
        Some(h.context.execution_nonce.as_str())
    );
    assert_eq!(event.cause, binding_events::BindingCause::Startup);
    assert_eq!(source(event).session_id, h.command);
    assert_eq!(
        h.binding().output_path,
        source(event).path.display().to_string()
    );
}

#[test]
fn unregistered_tmux_native_start_preserves_binding_and_history() {
    let h = native_start();
    assert_eq!(
        dedupe::herdr_listed_execution(&h.context.tmux_session),
        None
    );
    assert_proceeds_unchanged(&h);
}

#[test]
fn replaced_herdr_nonce_cannot_route_even_while_its_spawn_marker_still_matches() {
    let h = native_start();
    let _listed = ListedExecution::new(&h, "replacement-execution");
    assert_proceeds_unchanged(&h);
}

#[test]
fn released_herdr_execution_leaves_late_native_start_unchanged() {
    let h = native_start();
    drop(ListedExecution::new(&h, &h.context.execution_nonce));
    assert_proceeds_unchanged(&h);
}

#[test]
fn registered_herdr_native_start_still_checks_channel_and_superseded_source() {
    let mut h = native_start();
    let _listed = ListedExecution::new(&h, &h.context.execution_nonce);
    let before = h.snapshot();
    let mut wrong_channel = h.context.clone();
    wrong_channel.channel_id = Some(8746);
    let (status, body) = h.send(&h.payload, Some(wrong_channel), "wrong-channel");
    assert_eq!(status, 202);
    assert_eq!(
        body["binding_observation"],
        "NotApplicable(CodexContextUnavailable)"
    );
    assert_eq!(h.snapshot(), before);

    assert_eq!(h.hook().1["binding_observation"], "Durable(Adopted)");
    let first_payload = h.payload.clone();
    let next_id = uuid::Uuid::new_v4().to_string();
    let next_path = h
        .path
        .with_file_name(format!("rollout-2026-09-27T21-06-14-{next_id}.jsonl"));
    let mut header = h.header.clone();
    header["payload"]["id"] = json!(next_id);
    write(&next_path, &header);
    h.command = next_id.clone();
    h.payload["session_id"] = json!(next_id);
    h.payload["transcript_path"] = json!(next_path);
    h.payload["source"] = json!("clear");
    assert_eq!(h.hook().1["binding_observation"], "Durable(Adopted)");
    let stable = h.snapshot();
    h.command = first_payload["session_id"].as_str().unwrap().to_owned();
    let (status, body) = h.send(&first_payload, Some(h.context.clone()), "late-startup");
    assert_eq!(status, 202);
    assert_eq!(
        body["binding_observation"],
        "NotApplicable(CodexSourceRejected)"
    );
    assert_eq!(h.snapshot(), stable);
}
