//! `tui_o.repost`: the re-post switch. Off unless set, and read from the live config on every check
//! so turning it off stops new re-post work without a restart.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::config::Config;
use crate::services::tui_o::shadow::tap::TuiOConfig;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RepostConfig {
    pub enabled: bool,
}

impl RepostConfig {
    /// Keeps the default out of saved YAML, so a file without the section saves back unchanged.
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn of(config: Option<&TuiOConfig>) -> Self {
        config.map(|config| config.repost).unwrap_or_default()
    }
}

/// Where the effective value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum SnapshotSource {
    /// No live config was installed; the boot config decides.
    Boot,
    /// The last validated live config. A rejected reload never reaches it, so the prior value holds.
    Live,
}

/// The switch value this node acts on, as health reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RepostSnapshot {
    pub enabled: bool,
    /// Live configs observed, counting a reload that left the value unchanged.
    pub generation: u64,
    pub source: SnapshotSource,
}

/// Validated live configs, as `config_live_reload::subscribe` hands them out.
pub(crate) type LiveConfig = watch::Receiver<Option<Arc<Config>>>;

/// One node's effective switch: the boot value until a live config is installed, then the latest.
pub(crate) struct RepostSwitch {
    live: LiveConfig,
    snapshot: RepostSnapshot,
}

impl RepostSwitch {
    pub(crate) fn new(boot: Option<&TuiOConfig>, live: LiveConfig) -> Self {
        let snapshot = RepostSnapshot {
            enabled: RepostConfig::of(boot).enabled,
            generation: 0,
            source: SnapshotSource::Boot,
        };
        let mut switch = Self { live, snapshot };
        let installed = switch.live.borrow_and_update().clone();
        switch.observe(installed);
        switch
    }

    pub(crate) fn current(&mut self) -> RepostSnapshot {
        // A closed sender leaves the last observed value in force.
        if self.live.has_changed().unwrap_or(false) {
            let installed = self.live.borrow_and_update().clone();
            self.observe(installed);
        }
        self.snapshot
    }

    /// Runs `work` only while the switch is on; off returns before `work` runs, so it may hold
    /// every lock, query and write a re-post entry needs.
    pub(crate) fn when_enabled<T>(&mut self, work: impl FnOnce(RepostSnapshot) -> T) -> Option<T> {
        let snapshot = self.current();
        if !snapshot.enabled {
            return None;
        }
        Some(work(snapshot))
    }

    fn observe(&mut self, installed: Option<Arc<Config>>) {
        let Some(config) = installed else {
            return;
        };
        self.snapshot = RepostSnapshot {
            enabled: RepostConfig::of(config.tui_o.as_ref()).enabled,
            generation: self.snapshot.generation + 1,
            source: SnapshotSource::Live,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "server:\n  port: 8791\n";

    fn parse(tui_o: &str) -> Config {
        serde_yaml::from_str(&format!("{SERVER}{tui_o}")).unwrap()
    }

    fn config(tui_o: &str) -> Arc<Config> {
        Arc::new(parse(tui_o))
    }

    #[test]
    fn repost_is_off_by_default_and_a_saved_config_keeps_its_old_form() {
        assert!(!RepostConfig::default().enabled);
        for tui_o in [
            "",
            "tui_o: {}",
            "tui_o:\n  writer:\n    channels: [63250001]\n",
        ] {
            let parsed = parse(tui_o);
            assert!(!RepostConfig::of(parsed.tui_o.as_ref()).enabled, "{tui_o}");
            let saved = serde_yaml::to_string(&parsed).unwrap();
            assert!(!saved.contains("repost"), "{saved}");
        }

        let saved =
            serde_yaml::to_string(&parse("tui_o:\n  repost:\n    enabled: true\n")).unwrap();
        let reparsed: Config = serde_yaml::from_str(&saved).unwrap();
        assert!(RepostConfig::of(reparsed.tui_o.as_ref()).enabled);
    }

    #[test]
    fn repost_key_typo_rejects_the_reload_and_a_section_typo_stays_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        std::fs::write(
            &path,
            format!("{SERVER}tui_o:\n  repost:\n    enabled: true\n"),
        )
        .unwrap();
        assert!(crate::config::load_from_path(&path).is_ok());
        std::fs::write(
            &path,
            format!("{SERVER}tui_o:\n  repost:\n    enabeld: true\n"),
        )
        .unwrap();
        let outcome = crate::config_live_reload::reload_from_path(&path);
        let crate::config_live_reload::ReloadOutcome::Rejected { error } = outcome else {
            panic!("a misspelt key must not install: {outcome:?}");
        };
        assert!(error.contains("enabeld"), "{error}");

        // The outer section stays permissive, so a misspelt section name loads and reads as off.
        std::fs::write(
            &path,
            format!("{SERVER}tui_o:\n  reposts:\n    enabled: true\n"),
        )
        .unwrap();
        let loaded = crate::config::load_from_path(&path).unwrap();
        let (_tx, rx) = watch::channel(Some(Arc::new(loaded)));
        let snapshot = RepostSwitch::new(None, rx).current();
        assert!(!snapshot.enabled);
        assert_eq!(snapshot.source, SnapshotSource::Live);
    }

    #[test]
    fn repost_switch_follows_the_live_config_and_runs_no_work_while_off() {
        let on = config("tui_o:\n  repost:\n    enabled: true\n");
        let boot_on = on.tui_o.clone();
        let (tx, rx) = watch::channel(None);
        let mut switch = RepostSwitch::new(boot_on.as_ref(), rx);
        let boot = switch.current();
        assert_eq!(
            (boot.enabled, boot.generation, boot.source),
            (true, 0, SnapshotSource::Boot)
        );

        let mut ran = 0;
        tx.send_replace(Some(config("tui_o: {}")));
        assert_eq!(switch.when_enabled(|_| ran += 1), None);
        assert_eq!(ran, 0, "off must not run re-post work");
        assert_eq!(switch.current().generation, 1);

        tx.send_replace(Some(on.clone()));
        let seen = switch.when_enabled(|snapshot| {
            ran += 1;
            snapshot
        });
        assert_eq!(ran, 1);
        assert_eq!(
            seen.map(|s| (s.enabled, s.generation, s.source)),
            Some((true, 2, SnapshotSource::Live))
        );

        // A same-value reload is still a new generation; a config without `tui_o` is off.
        tx.send_replace(Some(on));
        assert_eq!(switch.current().generation, 3);
        tx.send_replace(Some(config("")));
        assert!(!switch.current().enabled);
        drop(tx);
        assert!(
            !switch.current().enabled,
            "a closed sender keeps the last value"
        );
    }
}
