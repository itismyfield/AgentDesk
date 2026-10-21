//! Publication validates external observations before taking the owner and watch fences.
use super::*;

impl Stamp {
    pub(in crate::services::discord::turn_presence::activity) async fn publish<R>(
        &self,
        watch: &Mutex<Watch>,
        publish: impl FnOnce() -> R,
    ) -> Option<R> {
        self.publish_checked(watch, || ())?;
        if let Some(snapshot) = &self.snapshot {
            if !snapshot.revalidate(self.namespace.as_ref()?).await {
                return None;
            }
        }
        #[cfg(test)]
        crate::services::discord::queue_dispatch::kickoff::owner_race_pause(
            self.channel,
            "activity_before_final_publication",
        )
        .await;
        self.publish_checked(watch, publish)
    }

    fn publish_checked<R>(&self, watch: &Mutex<Watch>, publish: impl FnOnce() -> R) -> Option<R> {
        if self.superseded {
            return None;
        }
        if crate::config::runtime_root() != self.runtime_root {
            return None;
        }
        let config = crate::config_live_reload::current();
        if !match (&self.config, &config) {
            (Some(old), Some(now)) => Arc::ptr_eq(old, now),
            (None, None) => true,
            _ => false,
        } || !self.namespace.as_ref().is_none_or(Observation::current)
            || !self
                .snapshot
                .as_ref()
                .and_then(|s| s.ownership.as_ref())
                .is_none_or(ownership::OwnerInputs::files_current)
        {
            return None;
        }
        #[cfg(test)]
        owner_tests::before_owner_publication(self.channel.get());
        let shared = self.shared.upgrade()?;
        let apply = || -> Result<Option<R>, &'static str> {
            let guard = watch.try_lock().map_err(|_| "watch_busy")?;
            let core = shared.core.try_lock().map_err(|_| "core_busy")?;
            if shared
                .tmux_watchers
                .channel_binding(&self.channel)
                .is_some()
                || core
                    .sessions
                    .get(&self.channel)
                    .and_then(|s| s.channel_name.as_ref())
                    != self.basis.as_ref()
                || guard.key.is_some()
                || (guard.generation, guard.revision) != (self.generation, self.revision)
                || self
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| *s.files.seq_watch.borrow() != s.files.seq)
            {
                return Ok(None);
            }
            Ok(Some(publish()))
        };
        let result = match self.snapshot.as_ref().and_then(|s| s.ownership.as_ref()) {
            Some(inputs) => inputs.with_current(apply).and_then(std::convert::identity),
            None => apply(),
        };
        result.map_err(ownership::rejected).ok().flatten()
    }
}
