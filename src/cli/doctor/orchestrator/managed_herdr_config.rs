//! Doctor's view of the dedicated Herdr configs and the herdr launchd job. Only `--fix` writes a
//! config, through the dcserver upkeep's writer; nothing here installs, edits or restarts a job.
use super::*;

#[cfg(unix)]
const ID: &str = "herdr_managed_config";
#[cfg(unix)]
const NAME: &str = "Herdr managed config";
#[cfg(target_os = "macos")]
const JOB_LABEL: &str = "com.agentdesk.herdr";
#[cfg(unix)]
const RESTART: &str = "restart the herdr server with its service manager at least 2.1s after the repair; a server started earlier keeps E7 unverified";

/// A dedicated endpoint on this node in the live config or in the snapshot this process booted.
#[cfg(unix)]
fn relevant(cfg: &config::Config) -> bool {
    use config::session_hosts::{EndpointMode, with_boot};
    let node = cfg.cluster.instance_id.as_deref().map(str::trim);
    with_boot(|boot| boot.is_some())
        || cfg.session_hosts.herdr.endpoints.values().any(|endpoint| {
            endpoint.mode == EndpointMode::LegacyDedicated
                && Some(endpoint.execution_node.trim()) == node
        })
}

/// The snapshot the dcserver booted; elsewhere the hosts a restart would install, read only.
#[cfg(unix)]
fn hosts() -> Result<config::session_hosts::BootSessionHosts, String> {
    use config::session_hosts::{BootSessionHosts, approval_store::FileApprovalStore, migration};
    if let Some(boot) = config::session_hosts::with_boot(|boot| boot.cloned()) {
        return Ok(boot);
    }
    let cfg = config::load().map_err(|error| format!("config unreadable: {error}"))?;
    let candidate = BootSessionHosts::from_config(&cfg)
        .map_err(|error| format!("session_hosts invalid: {error}"))?;
    let selection = futures::executor::block_on(migration::select(
        candidate,
        &FileApprovalStore::runtime(),
        &migration::PgRows(None),
        &migration::NoHoldStore,
        true,
    ));
    Ok(selection.hosts().clone())
}

pub(super) fn fixed(cfg: &config::Config, actions: Vec<FixAction>) -> Vec<FixAction> {
    #[cfg(unix)]
    let mut actions = actions;
    #[cfg(unix)]
    if relevant(cfg) {
        use crate::services::session_host::managed_config::path::PersonalPaths;
        match hosts() {
            Ok(boot) => actions.extend(fixes_for(
                boot.local_node(),
                boot.config(),
                &PersonalPaths::current(),
            )),
            Err(error) => actions.push(FixAction::fail(
                ID,
                NAME,
                format!("{error}; nothing written"),
            )),
        }
    }
    #[cfg(not(unix))]
    let _ = cfg;
    actions
}

pub(super) fn checked(cfg: &config::Config, checks: Vec<Check>) -> Vec<Check> {
    #[cfg(unix)]
    let mut checks = checks;
    #[cfg(unix)]
    if relevant(cfg) {
        use crate::services::session_host::managed_config::path::PersonalPaths;
        let personal = PersonalPaths::current();
        let job = personal
            .home
            .join("Library/LaunchAgents/com.agentdesk.herdr.plist");
        match hosts() {
            Ok(boot) => checks.extend(checks_for(
                boot.local_node(),
                boot.config(),
                &personal,
                &job,
            )),
            Err(error) => checks.push(
                Check::warn(
                    ID,
                    CheckGroup::Core,
                    NAME,
                    error,
                    "Fix the config; no dedicated Herdr home was inspected.",
                )
                .with_subsystem("herdr"),
            ),
        }
    }
    #[cfg(not(unix))]
    let _ = cfg;
    checks
}

/// The `--fix` writer for each dedicated home; a refused or busy home never stops another.
#[cfg(unix)]
pub(super) fn fixes_for(
    node: Option<&str>,
    hosts: &config::session_hosts::SessionHostsConfig,
    personal: &crate::services::session_host::managed_config::path::PersonalPaths,
) -> Vec<FixAction> {
    use crate::services::session_host::managed_config::{EnsureResult, Inspection, ManagedHome};
    let mut actions = Vec::new();
    for target in ManagedHome::of(node, hosts, personal) {
        let at = format!("{} ({})", target.keys.join(","), target.declared.display());
        let home = match &target.home {
            Ok(home) => home,
            Err(error) => {
                actions.push(FixAction::fail(
                    ID,
                    NAME,
                    format!("{at}: home refused, nothing written: {error}"),
                ));
                continue;
            }
        };
        actions.push(match home.ensure() {
            Ok(EnsureResult::Unchanged) => FixAction::ok(
                ID,
                NAME,
                format!("{at}: already canonical, nothing written"),
            ),
            Ok(EnsureResult::Prepared { backup }) => {
                let kept = backup.map_or_else(String::new, |path| {
                    format!("; original kept at {}", path.display())
                });
                FixAction::ok(
                    ID,
                    NAME,
                    format!("{at}: wrote the canonical config{kept}; {RESTART}"),
                )
            }
            Ok(EnsureResult::Recovered) => FixAction::ok(
                ID,
                NAME,
                format!("{at}: completed an interrupted publication; {RESTART}"),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => FixAction::skipped(
                ID,
                NAME,
                format!("{at}: another writer holds the home lock"),
                FixSafety::ReadOnly,
                "retry; the dcserver upkeep retries every 30 seconds",
            ),
            Err(error) => match home.inspect() {
                Inspection::RecordUnreadable(_) => {
                    FixAction::fail(ID, NAME, format!("{at}: {RECORD}"))
                }
                _ => FixAction::fail(ID, NAME, format!("{at}: not repaired: {error}")),
            },
        });
    }
    actions
}

#[cfg(unix)]
const RECORD: &str = "the publication record .agentdesk-config-state is unreadable and is never removed automatically; stop the herdr server, confirm config.toml is canonical, move the record aside, run `agentdesk doctor --fix`, then start the server at least 2.1s later";

/// Read-only: inspects each dedicated home and the herdr job without a lock or any write.
#[cfg(unix)]
pub(super) fn checks_for(
    node: Option<&str>,
    hosts: &config::session_hosts::SessionHostsConfig,
    personal: &crate::services::session_host::managed_config::path::PersonalPaths,
    job: &std::path::Path,
) -> Vec<Check> {
    use crate::services::session_host::managed_config::{Inspection, ManagedHome};
    let targets = ManagedHome::of(node, hosts, personal);
    if targets.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut guidance = Vec::new();
    for target in &targets {
        let at = format!("{} ({})", target.keys.join(","), target.declared.display());
        let (state, advice) = match target.home.as_ref().map(|home| home.inspect()) {
            Err(error) => (format!("home refused: {error}"), Some("Use a dedicated directory owned by the service user, not group- or world-writable and outside personal herdr paths; nothing is written until then.".to_string())),
            Ok(Inspection::Canonical { committed: None }) => ("canonical".to_string(), None),
            Ok(Inspection::Canonical { committed: Some(at) }) => {
                let after = chrono::DateTime::<chrono::Utc>::from(at + std::time::Duration::from_millis(2_100));
                (format!("canonical; E7 accepts a server started after {}", after.to_rfc3339()), None)
            }
            Ok(Inspection::Missing) => ("config missing".to_string(), Some(format!("Run `agentdesk doctor --fix` or let the dcserver upkeep repair it within 30s, then {RESTART}."))),
            Ok(Inspection::Different) => ("config differs from canonical".to_string(), Some(format!("Run `agentdesk doctor --fix` or let the dcserver upkeep repair it within 30s, then {RESTART}."))),
            Ok(Inspection::Preparing) => ("publication interrupted".to_string(), Some(format!("Run `agentdesk doctor --fix` or let the dcserver upkeep complete it within 30s, then {RESTART}."))),
            Ok(Inspection::Unreadable(error)) => (format!("config unreadable: {error}"), Some("Replace config.toml with a single-link regular file; doctor never follows a link.".to_string())),
            Ok(Inspection::RecordUnreadable(error)) => (format!("publication record unreadable: {error}"), Some(format!("Manual: {RECORD}."))),
        };
        lines.push(format!("{at}: {state}"));
        guidance.extend(advice);
    }
    guidance.dedup();
    let detail = lines.join("; ");
    let mut checks = vec![
        if guidance.is_empty() {
            Check::ok(ID, CheckGroup::Core, NAME, detail)
        } else {
            Check::warn(ID, CheckGroup::Core, NAME, detail, guidance.join(" "))
        }
        .with_subsystem("herdr"),
    ];
    #[cfg(target_os = "macos")]
    checks.push(job_check(&targets, job));
    #[cfg(not(target_os = "macos"))]
    let _ = job;
    checks
}

/// The job's next-start definition against the one dedicated home; the running server stays E7's.
#[cfg(target_os = "macos")]
fn job_check(
    targets: &[crate::services::session_host::managed_config::path::Target],
    job: &std::path::Path,
) -> Check {
    const JOB_ID: &str = "herdr_launchd_job";
    const JOB_NAME: &str = "Herdr launchd job";
    const HANDS_OFF: &str = "Edit the job yourself; doctor never installs, edits, loads or restarts it, and E7 still decides admission from the running server.";
    let warn = |detail: String| {
        Check::warn(JOB_ID, CheckGroup::Core, JOB_NAME, detail, HANDS_OFF)
            .with_subsystem("herdr")
            .with_path(job.display().to_string())
    };
    let mut homes: Vec<_> = targets.iter().map(|target| &target.declared).collect();
    homes.sort();
    homes.dedup();
    let [home] = homes.as_slice() else {
        return warn(format!(
            "{} dedicated homes; the one {JOB_LABEL} job cannot be matched to one",
            homes.len()
        ));
    };
    let read = || -> Result<plist::Dictionary, String> {
        let mut bytes = Vec::new();
        fs::File::open(job)
            .and_then(|file| {
                std::io::Read::read_to_end(&mut std::io::Read::take(file, 256 * 1024), &mut bytes)
            })
            .map_err(|error| format!("job definition unreadable: {error}"))?;
        plist::Value::from_reader(std::io::Cursor::new(bytes))
            .map_err(|error| format!("job definition invalid: {error}"))?
            .into_dictionary()
            .ok_or_else(|| "job definition is not a dictionary".to_string())
    };
    let job_plist = match read() {
        Ok(dictionary) => dictionary,
        Err(detail) => return warn(detail),
    };
    let env = job_plist
        .get("EnvironmentVariables")
        .and_then(plist::Value::as_dictionary);
    let value = |key: &str| {
        env.and_then(|env| env.get(key))
            .and_then(plist::Value::as_string)
    };
    let mut drift = Vec::new();
    if job_plist.get("Label").and_then(plist::Value::as_string) != Some(JOB_LABEL) {
        drift.push(format!("Label is not {JOB_LABEL}"));
    }
    for (key, expected) in [
        ("HERDR_CONFIG_PATH", home.join("config.toml")),
        ("XDG_CONFIG_HOME", home.join("xdg")),
    ] {
        if value(key).map(std::path::Path::new) != Some(expected.as_path()) {
            drift.push(format!(
                "{key} is {:?}, expected {}",
                value(key),
                expected.display()
            ));
        }
    }
    if drift.is_empty() {
        Check::ok(
            JOB_ID,
            CheckGroup::Core,
            JOB_NAME,
            format!("next start uses {}", home.display()),
        )
        .with_subsystem("herdr")
    } else {
        warn(drift.join("; "))
    }
}

#[cfg(all(test, unix))]
#[path = "managed_herdr_config_tests.rs"]
mod tests;
