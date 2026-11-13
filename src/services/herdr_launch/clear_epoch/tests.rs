use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Point {
    Checked,
    Authorized,
}
type Hook = Arc<dyn Fn() + Send + Sync>;
static HOOKS: LazyLock<Mutex<HashMap<(Key, Point), Hook>>> = LazyLock::new(Default::default);
pub(crate) struct Watch(Key, Point);

pub(crate) fn watch(
    owner: &HostedOwner,
    at: Point,
    hook: impl Fn() + Send + Sync + 'static,
) -> Watch {
    let key = (
        owner.provider.clone(),
        owner.discord_token_hash.clone(),
        owner.channel_id.parse().unwrap(),
    );
    HOOKS
        .lock()
        .unwrap()
        .insert((key.clone(), at), Arc::new(hook));
    Watch(key, at)
}

pub(super) fn point(key: &Key, at: Point) {
    let hook = HOOKS.lock().unwrap().get(&(key.clone(), at)).cloned();
    if let Some(hook) = hook {
        hook();
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        HOOKS.lock().unwrap().remove(&(self.0.clone(), self.1));
    }
}

fn owner(channel: u64) -> HostedOwner {
    HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "epoch-test".into(),
        channel_id: channel.to_string(),
        logical_key: "epoch-logical".into(),
        owner_node: "test".into(),
        runtime_root: "test".into(),
    }
}

#[tokio::test]
async fn checked_permission_rechecks_clear_and_keeps_sticky_overlap() {
    let owner = owner(812801);
    let launch = Launching::for_launch(&owner).unwrap();
    let checked = launch.scope().check_epoch().unwrap();
    let clear = begin_clear(&owner.provider, &owner.discord_token_hash, 812801);
    assert!(
        checked.begin_effect().is_err(),
        "a completed check is not effect permission"
    );
    assert!(clear.overlapped_startup());
    drop(checked);
    drop(launch);
    clear.wait_idle().await;
    assert!(
        clear.overlapped_startup(),
        "startup completion cannot erase clear overlap"
    );
}

#[tokio::test]
async fn authorized_effect_keeps_clear_waiting_until_io_returns() {
    let owner = owner(812802);
    let launch = Launching::for_launch(&owner).unwrap();
    let scope = launch.scope();
    let permit = scope.check_epoch().unwrap().begin_effect().unwrap();
    scope.finish_start();
    let clear = begin_clear(&owner.provider, &owner.discord_token_hash, 812802);
    assert!(
        clear.overlapped_startup(),
        "an active effect keeps overlap after startup completion"
    );
    let waiting = clear.wait_idle();
    tokio::pin!(waiting);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            waiting.as_mut(),
            cx
        )))
        .await
        .is_pending()
    );
    drop(permit);
    waiting.await;
}

#[test]
fn scoped_registration_isolated_by_provider_token_and_channel() {
    let owner = owner(812803);
    let launch = Launching::enter(&owner).unwrap();
    let worker = current().unwrap();
    let _other = begin_clear("codex", &owner.discord_token_hash, 812803);
    let _other_token = begin_clear("claude", "another-token", 812803);
    let _other_channel = begin_clear("claude", &owner.discord_token_hash, 812804);
    assert!(worker.check_epoch().is_ok());
    drop(launch);
    assert!(current().is_none());
}

#[test]
fn finished_scope_is_not_reused_for_a_later_launch() {
    let owner = owner(812805);
    let first = Launching::enter(&owner).unwrap();
    first.scope().finish_start();
    let second = Launching::for_launch(&owner).unwrap();
    assert!(!Arc::ptr_eq(&first.scope(), &second.scope()));
    let clear = begin_clear(&owner.provider, &owner.discord_token_hash, 812805);
    assert!(clear.overlapped_startup());
    assert!(Launching::for_launch(&owner).is_err());
}
