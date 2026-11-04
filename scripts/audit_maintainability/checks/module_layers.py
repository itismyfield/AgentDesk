"""Informational, source-based module dependencies and direct boundary census."""
from __future__ import annotations

from bisect import bisect_right
from collections import defaultdict
from pathlib import Path
import re
import subprocess
import tomllib

from ci.rust_test_sources import attribute_span, crate_roots, walk
from .service_server_backflow import USE_TREE, _use_paths
from .. import common
from ..common import Finding
from . import CheckSpec
from .module_layer_baseline import load_mapping, project

SCHEMA_VERSION = 1
ITEMS = {"fn", "struct", "enum", "union", "trait", "type", "const", "static"}


def _code(tokens):
    parts, end = [], 0
    for token in tokens:
        parts.append(" " * max(0, token.start - end))
        value = token.value if token.kind != "string" else " " * len(token.value)
        parts.append(value)
        end = token.start + len(value)
    return "".join(parts)


def _sccs(nodes, edges):
    graph, reverse = defaultdict(set), defaultdict(set)
    for source, target, _kind in edges:
        graph[source].add(target)
        reverse[target].add(source)
    seen, order = set(), []
    for node in sorted(nodes):
        stack = [(node, False)]
        while stack:
            current, done = stack.pop()
            if done:
                order.append(current)
            elif current not in seen:
                seen.add(current)
                stack.append((current, True))
                stack.extend((n, False) for n in sorted(graph[current], reverse=True) if n not in seen)
    seen, groups = set(), []
    for node in reversed(order):
        if node in seen:
            continue
        group, stack = [], [node]
        seen.add(node)
        while stack:
            current = stack.pop()
            group.append(current)
            for neighbor in reverse[current] - seen:
                seen.add(neighbor)
                stack.append(neighbor)
        groups.append(sorted(group))
    return sorted(groups, key=lambda group: (-len(group), group))


def _crate(root, repo, coverage, external, occurrences=None):
    inv = walk(root, repo, production=True)
    root, repo = root.resolve(), repo.resolve()
    rel_root = root.relative_to(repo).as_posix()
    node = lambda names: rel_root + "::crate" + ("::" + "::".join(names) if names else "")
    nodes = {node(names) for names in inv.logical_modules}
    coverage["reached_files"].update(p.relative_to(repo).as_posix() for p in inv.reached)
    coverage["read_failures"].extend(inv.read_failures)
    coverage["module_errors"].extend(inv.errors)
    coverage["cfg_uncertainties"].extend(inv.unreadable)
    coverage["cfg_conditions"].extend({"root": rel_root, "module": node(outer), "conditions": cfgs}
                                      for _source, outer, cfgs in inv.sources if cfgs)
    symbols, imports, exports, refs, macros = {}, {}, {}, [], {}
    module_scopes, symbol_kinds, scoped_items = {}, {}, {}
    for source, outer, cfgs in inv.sources:
        tokens = inv.source_tokens[source]
        values = [t.value for t in tokens]
        offsets = [t.start for t in tokens]
        rel = source.relative_to(repo).as_posix()
        scopes, owners, metadata, openings = [], [outer], [], {}
        # Scope identities include the source file because literal includes can share a module.
        for i, token in enumerate(tokens):
            scope = (str(source), *scopes)
            metadata.append((owners[-1], scope))
            module_scopes.setdefault((owners[-1], str(source)), scope)
            if token.kind == "ident" and token.value == "mod" and values[i + 2:i + 3] == ["{"]:
                openings[i + 2] = owners[-1] + (values[i + 1],)
            if token.value == "{" and token.kind == "punct":
                scopes.append(token.start)
                if i in openings:
                    owners.append(openings[i])
                    module_scopes[(owners[-1], str(source))] = (str(source), *scopes)
            elif token.value == "}" and token.kind == "punct" and scopes:
                if module_scopes.get((owners[-1], str(source))) == (str(source), *scopes) and len(owners) > 1:
                    owners.pop()
                scopes.pop()
            if (token.kind == "ident" and token.value in ITEMS and i + 1 < len(tokens)
                    and tokens[i + 1].kind == "ident" and values[max(0, i - 1):i] != ["'"]):
                name, owner = values[i + 1], owners[-1]
                if scope == module_scopes.get((owner, str(source))):
                    symbols[owner + (name,)] = owner
                    symbol_kinds[owner + (name,)] = token.value
                else:
                    scoped_items[(owner, scope, name)] = token.value
        code = _code(tokens)
        used = set()
        for match in USE_TREE.finditer(code):
            index = max(0, bisect_right(offsets, match.start("tree")) - 1)
            owner, scope = metadata[index]
            kind = "reexport" if match.group("public") else "import"
            for path in _use_paths(code, match.start("tree"), match.end("tree")):
                if not path:
                    continue
                names = tuple(p[0] for p in path)
                leaf = path[-1][1]
                line = tokens[max(0, bisect_right(offsets, leaf) - 1)].line
                ref = (names, owner, scope, rel, line, kind, leaf, "unknown")
                refs.append(ref)
                alias = re.match(r"\s+as\s+((?:r#)?[A-Za-z_]\w*)", code[leaf + len(names[-1]):])
                local = alias[1].removeprefix("r#") if alias else names[-1]
                if local == "self" and len(names) > 1:
                    local = names[-2]
                if local not in {"*", "_"}:
                    imports.setdefault((owner, scope, local), []).append(ref)
                    if scope == module_scopes.get((owner, str(source))):
                        exports.setdefault((owner, local), []).append(ref)
            lo, hi = bisect_right(offsets, match.start()) - 1, bisect_right(offsets, match.end() - 1)
            used.update(range(max(0, lo), hi))
        i = 0
        while i < len(tokens):
            token = tokens[i]
            span = attribute_span(tokens, i)
            if span is not None:
                # Any surviving test gate denotes a mixed line retained by the production view.
                attr = values[span[0] + 1:span[1] - 1]
                if "test" in attr or attr[:1] == ["cfg_attr"]:
                    coverage["cfg_uncertainties"].append(f"{rel}:{token.line} retained attribute: {' '.join(attr)}")
                i = span[1]
                continue
            if token.value == "macro_rules" and values[i + 1:i + 2] == ["!"] and i + 3 < len(tokens):
                owner, scope = metadata[i]
                start, end, depth = i + 3, i + 4, 1
                closing = {"{": "}", "(": ")", "[": "]"}.get(values[start], "}")
                while end < len(tokens) and depth:
                    depth += (values[end] == values[start]) - (values[end] == closing)
                    end += 1
                key = owner + (values[i + 2],)
                macros[key] = (owner, start, end, rel, token.line)
                if "macro_export" in values[max(0, i - 8):i]:
                    macros[(values[i + 2],)] = macros[key]
                used.add(i + 2)
            if token.value == "pub" and values[i + 1:i + 2] == ["("]:
                i += 2
                while i < len(tokens) and values[i] != ")":
                    i += 1
                i += 1
                continue
            if i in used or token.kind == "string":
                i += 1
                continue
            start, names = i, []
            if token.value == "$" and values[i + 1:i + 2] == ["crate"]:
                names.append("$crate")
                i += 2
            elif token.kind == "ident":
                names.append(token.value)
                i += 1
            else:
                i += 1
                continue
            while values[i:i + 2] == [":", ":"] and i + 2 < len(tokens) and tokens[i + 2].kind == "ident":
                names.append(values[i + 2])
                i += 3
            owner, scope = metadata[start]
            is_macro = (values[i:i + 1] == ["!"] and i + 1 < len(tokens)
                        and tokens[i + 1].kind == "punct" and values[i + 1] in {"(", "[", "{"}
                        and names[0] not in {"if", "while", "return", "break", "yield", "match", "let"})
            method = start > 0 and values[start - 1] == "." and values[max(0, start - 2):start] != [".", "."]
            if (len(names) > 1 or is_macro) and not method:
                namespace = "value" if values[i:i + 1] == ["("] else "unknown"
                refs.append((tuple(names), owner, scope, rel, token.line, "macro" if is_macro else "path", token.start, namespace))
    modules = set(inv.logical_modules)

    macro_symbols = {key: definition[0] for key, definition in macros.items()}
    symbol_tables = ({**macro_symbols, **symbols}, {**symbols, **macro_symbols})

    def resolve(path, owner, scope, trail=frozenset(), macro=False, namespace="unknown"):
        symbols = symbol_tables[macro]
        if not path or len(trail) > 64:
            return None, "alias cycle or depth limit"
        first, *rest = path
        if first in {"crate", "$crate"}:
            prefix = ()
        elif first == "self":
            prefix = owner
        elif first == "super":
            if not owner:
                return None, "super outside crate"
            prefix = owner[:-1]
            while rest and rest[0] == "super":
                if not prefix:
                    return None, "super outside crate"
                prefix, rest = prefix[:-1], rest[1:]
        else:
            for n in range(len(scope), 0, -1):
                key = (owner, scope[:n], first)
                if key in imports:
                    if (rest and owner + (first,) in modules
                            and scope[:n] == module_scopes.get((owner, scope[0]))
                            and len(imports[key]) == 1 and imports[key][0][0][:1] == (first,)):
                        return follow(owner, [first, *rest], trail, macro, namespace)
                    if key in trail:
                        if owner + (first,) in modules:
                            return follow(owner, [first, *rest], trail, macro, namespace)
                        if first in external and len(imports[key]) == 1 and imports[key][0][0][:1] == (first,):
                            return (None, path), "external"
                        return None, "alias cycle"
                    if len(imports[key]) != 1:
                        return None, "cfg-ambiguous alias"
                    binding = imports[key][0]
                    result, reason = resolve(binding[0], binding[1], binding[2], trail | {key}, macro, "module" if rest else namespace)
                    if result is None:
                        return None, reason
                    if result[0] is None:
                        return (None, result[1] + tuple(rest)), reason
                    return follow(result[1], rest, trail | {key}, macro, namespace)
            for n in range(len(scope), 0, -1):
                if scoped_items.get((owner, scope[:n], first)) in {"struct", "enum", "union", "trait", "type"}:
                    return (owner, owner + path), "resolved"
            if owner + (first,) in symbols or owner + (first,) in modules:
                prefix = owner
            elif (first,) in symbols or (first,) in modules:
                prefix = ()
            elif first in external:
                return (None, path), "external"
            else:
                return None, "unknown path root"
            rest = [first, *rest]
        return follow(prefix, rest, trail, macro, namespace)

    def follow(prefix, rest, trail, macro, namespace):
        symbols = symbol_tables[macro]
        if rest and rest[-1] == "self":
            rest = rest[:-1]
        while rest:
            for n in range(len(prefix), 0, -1):
                if prefix[:n] in symbols and prefix[:n] not in modules:
                    if not macro and symbol_kinds.get(prefix[:n]) in {"fn", "const", "static"}:
                        return None, "value item used as path prefix"
                    return (symbols[prefix[:n]], prefix + tuple(rest)), "resolved"
            name, *rest = rest
            if name == "*":
                return None, "unresolved glob"
            if (rest or namespace == "module") and prefix + (name,) in modules:
                prefix += (name,)
                continue
            key = (prefix, name)
            if prefix + (name,) in modules and namespace == "unknown" and (prefix + (name,) in symbols or key in exports):
                return None, "ambiguous module/value namespace"
            if key in exports:
                if key in trail or len(exports[key]) != 1:
                    return None, "cyclic or cfg-ambiguous reexport"
                binding = exports[key][0]
                result, reason = resolve(binding[0], binding[1], binding[2], trail | {key}, macro, "module" if rest else namespace)
                if result is None:
                    return None, reason
                if result[0] is None:
                    return (None, result[1] + tuple(rest)), reason
                prefix = result[1]
            else:
                prefix += (name,)
                if prefix in symbols and (prefix not in modules or namespace == "value"):
                    if rest and not macro and symbol_kinds.get(prefix) in {"fn", "const", "static"}:
                        return None, "value item used as path prefix"
                    return (symbols[prefix], prefix + tuple(rest)), "resolved"
                if prefix not in modules:
                    return None, "unknown internal item"
        if prefix in modules and namespace != "value":
            if prefix in symbols and namespace == "unknown":
                return None, "ambiguous module/value namespace"
            return (prefix, prefix), "resolved"
        for n in range(len(prefix), 0, -1):
            if prefix[:n] in symbols:
                return (symbols[prefix[:n]], prefix), "resolved"
        return (prefix, prefix), "resolved"

    edges, census = defaultdict(set), []
    resolved_refs = []
    for ref in refs:
        path, owner, scope, file, line, kind, _offset, namespace = ref
        result, reason = resolve(path, owner, scope, macro=kind == "macro", namespace=namespace)
        site = f"{file}:{line}"
        if result is None or kind == "macro":
            coverage["unresolved_references"].append({"source": node(owner), "site": site, "path": "::".join(path),
                                                        "reason": reason if result is None else "macro expansion not evaluated"})
        if result is not None and result[0] is not None:
            target, canonical = result
            if target != owner:
                key = (node(owner), node(target), kind)
                edges[key].add(site)
                if occurrences is not None:
                    occurrences.add((*key, file, _offset))
            resolved_refs.append((ref, canonical, target))
        # This census counts written boundary paths, independent of canonical ownership.
        clean = list(path)
        if clean and clean[0] in {"crate", "$crate", "self", "super"}:
            base = () if clean[0] in {"crate", "$crate"} else owner
            while clean and clean[0] in {"crate", "$crate", "self", "super"}:
                segment = clean.pop(0)
                if segment == "super":
                    base = base[:-1]
            written = base + tuple(clean)
        else:
            written = tuple(clean)
        boundary = next((b for b in (("services", "discord"), ("server",), ("cli",)) if written[:len(b)] == b), None)
        if boundary and owner[:len(boundary)] != boundary:
            census.append({"source": node(owner), "target": node(boundary), "path": "::".join(path),
                           "kind": kind, "file": file, "line": line})
    body_refs = defaultdict(list)
    for body, path, target in resolved_refs:
        if body[0][:1] in {("crate",), ("$crate",)}:
            body_refs[(body[3], body[1])].append((body, target))
    # Exported macros carry statically visible body dependencies to every known caller.
    for caller, canonical, _target in resolved_refs:
        if caller[5] != "macro" or canonical not in macros:
            continue
        definition_owner, start, end, definition_file, _line = macros[canonical]
        definition_tokens = next((inv.source_tokens[s] for s, _o, _c in inv.sources if s.relative_to(repo).as_posix() == definition_file), [])
        for body, body_target in body_refs[(definition_file, definition_owner)]:
            if definition_tokens[start].start <= body[6] <= definition_tokens[end - 1].start and caller[1] != body_target:
                key = (node(caller[1]), node(body_target), "macro")
                edges[key].add(f"{caller[3]}:{caller[4]}")
                if occurrences is not None:
                    occurrences.add((*key, caller[3], caller[6], body[3], body[6]))
    return nodes, edges, census


def analyze(repo: Path, *, occurrence_projection: list[dict] | None = None) -> dict:
    """Measure every production crate root; unknown constructs always retain evidence."""
    coverage = {"reached_files": set(), "cfg_scope": "all-non-test-cfg-union", "read_failures": [],
                "module_errors": [], "cfg_uncertainties": [], "unresolved_references": [], "cfg_conditions": [],
                "target_os": "all (source union; cfg feasibility not evaluated)"}
    external = {"std", "core", "alloc", "Self", "Vec", "String", "Box", "Option", "Result"}
    manifest = repo / "Cargo.toml"
    if manifest.is_file():
        try:
            cargo = tomllib.loads(manifest.read_text("utf-8"))
            for section in ("dependencies", "dev-dependencies", "build-dependencies"):
                external.update(name.replace("-", "_") for name in cargo.get(section, {}))
        except (OSError, UnicodeError, ValueError) as exc:
            coverage["read_failures"].append(f"Cargo.toml: {exc}")
    nodes, edges, census = set(), defaultdict(set), []
    # Physical token provenance preserves repeated references and removes repeated cfg visits.
    occurrences = set() if occurrence_projection is not None else None
    roots = crate_roots(repo)
    if not roots:
        coverage["module_errors"].append("no crate roots found")
    for root in roots:
        found, dependencies, references = _crate(root, repo, coverage, external, occurrences)
        nodes.update(found)
        for key, sites in dependencies.items():
            edges[key].update(sites)
        census.extend(references)
    if occurrence_projection is not None:
        counts = defaultdict(int)
        for source, target, kind, *_provenance in occurrences:
            counts[(source, target, kind)] += 1
        occurrence_projection.extend({"source": source, "target": target, "kind": kind, "count": count}
                                     for (source, target, kind), count in sorted(counts.items()))
    git = subprocess.run(["git", "-C", str(repo), "rev-parse", "HEAD"], capture_output=True, text=True)
    coverage.update(reached_files=sorted(coverage["reached_files"]), logical_modules=len(nodes),
                    unclassified_modules=sorted(nodes), classification="not-configured", complete=False)
    return {"schema_version": SCHEMA_VERSION, "tool_version": 1, "input_sha": git.stdout.strip() if git.returncode == 0 else None,
            "coverage": coverage, "nodes": sorted(nodes), "census": census, "sccs": _sccs(nodes, edges),
            "edges": [{"source": s, "target": t, "kind": k, "sites": sorted(sites)} for (s, t, k), sites in sorted(edges.items())]}


def _run(allowlist):
    occurrences = []
    result = analyze(common.REPO_ROOT, occurrence_projection=occurrences)
    assessment = project(result, load_mapping(common.REPO_ROOT), occurrences)
    from .module_layer_baseline import baseline, _reason
    from .module_layer_ratchet import compare, load_inputs
    try:
        accepted, moves = load_inputs(common.REPO_ROOT)
        comparison = compare(accepted, baseline(result, load_mapping(common.REPO_ROOT), result["input_sha"], occurrences), moves)
    except (OSError, UnicodeError, ValueError, TypeError) as exc:
        comparison = {"errors": [{"code": "invalid-input", "message": _reason(str(exc))}], "warnings": [], "decreases": []}
    yield Finding("module_layers", "info", "src", None,
                  f"{len(result['nodes'])} modules, {len(result['edges'])} dependency edges; coverage incomplete; {len(comparison['decreases'])} debt reductions",
                  {"measurement": result, "reference_occurrences": occurrences,
                   "layer_analysis": assessment, "ratchet": comparison})
    for warning in comparison["warnings"]:
        yield Finding("module_layers", "warn", "src", None, warning["message"], warning)
    for error in comparison["errors"]:
        yield Finding("module_layers", "error", "src", None, error["message"], error)


CHECK = CheckSpec(key="module_layers", title="Module dependency coverage", description="Canonical module graph and layer debt ratchet",
                  hard_gate=True, runner=_run)
