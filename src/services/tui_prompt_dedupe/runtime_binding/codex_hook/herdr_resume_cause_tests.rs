use super::*;

#[test]
fn first_resumed_herdr_startup_hook_cannot_publish_the_resume_source() {
    refuses_first_hook("session_start", Some("startup"));
}

#[test]
fn first_resumed_herdr_prompt_hook_cannot_publish_the_resume_source() {
    refuses_first_hook("user_prompt_submit", Some("resume"));
}

fn refuses_first_hook(event: &str, reason: Option<&str>) {
    let fixture = Fixture::new(true, true);
    assert_eq!(
        fixture.hook_event(&fixture.source, event, reason),
        IngressOutcome::NotApplicable(NotApplicableReason::CodexSourceRejected)
    );
    assert!(
        fixture.marker().is_none(),
        "refusal writes no rollout marker"
    );
    let unpublished = fixture.binding();
    assert!(unpublished.output_path.is_empty());
    assert!(unpublished.session_id.is_none());
    assert_eq!(unpublished.last_offset, 0);
    assert!(
        binding_events::binding_events_since(812812, 0)
            .unwrap()
            .is_empty(),
        "the rejected hook cannot become the first Source event"
    );

    assert_eq!(
        fixture.hook(&fixture.source),
        IngressOutcome::Durable(DurableKind::Adopted)
    );
    let events = binding_events::binding_events_since(812812, 0).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cause, binding_events::BindingCause::Resume);
    assert_eq!(
        events[0].execution_nonce.as_deref(),
        Some(fixture.context.execution_nonce.as_str())
    );
    assert!(
        matches!(&events[0].new, binding_events::BindingTarget::Source(source) if source == &fixture.source)
    );
    assert_eq!(
        fixture.marker().unwrap().rollout_start_offset,
        Some(fixture.offset)
    );
    assert_eq!(fixture.binding().last_offset, fixture.offset);
    assert_eq!(fixture.binding().relay_last_offset, Some(fixture.offset));
}
