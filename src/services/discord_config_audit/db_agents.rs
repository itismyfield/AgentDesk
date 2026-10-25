//! Compares the `agents` table with the YAML roster and syncs it when asked.

use super::*;

pub(super) fn audit_db_agents(
    config: &Config,
    yaml_present: bool,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> Result<(), String> {
    let db_agents = match load_db_agents_pg(config) {
        Ok(Some(agents)) => agents,
        Ok(None) => {
            report
                .actions
                .push("skipped DB agent audit because PostgreSQL is not enabled".to_string());
            return Ok(());
        }
        Err(error) => {
            report.warnings.push(format!(
                "Failed to load agents from PostgreSQL during config audit: {error}; legacy fallback disabled"
            ));
            return Ok(());
        }
    };
    let yaml_agents = config
        .agents
        .iter()
        .map(|agent| (agent.id.clone(), ComparableAgent::from_yaml(agent)))
        .collect::<BTreeMap<_, _>>();

    let pre_sync_drift = compute_db_agent_drift(&yaml_agents, &db_agents, yaml_present);

    if dry_run || !yaml_present {
        apply_db_agent_drift(report, pre_sync_drift);
        return Ok(());
    }

    // #3692: this config-audit sync runs before `startup_reseed`, so it needs the
    // same leader-ownership gate. Only a single-node deployment or the configured
    // cluster leader runs the destructive config→DB agent sync. A cluster
    // worker/auto node reports drift (without warnings — divergence from the
    // leader-owned roster is expected on a worker) and must not mutate the shared
    // agents table, otherwise it clobbers the leader's roster.
    if !crate::db::postgres::shared_config_sync_enabled(config) {
        report.actions.push(
            "skipped DB agent sync on non-leader cluster node; the leader owns the shared agents roster (#3692)"
                .to_string(),
        );
        apply_db_agent_drift(report, pre_sync_drift.without_warnings());
        return Ok(());
    }

    match sync_db_agents_pg(config, &config.agents) {
        Ok(Some(count)) => {
            report.storage.synced_agents = Some(count);
            report.actions.push(format!(
                "synced {} agent definitions from agentdesk.yaml into the PostgreSQL agents table",
                count
            ));
            match load_db_agents_pg(config) {
                Ok(Some(post_sync_agents)) => {
                    let post_sync_drift =
                        compute_db_agent_drift(&yaml_agents, &post_sync_agents, yaml_present);
                    apply_db_agent_drift(report, post_sync_drift);
                }
                Ok(None) => {
                    apply_db_agent_drift(report, pre_sync_drift.without_warnings());
                    report.warnings.push(
                        "Unable to verify PostgreSQL agents after config sync because no database pool was returned"
                            .to_string(),
                    );
                }
                Err(error) => {
                    apply_db_agent_drift(report, pre_sync_drift.without_warnings());
                    report.warnings.push(format!(
                        "Failed to verify PostgreSQL agents after config sync: {error}"
                    ));
                }
            }
            return Ok(());
        }
        Ok(None) => {
            apply_db_agent_drift(report, pre_sync_drift);
        }
        Err(error) => {
            apply_db_agent_drift(report, pre_sync_drift);
            report.warnings.push(format!(
                "Failed to sync agents from agentdesk.yaml into PostgreSQL: {error}; legacy fallback disabled"
            ));
        }
    }

    Ok(())
}
pub(super) fn compute_db_agent_drift(
    yaml_agents: &BTreeMap<String, ComparableAgent>,
    db_agents: &BTreeMap<String, ComparableAgent>,
    yaml_present: bool,
) -> DbAgentDrift {
    let mut drift = DbAgentDrift::default();

    for (agent_id, yaml_agent) in yaml_agents {
        match db_agents.get(agent_id) {
            None => {
                drift.storage.missing_agents.push(agent_id.clone());
                if yaml_present {
                    drift.warnings.push(format!(
                        "DB is missing agent '{}' from agentdesk.yaml; agentdesk.yaml will restore it",
                        agent_id
                    ));
                }
            }
            Some(db_agent) => {
                let differing_fields = compare_comparable_agents(yaml_agent, db_agent);
                if !differing_fields.is_empty() {
                    drift.storage.mismatched_agents.push(agent_id.clone());
                    drift.warnings.push(format!(
                        "DB agent '{}' differs from agentdesk.yaml on {}; agentdesk.yaml wins",
                        agent_id,
                        differing_fields.join(", ")
                    ));
                }
            }
        }
    }

    for agent_id in db_agents.keys() {
        if yaml_agents.contains_key(agent_id) {
            continue;
        }
        if yaml_present {
            drift.storage.extra_agents.push(agent_id.clone());
            drift.warnings.push(format!(
                "DB contains extra agent '{}' not present in agentdesk.yaml; agentdesk.yaml will remove it",
                agent_id
            ));
        }
    }

    drift.storage.missing_agents.sort();
    drift.storage.extra_agents.sort();
    drift.storage.mismatched_agents.sort();
    drift
}

pub(super) fn apply_db_agent_drift(report: &mut ConfigAuditReport, drift: DbAgentDrift) {
    report.storage.missing_agents = drift.storage.missing_agents;
    report.storage.extra_agents = drift.storage.extra_agents;
    report.storage.mismatched_agents = drift.storage.mismatched_agents;
    report.warnings.extend(drift.warnings);
}

pub(super) fn sync_db_agents_pg(
    config: &Config,
    agents: &[AgentDef],
) -> Result<Option<usize>, String> {
    if !crate::db::postgres::database_enabled(config) {
        return Ok(None);
    }

    let config = config.clone();
    let agents = agents.to_vec();
    crate::utils::async_bridge::block_on_result(
        async move {
            let Some(pool) = crate::db::postgres::connect(&config).await? else {
                return Ok(None);
            };
            let count = crate::db::postgres::sync_agents_from_config_pg(&pool, &agents).await?;
            Ok(Some(count))
        },
        |message| message,
    )
}

pub(super) fn load_db_agents_pg(
    config: &Config,
) -> Result<Option<BTreeMap<String, ComparableAgent>>, String> {
    if !crate::db::postgres::database_enabled(config) {
        return Ok(None);
    }

    let config = config.clone();
    crate::utils::async_bridge::block_on_result(
        async move {
            let Some(pool) = crate::db::postgres::connect(&config).await? else {
                return Ok(None);
            };
            let rows = crate::db::agents::load_all_agent_channel_bindings_pg(&pool)
                .await
                .map_err(|error| format!("Failed to query postgres agent audit rows: {error}"))?;

            let mut agents = BTreeMap::new();
            for (id, bindings) in rows {
                agents.insert(
                    id.clone(),
                    ComparableAgent {
                        id,
                        provider: normalize_provider(bindings.provider.as_deref())
                            .unwrap_or_else(|| "claude".to_string()),
                        discord_channel_id: normalize_optional_string(bindings.discord_channel_id),
                        discord_channel_alt: normalize_optional_string(
                            bindings.discord_channel_alt,
                        ),
                        discord_channel_cc: normalize_optional_string(bindings.discord_channel_cc),
                        discord_channel_cdx: normalize_optional_string(
                            bindings.discord_channel_cdx,
                        ),
                    },
                );
            }
            Ok(Some(agents))
        },
        |message| message,
    )
}
