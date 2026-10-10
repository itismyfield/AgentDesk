#!/usr/bin/env python3
"""Pin mapping expressions and release callers; reject unclassified mutations/aliases.

This lexical census uses cfg item/module boundaries, including inline fixtures.
It records admission syntax/lifetime, not the deferred G2 E1/E2 proof.
"""
from collections import Counter
import os
from pathlib import Path
import re
import tempfile
import unittest

import check_durable_frontier_writer_call_sites as rust
import generate_inventory_docs as inventory

ROOT = Path(__file__).resolve().parents[1]
D = "src/services/discord/"


def cfg_test_files(files):
    # Derive exclusions from declarations, never from file/directory names.
    files = [Path(path).resolve() for path in files]
    declarations = {
        path: inventory._module_file_declarations(path, path.read_text())
        for path in files
    }
    excluded = set()
    while True:
        test, production = set(), set()
        for parent, children in declarations.items():
            for child in children:
                target = test if parent in excluded or child.requires_test else production
                target.update(child.child_paths)
        updated = set(files) & (test - production)
        if updated == excluded:
            return excluded
        excluded = updated


def expressions(source, symbol="thread_parents"):
    result = []
    for ref in re.finditer(r"\b" + symbol + r"\b", source):
        suffix = source[ref.end():]
        method = re.match(r"\s*\.\s*(\w+)\s*\(", suffix)
        if method:
            operation = method[1]
        elif re.match(r"\s*:\s*dashmap\s*::\s*DashMap\s*::\s*new", suffix):
            operation = "initialize"
        elif re.match(r"\s*:", suffix):
            operation = "declare"
        else:
            operation = "unclassified"
        result.append(operation)
    return result


def retain_arguments(source):
    compact = re.sub(r"\s+", "", source)
    arguments = []
    for call in re.finditer(r"\bthread_parents\.retain\(", compact):
        depth = 1
        for end in range(call.end(), len(compact)):
            depth += (compact[end] == "(") - (compact[end] == ")")
            if depth == 0:
                arguments.append(compact[call.end():end])
                break
        else:
            raise AssertionError("unterminated mapping retain")
    return arguments


EXPECTED = {
    "health/snapshot.rs": {"get": 1, "iter": 2, "contains_key": 1},
    "input_runtime/mapping.rs": {"iter": 1},
    "reaction_lifecycle.rs": {"declare": 1, "into_iter": 1, "iter": 1},
    "relay_recovery/apply.rs": {"len": 2, "retain": 1},
    "router/intake_gate.rs": {"get": 1, "remove": 1},
    "router/message_handler/intake_turn/adk_thread.rs": {"contains_key": 1, "insert": 2},
    "router/message_handler/intake_turn.rs": {"contains_key": 1},
    "runtime_bootstrap/shared_data.rs": {"initialize": 1},
    "shared_state.rs": {"declare": 1},
    "turn_finalizer/cleanup.rs": {"retain": 1},
}

RETAIN_ARGUMENTS = {
    "turn_finalizer/cleanup.rs": [
        "|parent,thread|{letremove=*thread==channel_id;ifremove{parents.push(*parent);}!remove}",
    ],
    "relay_recovery/apply.rs": [
        "|parent,thread|{letremove=*parent==channel||*thread==channel;ifremove{removed_parents.push(*parent);}!remove}",
    ],
}

RELEASES = {
    "collect_and_clear_thread_parents": {
        "turn_finalizer/cleanup.rs": 1,
        "router/intake_gate/stale_turn.rs": 2,
        "relay_recovery/apply.rs": 1,
    },
    "kick_thread_parents_after_turn_release": {
        "turn_finalizer/finalize.rs": 1,
        "tmux_watcher/completion_producer.rs": 1,
        "health/recovery.rs": 2,
        "turn_finalizer/guarded_finish_residue.rs": 1,
        "turn_finalizer/cleanup.rs": 1,
    },
}


class Census(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        files = sorted((ROOT / "src").rglob("*.rs"))
        excluded = cfg_test_files(files)
        cls.sources = {
            path.relative_to(ROOT).as_posix(): rust._production_text(path)
            for path in files if path.resolve() not in excluded
        }

    def test_all_production_mapping_expressions_are_classified(self):
        actual = {
            path.removeprefix(D): Counter(expressions(source))
            for path, source in self.sources.items() if expressions(source)
        }
        self.assertEqual(actual, {path: Counter(ops) for path, ops in EXPECTED.items()})

    def test_all_eleven_release_paths_remain_in_the_census(self):
        for helper, expected in RELEASES.items():
            actual = {}
            for path, source in self.sources.items():
                count = sum(not re.search(r"\bfn\s*$", source[:match.start()])
                            for match in re.finditer(r"\b" + helper + r"\s*\(", source))
                if count:
                    actual[path.removeprefix(D)] = count
            self.assertEqual(actual, expected, helper)
        self.assertEqual(sum(RELEASES["kick_thread_parents_after_turn_release"].values())
                         + sum(RELEASES["collect_and_clear_thread_parents"].values())
                         - 1 + 2, 11)  # Remove the wrapper; add two direct deletions.

    def test_retain_predicates_only_remove_edges(self):
        # Retain exposes a mutable value, so operation counts alone miss edge replacement.
        actual = {
            path.removeprefix(D): retain_arguments(source)
            for path, source in self.sources.items() if retain_arguments(source)
        }
        self.assertEqual(actual, RETAIN_ARGUMENTS)
        predicate = RETAIN_ARGUMENTS["turn_finalizer/cleanup.rs"][0]
        changed = predicate.replace("letremove=", "*thread=channel_id;letremove=")
        self.assertNotEqual(retain_arguments("m.thread_parents.retain(" + changed + ");"), [predicate])

    def test_writer_target_admission_and_root_permit_lifetime_remain(self):
        writer = self.sources[D + "router/message_handler/intake_turn/adk_thread.rs"]
        compact = re.sub(r"\s+", "", writer)
        inserts = [m.start() for m in re.finditer(r"thread_parents\.insert\(", compact)]
        self.assertEqual(len(inserts), 2)
        admission = "redirected_permit=crate::services::discord::input_runtime::fence::effect::admit("
        for index, target in enumerate(["tid", "thread.id"]):
            start = compact.index(admission + "provider," + target + ".get(),)")
            bootstrap = compact.index("bootstrap_admitted(redirected_permit.clone(),shared," + target + ",", start)
            self.assertLess(start, bootstrap)
            self.assertLess(bootstrap, inserts[index])
            self.assertNotIn("redirected_permit=None", compact[start:inserts[index]])
            self.assertNotIn("drop(redirected_permit", compact[start:inserts[index]])
        root = re.sub(r"\s+", "", self.sources[D + "router/message_handler/intake_turn.rs"])
        admit = root.index("effect::admit(&deps.shared.provider,request.channel_id.get())")
        run = root.index("effect::run(permit,", admit)
        body = root.index("handle_text_message_admitted", run)
        redirect = root.index("adk_thread::redirect_dispatch(", body)
        self.assertLess(admit, run)
        self.assertLess(body, redirect)
        self.assertIn("letIntakeRequest{intake_outbox_id,channel_id,", root[body:redirect])
        self.assertIn("letoriginal_channel_id=channel_id;", root[body:redirect])

    def test_scanner_rejects_new_mutators_aliases_and_field_replacement(self):
        for operation in ["entry", "get_mut", "iter_mut", "extend", "clear", "insert"]:
            self.assertNotEqual(Counter(expressions("shared.dispatch.thread_parents." + operation + "(x);")), Counter())
        for source in ["let map = &shared.dispatch.thread_parents;", "shared.dispatch.thread_parents = map;", "let D { thread_parents, .. } = state;"]:
            self.assertEqual(expressions(source), ["unclassified"])

    def test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation(self):
        for path, source in self.sources.items():
            # Turn presence has its own private Ports trait.
            if path != D + "turn_presence/activity.rs":
                self.assertIsNone(re.search(r"\bimpl\s*(<[^>]*>)?\s*(?:[\w:]+::)?Ports\s+for", source), path)
            self.assertIsNone(re.search(r"\bSupervisor\s*::\s*(<[^>]*>\s*::\s*)?start\s*\(", source), path)
            self.assertEqual(len(re.findall(r"\bmapping\s*::\s*inspect\s*\(", source)), 0, path)
        # The probe's only map read is one iter() inside Probe::check, over each handed-over runtime.
        probe = re.sub(r"\s+", "", self.sources[D + "input_runtime/mapping.rs"])
        self.assertEqual(Counter(expressions(probe)), {"iter": 1})
        check = probe.index("pub(crate)fncheck(&self,channel:u64)->Check{")
        read = probe.index("forsharedinruntimes{if letSome(edge)=(shared.dispatch.thread_parents.iter())".replace(" ", ""))
        self.assertLess(check, read)
        self.assertNotIn("fn", probe[check + len("pub(crate)fncheck"):read])

    def test_test_items_are_excluded_by_cfg_instead_of_filename(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            parent = root / "lib.rs"
            fixture = root / "fixture.rs"
            parent.write_text('#[cfg(test)]\nmod fixture;\nfn live() { s.thread_parents.insert(p,t); }\n#[cfg(test)]\nmod inline { fn f() { s.thread_parents.insert(p,t); } }\n')
            fixture.write_text("fn fixture() { s.thread_parents.insert(p,t); }")
            self.assertEqual(cfg_test_files([parent, fixture]), {fixture.resolve()})
            self.assertEqual(expressions(rust._production_text(parent)), ["insert"])
            parent.write_text("mod fixture;\n")
            self.assertEqual(cfg_test_files([parent, fixture]), set())
            parent.write_text('#[cfg(any(test, feature="live"))]\nmod fixture;\n')
            self.assertEqual(cfg_test_files([parent, fixture]), set())

    def test_cfg_exclusions_resolve_symlinked_directories(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            real = root / "real"
            real.mkdir()
            linked = root / "linked"
            os.symlink(real, linked, target_is_directory=True)
            parent = linked / "lib.rs"
            fixture = linked / "fixture.rs"
            parent.write_text('#[cfg(test)]\nmod fixture;\n')
            fixture.write_text("fn fixture() { s.thread_parents.insert(p,t); }")
            self.assertEqual(cfg_test_files([parent, fixture]), {fixture.resolve()})
            parent.write_text("mod fixture;\n")
            self.assertEqual(cfg_test_files([parent, fixture]), set())
            parent.write_text('#[cfg(any(test, feature="live"))]\nmod fixture;\n')
            self.assertEqual(cfg_test_files([parent, fixture]), set())

    def test_dormant_guard_rejects_installation_in_other_sources(self):
        guard = Census("test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation")
        helper_path = D + "input_runtime/mapping.rs"
        private_ports_path = D + "turn_presence/activity.rs"
        guard.sources = {helper_path: self.sources[helper_path], private_ports_path: "impl Ports for LivePorts {}"}
        guard.test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation()
        fixtures = [
            "impl crate::services::discord::input_runtime::supervisor::Ports for Live {}",
            "impl<T> input_runtime::supervisor::Ports for Live<T> {}",
            "impl Ports for Live {}",
            "fn activate() { input_runtime::supervisor::Supervisor :: start(); }",
            "fn activate() { Supervisor :: <Live> :: start (); }",
            "fn inspect() { crate::services::discord::input_runtime::supervisor::mapping :: inspect (&parents, channel); }",
        ]
        for source in fixtures:
            with self.subTest(source=source):
                guard.sources = {helper_path: self.sources[helper_path], "src/other_module.rs": source}
                with self.assertRaises(AssertionError):
                    guard.test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation()
        for source in fixtures[-3:]:
            with self.subTest(private_ports_path=source):
                guard.sources = {helper_path: self.sources[helper_path], private_ports_path: source}
                with self.assertRaises(AssertionError):
                    guard.test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation()


if __name__ == "__main__":
    unittest.main()
