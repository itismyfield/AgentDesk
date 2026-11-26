//! Exact membership facts share the authenticated projection's byte pass.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct MembershipReceipt {
    namespace: String,
    ticket: Ticket,
    row: String,
    kind: String,
    range: (u64, u64),
    frame_hash: String,
    row_digest: String,
}
impl MembershipReceipt {
    pub(crate) fn identity(&self) -> (&str, &Ticket) {
        (&self.namespace, &self.ticket)
    }
    pub(crate) fn record(&self) -> (&str, &str, (u64, u64), &str) {
        (&self.kind, &self.row, self.range, &self.frame_hash)
    }
}

pub(super) struct Fact {
    row: String,
    kind: String,
    range: (u64, u64),
    hashes: Vec<String>,
    digest: String,
}
fn exact_body(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) if !blocks.is_empty() => blocks
            .iter()
            .map(|b| (b["type"] == "text").then(|| b["text"].as_str()).flatten())
            .collect::<Option<Vec<_>>>()
            .map(|texts| texts.join("\n")),
        _ => None,
    }
}
pub(super) fn read(value: &Value, line: &[u8], range: (u64, u64)) -> Option<Fact> {
    if ["isMeta", "isSidechain"].iter().any(|k| value[*k] == true)
        || crate::services::tui_o::shadow::identity::record_is_compact_summary(value)
    {
        return None;
    }
    let (kind, body) = match value["type"].as_str()? {
        "user" => ("user", exact_body(&value["message"]["content"])?),
        "queue-operation" if value["operation"] == "enqueue" => {
            ("enqueue", exact_body(&value["content"])?)
        }
        "attachment"
            if value["attachment"]["type"] == "queued_command"
                && value["attachment"]["commandMode"] == "prompt"
                && value["attachment"]["origin"]["kind"] == "human" =>
        {
            ("command", exact_body(&value["attachment"]["prompt"])?)
        }
        _ => return None,
    };
    let row = if kind == "enqueue" {
        String::new()
    } else {
        value["uuid"].as_str().filter(|r| !r.is_empty())?.to_owned()
    };
    let mut hashes = vec![format!("{:x}", Sha256::digest(body.as_bytes()))];
    if let Some(inner) = crate::services::claude_tui::busy_inject::pasted_inner(&body) {
        hashes.push(format!("{:x}", Sha256::digest(inner.as_bytes())));
    }
    let mut digest = Sha256::new();
    digest.update(line);
    digest.update(b"\n");
    Some(Fact {
        row,
        kind: kind.into(),
        range,
        hashes,
        digest: format!("{:x}", digest.finalize()),
    })
}
/// A row with an id needs its recorded receipt; a queue record without one needs the ticket's
/// single exact enqueue after its offset.
pub(super) fn exact(facts: &[&Fact], registration: &Registration, row: Option<&str>) -> bool {
    if let Some(row) = row {
        return registration.receipts.iter().any(|known| known == row);
    }
    let offset = registration
        .ticket
        .source
        .as_ref()
        .map_or(u64::MAX, |s| s.offset);
    let enqueue = |f: &&&Fact| f.kind == "enqueue" && f.range.0 >= offset;
    let hash = |f: &&&Fact| f.hashes.contains(&registration.exact_frame_hash);
    facts.iter().filter(enqueue).filter(hash).count() == 1
}

/// More than one row of a kind after the ticket's offset carries its exact hash.
pub(super) fn repeated(facts: &[&Fact], registration: &Registration) -> bool {
    let offset = registration
        .ticket
        .source
        .as_ref()
        .map_or(u64::MAX, |s| s.offset);
    ["user", "command", "enqueue"].iter().any(|kind| {
        let same = |f: &&&Fact| f.kind == *kind && f.range.0 >= offset;
        let hash = |f: &&&Fact| f.hashes.contains(&registration.exact_frame_hash);
        facts.iter().filter(same).filter(hash).count() > 1
    })
}
pub(super) fn candidates(
    binding: &SourceBinding,
    facts: &[&Fact],
    registrations: &[Registration],
) -> Vec<MembershipReceipt> {
    let mut result = Vec::new();
    let mut rows = std::collections::HashMap::new();
    let mut duplicates = std::collections::HashSet::new();
    for fact in facts {
        if fact.row.is_empty() {
            continue;
        }
        if rows
            .insert(&fact.row, fact.range)
            .is_some_and(|old| old != fact.range)
        {
            duplicates.insert(&fact.row);
        }
    }
    for fact in facts {
        if !matches!(fact.kind.as_str(), "user" | "command") {
            continue;
        }
        if duplicates.contains(&fact.row)
            || registrations.iter().any(|r| r.receipts.contains(&fact.row))
        {
            continue;
        }
        let owners: Vec<_> = registrations
            .iter()
            .filter(|r| {
                fact.hashes.contains(&r.exact_frame_hash)
                    && (r.ticket.source.is_none()
                        || r.receipts.is_empty()
                        || (facts.iter().any(|known| r.receipts.contains(&known.row))
                            && !facts.iter().any(|known| {
                                r.receipts.contains(&known.row) && known.kind == fact.kind
                            })))
                    && (r.ticket.source.is_none()
                        || (r.matches(binding)
                            && r.ticket
                                .source
                                .as_ref()
                                .is_some_and(|s| s.offset <= fact.range.0)))
            })
            .collect();
        let [owner] = owners.as_slice() else { continue };
        if !owner.matches(binding) {
            continue;
        }
        let offset = owner.ticket.source.as_ref().map_or(u64::MAX, |s| s.offset);
        let matching = |other: &&Fact| {
            other.range.0 >= offset && other.hashes.contains(&owner.exact_frame_hash)
        };
        if facts
            .iter()
            .copied()
            .filter(matching)
            .filter(|other| other.kind == fact.kind)
            .count()
            != 1
            || facts
                .iter()
                .copied()
                .filter(matching)
                .filter(|other| other.kind == "enqueue")
                .count()
                > 1
        {
            continue;
        }
        let Some(observed_hash) = fact.hashes.iter().find(|h| *h == &owner.exact_frame_hash) else {
            continue;
        };
        result.push(MembershipReceipt {
            namespace: owner.namespace.clone(),
            ticket: owner.ticket.clone(),
            row: fact.row.clone(),
            kind: fact.kind.clone(),
            range: fact.range,
            frame_hash: observed_hash.clone(),
            row_digest: fact.digest.clone(),
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_input::human_frame::history::v2::{Registry, native_tests::Fixture};
    use serde_json::json;
    fn fixture() -> (crate::config::TestRuntimeRootGuard, Fixture, SourceBinding) {
        let guard = crate::config::TestRuntimeRootGuard::new();
        let f = Fixture::at(guard.path().join("runtime/receipt.json"), 81489981, true);
        let source = f.ticket.source.as_ref().unwrap();
        let binding = SourceBinding {
            channel_id: f.ticket.channel_id,
            provider: super::super::ShadowProvider::Claude,
            source: crate::services::tui_o::shadow::SourceId {
                session_id: source.session_id.clone(),
                path: source.transcript.clone(),
                dev: source.file[0],
                ino: source.file[1],
            },
        };
        (guard, f, binding)
    }
    #[test]
    fn native_8148_receipt_compaction_markers_are_not_delivery() {
        let (_guard, f, binding) = fixture();
        let regs = f.history.native_classification_registrations().unwrap();
        for marker in ["is_compact_summary", "isCompactSummary"] {
            let mut summary =
                json!({"type":"user","uuid":"summary","message":{"content":"한국어 원본 입력"}});
            summary[marker] = json!(true);
            let line = serde_json::to_vec(&summary).unwrap();
            assert!(
                read(&summary, &line, (0, line.len() as u64 + 1)).is_none(),
                "{marker}: summary is not a receipt fact"
            );
            let rows = [
                json!({"type":"user","uuid":"parent","message":{"content":"parent turn"}}),
                summary,
                json!({"type":"assistant","apiBlockIndex":0,"uuid":"answer","sessionId":"native","message":{"id":"answer","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}),
                json!({"type":"system","subtype":"turn_duration"}),
            ];
            let bytes = rows.iter().map(|r| format!("{r}\n")).collect::<String>();
            std::fs::write(&binding.source.path, &bytes).unwrap();
            let (_, projection) = Projection::reopen(
                binding.clone(),
                bytes.len() as u64,
                &format!("{:x}", Sha256::digest(bytes.as_bytes())),
            )
            .unwrap();
            let projection = projection.unwrap();
            assert!(
                projection.receipts(&regs).is_empty(),
                "{marker}: summary cannot bootstrap a receipt"
            );
            assert!(
                projection.evidence(&regs).unwrap().is_empty(),
                "{marker}: summary cannot execute the input"
            );
            assert!(
                f.history
                    .native_settlement("native-fixture", &f.ticket)
                    .unwrap()
                    .outcome
                    .is_none()
            );
        }
    }

    #[test]
    fn native_8148_projection_compaction_markers_are_not_own_prompts() {
        let (_guard, f, binding) = fixture();
        let regs = f.history.native_classification_registrations().unwrap();
        for marker in ["is_compact_summary", "isCompactSummary"] {
            let mut summary =
                json!({"type":"user","uuid":"summary","message":{"content":"한국어 원본 입력"}});
            summary[marker] = json!(true);
            let rows = [
                json!({"type":"user","uuid":"parent","message":{"content":"parent turn"}}),
                summary,
                json!({"type":"assistant","apiBlockIndex":0,"uuid":"answer","sessionId":"native","message":{"id":"answer","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}),
                json!({"type":"system","subtype":"turn_duration"}),
            ];
            let bytes = rows.iter().map(|r| format!("{r}\n")).collect::<String>();
            std::fs::write(&binding.source.path, &bytes).unwrap();
            let (_, projection) = Projection::reopen(
                binding.clone(),
                bytes.len() as u64,
                &format!("{:x}", Sha256::digest(bytes.as_bytes())),
            )
            .unwrap();
            let projection = projection.unwrap();
            assert!(projection.receipts(&regs).is_empty());
            let analysis = projection.analyze(&regs).unwrap();
            assert!(
                analysis.evidence.is_empty(),
                "{marker}: a summary is not an own Prompt without a confirmed receipt"
            );
            assert!(
                analysis.barriers.is_empty(),
                "{marker}: summary is not a foreign Prompt either"
            );
        }
    }

    #[test]
    fn native_8148_receipt_exact_fact_rejects_normalized_mixed_meta_and_duplicate_uuid() {
        let (_guard, f, binding) = fixture();
        let regs = f.history.native_classification_registrations().unwrap();
        for (body, meta, count) in [
            (json!("한국어 원본 입력"), false, 1),
            (
                json!(
                    "<pasted_content id=\"abcd\">\n한국어 원본 입력\n</pasted_content id=\"abcd\">"
                ),
                false,
                1,
            ),
            (json!(" 한국어 원본 입력 "), false, 0),
            (
                json!([{"type":"text","text":"한국어 원본 입력"},{"type":"image"}]),
                false,
                0,
            ),
            (json!("한국어 원본 입력"), true, 0),
        ] {
            let row = json!({"type":"user","uuid":"own","isMeta":meta,"message":{"content":body}});
            let line = serde_json::to_vec(&row).unwrap();
            let fact = read(&row, &line, (0, line.len() as u64 + 1));
            let facts: Vec<_> = fact.iter().collect();
            assert_eq!(candidates(&binding, &facts, &regs).len(), count, "{row}");
            if let Some(mut second) = read(&row, &line, (1, line.len() as u64 + 2)) {
                second.range.0 += 1;
                assert!(candidates(&binding, &[fact.as_ref().unwrap(), &second], &regs).is_empty());
            }
        }
        let mut unresolved = regs.clone();
        unresolved[0].ticket.source = None;
        let row = json!({"type":"user","uuid":"own","message":{"content":"한국어 원본 입력"}});
        let fact = read(&row, b"row", (0, 4)).unwrap();
        assert!(candidates(&binding, &[&fact], &unresolved).is_empty());
        unresolved.extend(regs);
        unresolved.reverse();
        assert!(candidates(&binding, &[&fact], &unresolved).is_empty());
    }
    #[test]
    fn native_8148_receipt_duplicate_prompts_and_fifo_do_not_choose_the_first_row() {
        let (_guard, f, binding) = fixture();
        let regs = f.history.native_classification_registrations().unwrap();
        let text = "한국어 원본 입력";
        let first = read(
            &json!({"type":"user","uuid":"one","message":{"content":text}}),
            b"one",
            (0, 4),
        )
        .unwrap();
        let second = read(
            &json!({"type":"user","uuid":"two","message":{"content":text}}),
            b"two",
            (4, 8),
        )
        .unwrap();
        assert!(candidates(&binding, &[&first, &second], &regs).is_empty());
        let q1 = read(
            &json!({"type":"queue-operation","operation":"enqueue","content":text}),
            b"queue",
            (0, 6),
        )
        .unwrap();
        let q2 = read(
            &json!({"type":"queue-operation","operation":"enqueue","content":text}),
            b"queue",
            (6, 12),
        )
        .unwrap();
        assert!(candidates(&binding, &[&q1, &q2, &second], &regs).is_empty());
        assert_eq!(candidates(&binding, &[&q1, &second], &regs).len(), 1);
        for removed in [false, true] {
            let mut rows =
                vec![json!({"type":"queue-operation","operation":"enqueue","content":text})];
            if removed {
                rows.push(json!({"type":"queue-operation","operation":"remove","content":text}));
            }
            rows.extend([
                json!({"type":"user","uuid":"later-human","message":{"content":text}}),
                json!({"type":"system","subtype":"turn_duration"}),
            ]);
            let bytes = rows.iter().map(|r| format!("{r}\n")).collect::<String>();
            std::fs::write(&binding.source.path, &bytes).unwrap();
            let (_, projection) = Projection::reopen(
                binding.clone(),
                bytes.len() as u64,
                &format!("{:x}", Sha256::digest(bytes.as_bytes())),
            )
            .unwrap();
            let projection = projection.unwrap();
            assert!(
                !projection
                    .analyze(&regs)
                    .unwrap()
                    .evidence
                    .iter()
                    .any(|e| e.disposition == Disposition::Executed)
            );
            assert!(
                projection.receipts(&regs).is_empty(),
                "removed={removed}: a later human prompt is not this queued input's membership"
            );
        }
    }
    #[test]
    fn native_8148_receipt_command_and_user_are_confirmed_without_reviving_a_later_prompt() {
        let (_guard, f, binding) = fixture();
        let mut regs = f.history.native_classification_registrations().unwrap();
        let text = "한국어 원본 입력";
        let command = read(&json!({"type":"attachment","uuid":"command","attachment":{"type":"queued_command","commandMode":"prompt","origin":{"kind":"human"},"prompt":text}}), b"command",(0,8)).unwrap();
        let user = read(
            &json!({"type":"user","uuid":"user","message":{"content":text}}),
            b"user",
            (8, 13),
        )
        .unwrap();
        assert_eq!(candidates(&binding, &[&command, &user], &regs).len(), 2);
        regs[0].receipts.push("command".into());
        regs[0].candidate = false;
        let next = candidates(&binding, &[&command, &user], &regs);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].row, "user");
        let rows = [
            json!({"type":"attachment","uuid":"command","attachment":{"type":"queued_command","commandMode":"prompt","origin":{"kind":"human"},"prompt":text}}),
            json!({"type":"user","uuid":"user","message":{"content":text}}),
        ];
        let bytes = rows.iter().map(|r| format!("{r}\n")).collect::<String>();
        std::fs::write(&binding.source.path, &bytes).unwrap();
        let (_, projection) = Projection::reopen(
            binding.clone(),
            bytes.len() as u64,
            &format!("{:x}", Sha256::digest(bytes.as_bytes())),
        )
        .unwrap();
        let projected = projection.unwrap().receipts(&regs);
        assert_eq!(
            projected.len(),
            1,
            "historical command confirmation keeps the real user membership"
        );
        assert_eq!(projected[0].row, "user");
        for closed in [false, true] {
            let mut rows = vec![
                json!({"type":"user","uuid":"parent","message":{"content":"parent turn"}}),
                json!({"type":"queue-operation","operation":"enqueue","content":text}),
                json!({"type":"queue-operation","operation":"dequeue"}),
                json!({"type":"attachment","uuid":"command","attachment":{"type":"queued_command","commandMode":"prompt","origin":{"kind":"human"},"prompt":text}}),
            ];
            if closed {
                rows.extend([
                    json!({"type":"assistant","apiBlockIndex":0,"uuid":"answer","sessionId":"native","message":{"id":"answer","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}),
                    json!({"type":"system","subtype":"turn_duration"}),
                ]);
            }
            rows.push(json!({"type":"user","uuid":"user","message":{"content":text}}));
            let bytes = rows.iter().map(|r| format!("{r}\n")).collect::<String>();
            std::fs::write(&binding.source.path, &bytes).unwrap();
            let (_, projection) = Projection::reopen(
                binding.clone(),
                bytes.len() as u64,
                &format!("{:x}", Sha256::digest(bytes.as_bytes())),
            )
            .unwrap();
            let receipts = projection.unwrap().receipts(&regs);
            assert_eq!(
                receipts.len(),
                usize::from(!closed),
                "closed={closed}: command/user membership stays inside its own run"
            );
        }

        regs[0].receipts.push("user".into());
        let late = read(
            &json!({"type":"user","uuid":"later","message":{"content":text}}),
            b"late",
            (13, 18),
        )
        .unwrap();
        assert!(candidates(&binding, &[&command, &user, &late], &regs).is_empty());
    }
    #[test]
    fn native_8148_receipt_full_ticket_hash_stamp_key_and_fingerprint_compatibility() {
        let (_guard, f, _binding) = fixture();
        let hash = format!("{:x}", Sha256::digest("한국어 원본 입력".as_bytes()));
        let stamp = || {
            super::super::registrations_fingerprint(
                &f.history.native_classification_registrations().unwrap(),
            )
            .unwrap()
        };
        assert!(
            f.history
                .try_native_receipt(
                    "native-fixture",
                    &f.ticket,
                    ("user", "own", (0, 4), 4, &hash),
                    &stamp()
                )
                .is_err()
        );
        f.history
            .note_native_any_key("native-fixture", &f.ticket)
            .unwrap();
        let fp = stamp();
        let before = f.history.json().unwrap();
        for axis in 0..6 {
            let mut ticket = f.ticket.clone();
            let mut wrong_hash = hash.clone();
            let mut wrong_stamp = fp.clone();
            let mut range = (0, 4);
            match axis {
                0 => ticket
                    .extra
                    .insert("changed".into(), json!(true))
                    .map(|_| ())
                    .unwrap_or(()),
                1 => ticket.source.as_mut().unwrap().target["execution_nonce"] = json!("other"),
                2 => wrong_hash.push('x'),
                3 => wrong_stamp.push('x'),
                4 => range = (4, 0),
                _ => range = (0, 5),
            }
            assert!(
                f.history
                    .try_native_receipt(
                        "native-fixture",
                        &ticket,
                        ("user", "own", range, 4, &wrong_hash),
                        &wrong_stamp
                    )
                    .is_err(),
                "axis {axis}"
            );
            assert_eq!(f.history.json().unwrap(), before);
        }
        let mut regs = f.history.native_classification_registrations().unwrap();
        regs[0].exact_frame_hash.push('x');
        assert_eq!(
            super::super::registrations_fingerprint(&regs).unwrap(),
            fp,
            "reader compatibility"
        );
        f.history
            .try_native_receipt(
                "native-fixture",
                &f.ticket,
                ("user", "own", (0, 4), 4, &hash),
                &fp,
            )
            .unwrap();
        assert_ne!(stamp(), fp);
        let once = f.history.json().unwrap();
        f.history
            .try_native_receipt(
                "native-fixture",
                &f.ticket,
                ("user", "own", (0, 4), 4, &hash),
                &stamp(),
            )
            .unwrap();
        assert_eq!(f.history.json().unwrap(), once);
        assert!(
            f.history
                .native_settlement("native-fixture", &f.ticket)
                .unwrap()
                .outcome
                .is_none()
        );
    }
    #[test]
    fn native_8148_receipt_registry_keeps_twin_and_occupied_row_exclusion() {
        for occupied in [false, true] {
            let (_guard, f, _binding) = fixture();
            f.history
                .note_native_any_key("native-fixture", &f.ticket)
                .unwrap();
            let hash = format!("{:x}", Sha256::digest("한국어 원본 입력".as_bytes()));
            let stamp = || {
                super::super::registrations_fingerprint(
                    &f.history.native_classification_registrations().unwrap(),
                )
                .unwrap()
            };
            if occupied {
                f.history
                    .try_native_receipt(
                        "native-fixture",
                        &f.ticket,
                        ("user", "own", (0, 4), 4, &hash),
                        &stamp(),
                    )
                    .unwrap();
            }
            let mut input = f.input.clone();
            input["attempt"]["nonce"] = json!("twin");
            f.history.prepare("native-fixture", &input).unwrap();
            let mut ticket = f.ticket.clone();
            ticket.key["attempt"]["nonce"] = json!("twin");
            f.history.prepare_native("native-fixture", &ticket).unwrap();
            f.history
                .note_native_any_key("native-fixture", &ticket)
                .unwrap();
            let before = f.history.json().unwrap();
            let target = if occupied { &ticket } else { &f.ticket };
            assert!(
                f.history
                    .try_native_receipt(
                        "native-fixture",
                        target,
                        ("user", "own", (0, 4), 4, &hash),
                        &stamp()
                    )
                    .is_err()
            );
            assert_eq!(f.history.json().unwrap(), before);
        }
        let (guard, f, _binding) = fixture();
        f.history
            .note_native_any_key("native-fixture", &f.ticket)
            .unwrap();
        let hash = format!("{:x}", Sha256::digest("한국어 원본 입력".as_bytes()));
        let mut malformed = f.history.json().unwrap();
        malformed["live"][0]["events"] = json!([{"key":"user:own","kind":"user","row":null}]);
        std::fs::write(
            guard.path().join("runtime/receipt.json"),
            malformed.to_string(),
        )
        .unwrap();
        let stamp = super::super::registrations_fingerprint(
            &f.history.native_classification_registrations().unwrap(),
        )
        .unwrap();
        assert!(
            f.history
                .try_native_receipt(
                    "native-fixture",
                    &f.ticket,
                    ("user", "own", (0, 4), 4, &hash),
                    &stamp
                )
                .is_err()
        );
        assert_eq!(f.history.json().unwrap(), malformed);
    }
    #[test]
    fn native_8148_receipt_hash_change_keeps_fingerprint_but_rejects_cas() {
        let (guard, f, _binding) = fixture();
        f.history
            .note_native_any_key("native-fixture", &f.ticket)
            .unwrap();
        let before = f.history.native_classification_registrations().unwrap();
        let fp = super::super::registrations_fingerprint(&before).unwrap();
        let hash = before[0].exact_frame_hash.clone();
        let mut changed = f.history.json().unwrap();
        changed["live"][0]["registration"]["frame"]["hash"] = json!("different-exact-hash");
        std::fs::write(
            guard.path().join("runtime/receipt.json"),
            changed.to_string(),
        )
        .unwrap();
        assert_eq!(
            super::super::registrations_fingerprint(
                &f.history.native_classification_registrations().unwrap()
            )
            .unwrap(),
            fp
        );
        assert!(
            f.history
                .try_native_receipt(
                    "native-fixture",
                    &f.ticket,
                    ("user", "own", (0, 4), 4, &hash),
                    &fp
                )
                .is_err()
        );
        assert_eq!(f.history.json().unwrap(), changed);
    }
    #[test]
    fn native_8148_receipt_visible_is_not_durable_ack_on_reopened_registry() {
        use crate::services::discord::runtime_store::durable_publish::flush_probe::{
            Plan, Step, Watch,
        };
        let (guard, f, _binding) = fixture();
        f.history
            .note_native_any_key("native-fixture", &f.ticket)
            .unwrap();
        let hash = format!("{:x}", Sha256::digest("한국어 원본 입력".as_bytes()));
        let fp = super::super::registrations_fingerprint(
            &f.history.native_classification_registrations().unwrap(),
        )
        .unwrap();
        let reopened = Registry::at(guard.path().join("runtime/receipt.json"));
        {
            let _fault = Watch::new(
                &guard.path().join("runtime"),
                Plan {
                    fail: vec![Step::ParentSync],
                    block: None,
                },
            );
            assert!(
                f.history
                    .try_native_receipt(
                        "native-fixture",
                        &f.ticket,
                        ("user", "own", (0, 4), 4, &hash),
                        &fp
                    )
                    .is_err()
            );
            assert_eq!(
                reopened.native_classification_registrations().unwrap()[0].receipts,
                vec!["own"]
            );
            assert!(
                reopened
                    .try_native_settlement("native-fixture", &f.ticket)
                    .is_err()
            );
        }
        reopened
            .try_native_settlement("native-fixture", &f.ticket)
            .unwrap();
        assert_eq!(
            reopened.native_classification_registrations().unwrap()[0].receipts,
            vec!["own"]
        );
    }
}
