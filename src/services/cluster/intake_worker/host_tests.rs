use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// Neither `Clone` nor `Copy`, like the runtime the Discord layer hands in.
struct FixtureHost {
    lifecycle: IntakeWorkerLifecycle,
    shutting_down: Arc<AtomicBool>,
}

impl IntakeHost for FixtureHost {
    fn intake_lifecycle(&self) -> &IntakeWorkerLifecycle {
        &self.lifecycle
    }

    fn shutdown_probe(&self) -> ShutdownProbe {
        let flag = Arc::clone(&self.shutting_down);
        Box::new(move || flag.load(Ordering::Acquire))
    }
}

fn execute<'a>(
    _: &'a Arc<serenity::http::Http>,
    _: &'a Arc<FixtureHost>,
    _: &'a str,
    _: IntakeRequest,
    _: PendingUploads,
) -> IntakeTurnFuture<'a> {
    Box::pin(async { Ok(IntakeExecution::Executed) })
}

fn resolve<'a>(_: &'a FixtureHost, _: ChannelId) -> ResolveRuntimeFuture<'a, FixtureHost> {
    Box::pin(async { Err("the fixture resolves no runtime".to_string()) })
}

const PORT: IntakeTurnPort<FixtureHost> = IntakeTurnPort {
    checkpoint_waits: |_| false,
    execute,
    resolve_runtime: resolve,
};

fn checkpoint_of<S>(port: IntakeTurnPort<S>) -> CheckpointWaits {
    port.checkpoint_waits
}

#[test]
fn the_port_passes_by_value_for_a_host_that_is_neither_clone_nor_copy() {
    let port = PORT;
    let (first, second) = (checkpoint_of(port), checkpoint_of(port));
    let channel = ChannelId::new(7_353_201);
    assert!(!first(channel) && !second(channel));
}

#[test]
fn a_shutdown_probe_reads_its_hosts_flag_live() {
    let host = FixtureHost {
        lifecycle: IntakeWorkerLifecycle::default(),
        shutting_down: Arc::new(AtomicBool::new(false)),
    };
    let probe = host.shutdown_probe();
    assert!(!probe());
    host.shutting_down.store(true, Ordering::Release);
    assert!(probe());
    assert!(!host.intake_lifecycle().admission_is_fenced());
}
