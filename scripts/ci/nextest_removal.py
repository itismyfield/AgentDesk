"""Parse and match base-diff test removal justifications."""

from pathlib import Path
import re
import sys

PLATFORMS = ("linux", "macos", "windows")
REMOVAL_KINDS = ("removed", "runnable->ignored")
PREFIX_SUFFIX = "::*"
# IDs outside this alphabet would need unverified filterset escaping.
ID_RE = re.compile(r"^[A-Za-z0-9_:.\-]+$")


class PlanError(ValueError):
    pass


def check_id(binary_id, test_name):
    if any(not isinstance(part, str) or not ID_RE.fullmatch(part) for part in (binary_id, test_name)):
        raise PlanError(f"unsupported characters in test ID {binary_id!r} {test_name!r}")
    return (binary_id, test_name)


def _module_targets(test):
    binary, name = test.split(" ")
    while "::" in name:
        name = name.rsplit("::", 1)[0]
        yield f"{binary} {name}{PREFIX_SUFFIX}"


def _overlap(left, right):
    return left == right or left in _module_targets(right) or right in _module_targets(left)


def load_justifications(path):
    """Reasons keyed by exact test or module prefix, kind and optional platform."""
    rows = {}
    prefixes = set()
    for n, line in enumerate(Path(path).read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        parts = [part.strip() for part in line.split("|")]
        if len(parts) != 3 or parts[1] not in REMOVAL_KINDS or not parts[2] or len(parts[0].split(" ")) != 2:
            raise PlanError(f"{path}:{n}: need `<binary-id> <test> | removed|runnable->ignored | [platform=<os>] <reason>` (scope optional)")
        target, kind, reason = parts
        binary, name = target.split(" ")
        prefix = name.endswith(PREFIX_SUFFIX)
        module_or_test = name.removesuffix(PREFIX_SUFFIX)
        check_id(binary, module_or_test)
        if prefix and (kind != "removed" or not all(module_or_test.split("::"))):
            raise PlanError(f"{path}:{n}: module prefixes must use `<module>::* | removed`")
        platform = None
        if reason.startswith("[platform="):
            marker, separator, reason = reason.partition("] ")
            platform = marker.removeprefix("[platform=")
            if kind != "removed" or platform not in PLATFORMS or not separator or not reason.strip():
                raise PlanError(f"{path}:{n}: only removed rows may name platform=linux, platform=macos or platform=windows")
        scopes = PLATFORMS + (None,) if platform is None else (None, platform)
        if (any((target, kind, scope) in rows for scope in scopes)
                or any(k == kind and p in scopes and _overlap(target, t)
                       for t, k, p in (rows if prefix else prefixes))):
            raise PlanError(f"{path}:{n}: {target} {kind} is listed twice (overlapping justifications)")
        key = (target, kind, platform)
        if prefix:
            prefixes.add(key)
        rows[key] = reason
    return rows


def _presence_targets(ids, prefixes):
    tests = {f"{binary} {name}" for binary, name in ids}
    if not prefixes:
        return tests
    return tests | {target for test in tests for target in _module_targets(test) if target in prefixes}


def judge_justifications(rows, losses, removed, present_sources, linux_base, linux_head):
    """Return missing loss platforms and the stale, landed and carried row sets."""
    source_presence = present_sources or {}
    # The source report records base | head. Only the current head can block a prefix.
    heads = {p: ids - removed[p] for p, ids in source_presence.items()}
    heads.setdefault("linux", set()).update(linux_head)
    prefixes = {target for target, _, _ in rows if target.endswith(PREFIX_SUFFIX)}
    head_targets = {p: _presence_targets(ids, prefixes) for p, ids in heads.items()}

    def absent(target, platforms):
        return all(p in head_targets and target not in head_targets[p] for p in platforms)

    eligible = {row for row in rows if row[0] in prefixes and absent(row[0], (row[2],) if row[2] else PLATFORMS)}
    matched = set()
    missing = {}
    for (test, kind), platforms in losses.items():
        prefix_targets = set(_module_targets(test)) & prefixes if eligible and kind == "removed" else ()
        for platform in platforms:
            matches = {(test, kind, scope) for scope in (None, platform)} & rows.keys()
            matches.update({(target, kind, scope) for target in prefix_targets for scope in (None, platform)} & eligible)
            matched.update(matches)
            if not matches:
                missing.setdefault((test, kind), set()).add(platform)

    stale = rows.keys() - matched
    # A continuing exclusion retains its scope only while another platform has coverage.
    carried = {row for row in stale if row[2] in source_presence and absent(row[0], (row[2],))
               and any(row[0] in targets for p, targets in head_targets.items() if p != row[2])}
    stale -= carried
    landed = set()
    if {"macos", "windows"} <= source_presence.keys():
        known_tests = linux_base | linux_head | source_presence["macos"] | source_presence["windows"]
        known_targets = _presence_targets(known_tests, prefixes)
        landed = {row for row in stale if row[2] is None and row[0] not in known_targets}
    return missing, stale, landed, carried


def row_text(row):
    test, kind, platform = row
    return f"{test} | {kind}" + (f" | platform={platform}" if platform else "")


def warn_stale(rows, stale, landed, base_sha, *, stream=None, summary=None):
    warnings = []
    for row in sorted(stale, key=row_text):
        message = ("refers to a test absent from base and head; clear in any PR" if row in landed else
                   f"matches no loss against {base_sha}")
        warnings.append(f"WARN justification row {message}: {row_text(row)} | {rows[row]}")
    for warning in warnings:
        print(warning, file=stream if stream is not None else sys.stdout)
    if warnings and summary:
        with Path(summary).open("a", encoding="utf-8") as out:
            out.write("\n### Test removal justification warnings\n\n")
            out.writelines(f"- {warning}\n" for warning in warnings)
