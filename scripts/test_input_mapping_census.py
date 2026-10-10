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


TOKEN = re.compile(r"->|::|[A-Za-z_]\w*|\S")
# The one private turn-presence Ports impl; any other Ports impl, or a second copy, is installation.
ALLOWED_PORTS = {(D + "turn_presence/activity.rs", "impl Ports for LivePorts"): 1}


def skip_generics(tokens, index):
    """Index after a balanced <...>; `->` is one token and `>>` closes twice."""
    depth = 0
    for end in range(index, len(tokens)):
        depth += (tokens[end] == "<") - (tokens[end] == ">")
        if depth == 0:
            return end + 1
    raise AssertionError("unbalanced generic arguments")


def ports_impls(source):
    """Normalized headers of every `impl ... Ports ... for` item, whatever its generics."""
    tokens, headers = TOKEN.findall(source), []
    for start, token in enumerate(tokens):
        if token != "impl":
            continue
        index = skip_generics(tokens, start + 1) if tokens[start + 1:start + 2] == ["<"] else start + 1
        trait = []
        while index < len(tokens) and tokens[index] not in {"for", "{", "where", ";"}:
            if tokens[index] == "<":
                index = skip_generics(tokens, index)
                continue
            trait.append(tokens[index])
            index += 1
        if index == len(tokens):
            raise AssertionError("unterminated impl header")
        if tokens[index] == "for" and [t for t in trait if t != "::"][-1:] == ["Ports"]:
            end = next((i for i in range(index, len(tokens)) if tokens[i] in {"{", "where"}), None)
            if end is None:
                raise AssertionError("unterminated Ports impl")
            headers.append(" ".join(tokens[start:end]))
    return headers


def supervisor_starts(source):
    """`Supervisor::start(` with any turbofish, nested or spaced."""
    tokens, calls = TOKEN.findall(source), 0
    for index, token in enumerate(tokens):
        if token != "Supervisor" or tokens[index + 1:index + 2] != ["::"]:
            continue
        at = index + 2
        if tokens[at:at + 1] == ["<"]:
            at = skip_generics(tokens, at)
            if tokens[at:at + 1] != ["::"]:
                raise AssertionError("unparsed Supervisor turbofish")
            at += 1
        calls += tokens[at:at + 2] == ["start", "("]
    return calls


def calls(source, name):
    tokens = TOKEN.findall(source)
    return sum(tokens[i + 1:i + 2] == ["("] and tokens[i - 1:i] != ["fn"]
               for i, token in enumerate(tokens) if token == name)


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
        impls = Counter()
        for path, source in self.sources.items():
            impls.update((path, header) for header in ports_impls(source))
            self.assertEqual(supervisor_starts(source), 0, path)
            self.assertEqual(calls(source, "reserve_boot"), 0, path)
            self.assertEqual(len(re.findall(r"\bmapping\s*::\s*inspect\s*\(", source)), 0, path)
        self.assertEqual(dict(impls), ALLOWED_PORTS)
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
        allowed = "impl Ports for LivePorts {}"
        base = {helper_path: self.sources[helper_path], private_ports_path: allowed}
        guard.sources = dict(base)
        guard.test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation()
        fixtures = [
            "impl crate::services::discord::input_runtime::supervisor::Ports for Live {}",
            "impl<T> input_runtime::supervisor::Ports for Live<T> {}",
            "impl<T: Bound<Inner>> Ports for Live<T> {}",
            "impl<T: Bound<Inner<u8>>>\n    supervisor :: Ports<X>\n    for Live<T> where T: Send {}",
            "impl<F: Fn() -> u8> Ports for Live<F> {}",
            "impl Ports for Live {}",
            "fn activate() { input_runtime::supervisor::Supervisor :: start(); }",
            "fn activate() { Supervisor :: <Live> :: start (); }",
            "fn activate() { Supervisor::<Live<Vec<u8>>>::start(); }",
            "fn activate() { supervisor::Supervisor\n  ::<Live<Vec<Box<dyn Fn() -> u8>>>>\n  ::start(); }",
            "fn boot() { REGISTRY.reserve_boot(root, &selection, &config, &snapshots, &probe); }",
            "fn inspect() { crate::services::discord::input_runtime::supervisor::mapping :: inspect (&parents, channel); }",
        ]
        for path in ["src/other_module.rs", private_ports_path]:
            for source in fixtures:
                with self.subTest(path=path, source=source):
                    extra = allowed + "\n" + source if path == private_ports_path else source
                    guard.sources = dict(base, **{path: extra})
                    with self.assertRaises(AssertionError):
                        guard.test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation()
        # The one allowed impl is exact: zero, two, or a moved copy all fail.
        for sources in [{private_ports_path: "fn none() {}"},
                        {private_ports_path: allowed + "\n" + allowed},
                        {private_ports_path: "fn none() {}", "src/other_module.rs": allowed}]:
            with self.subTest(sources=sources):
                guard.sources = dict(base, **sources)
                with self.assertRaises(AssertionError):
                    guard.test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation()
        for source in ["fn a() { Supervisor::<Live<u8>::start(); }", "impl<T: Bound<Inner> Ports for X {}"]:
            with self.subTest(unparsed=source), self.assertRaises(AssertionError):
                guard.sources = dict(base, **{"src/other_module.rs": source})
                guard.test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation()

    def test_input_runtime_reader_pin_rejects_aliases_mutators_and_extra_reads(self):
        probe = D + "input_runtime/mapping.rs"
        additions = [
            (probe, "fn alias(s: &SharedData) { let m = &s.dispatch.thread_parents; }"),
            (probe, "fn more(s: &SharedData) { s.dispatch.thread_parents.iter(); }"),
            (D + "input_runtime/supervisor.rs", "fn w(s: &SharedData) { s.dispatch.thread_parents.insert(a, b); }"),
            (D + "input_runtime/supervisor/drive.rs", "fn r(s: &SharedData) { s.dispatch.thread_parents.iter(); }"),
        ]
        for path, extra in additions:
            with self.subTest(path=path, extra=extra):
                guard = Census("test_all_production_mapping_expressions_are_classified")
                guard.sources = dict(self.sources)
                guard.sources[path] += "\n" + extra
                with self.assertRaises(AssertionError):
                    guard.test_all_production_mapping_expressions_are_classified()
        moved = Census("test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation")
        moved.sources = dict(self.sources)
        moved.sources[probe] = moved.sources[probe].replace("pub(crate) fn check(&self, channel: u64)", "pub(crate) fn peek(&self, channel: u64)")
        with self.assertRaises((AssertionError, ValueError)):
            moved.test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation()

    def test_tokens_balance_nested_generics_exactly(self):
        self.assertEqual(ports_impls("impl<T: Bound<Inner>> Ports for Live<T> {}"),
                         ["impl < T : Bound < Inner > > Ports for Live < T >"])
        self.assertEqual(ports_impls("impl<F: Fn() -> u8> a::Ports<X<Y>> for Live<F> where F: Send {}"),
                         ["impl < F : Fn ( ) -> u8 > a :: Ports < X < Y > > for Live < F >"])
        self.assertEqual(supervisor_starts("Supervisor::<Live<Vec<u8>>>::start();"), 1)
        self.assertEqual(supervisor_starts("Supervisor :: < F < fn() -> u8 >> :: start ( )"), 1)
        self.assertEqual(supervisor_starts("Supervisor::<Live<Vec<u8>>>::stop();"), 0)
        for source in ["Supervisor::<Live<u8>::start();", "impl<T: Bound<Inner> Ports for X {}"]:
            with self.subTest(source=source), self.assertRaisesRegex(AssertionError, "unbalanced"):
                ports_impls(source), supervisor_starts(source)

    def test_comments_strings_and_test_items_are_not_installation(self):
        source = (
            "// impl<T: Bound<Inner>> Ports for Live<T> {}\n"
            "/* Supervisor::<Live<Vec<u8>>>::start(); */\n"
            "fn text() -> &'static str { \"impl Ports for Live {} reserve_boot(x)\" }\n"
            "#[cfg(test)]\nmod tests { impl<T: Bound<Inner>> Ports for Live<T> {}\n"
            "fn t() { Supervisor::<Live<Vec<u8>>>::start(); reserve_boot(x); } }\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture.rs"
            path.write_text(source)
            text = rust._production_text(path)
        self.assertEqual((ports_impls(text), supervisor_starts(text), calls(text, "reserve_boot")), ([], 0, 0))
        self.assertEqual(len(ports_impls(source)), 3)


if __name__ == "__main__":
    unittest.main()
