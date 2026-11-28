use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedObservation;
use crate::services::herdr_launch::HerdrLaunchEndpoint;

struct Forced {
    real_close: bool,
    key: String,
    row: HostedObservation,
    attempts: usize,
    done: Option<tokio::sync::oneshot::Sender<()>>,
}
static FORCED: LazyLock<Mutex<std::collections::HashMap<u64, Forced>>> =
    LazyLock::new(Default::default);
pub(in crate::services::discord) struct Guard(u64);
impl Drop for Guard {
    fn drop(&mut self) {
        FORCED.lock().unwrap().remove(&self.0);
    }
}
pub(crate) fn force_host(channel: u64, key: String, row: HostedObservation) -> impl Drop {
    install_forced(channel, key, row, false, None)
}

pub(in crate::services::discord) fn install_forced(
    channel: u64,
    key: String,
    row: HostedObservation,
    real_close: bool,
    done: Option<tokio::sync::oneshot::Sender<()>>,
) -> Guard {
    FORCED.lock().unwrap().insert(
        channel,
        Forced {
            real_close,
            key,
            row,
            attempts: 0,
            done,
        },
    );
    Guard(channel)
}

pub(in crate::services::discord) fn close_attempts(channel: u64) -> usize {
    FORCED.lock().unwrap()[&channel].attempts
}

static REFUSED: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(Default::default);
struct Refusing(u64);
impl Drop for Refusing {
    fn drop(&mut self) {
        REFUSED.lock().unwrap().remove(&self.0);
    }
}
/// The channel stays mapped while its judged host refuses every turn.
pub(in crate::services::discord) fn refuse_host(channel: u64) -> impl Drop {
    REFUSED.lock().unwrap().insert(channel);
    Refusing(channel)
}
/// A mapped channel's rowless Herdr host: the session key it names and whether its close fails.
static ROWLESS: LazyLock<Mutex<std::collections::HashMap<u64, (String, bool)>>> =
    LazyLock::new(Default::default);
struct Rowless(u64);
impl Drop for Rowless {
    fn drop(&mut self) {
        ROWLESS.lock().unwrap().remove(&self.0);
    }
}
/// The channel's host takes Herdr turns under `key` with no row; its old pane's close succeeds
/// unless `close_fails`.
pub(crate) fn herdr_host(channel: u64, key: &str, close_fails: bool) -> impl Drop {
    let host = (key.to_string(), close_fails);
    ROWLESS.lock().unwrap().insert(channel, host);
    Rowless(channel)
}
fn plan(row: Option<HostedObservation>) -> TurnHost {
    TurnHost::Herdr(Box::new(HerdrTurnPlan {
        endpoint: HerdrLaunchEndpoint {
            execution_node: "test-node".into(),
            config_key: "test".into(),
            socket_addr: "/not-used".into(),
            herdr_session: "not-used".into(),
        },
        row,
        bootstrap: false,
    }))
}
pub(super) fn judged(channel: u64) -> Option<(String, TurnHost)> {
    if REFUSED.lock().unwrap().contains(&channel) {
        let refusal = crate::services::turn_host::HerdrRefusal::ExecutorNotWired;
        return Some(("claude/t/h:left".into(), TurnHost::Refused(refusal)));
    }
    if let Some((key, _)) = ROWLESS.lock().unwrap().get(&channel) {
        return Some((key.clone(), plan(None)));
    }
    let forced = FORCED.lock().unwrap();
    let f = forced.get(&channel)?;
    Some((f.key.clone(), plan(Some(f.row.clone()))))
}
pub(super) fn close_result(channel: u64) -> Option<Result<(), String>> {
    if let Some((_, fails)) = ROWLESS.lock().unwrap().get(&channel) {
        let failure = "fixed close failure; no legacy input transport exists";
        return Some(if *fails { Err(failure.into()) } else { Ok(()) });
    }
    let mut all = FORCED.lock().unwrap();
    let f = all.get_mut(&channel)?;
    if f.real_close {
        return None;
    }
    f.attempts += 1;
    Some(Err(
        "fixed close failure; no legacy input transport exists".into()
    ))
}
pub(super) fn finished(channel: u64) {
    if let Some(f) = FORCED.lock().unwrap().get_mut(&channel)
        && let Some(done) = f.done.take()
    {
        done.send(()).unwrap();
    }
}

pub(in crate::services::discord) fn retry_waiting(channel: u64, handoff: &str) -> bool {
    WAITING
        .lock()
        .unwrap()
        .contains(&(channel, handoff.to_owned()))
}
