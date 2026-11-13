"""Advisory clustering of copied Rust boolean logic, without reordering effects."""
from __future__ import annotations

from collections import defaultdict
from hashlib import sha256
import json
from pathlib import Path

from ci.rust_test_sources import Inventory, crate_roots, walk
from .. import common
from ..common import Finding
from ..rust_accessor_scan import exports_for, imports, pairs, resolve, scopes, tokens
from . import CheckSpec
from .low_level_accessors import RULES

THRESHOLD = 3
ACCESSORS = {r["accessor"] for r in RULES.values()} - {"with_boot", "channels"}


def _views(repo):
    roots, inventory = crate_roots(repo), Inventory()
    if not roots:
        raise ValueError("no Rust crate roots found")
    for root in roots:
        walk(root, repo, inventory, production=True)
    if inventory.read_failures or inventory.errors:
        raise ValueError("; ".join(inventory.read_failures + inventory.errors))
    views, exports = [], {}
    for path, production in sorted(inventory.source_tokens.items()):
        if production is None:
            continue
        text = path.read_text("utf-8")
        ts = tokens(text)
        matched = pairs(ts)
        metadata, _ = scopes(ts, matched)
        offsets = {t.start for t in production if t.kind != "string"}
        aliases, _ = imports(ts, offsets, metadata)
        module = min(inventory.modules[path])
        exports.update(exports_for(ts, metadata, aliases, module))
        views.append((path.relative_to(repo).as_posix(), text, ts, matched,
                      metadata, offsets, aliases, module))
    return views, exports


def _double(ts, i, char):
    return (i + 1 < len(ts) and ts[i].kind == ts[i + 1].kind == "code"
            and ts[i].value == ts[i + 1].value == char
            and ts[i].start + 1 == ts[i + 1].start)


def _segments(ts, matched, lo=0, hi=None):
    """Visit expression containers; statement/block/closure boundaries stop a chain."""
    hi = len(ts) if hi is None else hi
    start, i = lo, lo
    while i < hi:
        token = ts[i]
        value = token.value
        if token.kind == "literal":
            i += 1
            continue
        if value in {"(", "["}:
            yield from _segments(ts, matched, i + 1, matched[i])
            i = matched[i] + 1
            continue
        if value == "{":
            if start < i:
                yield start, i
            yield from _segments(ts, matched, i + 1, matched[i])
            i = matched[i] + 1
            start = i
            continue
        if _double(ts, i, "|") and (i == start or ts[i - 1].value == "move"):
            i += 2
            start = i
            continue
        assignment = value == "=" and (i == 0 or ts[i - 1].value not in {"=", "!", "<", ">"}) and (
            i + 1 == hi or ts[i + 1].value not in {"=", ">"})
        pipe = value == "|" and not _double(ts, i, "|") and not (i and _double(ts, i - 1, "|"))
        if value in {";", ",", "=>", "if", "while", "return", "else", "let"} or assignment or pipe:
            if start < i:
                yield start, i
            start = i + 1
        i += 1
    if start < hi:
        yield start, hi


def _bindings(ts, matched, metadata):
    """Only explicit parameters, local patterns and closure parameters are alpha locals."""
    result = []
    values = [t.value for t in ts]
    reverse, matches = {b: a for a, b in matched.items()}, set()
    for i, token in enumerate(ts):
        if token.kind == "code" and token.value == "match":
            j = i + 1
            while j < len(ts) and values[j] not in {"{", ";"}:
                j = matched[j] + 1 if j in matched else j + 1
            if j < len(ts) and values[j] == "{":
                matches.add(j)

    def declared_type(j):
        if values[j + 1:j + 2] != [":"]:
            return ""
        j += 2
        while values[j:j + 1] in (["&"], ["mut"]):
            j += 1
        while values[j + 1:j + 2] == ["::"]:
            j += 2
        return values[j] if j < len(values) else ""

    for i, token in enumerate(ts):
        if token.kind != "code":
            continue
        scope = metadata[i][0]
        end = matched[scope[-1]] if scope else len(ts)
        if token.value in {"if", "=>"} and scope and scope[-1] in matches:
            j = i - 1
            while j > scope[-1] and values[j] not in {",", "=>"}:
                j = reverse[j] - 1 if j in reverse else j - 1
            start = j + 1
            stop = next((j for j in range(start, i) if values[j] == "if"), i)
            arm_end = i + 1
            while arm_end < end and values[arm_end] != ",":
                arm_end = matched[arm_end] + 1 if arm_end in matched else arm_end + 1
            for j in range(start, stop):
                if (values[j].isidentifier() and values[j] not in {"mut", "ref", "_", "true", "false"}
                        and not values[j][:1].isupper() and values[j - 1] != "::"
                        and values[j + 1:j + 2] != ["::"]):
                    result.append((values[j], j, arm_end, ""))
        if token.value == "fn":
            opening = next((j for j in range(i + 2, len(ts)) if values[j] in {"(", "{", ";"}), None)
            if opening is None or values[opening] != "(":
                continue
            closing = matched[opening]
            body = next((j for j in range(closing + 1, len(ts)) if values[j] in {"{", ";"}), None)
            if body is not None and values[body] == "{":
                for j in range(opening + 1, closing):
                    if values[j].isidentifier() and values[j + 1:j + 2] == [":"]:
                        result.append((values[j], body, matched[body], declared_type(j)))
        elif token.value == "let":
            j = i + 1
            while j < len(ts) and values[j] not in {"=", ";", ":"}:
                if (values[j].isidentifier() and values[j] not in {"mut", "ref", "_", "true", "false"}
                        and not values[j][:1].isupper()
                        and values[j + 1:j + 2] != ["::"] and values[j - 1] != "::"):
                    kind = declared_type(j)
                    if values[j + 1:j + 2] == ["="] and j + 2 < len(ts):
                        previous = [b for b in result if b[0] == values[j + 2] and b[1] <= i < b[2]]
                        kind = previous[-1][3] if previous else ""
                    result.append((values[j], j, end, kind))
                j += 1
        elif token.value == "|" and not _double(ts, i, "|") and not (i and _double(ts, i - 1, "|")):
            closing = next((j for j in range(i + 1, end) if values[j] in {"|", ";", "{"}), None)
            if closing is not None and values[closing] == "|":
                closure_end = (matched[closing + 1] if values[closing + 1:closing + 2] == ["{"] else
                               min((b for a, b in matched.items() if a < i < b), default=end))
                for j in range(i + 1, closing):
                    if (values[j].isidentifier() and values[j] not in {"mut", "ref"}
                            and (j == i + 1 or values[j - 1] in {",", "mut", "ref"})):
                        result.append((values[j], j, closure_end, declared_type(j)))
    return result


def _accessor_calls(ts, matched, metadata, aliases, module, exports):
    values, result = [t.value for t in ts], {}
    reverse = {v: k for k, v in matched.items()}
    for i, token in enumerate(ts[:-1]):
        if token.kind != "code" or values[i + 1] != "(" or not token.value.isidentifier():
            continue
        start = i
        while start >= 2 and values[start - 1] == "::":
            start -= 2
        names = values[start:i + 1:2]
        accessor = resolve(names, metadata[i][0], aliases, module=module, exports=exports)[-1]
        if accessor not in ACCESSORS:
            continue
        if start and values[start - 1] == ".":
            start -= 2
            while start >= 0:
                if values[start] == ")":
                    start = reverse[start]
                    if start and values[start - 1].isidentifier():
                        start -= 1
                if start >= 2 and values[start - 1] == ".":
                    start -= 2
                else:
                    break
        fields = [values[j + 1] for j in range(start, i - 1) if values[j] == "."]
        name = accessor + (":" + ".".join(fields) if fields else "")
        if "(" in values[start:i - 1]:
            name = accessor + ":receiver=" + " ".join(values[start:i - 1])
        result[start] = (i + 1, matched[i + 1], name)
    return result


def _normalize(ts, lo, hi, bindings, calls, mode):
    local_ids, notes = {}, set()

    def visit(start, end, argument=False):
        result, i = [], start
        while i < end:
            token = ts[i]
            if mode != "exact" and i in calls and calls[i][1] < end:
                opening, closing, name = calls[i]
                result.extend([("accessor", name), ("code", "(")])
                result.extend(visit(opening + 1, closing, True))
                result.append(("code", ")"))
                i = closing + 1
                continue
            if argument and i == start + 1 and i + 4 == end and [
                    t.value for t in ts[i:i + 4]] == [".", "get", "(", ")"]:
                receiver = max((b for b in bindings if b[0] == ts[i - 1].value and b[1] <= i - 1 < b[2]),
                               key=lambda b: b[1], default=None)
                if receiver and (i < 2 or ts[i - 2].value not in {".", "::"}):
                    if receiver[3] != "ChannelId":
                        notes.add("unverified ChannelId receiver")
                    i += 4
                    continue
            value = token.value
            if token.kind == "code" and value[:1].isdigit():
                value = value.replace("_", "")
            if value == "(" and i + 2 < end and ts[i + 2].value == ")" and (
                    i == start or not (ts[i - 1].value.isidentifier() or ts[i - 1].value == ")")):
                result.extend(visit(i + 1, i + 2, argument))
                i += 3
                continue
            binding = max((b for b in bindings if b[0] == value and b[1] <= i < b[2]),
                          key=lambda b: b[1], default=None)
            if (mode != "exact" and token.kind == "code" and value.isidentifier()
                    and (i == start or ts[i - 1].value not in {".", "::"})
                    and (i + 1 == end or ts[i + 1].value != "::")
                    and binding is not None):
                value = local_ids.setdefault(binding, f"$local{len(local_ids)}")
            result.append((token.kind, value))
            i += 1
        return result

    return visit(lo, hi), sorted(notes)


def _sites(view, exports, mode):
    path, text, ts, matched, metadata, production, aliases, module = view
    bindings = _bindings(ts, matched, metadata)
    calls = _accessor_calls(ts, matched, metadata, aliases, module, exports)
    for lo, hi in _segments(ts, matched):
        identifiers = [t for t in ts[lo:hi] if t.kind == "code" and t.value.isidentifier()]
        if not identifiers or any(t.start not in production for t in identifiers):
            continue
        # A single helper call is reuse. A registered raw accessor plus a predicate is policy logic.
        logic, i = False, lo
        while i < hi:
            if i in matched:
                i = matched[i] + 1
                continue
            logic |= (_double(ts, i, "&") or _double(ts, i, "|")) and lo < i and i + 2 < hi
            i += 1
        raw_predicate = any(lo <= i < end < hi and any(
            t.value in {"is_none", "is_some", "is_some_and", "is_ok", "is_err"} for t in ts[end + 1:hi])
            for i, (_, end, _) in calls.items())
        if not logic and not raw_predicate:
            continue
        normalized, notes = _normalize(ts, lo, hi, bindings, calls, mode)
        digest = sha256(json.dumps(normalized, separators=(",", ":")).encode()).hexdigest()
        first, last = ts[lo], ts[hi - 1]
        yield digest, normalized, {"path": path, "line": first.line, "column": first.start - text.rfind("\n", 0, first.start),
                                   "enclosing_fn": metadata[lo][1], "normalization_notes": notes,
                                   "kind": "compound" if logic else "known_accessor",
                                   "expression": text[first.start:last.start + len(last.value)]}


def _clusters(views, exports, mode):
    groups, normalized = defaultdict(dict), {}
    for view in views:
        for digest, expression, site in _sites(view, exports, mode):
            groups[digest][(site["path"], site["line"], site["column"])] = site
            normalized[digest] = expression
    return [{"rule": "duplicate_conditions", "normalized_hash": digest,
             "count": len(sites), "mode": mode, "normalized_tokens": normalized[digest],
             "function_count": len({(s["path"], s["enclosing_fn"]) for s in sites.values()}),
             "ambiguous": any(s["normalization_notes"] for s in sites.values()),
             "sites": [sites[key] for key in sorted(sites)]}
            for digest, sites in sorted(groups.items(), key=lambda item: (-len(item[1]), item[0]))
            if len(sites) >= THRESHOLD]


def scan(repo, mode="normalized"):
    """Report distinct source locations, not repeated visits through cfg/crate roots."""
    if mode not in {"exact", "normalized"}:
        raise ValueError(f"unknown condition mode: {mode}")
    return _clusters(*_views(repo.resolve()), mode)


def _run(_allowlist):
    try:
        for row in scan(common.REPO_ROOT):
            site = row["sites"][0]
            yield Finding("duplicate_conditions", "warn", site["path"], site["line"],
                          f"copied condition at {row['count']} sites; review for a shared policy", row)
    except (OSError, ValueError, KeyError, TypeError) as exc:
        yield Finding("duplicate_conditions", "warn", "src", None, f"condition scan failed: {exc}")


CHECK = CheckSpec(key="duplicate_conditions", title="Repeated conditions (advisory)", hard_gate=False,
                  description="Three or more normalized copies of boolean policy logic; review only.", runner=_run)


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=common.REPO_ROOT)
    parser.add_argument("--mode", choices=("exact", "normalized", "both"), default="normalized")
    args = parser.parse_args()
    views, exports = _views(args.repo.resolve())
    result = ({m: _clusters(views, exports, m) for m in ("exact", "normalized")}
              if args.mode == "both" else _clusters(views, exports, args.mode))
    print(json.dumps(result, ensure_ascii=False, indent=2))
