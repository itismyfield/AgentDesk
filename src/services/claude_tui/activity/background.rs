//! Parent completion never closes children; only exact child-scoped evidence does.
mod child;
use super::{events, source::Source, transcript::Turn};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Background {
    Unknown,
    ChildPending,
    Clear,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Child {
    pub tool_id: String,
    pub tool_name: String,
    pub agent_id: Option<String>,
    pub subagent: bool,
    pub finished: Option<bool>,
    at: DateTime<Utc>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Slot {
    child: Child,
    cursor: Option<child::Cursor>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Children {
    slots: BTreeMap<(Turn, String), Slot>,
    stops: Vec<(String, DateTime<Utc>)>,
    overflow: bool,
    unread: bool,
}
impl Children {
    pub(super) fn merge(&mut self, previous: &Self) {
        self.overflow |= previous.overflow;
        for (key, old) in &previous.slots {
            let slot = self.slots.entry(key.clone()).or_insert_with(|| old.clone());
            if old.child.finished.is_some() {
                slot.child.finished = old.child.finished;
            }
            if let Some(cursor) = &old.cursor
                && slot
                    .cursor
                    .as_ref()
                    .is_none_or(|current| current.through < cursor.through)
            {
                slot.cursor = Some(cursor.clone());
            }
        }
        for (id, at) in &previous.stops {
            if let Some((_, time)) = self.stops.iter_mut().find(|(old, _)| old == id) {
                *time = (*time).max(*at);
            } else if self.stops.len() < 4096 {
                self.stops.push((id.clone(), *at));
            } else {
                self.overflow = true;
            }
        }
        self.reconcile_stops();
    }
    pub(super) fn observe_record(&mut self, turn: Option<&Turn>, row: &Value) {
        let Some(turn) = turn else {
            return;
        };
        let blocks = row["message"]["content"].as_array();
        if row["type"] == "assistant" {
            for block in blocks
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_use")
            {
                let subagent = matches!(block["name"].as_str(), Some("Task" | "Agent"));
                if !subagent && block["input"]["run_in_background"] != true {
                    continue;
                }
                let Some(id) = block["id"]
                    .as_str()
                    .filter(|id| !id.trim().is_empty() && id.trim() == *id)
                else {
                    self.overflow = true;
                    continue;
                };
                let Some(at) = row["timestamp"]
                    .as_str()
                    .and_then(|s| s.parse::<DateTime<Utc>>().ok())
                else {
                    self.overflow = true;
                    continue;
                };
                let key = (turn.clone(), id.to_owned());
                if self
                    .slots
                    .keys()
                    .any(|(old, tool)| tool == id && old != turn)
                {
                    self.overflow = true;
                }
                if self.slots.len() >= 4096 && !self.slots.contains_key(&key) {
                    self.overflow = true;
                    continue;
                }
                self.slots.entry(key).or_insert_with(|| Slot {
                    child: Child {
                        tool_id: id.into(),
                        tool_name: block["name"].as_str().unwrap_or("Background").into(),
                        agent_id: None,
                        subagent,
                        finished: None,
                        at,
                    },
                    cursor: None,
                });
            }
        }
        let results: Vec<_> = blocks
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "tool_result" && b["tool_use_id"].is_string())
            .collect();
        for (i, block) in results.iter().enumerate() {
            let Some(id) = block["tool_use_id"].as_str() else {
                continue;
            };
            let aggregate = if block["toolUseResult"].is_object() {
                *block
            } else if i == 0 {
                row
            } else {
                *block
            };
            let Some(slot) = self.unique_tool(id) else {
                if events::tool_use_result_is_async_launch(aggregate)
                    || events::agent_id_from_tool_use_result(aggregate).is_some()
                    || events::summary_from_tool_use_result(aggregate).is_some()
                {
                    self.overflow = true;
                }
                continue;
            };
            if block["is_error"] == true {
                slot.child.finished = Some(false);
                continue;
            }
            if events::tool_use_result_is_async_launch(aggregate) {
                slot.child.agent_id = events::agent_id_from_tool_use_result(aggregate);
            } else if events::summary_from_tool_use_result(aggregate).is_some() {
                slot.child.finished = Some(true);
            }
        }
        let duplicate_agent = {
            let mut agents = std::collections::HashSet::new();
            self.slots
                .values()
                .filter_map(|slot| slot.child.agent_id.as_ref())
                .any(|id| !agents.insert(id))
        };
        if duplicate_agent {
            self.overflow = true;
        }
        if let Some((id, agent, success)) = notice(row)
            && let Some(slot) = self.unique_tool(id)
            && agent.is_none_or(|agent| slot.child.agent_id.as_deref() == Some(agent))
        {
            slot.child.finished = Some(success);
        }
    }
    fn unique_tool(&mut self, id: &str) -> Option<&mut Slot> {
        let keys: Vec<_> = self
            .slots
            .keys()
            .filter(|(_, tool)| tool == id)
            .cloned()
            .collect();
        if keys.len() != 1 {
            if keys.len() > 1 {
                self.overflow = true;
            }
            return None;
        }
        self.slots.get_mut(&keys[0])
    }
    pub(super) fn stop(&mut self, source: &Source, payload: &Value, at: DateTime<Utc>) {
        let Some(id) = payload["agent_id"].as_str() else {
            return;
        };
        let Some(expected) = child::path(source, id) else {
            return;
        };
        if payload["agent_transcript_path"]
            .as_str()
            .map(std::path::Path::new)
            != Some(expected.as_path())
        {
            return;
        }
        if let Some((_, old)) = self.stops.iter_mut().find(|(old, _)| old == id) {
            *old = (*old).max(at);
        } else {
            if self.stops.len() >= 4096 {
                self.overflow = true;
                return;
            }
            self.stops.push((id.into(), at));
        }
        self.reconcile_stops();
    }
    fn reconcile_stops(&mut self) {
        for (id, at) in &self.stops {
            let keys: Vec<_> = self
                .slots
                .iter()
                .filter(|(_, s)| {
                    s.child.agent_id.as_deref() == Some(id)
                        && s.child.finished.is_none()
                        && *at >= s.child.at
                })
                .map(|(k, _)| k.clone())
                .collect();
            if keys.len() > 1 {
                continue;
            }
            if let Some(key) = keys.first()
                && let Some(slot) = self.slots.get_mut(key)
                && *at >= slot.child.at
            {
                slot.child.finished = Some(true);
            }
        }
    }
    pub(super) fn refresh(&mut self, source: &Source) {
        self.reconcile_stops();
        self.unread = false;
        if self
            .slots
            .values()
            .filter(|s| s.child.finished.is_none())
            .count()
            > 32
        {
            self.unread = true;
            return;
        }
        for slot in self
            .slots
            .values_mut()
            .filter(|s| s.child.finished.is_none())
        {
            if let Some(id) = &slot.child.agent_id {
                match child::read(source, id, slot.child.at, &mut slot.cursor) {
                    Ok(Some(success)) => slot.child.finished = Some(success),
                    Ok(None) => {}
                    Err(()) => self.unread = true,
                }
            }
        }
    }
    pub(super) fn state(&self, source: &Source) -> Background {
        if self.overflow
            || self.unread
            || self.stops.iter().any(|(id, _)| {
                !self
                    .slots
                    .values()
                    .any(|s| s.child.agent_id.as_ref() == Some(id))
            })
        {
            return Background::Unknown;
        }
        if self
            .slots
            .values()
            .any(|s| s.child.finished.is_none() && s.child.at < source.visit)
        {
            return Background::Unknown;
        }
        if self.slots.values().any(|s| s.child.finished.is_none()) {
            Background::ChildPending
        } else {
            Background::Clear
        }
    }
    pub(super) fn children(&self) -> Vec<Child> {
        if self.overflow {
            Vec::new()
        } else {
            self.slots.values().map(|s| s.child.clone()).collect()
        }
    }
}
fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    Some(
        text.split_once(&format!("<{name}>"))?
            .1
            .split_once(&format!("</{name}>"))?
            .0
            .trim(),
    )
}
pub(super) fn notice(row: &Value) -> Option<(&str, Option<&str>, bool)> {
    if row["type"] != "user" || row["isMeta"] != true {
        return None;
    }
    let content = &row["message"]["content"];
    let text = content
        .as_str()
        .or_else(|| {
            let blocks = content.as_array()?;
            (blocks.len() == 1 && blocks[0]["type"] == "text")
                .then(|| blocks[0]["text"].as_str())
                .flatten()
        })?
        .trim();
    let text = text
        .strip_prefix("<task-notification>")?
        .strip_suffix("</task-notification>")?;
    let success = match tag(text, "status")? {
        "completed" | "done" | "finished" | "success" => true,
        "failed" | "error" | "aborted" | "killed" | "stopped" | "cancelled" => false,
        _ => return None,
    };
    Some((tag(text, "tool-use-id")?, tag(text, "task-id"), success))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Children {
        let at = Utc::now();
        let turn = Turn {
            floor: 1,
            prompt: Some("turn".into()),
            at,
        };
        let mut children = Children::default();
        for id in ["a", "b"] {
            children.slots.insert(
                (turn.clone(), id.into()),
                Slot {
                    child: Child {
                        tool_id: id.into(),
                        tool_name: "Agent".into(),
                        agent_id: Some(id.into()),
                        subagent: true,
                        finished: None,
                        at,
                    },
                    cursor: None,
                },
            );
        }
        children
    }
    #[test]
    fn concurrent_child_completions_merge_without_reopening_each_other() {
        let mut left = fixture();
        let mut right = left.clone();
        left.slots
            .values_mut()
            .find(|s| s.child.tool_id == "a")
            .unwrap()
            .child
            .finished = Some(true);
        right
            .slots
            .values_mut()
            .find(|s| s.child.tool_id == "b")
            .unwrap()
            .child
            .finished = Some(false);
        left.merge(&right);
        assert!(left.slots.values().all(|s| s.child.finished.is_some()));
        right.merge(&left);
        assert_eq!(left, right);
    }
    #[test]
    fn only_native_meta_notification_can_close_the_exact_child() {
        let mut children = fixture();
        let turn = children.slots.keys().next().unwrap().0.clone();
        let text = "<task-notification><tool-use-id>a</tool-use-id><task-id>a</task-id><status>completed</status></task-notification>";
        for row in [
            serde_json::json!({"type":"assistant","message":{"content":text}}),
            serde_json::json!({"type":"user","message":{"content":text}}),
        ] {
            children.observe_record(Some(&turn), &row);
            assert!(children.slots.values().all(|s| s.child.finished.is_none()));
        }
        children.observe_record(
            Some(&turn),
            &serde_json::json!({"type":"user","isMeta":true,"message":{"content":text}}),
        );
        assert_eq!(
            children
                .slots
                .values()
                .find(|s| s.child.tool_id == "a")
                .unwrap()
                .child
                .finished,
            Some(true)
        );
        assert!(
            children
                .slots
                .values()
                .find(|s| s.child.tool_id == "b")
                .unwrap()
                .child
                .finished
                .is_none()
        );
    }
}
