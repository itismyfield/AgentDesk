use std::collections::BTreeMap;
use std::path::Path;

use crate::runtime_layout;
use crate::services::discord::org_writer::{OrgAgentUpdate, OrgChannelBindingUpdate};

use super::super::OpenClawMigrateArgs;
use super::super::plan::ImportPlan;

pub(super) fn build_org_agent_updates(
    plan: &ImportPlan,
    runtime_root: &Path,
    args: &OpenClawMigrateArgs,
) -> Vec<OrgAgentUpdate> {
    plan.agents
        .iter()
        .filter(|agent| agent.eligible_for_v1)
        .map(|agent| OrgAgentUpdate {
            role_id: agent.final_role_id.clone(),
            display_name: agent.display_name.clone(),
            prompt_file: if args.no_prompts {
                None
            } else {
                Some(
                    runtime_layout::managed_agents_root(runtime_root)
                        .join(&agent.final_role_id)
                        .join("IDENTITY.md")
                        .display()
                        .to_string(),
                )
            },
            provider: agent.mapped_provider.clone(),
            model: agent.model_hint.clone(),
            workspace: if args.no_workspace {
                None
            } else {
                Some(
                    runtime_root
                        .join("openclaw")
                        .join("workspaces")
                        .join(&agent.final_role_id)
                        .display()
                        .to_string(),
                )
            },
        })
        .collect()
}

pub(super) fn build_org_channel_updates(
    plan: &ImportPlan,
    runtime_root: &Path,
    args: &OpenClawMigrateArgs,
) -> Vec<OrgChannelBindingUpdate> {
    if !args.with_channel_bindings {
        return Vec::new();
    }

    let agent_lookup = plan
        .agents
        .iter()
        .filter(|agent| agent.eligible_for_v1)
        .map(|agent| (agent.source_id.as_str(), agent))
        .collect::<BTreeMap<_, _>>();
    let mut updates = Vec::new();

    for binding in plan
        .discord
        .bindings
        .iter()
        .filter(|binding| binding.mode == "live_applicable")
    {
        let Some(agent) = agent_lookup.get(binding.agent_id.as_str()) else {
            continue;
        };
        for channel_id in &binding.channel_ids {
            updates.push(OrgChannelBindingUpdate {
                channel_id: channel_id.clone(),
                agent: agent.final_role_id.clone(),
                workspace: if args.no_workspace {
                    None
                } else {
                    Some(
                        runtime_root
                            .join("openclaw")
                            .join("workspaces")
                            .join(&agent.final_role_id)
                            .display()
                            .to_string(),
                    )
                },
                provider: agent.mapped_provider.clone(),
                model: agent.model_hint.clone(),
            });
        }
    }

    updates
}

pub(super) fn org_task_outputs(org_yaml_path: &Path, with_channel_bindings: bool) -> Vec<String> {
    let mut outputs = vec![org_yaml_path.display().to_string()];
    if with_channel_bindings {
        outputs.push(format!("{}#channels.by_id", org_yaml_path.display()));
    }
    outputs
}
