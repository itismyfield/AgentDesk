#!/usr/bin/env python3
"""Pin mapping expressions and release callers; reject unclassified mutations/aliases.

This lexical census uses cfg item/module boundaries, including inline fixtures.
It records admission syntax/lifetime, not the deferred G2 E1/E2 proof.
"""
from collections import Counter
import json
import os
from pathlib import Path
import re
import tempfile
import unittest

try:
    import check_durable_frontier_writer_call_sites as rust
    import generate_inventory_docs as inventory
    from rust_lex import COMMENT, StripState, lex_segments
except ModuleNotFoundError:
    from scripts import check_durable_frontier_writer_call_sites as rust
    from scripts import generate_inventory_docs as inventory
    from scripts.rust_lex import COMMENT, StripState, lex_segments

ROOT = Path(__file__).resolve().parents[1]
D = "src/services/discord/"


def cfg_test_files(files, declaration_text=None):
    # Derive exclusions from declarations, never from file/directory names.
    files = [Path(path).resolve() for path in files]
    declarations = {
        path: inventory._module_file_declarations(path, declaration_text(path) if declaration_text else path.read_text())
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
# Both impls are exact; the input adapter remains unreachable from production boot.
ALLOWED_PORTS = {
    (D + "turn_presence/activity.rs", "impl Ports for LivePorts"): 1,
    (D + "input_runtime/runtime/ports.rs", "impl Ports for RuntimePorts"): 1,
}
RUNTIME_PATH = D + "input_runtime/runtime.rs"
ALLOWED_STARTS = {RUNTIME_PATH: 1}


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
    """Supervisor::start references, including function values and nested turbofish."""
    tokens, calls = TOKEN.findall(source), 0
    for index, token in enumerate(tokens):
        if token != "Supervisor":
            continue
        at = index + 1
        if tokens[at:at + 1] == ["<"]:
            at = skip_generics(tokens, at)
        if tokens[at:at + 1] == [">"]:
            at += 1
        if tokens[at:at + 1] != ["::"]:
            continue
        at += 1
        if tokens[at:at + 1] == ["<"]:
            at = skip_generics(tokens, at)
            if tokens[at:at + 1] != ["::"]:
                raise AssertionError("unparsed Supervisor turbofish")
            at += 1
        calls += tokens[at:at + 1] == ["start"]
    return calls


def runtime_constructor_starts(source):
    """The pinned start must be inside the single inherent Runtime::new body."""
    tokens = TOKEN.findall(source)
    starts = []
    outer = 0
    for index, token in enumerate(tokens):
        outer += (token == "{") - (token == "}")
        if token != "impl" or outer or tokens[index + 1:index + 3] != ["Runtime", "{"]:
            continue
        depth, at = 1, index + 3
        while at < len(tokens) and depth:
            if depth == 1 and tokens[at:at + 2] == ["fn", "new"]:
                body = next((i for i in range(at + 2, len(tokens)) if tokens[i] == "{"), None)
                if body is None:
                    raise AssertionError("Runtime::new body missing")
                end, nested = body + 1, 1
                while end < len(tokens) and nested:
                    nested += (tokens[end] == "{") - (tokens[end] == "}")
                    end += 1
                if nested:
                    raise AssertionError("Runtime::new body unbalanced")
                starts.append(supervisor_starts(" ".join(tokens[body + 1:end - 1])))
                at = end
                continue
            depth += (tokens[at] == "{") - (tokens[at] == "}")
            at += 1
        if depth:
            raise AssertionError("Runtime impl unbalanced")
    return starts


def runtime_activations(source):
    """Reject dormant Runtime constructors/import indirection; known external runtimes differ."""
    allowed = set()
    modules = {"tokio": "tokio", "rquickjs": "rquickjs"}
    forbidden = 0
    for match in re.finditer(r"\buse\s+([^;]+);", source):
        item = match[1]
        prefix = re.sub(r"\s+", "", item).split("{", 1)[0].rstrip(":")
        module_alias = re.fullmatch(r"(tokio\s*::\s*runtime|rquickjs)\s+as\s+(\w+)", item.strip())
        if module_alias:
            modules[module_alias[2]] = module_alias[1].split("::", 1)[0].strip()
        elif re.search(r"\bas\s+(?:tokio|rquickjs)\b", item):
            forbidden += 1
        for name in re.finditer(r"\bRuntime\b(?:\s+as\s+(\w+))?", item):
            if prefix == "rquickjs" or prefix.startswith("rquickjs::") or prefix == "tokio::runtime" or prefix.startswith("tokio::runtime::"):
                allowed.add(name[1] or "Runtime")
            else:
                forbidden += 1
    forbidden += len(re.findall(r"\bmod\s+(?:tokio|rquickjs)\b", source))
    # Alias/re-export declarations cannot hide an input type behind a new constructor spelling.
    for item in re.finditer(r"\btype\s+\w+(?:\s*<[^;]+?>)?\s*=\s*([^;]+);", source):
        if re.search(r"\bRuntime\b", item[1]):
            forbidden += 1
    tokens = TOKEN.findall(source)
    for index, token in enumerate(tokens):
        if token != "Runtime" and token not in allowed:
            continue
        at = index + 1
        if tokens[at:at + 1] == ["<"]:
            at = skip_generics(tokens, at)
        if tokens[at:at + 1] == ["as"]:
            at += 1
            while at < len(tokens) and tokens[at] != ">":
                at = skip_generics(tokens, at) if tokens[at] == "<" else at + 1
        if tokens[at:at + 1] == [">"]:
            at += 1
        if tokens[at:at + 1] != ["::"]:
            continue
        at += 1
        if tokens[at:at + 1] == ["<"]:
            at = skip_generics(tokens, at)
            if tokens[at:at + 1] != ["::"]:
                raise AssertionError("unparsed Runtime turbofish")
            at += 1
        if tokens[at:at + 1] != ["new"]:
            continue
        start = index
        while start >= 2 and tokens[start - 1] == "::" and re.fullmatch(r"[A-Za-z_]\w*", tokens[start - 2]):
            start -= 2
        external = modules.get(tokens[start]) if start != index else None
        if not external and not (start == index and token in allowed):
            forbidden += 1
    return forbidden


def reject_installation_aliases(source):
    for item in re.finditer(r"\buse\s+([^;]+);", source):
        if re.search(r"\b(?:Supervisor|Ports|reserve_boot)\s+as\s+\w+", item[1]):
            raise AssertionError("installation symbol renamed by use")
    for item in re.finditer(r"\btype\s+\w+(?:\s*<[^;]+?>)?\s*=\s*([^;]+);", source):
        if re.search(r"\b(?:Supervisor|Ports)\b", item[1]):
            raise AssertionError("installation symbol hidden by type alias")


def calls(source, name):
    tokens = TOKEN.findall(source)
    return sum(tokens[i - 1:i] != ["fn"] for i, token in enumerate(tokens) if token == name)


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


SYMBOLS = (*RELEASES, "submit_slash")
SYMBOL_MANIFEST = ROOT / "scripts/input_mapping_symbol_manifest.json"


def stripped_source(path):
    state = rust.StripState()
    return "\n".join(rust.strip_line(line, state) for line in path.read_text().splitlines())


def declaration_source(path):
    state = StripState()
    return "\n".join("".join(" " * len(text) if kind == COMMENT else text
                              for kind, text in lex_segments(line, state))
                     for line in path.read_text().splitlines())


def balanced_end(tokens, start, opener="{", closer="}"):
    depth = 0
    for index in range(start, len(tokens)):
        depth += (tokens[index] == opener) - (tokens[index] == closer)
        if depth == 0:
            return index + 1
    raise AssertionError("unbalanced symbol census item")


def item_body(tokens, start):
    index = start
    while index < len(tokens):
        if tokens[index] in {"{", ";"}:
            return index
        if tokens[index] in {"(", "[", "<"}:
            opener = tokens[index]
            index = balanced_end(tokens, index, opener, {"(": ")", "[": "]", "<": ">"}[opener])
        else:
            index += 1
    raise AssertionError("symbol census item body missing")


def symbol_uses(path, whole_file_test=False):
    """Classify every protected spelling; indirection never becomes an allowed call."""
    source = stripped_source(path)
    matches = list(TOKEN.finditer(source))
    tokens = [match[0] for match in matches]
    functions, test_ranges, imports = [], [], {}
    for index, token in enumerate(tokens):
        if token == "fn" and index + 1 < len(tokens):
            body = item_body(tokens, index + 2)
            if tokens[body] == "{":
                functions.append((index, balanced_end(tokens, body), tokens[index + 1], body))
        if token == "use":
            end = next((i for i in range(index + 1, len(tokens)) if tokens[i] == ";"), None)
            if end is None:
                raise AssertionError("unterminated symbol import")
            prefix = index - 1
            if tokens[prefix:prefix + 1] == [")"]:
                while prefix >= 0 and tokens[prefix] != "(":
                    prefix -= 1
                prefix -= 1
            exported = tokens[prefix:prefix + 1] == ["pub"]
            for at in range(index + 1, end):
                if tokens[at] in SYMBOLS:
                    kind = "reexport" if exported else "alias" if tokens[at + 1:at + 2] == ["as"] else "import"
                    imports[at] = kind
    # Bound each cfg item precisely so a live item later on the same line stays live.
    offset = 0
    while (attribute := rust._cfg_test_only_match(source[offset:])) is not None:
        end = offset + attribute.attribute_end()
        start = next((i for i, match in enumerate(matches) if match.start() >= end), len(tokens))
        at = start
        while tokens[at:at + 2] == ["#", "["]:
            at = balanced_end(tokens, at + 1, "[", "]")
        body = item_body(tokens, at)
        stop = balanced_end(tokens, body) if tokens[body] == "{" else body + 1
        test_ranges.append((start, stop))
        offset = end
    result = []
    for index, token in enumerate(tokens):
        if token not in SYMBOLS:
            continue
        contexts = [fn for fn in functions if fn[0] <= index < fn[1]]
        context = contexts[-1][2] if contexts else "<module>"
        if index in imports:
            kind = imports[index]
        elif tokens[index - 1:index] == ["fn"]:
            kind = "definition"
        else:
            at = index + 1
            if tokens[at:at + 2] == ["::", "<"]:
                at = skip_generics(tokens, at + 1)
            if tokens[at:at + 1] == ["("]:
                kind = "direct_call"
            else:
                begin = index - 1
                while begin >= 0 and tokens[begin] not in {";", "{", "}"}:
                    begin -= 1
                statement = tokens[begin + 1:index]
                kind = "function_value" if statement[:1] == ["let"] and "=" in statement else "unclassified"
        test_only = whole_file_test or any(start <= index < stop for start, stop in test_ranges)
        result.append((token, context, ("test_only/" if test_only else "") + kind))
    return result


def symbol_census(root):
    files = sorted((root / "src").rglob("*.rs"))
    excluded = cfg_test_files(files, declaration_source)
    actual = Counter()
    for path in files:
        if not any(re.search(r"\b" + symbol + r"\b", path.read_text()) for symbol in SYMBOLS):
            continue
        for symbol, context, kind in symbol_uses(path, path.resolve() in excluded):
            if kind.endswith("unclassified"):
                raise AssertionError(f"unclassified {symbol}: {path}:{context}")
            actual[symbol, path.relative_to(root).as_posix(), context, kind] += 1
    return actual


def manifest_counts(manifest):
    if manifest.get("version") != 1:
        raise AssertionError("unsupported symbol census manifest version")
    counts = Counter()
    for symbol, path, context, kind, count in manifest["uses"]:
        key = symbol, path, context, kind
        if symbol not in SYMBOLS or kind.removeprefix("test_only/") not in {"definition", "direct_call", "import"} or not isinstance(count, int) or isinstance(count, bool) or count < 1 or key in counts:
            raise AssertionError(f"invalid symbol census manifest row: {key}")
        counts[key] = count
    if set(symbol for symbol, *_ in counts) != set(SYMBOLS):
        raise AssertionError("missing protected symbol in manifest")
    return counts


def assert_symbol_census(root, manifest):
    expected, actual = manifest_counts(manifest), symbol_census(root)
    if actual != expected:
        raise AssertionError(f"symbol census mismatch: extra={dict(actual - expected)} missing={dict(expected - actual)}")


def function_body(source, name):
    tokens = TOKEN.findall(source)
    starts = [i for i in range(len(tokens) - 1) if tokens[i:i + 2] == ["fn", name]]
    if len(starts) != 1:
        raise AssertionError(f"expected one function {name}, found {len(starts)}")
    body = item_body(tokens, starts[0] + 2)
    if tokens[body] != "{":
        raise AssertionError(f"missing body for {name}")
    return "".join(tokens[body + 1:balanced_end(tokens, body) - 1])


def assert_slash_case(root, case):
    case_id = case["id"]
    if not case["receipt_tests"]:
        raise AssertionError(f"{case_id}: receipt test case missing")
    definition_path, definition_fn = case["definition"]
    route_path, route_fn = case["route"]
    entry_path, entry_fn = case["entrypoint"]
    definition = function_body(stripped_source(root / definition_path), definition_fn)
    route = function_body(stripped_source(root / route_path), route_fn)
    entry = function_body(stripped_source(root / entry_path), entry_fn)
    submit = "letreceipt=submit_slash(&client,&source).await.map_err(|reason|format!())?;"
    received_ack = ("if!matches!(receipt,runtime::supervisor::external::ExternalReceipt::Received{..})"
                    "{returnErr(format!().into());}ctx.say().await?;Ok(true)")
    if route.count(submit) != 1 or route.count("ctx.say(") != 1 or not route.endswith(submit + received_ack):
        raise AssertionError(f"{case_id}: slash Received must precede success ACK")
    if not definition.endswith("client.submit_external(prepared).await"):
        raise AssertionError(f"{case_id}: missing durable submit")
    if entry.count("ifBox::pin(hold_slash_skill(ctx,invoked_as,&skill,args_str)).await?{returnOk(());}") != 1:
        raise AssertionError(f"{case_id}: slash entrypoint route missing")
    for test_path, test_name in case["receipt_tests"]:
        test = function_body(stripped_source(root / test_path), test_name)
        if "submit_slash(" not in test:
            raise AssertionError(f"{case_id}: receipt test does not call submit_slash: {test_name}")


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
        manifest = json.loads(SYMBOL_MANIFEST.read_text())
        assert_symbol_census(ROOT, manifest)
        for helper, legacy in RELEASES.items():
            calls_by_path = Counter()
            for (symbol, path, _context, kind), count in manifest_counts(manifest).items():
                if symbol == helper and kind == "direct_call":
                    calls_by_path[path.removeprefix(D)] += count
            self.assertEqual(calls_by_path, legacy, helper)
        self.assertEqual(sum(RELEASES["kick_thread_parents_after_turn_release"].values())
                         + sum(RELEASES["collect_and_clear_thread_parents"].values())
                         - 1 + 2, 11)  # Remove the wrapper; add two direct deletions.

    def test_k2_c27_slash_received_ack_case(self):
        manifest = json.loads(SYMBOL_MANIFEST.read_text())
        self.assertEqual([case["id"] for case in manifest["cases"]], ["K2-C27-SLASH-RECEIVED-ACK"])
        receipt_tests = {(path, context) for (symbol, path, context, kind) in manifest_counts(manifest)
                         if symbol == "submit_slash" and kind == "test_only/direct_call"}
        for case in manifest["cases"]:
            self.assertEqual([tuple(row) for row in case["receipt_tests"]], sorted(receipt_tests))
            assert_slash_case(ROOT, case)

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
        helper = re.sub(r"\s+", "", self.sources[D + "router/intake_gate/ledger.rs"])
        helper_start = helper.index("asyncfnlegacy_effect(")
        helper_end = helper.index("pub(incrate::services::discord)asyncfntry_text_reply(", helper_start)
        helper = helper[helper_start:helper_end]
        policy = "legacy_allowed(deps.shared,deps.http,&deps.shared.provider,channel).await?;"
        fence_admit = "runtime::fence::effect::admit(&deps.shared.provider,channel.get())"
        self.assertEqual(helper.count(policy), 1)
        self.assertEqual(helper.count(fence_admit), 1)
        self.assertEqual(helper.count("letpermit="), 1)
        self.assertIn(
            ")->Result<Option<runtime::fence::Permit>,Error>{letpermit=" + fence_admit + ".map_err(",
            helper,
        )
        self.assertIn(")?;" + policy + "Ok(permit)}", helper)
        self.assertIn("usesuper::super::intake_gate::ledgeraslive;", root)
        caller = "letpermit=live::legacy_effect(deps,request.channel_id).await?;"
        self.assertEqual(root.count(caller), 1)
        admit = root.index(caller)
        run_call = "effect::run(permit,"
        self.assertEqual(root.count(run_call), 1)
        run = root.index(run_call, admit)
        body = root.index("handle_text_message_admitted", run)
        redirect = root.index("adk_thread::redirect_dispatch(", body)
        self.assertLess(admit, run)
        self.assertEqual(root[admit:run].count("letpermit="), 1)
        self.assertNotIn("drop(permit", root[admit:run])
        self.assertIn("ifpermit.is_none()&&effect::current().is_none(){", root[admit:run])
        self.assertLess(body, redirect)
        self.assertIn("letIntakeRequest{intake_outbox_id,channel_id,", root[body:redirect])
        self.assertIn("letoriginal_channel_id=channel_id;", root[body:redirect])

    def test_scanner_rejects_new_mutators_aliases_and_field_replacement(self):
        for operation in ["entry", "get_mut", "iter_mut", "extend", "clear", "insert"]:
            self.assertNotEqual(Counter(expressions("shared.dispatch.thread_parents." + operation + "(x);")), Counter())
        for source in ["let map = &shared.dispatch.thread_parents;", "shared.dispatch.thread_parents = map;", "let D { thread_parents, .. } = state;"]:
            self.assertEqual(expressions(source), ["unclassified"])

    def test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation(self):
        impls, starts = Counter(), Counter()
        for path, source in self.sources.items():
            reject_installation_aliases(source)
            impls.update((path, header) for header in ports_impls(source))
            count = supervisor_starts(source)
            if count:
                starts[path] = count
            self.assertEqual(runtime_activations(source), 0, path)
            self.assertEqual(calls(source, "reserve_boot"), 0, path)
            self.assertEqual(len(re.findall(r"\bmapping\s*::\s*inspect\s*\(", source)), 0, path)
        self.assertEqual(dict(impls), ALLOWED_PORTS)
        self.assertEqual(dict(starts), ALLOWED_STARTS)
        self.assertEqual(runtime_constructor_starts(self.sources[RUNTIME_PATH]), [1])
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
        runtime_ports_path = D + "input_runtime/runtime/ports.rs"
        allowed = "impl Ports for LivePorts {}"
        runtime_ports = "impl Ports for RuntimePorts {}"
        runtime = "impl Runtime { fn new() { Supervisor::start(reserved, config, ports); } }"
        base = {helper_path: self.sources[helper_path], private_ports_path: allowed,
                runtime_ports_path: runtime_ports, RUNTIME_PATH: runtime}
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
            "fn activate() { <Supervisor<Live<Vec<u8>>>>::start(); }",
            "fn activate() { supervisor::Supervisor\n  ::<Live<Vec<Box<dyn Fn() -> u8>>>>\n  ::start(); }",
            "fn boot() { REGISTRY.reserve_boot(root, &selection, &config, &snapshots, &probe); }",
            "fn boot() { REGISTRY.reserve_boot::<View>(root); }",
            "fn boot() { let reserve = Registry::reserve_boot; reserve(root); }",
            "use input_runtime::supervisor::Supervisor as S; fn activate() { S::start(); }",
            "use input_runtime::supervisor::{Ports as P}; impl P for Live {}",
            "type S = input_runtime::supervisor::Supervisor<Live>; fn activate() { S::start(); }",
            "fn inspect() { crate::services::discord::input_runtime::supervisor::mapping :: inspect (&parents, channel); }",
        ]
        for path in ["src/other_module.rs", private_ports_path]:
            for source in fixtures:
                with self.subTest(path=path, source=source):
                    extra = allowed + "\n" + source if path == private_ports_path else source
                    guard.sources = dict(base, **{path: extra})
                    with self.assertRaises(AssertionError):
                        guard.test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation()
        # Zero, duplicate, or moved copies of either exact impl fail.
        for path, header in [(private_ports_path, allowed), (runtime_ports_path, runtime_ports)]:
            for sources in [{path: "fn none() {}"}, {path: header + "\n" + header},
                            {path: "fn none() {}", "src/other_module.rs": header}]:
                with self.subTest(sources=sources):
                    guard.sources = dict(base, **sources)
                    with self.assertRaises(AssertionError):
                        guard.test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation()
        for sources in [
            {RUNTIME_PATH: "impl Runtime { fn new() {} }"},
            {RUNTIME_PATH: runtime + " fn duplicate() { Supervisor::start(); }"},
            {RUNTIME_PATH: "impl Runtime { fn new() {} fn run() { Supervisor::start(); } }"},
            {RUNTIME_PATH: "mod nested { " + runtime + " }"},
            {RUNTIME_PATH: "impl Runtime { fn new() {} }", "src/other_module.rs": runtime},
        ]:
            with self.subTest(starts=sources):
                guard.sources = dict(base, **sources)
                with self.assertRaises(AssertionError):
                    guard.test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation()
        for source in ["fn a() { Supervisor::<Live<u8>::start(); }", "impl<T: Bound<Inner> Ports for X {}"]:
            with self.subTest(unparsed=source), self.assertRaises(AssertionError):
                guard.sources = dict(base, **{"src/other_module.rs": source})
                guard.test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation()

    def test_runtime_activation_rejects_qualified_turbofish_and_alias_callers(self):
        base = {
            D + "input_runtime/mapping.rs": self.sources[D + "input_runtime/mapping.rs"],
            D + "turn_presence/activity.rs": "impl Ports for LivePorts {}",
            D + "input_runtime/runtime/ports.rs": "impl Ports for RuntimePorts {}",
            RUNTIME_PATH: "impl Runtime { fn new() { Supervisor::start(reserved, config, ports); } }",
        }
        fixtures = [
            "fn activate() { Runtime::new(reserved, config); }",
            "fn activate() { crate::services::discord::input_runtime::runtime::Runtime::new(); }",
            "fn activate() { input_runtime::supervisor::runtime::Runtime::<View<Vec<u8>>>::new(); }",
            "fn activate() { runtime :: Runtime :: new :: <View> (); }",
            "fn activate() { <runtime::Runtime>::new(); }",
            "fn activate() { <runtime::Runtime as Construct<View>>::new(); }",
            "fn activate() { let create = runtime::Runtime::new; create(); }",
            "use input_runtime::runtime::Runtime as R; fn activate() { R::new(); }",
            "use input_runtime::runtime::{Runtime as R}; fn activate() { R::<View>::new(); }",
            "pub use input_runtime::runtime::Runtime as R;",
            "use input_runtime::runtime as adapter; fn activate() { adapter::Runtime::new(); }",
            "use input_runtime as ir; use ir::runtime::Runtime as R; fn activate() { R::new(); }",
            "type R = input_runtime::runtime::Runtime; fn activate() { R::new(); }",
            "use input_runtime::runtime::*; fn activate() { Runtime::new(); }",
            "use input_runtime as tokio; fn activate() { tokio::runtime::Runtime::new(); }",
            "use input_runtime as rquickjs; fn activate() { rquickjs::runtime::Runtime::new(); }",
            "mod tokio { pub use input_runtime::runtime; } fn activate() { tokio::runtime::Runtime::new(); }",
        ]
        for path in ["src/other_module.rs", RUNTIME_PATH]:
            for source in fixtures:
                with self.subTest(path=path, source=source):
                    guard = Census("test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation")
                    guard.sources = dict(base)
                    guard.sources[path] = guard.sources.get(path, "") + "\n" + source
                    with self.assertRaises(AssertionError):
                        guard.test_guard_helper_is_read_only_and_input_runtime_has_no_port_installation()
        for source in ["tokio::runtime::Runtime::new();", "use rquickjs::{Context, Runtime}; Runtime::new();",
                       "use tokio::runtime::Runtime as R; R::new();",
                       "use tokio::runtime as rt; rt::Runtime::new();"]:
            with self.subTest(external=source):
                self.assertEqual(runtime_activations(source), 0)

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


class SymbolScannerTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / "src").mkdir()
        self.live = self.root / "src/lib.rs"
        self.fixture = self.root / "src/fixture.rs"
        self.helper = SYMBOLS[0]
        self.live.write_text("\n".join(f"fn {symbol}() {{}}" for symbol in SYMBOLS)
                             + f"\nfn live() {{ {self.helper}(); }}\n#[cfg(test)]\nmod fixture;\n")
        self.fixture.write_text(f"fn fixture() {{ {self.helper}(); }}\n")
        self.manifest = {"version": 1, "uses": [
            *[[symbol, "src/lib.rs", symbol, "definition", 1] for symbol in SYMBOLS],
            [self.helper, "src/lib.rs", "live", "direct_call", 1],
            [self.helper, "src/fixture.rs", "fixture", "test_only/direct_call", 1],
        ]}

    def assert_mutant_fails(self, path, changed, reason="symbol census mismatch"):
        original = path.read_bytes()
        assert_symbol_census(self.root, self.manifest)
        try:
            path.write_text(changed)
            with self.assertRaisesRegex(AssertionError, reason):
                assert_symbol_census(self.root, self.manifest)
        finally:
            path.write_bytes(original)
        self.assertEqual(path.read_bytes(), original)
        assert_symbol_census(self.root, self.manifest)

    def test_c03_use_alias_mutant(self):
        for symbol in SYMBOLS:
            with self.subTest(symbol=symbol):
                self.assert_mutant_fails(self.live, self.live.read_text()
                                        + f"use cleanup::{{{symbol} as hidden}}; fn added() {{ hidden(); }}")

    def test_c03_function_value_mutant(self):
        for symbol in SYMBOLS:
            with self.subTest(symbol=symbol):
                self.assert_mutant_fails(self.live, self.live.read_text()
                                        + f"fn added() {{ let f = cleanup::{symbol}; f(); }}")

    def test_c03_new_caller_and_same_file_relocation_mutants(self):
        self.assert_mutant_fails(self.live, self.live.read_text() + f"fn added() {{ {self.helper}(); }}")
        self.assert_mutant_fails(self.live, self.live.read_text().replace("fn live()", "fn replacement()"))

    def test_c03_production_exposure_mutants(self):
        for attribute in ["", '#[cfg(any(test, feature="live"))]\n']:
            with self.subTest(attribute=attribute):
                self.assert_mutant_fails(self.live, self.live.read_text().replace("#[cfg(test)]\n", attribute))
        self.assert_mutant_fails(self.live, self.live.read_text() + '\n#[path="fixture.rs"] mod exposed;')
        self.assert_mutant_fails(self.live, self.live.read_text().replace("#[cfg(test)]", "// #[cfg(test)]"))

    def test_reexports_and_unclassified_uses_fail_closed(self):
        for extra, reason in [
            (f"pub use cleanup::{self.helper};", "symbol census mismatch"),
            (f"pub(crate) use cleanup::{self.helper} as hidden;", "symbol census mismatch"),
            (f"fn escape() {{ consume({self.helper}); }}", "unclassified"),
            (f"const F: fn() = {self.helper};", "unclassified"),
            (f"#[cfg(test)] fn escape() {{ consume({self.helper}); }}", "unclassified"),
        ]:
            with self.subTest(extra=extra):
                self.assert_mutant_fails(self.live, self.live.read_text() + extra, reason)

    def test_each_usage_kind_and_inline_test_boundary(self):
        self.fixture.write_text(
            f"fn {self.helper}() {{}}\nuse x::{self.helper};\n"
            f"use x::{{{self.helper} as renamed}};\npub use x::{self.helper};\n"
            f"fn value() {{ let f = {self.helper}; f(); }}\n"
            f"#[cfg(all(test, unix))] fn only_test() {{ {self.helper}(); }} fn production() {{ {self.helper}::<T>(); }}\n"
            f"fn unknown() {{ consume({self.helper}); }}\n"
        )
        uses = symbol_uses(self.fixture)
        self.assertEqual([(context, kind) for _, context, kind in uses], [
            (self.helper, "definition"), ("<module>", "import"), ("<module>", "alias"),
            ("<module>", "reexport"), ("value", "function_value"),
            ("only_test", "test_only/direct_call"), ("production", "direct_call"),
            ("unknown", "unclassified"),
        ])

    def test_comments_strings_and_nested_test_modules_are_classified(self):
        self.live.write_text(self.live.read_text() + f'\n// {self.helper}();\n'
                             + f'const TEXT: &str = r#"{{ {self.helper}();"#;\n')
        assert_symbol_census(self.root, self.manifest)
        nested = f"#[cfg(test)] mod outer {{ mod inner {{ fn t() {{ {self.helper}(); }} }} }}"
        original = self.fixture.read_bytes()
        self.fixture.write_text(nested)
        self.assertEqual(symbol_uses(self.fixture), [(self.helper, "t", "test_only/direct_call")])
        self.fixture.write_bytes(original)
        self.assert_mutant_fails(self.live, self.live.read_text() + f"\n// #[cfg(test)]\nfn production() {{ {self.helper}(); }}")

    def test_unknown_manifest_version_duplicates_and_empty_census_fail(self):
        for manifest in [{"version": 0, "uses": []}, {"version": 1, "uses": []},
                         dict(self.manifest, uses=self.manifest["uses"] * 2)]:
            with self.subTest(manifest=manifest), self.assertRaises(AssertionError):
                assert_symbol_census(self.root, manifest)
        self.assert_mutant_fails(self.live, "fn unrelated() {}")

    def test_c27_receipt_ack_and_entrypoint_mutants_restore_bytes(self):
        case = json.loads(SYMBOL_MANIFEST.read_text())["cases"][0]
        paths = {path for path, _ in [case["definition"], case["route"], case["entrypoint"], *case["receipt_tests"]]}
        for path in paths:
            target = self.root / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes((ROOT / path).read_bytes())
        assert_slash_case(self.root, case)
        with self.assertRaisesRegex(AssertionError, "receipt test case missing"):
            assert_slash_case(self.root, dict(case, receipt_tests=[]))
        route = self.root / case["route"][0]
        entry = self.root / case["entrypoint"][0]
        for path, old, new in [
            (route, "ExternalReceipt::Received", "ExternalReceipt::Duplicate"),
            (route, "let receipt = submit_slash", 'ctx.say("early").await?; let receipt = submit_slash'),
            (route, 'ctx.say("입력을 원장에 접수했습니다.").await?;', ""),
            (route, "submit_slash(&client, &source)", "other_submit(&client, &source)"),
            (entry, "Box::pin(hold_slash_skill(", "Box::pin(other_route("),
        ]:
            with self.subTest(path=path, old=old):
                original = path.read_bytes()
                self.assertIn(old, path.read_text())
                try:
                    path.write_text(path.read_text().replace(old, new, 1))
                    with self.assertRaisesRegex(AssertionError, case["id"]):
                        assert_slash_case(self.root, case)
                finally:
                    path.write_bytes(original)
                self.assertEqual(path.read_bytes(), original)
                assert_slash_case(self.root, case)


if __name__ == "__main__":
    unittest.main()
