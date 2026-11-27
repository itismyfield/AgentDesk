use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Point {
    Checked,
    Authorized,
    Projecting,
}
type Hook = Arc<dyn Fn() + Send + Sync>;
static HOOKS: LazyLock<Mutex<HashMap<(Key, Point), Hook>>> = LazyLock::new(Default::default);
pub struct Watch(Key, Point);

pub fn watch(owner: &HostedOwner, at: Point, hook: impl Fn() + Send + Sync + 'static) -> Watch {
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

pub(crate) fn before_projection(owner: &HostedOwner) {
    let key = (
        owner.provider.clone(),
        owner.discord_token_hash.clone(),
        owner.channel_id.parse().unwrap(),
    );
    point(&key, Point::Projecting);
}

impl Drop for Watch {
    fn drop(&mut self) {
        HOOKS.lock().unwrap().remove(&(self.0.clone(), self.1));
    }
}
