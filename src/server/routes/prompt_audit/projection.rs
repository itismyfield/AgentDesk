use std::ops::Range;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::db::{prompt_manifests::PromptManifest, provider_session_prompts::SessionPrompt};
use crate::services::{
    discord::prompt_builder::audit::CanonPrompt,
    memory::{ANCHOR_CONTEXT_END, ANCHOR_CONTEXT_START},
    provider::ProviderKind,
};

use super::redaction;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Origin {
    CurrentSession,
    LatestSaved,
    CanonReassembled,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    CanonFile,
    Memento,
    Runtime,
    Conditional,
}

#[derive(Serialize)]
pub(super) struct Section {
    name: String,
    source: Source,
    path: Option<&'static str>,
    chars: usize,
    text: String,
    masked_count: usize,
}

struct Segment<'a> {
    range: Range<usize>,
    name: &'a str,
    source: Source,
    path: Option<&'static str>,
}

#[derive(Serialize)]
pub(super) struct PromptResponse {
    agent_id: String,
    provider: String,
    session_id: Option<String>,
    origin: Origin,
    session_started_at: Option<DateTime<Utc>>,
    canon_modified_at: Option<DateTime<Utc>>,
    masked_count: usize,
    sections: Vec<Section>,
}

pub(super) fn prompt_response(
    id: &str,
    provider: &ProviderKind,
    current: Option<String>,
    snapshot: Option<&SessionPrompt>,
    manifest: Option<&PromptManifest>,
    canon: &[CanonPrompt],
) -> PromptResponse {
    let (origin, sections) = match snapshot {
        Some(saved) => (
            if current.is_some() {
                Origin::CurrentSession
            } else {
                Origin::LatestSaved
            },
            saved_sections(&saved.system_prompt, manifest, canon),
        ),
        None => (
            Origin::CanonReassembled,
            canon.iter().map(canon_section).collect(),
        ),
    };
    let session_id = snapshot
        .map(|saved| saved.provider_session_id.as_str())
        .or(current.as_deref());
    PromptResponse {
        agent_id: redaction::mask(id).0,
        provider: provider.as_str().to_owned(),
        session_id: session_id.map(|session| redaction::mask(session).0),
        origin,
        session_started_at: snapshot.map(|saved| saved.started_at),
        canon_modified_at: canon.iter().filter_map(|file| file.modified_at).max(),
        masked_count: sections.iter().map(|section| section.masked_count).sum(),
        sections,
    }
}

fn canon_section(file: &CanonPrompt) -> Section {
    let (text, masked_count) = redaction::mask(&file.content);
    Section {
        name: file.name.to_owned(),
        source: Source::CanonFile,
        path: Some(file.path),
        chars: text.chars().count(),
        text,
        masked_count,
    }
}

fn classify(name: &str, source: Option<&str>) -> Option<(Source, Option<&'static str>)> {
    Some(match name {
        "role_prompt" => (Source::CanonFile, Some("agents/role.prompt.md")),
        "shared_agent_rules" if source == Some("settings.load_shared_prompt_for_profile") => {
            (Source::CanonFile, Some("agents/_shared.prompt.md"))
        }
        "shared_agent_rules"
        | "shared_knowledge"
        | "tool_output_efficiency"
        | "role_binding_contract" => (Source::Runtime, None),
        "context_compression"
        | "memory_guidance"
        | "api_friction_guidance"
        | "longterm_catalog"
        | "longterm_memory_contract"
        | "review_rules"
        | "review_tuning"
        | "lite_channel_rules"
        | "queued_turn_rules"
        | "dispatch_contract" => (Source::Conditional, None),
        _ => return None,
    })
}

fn locate(input: &str, content: &str) -> Option<Range<usize>> {
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    let mut matches = input.match_indices(content);
    let (start, _) = matches.next()?;
    matches
        .next()
        .is_none()
        .then_some(start..start + content.len())
}

pub(super) fn anchor_context(input: &str) -> Option<Range<usize>> {
    let end = input.rfind(ANCHOR_CONTEXT_END)? + ANCHOR_CONTEXT_END.len();
    let start = input[..end].rfind(ANCHOR_CONTEXT_START)?;
    Some(start..end)
}

fn saved_sections(
    input: &str,
    manifest: Option<&PromptManifest>,
    canon: &[CanonPrompt],
) -> Vec<Section> {
    let secrets = redaction::ranges(input);
    let mut segments = Vec::new();
    if let Some(range) = anchor_context(input) {
        segments.push(Segment {
            range,
            name: "memento_anchors",
            source: Source::Memento,
            path: None,
        });
    }
    if let Some(manifest) = manifest {
        for layer in &manifest.layers {
            if !layer.enabled || layer.is_truncated {
                continue;
            }
            let name = layer.layer_name.as_str();
            let Some((source, path)) = classify(name, layer.source.as_deref()) else {
                continue;
            };
            if let Some(range) = layer
                .full_content
                .as_deref()
                .and_then(|text| locate(input, text))
            {
                segments.push(Segment {
                    range,
                    name,
                    source,
                    path,
                });
            }
        }
    }
    for file in canon {
        if !segments.iter().any(|segment| segment.name == file.name)
            && let Some(range) = locate(input, &file.content)
        {
            segments.push(Segment {
                range,
                name: file.name,
                source: Source::CanonFile,
                path: Some(file.path),
            });
        }
    }
    segments.sort_by_key(|segment| (segment.range.start, std::cmp::Reverse(segment.range.end)));
    let mut cursor = 0;
    let mut sections = Vec::new();
    for segment in segments {
        if segment.range.start < cursor {
            continue;
        }
        push_section(
            input,
            runtime(cursor..segment.range.start),
            &secrets,
            &mut sections,
        );
        cursor = segment.range.end;
        push_section(input, segment, &secrets, &mut sections);
    }
    push_section(input, runtime(cursor..input.len()), &secrets, &mut sections);
    sections
}

fn runtime(range: Range<usize>) -> Segment<'static> {
    Segment {
        range,
        name: "runtime",
        source: Source::Runtime,
        path: None,
    }
}

fn push_section(
    input: &str,
    segment: Segment<'_>,
    secrets: &[Range<usize>],
    sections: &mut Vec<Section>,
) {
    let raw = &input[segment.range.clone()];
    if raw.trim().is_empty() {
        return;
    }
    let (text, masked_count) = redaction::mask_range(input, segment.range, secrets);
    sections.push(Section {
        name: segment.name.to_owned(),
        source: segment.source,
        path: segment.path,
        chars: text.chars().count(),
        text,
        masked_count,
    });
}
