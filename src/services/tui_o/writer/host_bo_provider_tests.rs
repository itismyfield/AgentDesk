//! The production host, rather than a fixture-built port, identifies each actor's provider.

use super::*;

#[tokio::test(start_paused = true)]
async fn bo_production_host_registers_only_its_provider_on_the_ready_transition_port() {
    let _hosts = crate::config::session_hosts::force_for_test(Some("node-a"), &[]);
    for (provider, runtime, name, other) in [
        (
            ShadowProvider::Claude,
            ClaudeTui,
            "claude",
            ShadowProvider::Codex,
        ),
        (
            ShadowProvider::Codex,
            CodexTui,
            "codex",
            ShadowProvider::Claude,
        ),
    ] {
        let (harness, _, source) = switched_over(b"");
        let startup = p5_event(CHANNEL, name, p5::BindingTarget::Source(source));
        p5_log(harness._runtime.path(), CHANNEL, &startup);
        let _selected = test_override::force_channels(&[(CHANNEL, runtime)]);
        let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
        harness.gate.acquired();
        assert!(hosted_as(other, &harness, &io, (true, &ready)).is_empty());
        assert!(
            io.calls().is_empty(),
            "the other provider never starts this channel"
        );
        let tasks = hosted_as(provider, &harness, &io, (true, &ready));
        assert_eq!(
            tasks.len(),
            1,
            "the actual provider started its production host"
        );
        polls(3).await;
        assert!(ready.is_ready(CHANNEL), "{:?}", io.alarms.halted());
        let port = ready
            .transition(CHANNEL)
            .expect("the resumed host registered its actor port");
        assert_eq!(port.provider(), Some(provider));
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        polls(2).await;
        assert!(
            ready.transition(CHANNEL).is_none(),
            "an ended run is no longer selectable"
        );
    }
}
