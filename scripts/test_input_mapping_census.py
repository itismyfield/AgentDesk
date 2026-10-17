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


# The deleted dormant input module; split so this guard never matches its own source.
REMOVED_MODULE = "input" + "_runtime"


def module_revivals(root):
    """Paths under src that bring the removed module back by name or path."""
    word = re.compile(r"\b" + REMOVED_MODULE + r"\b")
    revived = []
    for path in sorted((root / "src").rglob("*")):
        relative = path.relative_to(root)
        if REMOVED_MODULE in relative.parts or path.stem == REMOVED_MODULE or (
            path.suffix == ".rs" and path.is_file() and word.search(path.read_text())
        ):
            revived.append(relative.as_posix())
    return revived


INPUT_ROOT = "src/services/tui_input"
# The ledger-owned input actor and its persistence closure; gate/ and pane/ under actor/ stay live.
REMOVED_INPUT_FILES = (
    "actor/capability", "actor/resume", "actor/token", "actor/witness",
    "attempt", "blob", "durable", "handover", "ledger", "receipt_identity", "rows", "transition",
)
REMOVED_INPUT_NAMES = (
    "Input" + "Actor", "EXTERNAL" + "_KEY_BASE", "EXTERNAL" + "_KEY_SPAN", "EXTERNAL" + "_KEY_END",
    "is_" + "external_key", "external" + "_key_v1",
)


def input_engine_revivals(root):
    """Paths that bring a removed tui_input engine file, its mod line or its type back."""
    base = root / INPUT_ROOT
    revived = set()
    for stem in REMOVED_INPUT_FILES:
        for candidate in (base / (stem + ".rs"), base / stem):
            if candidate.exists():
                revived.add(candidate.relative_to(root).as_posix())
    mods = re.compile(r"\bmod\s+(?:" + "|".join(Path(s).name for s in REMOVED_INPUT_FILES) + r")\b")
    names = re.compile(r"\b(?:" + "|".join(REMOVED_INPUT_NAMES) + r")\b")
    for path in sorted((root / "src").rglob("*.rs")):
        text = path.read_text()
        if names.search(text) or (base in path.parents and mods.search(text)):
            revived.add(path.relative_to(root).as_posix())
    return sorted(revived)


EXPECTED = {
    "health/snapshot.rs": {"get": 1, "iter": 2, "contains_key": 1},
    "reaction_lifecycle.rs": {"declare": 1, "into_iter": 1, "iter": 1},
    "relay_recovery/apply.rs": {"len": 2, "retain": 1},
    "router/intake_gate.rs": {"get": 1, "remove": 1},
    "router/message_handler/intake_turn/adk_thread.rs": {"contains_key": 1, "insert": 1, "iter": 1, "retain": 1},
    "router/message_handler/intake_turn.rs": {"contains_key": 1},
    "runtime_bootstrap/shared_data.rs": {"initialize": 1},
    "shared_state.rs": {"declare": 1},
    "turn_finalizer/cleanup.rs": {"retain": 1},
}

RETAIN_ARGUMENTS = {
    "router/message_handler/intake_turn/adk_thread.rs": [
        "|parent,thread|{letremove=*thread==self.target&&Some(*parent)!=self.parent;ifremove{replaced.push(*parent);}!remove}",
    ],
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
        "turn_finalizer/policy_commit.rs": 1,
    },
    "kick_thread_parents_after_turn_release": {
        "turn_finalizer/finalize.rs": 1,
        "turn_finalizer/cleanup/terminal_release.rs": 1,
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

    def test_all_thirteen_release_paths_remain_in_the_census(self):
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
                         - 1 + 3, 13)  # Remove the wrapper; add three direct deletions.

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

    def test_intake_takes_its_role_only_from_the_claimed_route(self):
        # The provider choice has no observable effect on an unprotected channel, so pin the reads.
        actual = {
            path.removeprefix(D): Counter(expressions(source, "role_overrides"))
            for path, source in self.sources.items()
            if "router/message_handler/intake_turn" in path and expressions(source, "role_overrides")
        }
        expected = {"router/message_handler/intake_turn/adk_thread.rs": {"get": 1, "insert": 1, "remove": 1}}
        self.assertEqual(actual, {path: Counter(ops) for path, ops in expected.items()})

    def test_writer_target_admission_and_claimed_route_order_remain(self):
        writer = self.sources[D + "router/message_handler/intake_turn/adk_thread.rs"]
        compact = re.sub(r"\s+", "", writer)
        # The one writer publishes a resolved route; each route names its parent only after its
        # target is admitted and bootstrapped, and intake publishes it only once the claim succeeded.
        writes = [m.start() for m in re.finditer(r"thread_parents\.insert\(", compact)]
        self.assertEqual(len(writes), 1)
        self.assertLess(compact.index("fnpublish(&self,shared:&Arc<SharedData>,provider:&ProviderKind)"), writes[0])
        # A replaced parent edge is released with its queue kick, like every other edge removal.
        kick = compact.index("cleanup::kickoff_thread_parents_after_finalize(shared,provider,replaced);")
        self.assertLess(writes[0], kick)
        self.assertLess(writes[0], compact.index("asyncfnredirect_dispatch("))
        inserts = [m.start() for m in re.finditer(r"parent=Some\(channel_id\);", compact)]
        self.assertEqual(len(inserts), 2)
        # A reused thread passes its host judgment before it is bootstrapped.
        start = compact.index("host_refusal::admitted_uploads(http,shared,provider,target,input)")
        for index, target in enumerate(["tid", "thread.id"]):
            bootstrap = compact.index("bootstrap_thread_session(shared," + target + ",", start)
            self.assertLess(bootstrap, inserts[index])
        root = re.sub(r"\s+", "", self.sources[D + "router/message_handler/intake_turn.rs"])
        claim = root.index(".complete_mailbox_claim(")
        self.assertEqual(root.count("route.publish(shared,&provider);"), 1)
        self.assertLess(claim, root.index("ifstarted{route.publish(shared,&provider);}"))
        body = root.index("pub(super)asyncfnhandle_text_message(")
        redirect = root.index("adk_thread::redirect_dispatch(", body)
        self.assertIn("letIntakeRequest{intake_outbox_id,channel_id,", root[body:redirect])
        self.assertIn("letoriginal_channel_id=channel_id;", root[body:redirect])
        self.assertNotIn("handle_text_message_" + "admitted", root)
        self.assertNotIn("intake_gate::" + "ledger", root)

    def test_scanner_rejects_new_mutators_aliases_and_field_replacement(self):
        for operation in ["entry", "get_mut", "iter_mut", "extend", "clear", "insert"]:
            self.assertNotEqual(Counter(expressions("shared.dispatch.thread_parents." + operation + "(x);")), Counter())
        for source in ["let map = &shared.dispatch.thread_parents;", "shared.dispatch.thread_parents = map;", "let D { thread_parents, .. } = state;"]:
            self.assertEqual(expressions(source), ["unclassified"])

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


    def test_removed_input_module_stays_removed(self):
        self.assertEqual(module_revivals(ROOT), [])

    def test_module_revivals_catch_paths_and_identifiers(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            live = root / "src" / "live.rs"
            live.write_text("fn live() { let input = 1; }\n")
            self.assertEqual(module_revivals(root), [])
            for revival in ["mod " + REMOVED_MODULE + ";", "use super::" + REMOVED_MODULE + "::fence;"]:
                with self.subTest(revival=revival):
                    live.write_text(revival)
                    self.assertEqual(module_revivals(root), ["src/live.rs"])
            live.write_text("fn live() {}\n")
            (root / "src" / (REMOVED_MODULE + ".rs")).write_text("")
            self.assertEqual(module_revivals(root), ["src/" + REMOVED_MODULE + ".rs"])

    def test_removed_input_engine_stays_removed(self):
        self.assertEqual(input_engine_revivals(ROOT), [])

    def test_input_engine_revivals_catch_files_mods_and_names(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            actor = root / INPUT_ROOT / "actor"
            actor.mkdir(parents=True)
            live = root / INPUT_ROOT / "actor.rs"
            live.write_text("pub mod gate;\npub mod pane;\n")
            outside = root / "src" / "token.rs"
            outside.write_text("mod token;\n")
            self.assertEqual(input_engine_revivals(root), [])
            relative = live.relative_to(root).as_posix()
            for revival in ["pub(crate) mod " + Path(REMOVED_INPUT_FILES[-1]).name + ";", "struct " + REMOVED_INPUT_NAMES[0] + ";"]:
                with self.subTest(revival=revival):
                    live.write_text(revival)
                    self.assertEqual(input_engine_revivals(root), [relative])
            live.write_text("pub mod gate;\n")
            (root / INPUT_ROOT / (REMOVED_INPUT_FILES[0] + ".rs")).write_text("")
            self.assertEqual(input_engine_revivals(root), [INPUT_ROOT + "/" + REMOVED_INPUT_FILES[0] + ".rs"])


if __name__ == "__main__":
    unittest.main()
