"""Full-fidelity Rust tokens and lexical scopes for source accessor audits."""
from __future__ import annotations

from dataclasses import dataclass
from collections import ChainMap
import re

from rust_lex import StripState, lex_segments

TOKEN = re.compile(r"(?:r#)?[A-Za-z_]\w*|[0-9][A-Za-z0-9_]*|::|->|=>|[^\s]")


@dataclass(frozen=True)
class Token:
    value: str
    start: int
    line: int
    kind: str = "code"


def tokens(text):
    """Keep literal values in hashes while never treating their contents as code."""
    result, state, offset, line = [], StripState(), 0, 1
    for raw in text.splitlines(keepends=True):
        for kind, part in lex_segments(raw, state):
            if kind == "literal":
                if result and result[-1].kind == "literal" and result[-1].start + len(result[-1].value) == offset:
                    old = result.pop()
                    result.append(Token(old.value + part, old.start, old.line, "literal"))
                else:
                    result.append(Token(part, offset, line, "literal"))
            elif kind == "code":
                result.extend(Token(m[0].removeprefix("r#"), offset + m.start(), line) for m in TOKEN.finditer(part))
            offset += len(part)
            line += part.count("\n")
    return result


def pairs(ts):
    stack, result = [], {}
    for i, token in enumerate(ts):
        if token.kind == "literal":
            continue
        if token.value in {"(", "[", "{"}:
            stack.append(i)
        elif token.value in {")", "]", "}"}:
            if not stack or ts[stack[-1]].value != {")": "(", "]": "[", "}": "{"}[token.value]:
                raise ValueError(f"unbalanced Rust delimiter at line {token.line}")
            start = stack.pop()
            result[start] = i
    if stack:
        raise ValueError(f"unclosed Rust delimiter at line {ts[stack[-1]].line}")
    return result


def _namespaces(ts, matched):
    modules, namespaces = {}, {}
    for i, token in enumerate(ts[:-2]):
        if token.kind != "code":
            continue
        if token.value in {"mod", "trait"} and ts[i + 2].value == "{":
            namespaces[i + 2] = ts[i + 1].value
            if token.value == "mod":
                modules[i + 2] = ts[i + 1].value
        elif token.value == "impl" and (i == 0 or ts[i - 1].value in {"{", "}", ";", "]", "unsafe", "default"}):
            j, angle = i + 1, 0
            while j < len(ts):
                value = ts[j].value
                if value == "{" and angle == 0:
                    namespaces[j] = "impl " + " ".join(t.value for t in ts[i + 1:j])
                    break
                if value == ";":
                    break
                if value in {"(", "[", "{"}:
                    j = matched[j] + 1
                    continue
                angle += (value == "<") - (value == ">")
                j += 1
    return modules, namespaces


def scopes(ts, matched):
    """Assign lexical scopes and restore the outer function after nested items."""
    declarations, names, openings = set(), {}, {}
    for i, token in enumerate(ts[:-1]):
        if token.kind == "code" and token.value == "fn":
            declarations.add(i + 1)
            j = i + 2
            while j < len(ts) and ts[j].value not in {"{", ";"}:
                j = matched[j] + 1 if ts[j].value in {"(", "["} else j + 1
            if j < len(ts) and ts[j].value == "{":
                openings[j] = ts[i + 1].value
    _, namespaces = _namespaces(ts, matched)
    metadata, stack = [], []
    for i, token in enumerate(ts):
        function = "::".join(names[x] for x in stack if x in names) or "<module>"
        namespace = [namespaces[x] for x in stack if x in namespaces]
        metadata.append((tuple(stack), "::".join([*namespace, function])))
        if token.kind == "code" and token.value == "{":
            stack.append(i)
            if i in openings:
                names[i] = openings[i]
        elif token.kind == "code" and token.value == "}":
            stack.pop()
    return metadata, declarations


def module_context(ts, scope):
    """Inline module names only; function and implementation braces are not modules."""
    return tuple(ts[i - 1].value for i in scope
                 if i >= 2 and ts[i - 2].value == "mod")


class AliasBindings(dict):
    pass


def imports(ts, production_offsets, metadata):
    """Expand grouped imports, retaining aliases with their lexical scope."""
    aliases, excluded = AliasBindings(), set()
    aliases.contexts = {scope: module_context(ts, scope) for scope in {row[0] for row in metadata}}
    aliases.module_scopes = {scope for scope, context in aliases.contexts.items() if len(scope) == len(context)}
    aliases.bindings_cache = {}
    values = [t.value for t in ts]
    for i, token in enumerate(ts):
        if token.value != "use" or token.start not in production_offsets or token.kind != "code":
            continue
        end = next((j for j in range(i + 1, len(ts)) if values[j] == ";"), len(ts))
        excluded.update(range(i, end + 1))
        cursor = i + 1

        def group(prefix):
            nonlocal cursor
            while cursor < end and values[cursor] != "}":
                path = list(prefix)
                while cursor < end and values[cursor] not in {"{", "}", ",", "as"}:
                    if values[cursor] != "::":
                        path.append(values[cursor])
                    cursor += 1
                if cursor < end and values[cursor] == "{":
                    cursor += 1
                    group(path)
                    cursor += 1
                elif path:
                    local = path[-1]
                    if cursor < end and values[cursor] == "as":
                        cursor += 1
                        local = values[cursor]
                        cursor += 1
                    if path[-1] == "self":
                        path.pop()
                        local = path[-1] if local == "self" else local
                    if local not in {"self", "super", "crate", "*"}:
                        aliases[(metadata[i][0], local)] = path
                if cursor < end and values[cursor] == ",":
                    cursor += 1
        group([])
    return aliases, excluded


def _absolute(names, context):
    names = list(names)
    if names[:1] == ["crate"]:
        return names[1:]
    if names[:1] == ["self"]:
        return list(context) + names[1:]
    if names[:1] == ["super"]:
        context = list(context)
        while names[:1] == ["super"]:
            names.pop(0)
            context = context[:-1]
        return context + names
    return names


def exports_for(ts, metadata, aliases, module, *, exports=None):
    """Expose module-level import bindings for lexical cross-file resolution."""
    result = {}
    for (scope, name), target in aliases.items():
        if scope in getattr(aliases, "module_scopes", {()}):
            key = (*module, *module_context(ts, scope), name)
            result[key] = _absolute(resolve(target, scope, aliases, module=module, exports=exports),
                                    (*module, *module_context(ts, scope)))
    return result


def resolve(names, scope, aliases, *, module=(), exports=None):
    """Resolve lexical imports and explicit module aliases without following call bodies."""
    contexts = getattr(aliases, "contexts", {})
    cache = getattr(aliases, "bindings_cache", {})
    if tuple(module) not in cache:
        cache[tuple(module)] = {
            (*module, *contexts.get(binding_scope, ()), name):
                _absolute(target, (*module, *contexts.get(binding_scope, ())))
            for (binding_scope, name), target in aliases.items()
            if binding_scope in getattr(aliases, "module_scopes", {()})}
    bindings = ChainMap(exports or {}, cache[tuple(module)])
    original = list(names)
    names, seen, lexical = list(names), set(), True
    while names:
        binding = next(((scope[:n], names[0]) for n in range(len(scope), -1, -1)
                        if (scope[:n], names[0]) in aliases), None) if lexical else None
        if binding is not None:
            marker = ("lexical", binding)
            if marker in seen:
                break
            seen.add(marker)
            names, scope = aliases[binding] + names[1:], binding[0]
            continue
        context = (*module, *contexts.get(scope, ()))
        names = _absolute(names, context)
        candidates = [tuple(names)]
        relative = (*context, *names)
        if relative != tuple(names):
            candidates.insert(0, relative)
        found = next(((path[:n], path[n:]) for path in candidates
                      for n in range(len(path), 0, -1) if path[:n] in bindings), None)
        if found is None:
            break
        key, tail = found
        marker = ("module", key)
        if marker in seen:
            break
        seen.add(marker)
        names, lexical = list(bindings[key]) + list(tail), False
    return names or original


def call_arguments(values, index):
    """Find the argument opener after an optional Rust turbofish."""
    cursor = index + 1
    if values[cursor:cursor + 2] == ["::", "<"]:
        cursor += 2
        depth = 1
        while cursor < len(values) and depth:
            depth += (values[cursor] == "<") - (values[cursor] == ">")
            cursor += 1
    return cursor if values[cursor:cursor + 1] == ["("] else None


def boot_bindings(ts, metadata, aliases, matched, *, module=(), exports=None):
    """Recognize explicit boot types and with_boot closure parameters, without type inference."""
    values, bindings = [t.value for t in ts], []
    functions = []
    for i, token in enumerate(ts[:-1]):
        if token.kind != "code" or token.value != "fn" or not values[i + 1].isidentifier():
            continue
        opening = i + 2
        while opening < len(ts) and values[opening] not in {"(", "{", ";"}:
            opening += 1
        if opening == len(ts) or values[opening] != "(":
            continue
        closing = matched[opening]
        body = closing + 1
        while body < len(ts) and values[body] not in {"{", ";"}:
            body += 1
        if body < len(ts) and values[body] == "{":
            functions.append((opening, closing, body, matched[body]))
    for i, token in enumerate(ts):
        scope, function = metadata[i]
        if token.kind == "code" and token.value == ":" and i and values[i - 1].isidentifier():
            end = i + 1
            while end < len(ts) and values[end] not in {",", ")", "=", ";", "{", "|"}:
                end += 1
            names = [v for v in values[i + 1:end] if v[0].isalpha()]
            for name in names:
                resolved = resolve([name], scope, aliases, module=module, exports=exports)
                if resolved[-1] in {"BootSessionHosts", "BootChannels"}:
                    region = next((r for r in functions if r[0] < i < r[1]), None)
                    if region:
                        lo, hi = region[2:]
                        where, owner = metadata[lo + 1]
                    else:
                        lo, hi = i, matched[scope[-1]] if scope else len(ts)
                        where, owner = scope, function
                        pipe = next((j for j in range(i - 1, -1, -1) if values[j] == "|"), -1)
                        if pipe >= 0 and not any(v in {";", "=", "{", "}"} for v in values[pipe + 1:i]):
                            closing = next((j for j in range(i + 1, len(ts)) if values[j] == "|"), None)
                            if closing is not None:
                                lo = closing + 1
                                if values[lo] == "{":
                                    hi = matched[lo]
                                    where, owner = metadata[lo + 1]
                                else:
                                    hi = lo
                                    while hi < len(ts) and values[hi] not in {";", ",", ")", "]", "}"}:
                                        hi = matched[hi] + 1 if hi in matched else hi + 1
                    bindings.append((values[i - 1], resolved[-1], owner, where, lo, hi))
        if token.kind == "code" and resolve([token.value], scope, aliases, module=module, exports=exports)[-1] == "with_boot":
            opening = call_arguments(values, i)
            if opening is not None:
                end = matched[opening]
                pipe = opening + 1 + (values[opening + 1:opening + 2] == ["move"])
                if values[pipe:pipe + 1] == ["|"] and pipe + 1 < end:
                    bindings.append((values[pipe + 1], "BootSessionHosts", function, scope, pipe, end))
    return bindings


def receiver_start(values, index, matched):
    reverse = {end: start for start, end in matched.items()}
    cursor = index - 2
    while cursor >= 0 and values[cursor] == ")":
        opening = reverse[cursor]
        if opening >= 2 and values[opening - 2] == ".":
            cursor = opening - 3
        else:
            cursor = opening + 1
            break
    return cursor
