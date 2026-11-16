"""Measure main and the candidate in one run; only new module-layer debt blocks.

The base is always a commit on main's history, chosen from the CI event payload (pull request: the
merge commit's first parent; main push: the event's ``before``) or, locally, ``MODULE_LAYER_BASE_REF``
or ``git merge-base HEAD origin/main``. Both sides are measured by the candidate's own code in the
same interpreter and compared per (section, id) with ``module_layer_ratchet.compare``. Increases are
admitted only by approval files that this candidate adds under ``layer_approvals/``; files already on
the base are immutable and have no effect. Contract: docs/module-layer-base-compare.design.md.
"""
from __future__ import annotations

import argparse
from copy import deepcopy
from hashlib import sha256
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import tomllib

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from audit_maintainability.checks.module_layer_baseline import (  # noqa: E402
    BASELINE, MAPPING, _edges, _id, _json, _matches_prefix, baseline, load_mapping, project, uncertainty_sources)
from audit_maintainability.checks.module_layer_ratchet import SECTIONS, _path_targets, compare  # noqa: E402

APPROVALS = "scripts/audit_maintainability/layer_approvals"
BASE_ENV = "MODULE_LAYER_BASE_REF"
EDGE_SECTIONS = ("reverse_dependencies", "indeterminate_dependencies")
# The measurement (analyze/baseline/project) and its local imports. A test pins that this covers them.
MEASURING_CODE = (
    "scripts/audit_maintainability/__init__.py",
    "scripts/audit_maintainability/common.py",
    "scripts/audit_maintainability/rust_accessor_scan.py",
    "scripts/audit_maintainability/checks/__init__.py",
    "scripts/audit_maintainability/checks/module_layers.py",
    "scripts/audit_maintainability/checks/module_layer_baseline.py",
    "scripts/audit_maintainability/checks/service_server_backflow.py",
    "scripts/ci/rust_test_sources.py",
    "scripts/rust_production.py",
    "scripts/rust_lex.py",
    "scripts/rust_cfg.py",
    "scripts/generate_inventory_docs.py",
)
NAME = re.compile(r"[0-9]+-[a-z0-9-]+\.toml")
SHA256 = re.compile(r"[0-9a-f]{64}")
COMMIT = re.compile(r"[0-9a-f]{40}")
INCREASE_KINDS = ("admit-classification", "accepted-debt")
TOP_KEYS = {"schema_version", "issue", "summary", "increase", "move", "reclassify", "analyzer"}
ENTRY_KEYS = {
    "increase": {"section", "id", "source", "target", "kind", "max_count", "reason"},
    "move": {"section", "old_id", "new_id", "reason"},
    "reclassify": {"prefix", "reason"},
    "analyzer": {"digest", "reverse_removed", "reverse_added", "reason"},
}
# Runs a measurement with a given code tree (the candidate's, or the base's for analyzer changes).
DRIVER = r"""
import json, sys, tomllib
from pathlib import Path
code, tree, mapping_path, out, sha = sys.argv[1:6]
sys.path.insert(0, code)
from audit_maintainability.checks.module_layers import analyze
from audit_maintainability.checks import module_layer_baseline as mb
try:
    mapping = tomllib.loads(Path(mapping_path).read_text("utf-8"))
except (OSError, UnicodeError, ValueError) as exc:
    mapping = {"schema_version": 1, "layers": {}, "_errors": [f"layers.toml unavailable: {type(exc).__name__}"]}
occurrences = []
graph = analyze(Path(tree), occurrence_projection=occurrences)
projection = mb.project(graph, mapping, occurrences)
Path(out).write_text(json.dumps({"payload": mb.baseline(graph, mapping, sha, occurrences),
                                 "layers_of": {r["module"]: r["layer"] for r in projection["classifications"]},
                                 "ranks": mapping.get("layers", {}) if isinstance(mapping.get("layers"), dict) else {}}))
"""


class GateError(ValueError):
    """The gate cannot establish a trustworthy base or input; it fails closed."""


def _git(repo, *args, check=True, binary=False):
    result = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=not binary)
    if check and result.returncode:
        raise GateError(f"git {' '.join(args[:3])} failed: {(result.stderr or b'')!s:.200}")
    return result.stdout if binary else result.stdout.strip()


def _commit(repo, ref):
    sha = _git(repo, "rev-parse", "--verify", "-q", f"{ref}^{{commit}}", check=False)
    if not COMMIT.fullmatch(sha or ""):
        raise GateError(f"unresolvable commit: {ref}")
    return sha


def _ancestor(repo, older, newer):
    return subprocess.run(["git", "-C", str(repo), "merge-base", "--is-ancestor", older, newer],
                          capture_output=True).returncode == 0


def resolve_base(repo, env):
    """Choose the base commit; any doubt is an error, never a skip."""
    head = _commit(repo, "HEAD")
    if env.get("GITHUB_ACTIONS") == "true":
        if env.get(BASE_ENV):
            raise GateError(f"{BASE_ENV} is refused in CI; the base comes from the event payload")
        event = env.get("GITHUB_EVENT_NAME", "")
        payload = {}
        if env.get("GITHUB_EVENT_PATH"):
            try:
                payload = json.loads(Path(env["GITHUB_EVENT_PATH"]).read_text("utf-8"))
            except (OSError, UnicodeError, ValueError) as exc:
                raise GateError(f"unreadable event payload: {type(exc).__name__}") from exc
        if event == "pull_request":
            parents = _git(repo, "rev-list", "--parents", "-n", "1", "HEAD").split()
            pull = payload.get("pull_request") or {}
            if len(parents) != 3:
                raise GateError("pull request candidate must be GitHub's two-parent merge commit")
            if parents[2] != (pull.get("head") or {}).get("sha"):
                raise GateError("merge commit's second parent is not the pull request head")
            base, payload_base = parents[1], (pull.get("base") or {}).get("sha", "")
            payload_base = _commit(repo, payload_base) if COMMIT.fullmatch(payload_base) else ""
            if not payload_base or not (_ancestor(repo, payload_base, base) or _ancestor(repo, base, payload_base)):
                raise GateError("merge commit's first parent is not on the pull request base history")
            return {"mode": "pull_request", "base": base, "head": head}
        if event == "push":
            before = payload.get("before", "")
            if payload.get("ref") != "refs/heads/main":
                raise GateError("push comparison runs only for refs/heads/main")
            if not COMMIT.fullmatch(before) or before == "0" * 40:
                raise GateError("push event has no previous main commit")
            before = _commit(repo, before)
            if before == head or not _ancestor(repo, before, head):
                raise GateError("push event's previous commit is not a strict ancestor of HEAD")
            return {"mode": "push", "base": before, "head": head}
        return {"mode": "self", "base": head, "head": head}
    main = _git(repo, "rev-parse", "--verify", "-q", "refs/remotes/origin/main^{commit}", check=False)
    if not COMMIT.fullmatch(main or ""):
        raise GateError("origin/main is required to choose the local base (fetch it)")
    if env.get(BASE_ENV):
        base = _commit(repo, env[BASE_ENV])
        if not _ancestor(repo, base, main):
            raise GateError(f"{BASE_ENV} must be a commit on origin/main's history")
    else:
        base = _git(repo, "merge-base", "HEAD", main)
    return {"mode": "local", "base": base, "head": head}


def _dirty(repo):
    return bool(_git(repo, "status", "--porcelain", "--untracked-files=all", "--",
                     "src", "Cargo.toml", MAPPING, APPROVALS))


def _materialize(repo, sha, destination, paths):
    listing = _git(repo, "ls-tree", "-r", "--name-only", sha, "--", *paths).splitlines()
    if not listing or any(Path(p).name in {"agentdesk.yaml", "launchd.env"} for p in listing):
        raise GateError(f"source of {sha[:10]} is unavailable for measurement")
    archive = _git(repo, "archive", sha, "--", *sorted({p.split("/", 1)[0] for p in listing}), binary=True)
    with tarfile.open(fileobj=io.BytesIO(archive)) as source:
        for entry in source:
            target = destination / entry.name
            if not target.resolve().is_relative_to(destination.resolve()):
                raise GateError("archive escapes its measurement directory")
            if entry.isfile():
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(source.extractfile(entry).read())
            elif not entry.isdir():
                raise GateError("measurement requires regular files")


def _start_measurement(code, tree, mapping_text, sha, workdir, label):
    mapping = workdir / f"{label}-layers.toml"
    if mapping_text is not None:
        mapping.write_text(mapping_text, encoding="utf-8")
    out = workdir / f"{label}.json"
    process = subprocess.Popen([sys.executable, "-I", "-c", DRIVER, str(code), str(tree), str(mapping), str(out), sha],
                               cwd=workdir, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    return process, out


def _finish(process, out, label):
    _, stderr = process.communicate()
    if process.returncode:
        raise GateError(f"{label} measurement failed: {stderr.strip()[-300:]}")
    return json.loads(out.read_text("utf-8"))


def code_hash(repo, rev=None):
    """Blob ids of the measuring code at ``rev`` (None: the working tree)."""
    parts = []
    for path in MEASURING_CODE:
        if rev is None:
            blob = _git(repo, "hash-object", path, check=False) if (Path(repo) / path).is_file() else "missing"
        else:
            blob = _git(repo, "rev-parse", "-q", "--verify", f"{rev}:{path}", check=False) or "missing"
        parts.append((path, blob))
    return sha256(_json(parts).encode()).hexdigest()


def _cache_path(repo, key):
    common = Path(_git(repo, "rev-parse", "--path-format=absolute", "--git-common-dir"))
    return common / "adk-module-layer-cache" / f"{key}.json"


def _base_measurement(repo, base, code_root, workdir, local, label="base"):
    mapping = _git(repo, "show", f"{base}:{MAPPING}", check=False) if _git(
        repo, "cat-file", "-t", f"{base}:{MAPPING}", check=False) == "blob" else None
    cache = None
    if local:
        key = sha256(_json([base, code_hash(repo, None) if code_root == "head" else code_root, sys.version]).encode()).hexdigest()
        cache = _cache_path(repo, key)
        if cache.is_file():
            try:
                return None, json.loads(cache.read_text("utf-8")), cache
            except (OSError, ValueError):
                pass
    tree = workdir / f"{label}-src"
    tree.mkdir()
    _materialize(repo, base, tree, ("src", "Cargo.toml"))
    code = Path(__file__).resolve().parents[2] if code_root == "head" else Path(code_root)
    process, out = _start_measurement(code, tree, mapping + "\n" if mapping is not None else None, base, workdir, label)
    return (process, out), None, cache


def _load_approvals(repo, base):
    """Return (effective entries by kind, used file names, errors) for files this candidate adds."""
    errors, effective = [], {kind: [] for kind in ENTRY_KEYS}
    landed = {}
    for path in _git(repo, "ls-tree", "-r", "--name-only", base, "--", APPROVALS).splitlines():
        name = path.rsplit("/", 1)[-1]
        if name.endswith(".toml"):
            landed[name] = _git(repo, "show", f"{base}:{path}", binary=True)
    directory = Path(repo) / APPROVALS
    current = {p.name: p.read_bytes() for p in sorted(directory.glob("*.toml"))} if directory.is_dir() else {}
    for name, data in sorted(landed.items()):
        if name not in current:
            errors.append(_error("approval-immutable", "approvals", f"landed approval file {name} was deleted; landed approvals are immutable"))
        elif current[name] != data:
            errors.append(_error("approval-immutable", "approvals", f"landed approval file {name} was modified; add a new file instead"))
    files = sorted(name for name in current if name not in landed)
    seen = set()
    for name in files:
        problems = []
        try:
            document = tomllib.loads(current[name].decode("utf-8"))
        except (UnicodeError, ValueError) as exc:
            errors.append(_error("approval-invalid", "approvals", f"{name}: unreadable TOML ({type(exc).__name__})"))
            continue
        if not NAME.fullmatch(name):
            problems.append("file name must match <issue>-<slug>.toml")
        if set(document) - TOP_KEYS:
            problems.append(f"unknown keys {sorted(set(document) - TOP_KEYS)}")
        if document.get("schema_version") != 1 or type(document.get("issue")) is not int or document["issue"] < 1:
            problems.append("schema_version must be 1 and issue a positive integer")
        if not _text(document.get("summary")):
            problems.append("summary must be a reviewed sentence (not empty or TODO)")
        if NAME.fullmatch(name) and type(document.get("issue")) is int and not name.startswith(f"{document['issue']}-"):
            problems.append("file name must start with the issue number")
        for kind, keys in ENTRY_KEYS.items():
            rows = document.get(kind, [])
            if not isinstance(rows, list) or any(not isinstance(row, dict) for row in rows):
                problems.append(f"{kind} must be an array of tables")
                continue
            for row in rows:
                problem = _entry_problem(kind, row, keys)
                identity = _entry_identity(kind, row)
                if problem:
                    problems.append(f"{kind} {problem}")
                elif identity in seen:
                    problems.append(f"{kind} duplicates {identity[1:]}")
                else:
                    seen.add(identity)
                    effective[kind].append({**row, "file": name})
        errors.extend(_error("approval-invalid", "approvals", f"{name}: {problem}") for problem in problems)
    return effective, files, errors


def _text(value):
    return isinstance(value, str) and value.strip() and not value.strip().upper().startswith("TODO")


def _entry_problem(kind, row, keys):
    if set(row) - keys:
        return f"has unknown keys {sorted(set(row) - keys)}"
    if kind == "increase":
        if row.get("section") not in SECTIONS or not SHA256.fullmatch(str(row.get("id", ""))):
            return "requires a known section and a SHA256 id"
        if type(row.get("max_count")) is not int or row["max_count"] < 1:
            return "requires a positive integer max_count"
        if row.get("reason") not in INCREASE_KINDS:
            return f"reason must be one of {list(INCREASE_KINDS)}"
        if row["section"] in EDGE_SECTIONS:
            if not all(isinstance(row.get(k), str) and row[k] for k in ("source", "target", "kind")):
                return "on an edge requires source, target and kind"
            if row["id"] != _id("edge", row["source"], row["target"], row["kind"]):
                return "id does not match its source, target and kind"
        elif any(k in row for k in ("source", "target", "kind")):
            return "on an uncertainty takes no endpoints"
        if row["reason"] == "admit-classification" and row["section"] != "reverse_dependencies":
            return "admit-classification applies only to reverse_dependencies"
        return None
    if not _text(row.get("reason")):
        return "requires a reviewed reason (not empty or TODO)"
    if kind == "move":
        if row.get("section") not in SECTIONS or not all(SHA256.fullmatch(str(row.get(k, ""))) for k in ("old_id", "new_id")):
            return "requires a known section and SHA256 old_id/new_id"
        if row["old_id"] == row["new_id"]:
            return "must change the identifier"
    elif kind == "reclassify":
        if not isinstance(row.get("prefix"), str) or not re.fullmatch(r"[A-Za-z_]\w*(?:::[A-Za-z_]\w*)*", row["prefix"]):
            return "requires a logical module prefix"
    elif kind == "analyzer":
        if not SHA256.fullmatch(str(row.get("digest", ""))) or any(
                type(row.get(k)) is not int or row[k] < 0 for k in ("reverse_removed", "reverse_added")):
            return "requires a SHA256 digest and nonnegative reverse_removed/reverse_added"
    return None


def _entry_identity(kind, row):
    if kind == "increase":
        return (kind, row.get("section"), row.get("id"))
    if kind == "move":
        return (kind, row.get("section"), row.get("old_id"))
    if kind == "reclassify":
        return (kind, row.get("prefix"))
    return (kind,)


def _error(code, section, message, **extra):
    return {"code": code, "section": section, "message": message, **extra}


def _tables(payload):
    return {section: {row["id"]: row for row in payload[section]} for section in SECTIONS}


def apply_approvals(base, head, entries, base_layers, head_layers, base_ranks=None, head_ranks=None):
    """Raise base ceilings by this candidate's approvals; moves are validated on the raw measurements.

    Returns (accepted payload, moves manifest, errors). Increase limits are judged on the debt after
    move normalization: an increase naming a move's new_id raises the base record of its old_id. An
    approved record holds the measured count, so unused headroom never reads as a later reduction."""
    errors, old, new = [], _tables(base), _tables(head)
    moves, target_of = [], {}
    for row in entries["move"]:
        section, source, target = row["section"], row["old_id"], row["new_id"]
        if source in old[section] and source not in new[section] and target in new[section] and target not in old[section]:
            moves.append({"section": section, "old_id": source, "new_id": target})
            target_of[(section, target)] = source
        else:
            errors.append(_error("approval-move", section, f"{row['file']}: move {source[:12]}->{target[:12]} does not match "
                                 "a base id that disappeared and a new head id in the same section"))
    targets = [m["new_id"] for m in moves]
    if len(targets) != len(set(targets)):
        errors.append(_error("approval-move", "approvals", "two moves share one destination"))
    accepted = deepcopy(base)
    index = {section: {row["id"]: row for row in accepted[section]} for section in SECTIONS}
    mapping_changed = base["mapping_sha256"] != head["mapping_sha256"]
    relayered = sorted(m for m in base_layers.keys() & head_layers.keys()
                       if base_layers[m] is not None and base_layers[m] != head_layers[m])
    for row in entries["increase"]:
        section, identity, ceiling = row["section"], row["id"], row["max_count"]
        current = new[section].get(identity, {"count": 0})["count"]
        if row["reason"] == "admit-classification":
            previous = old["indeterminate_dependencies"].get(identity)
            if (not mapping_changed or identity in old["reverse_dependencies"] or previous is None
                    or previous["count"] != current or current > ceiling or relayered
                    or (base_ranks or {}) != (head_ranks or {})):
                errors.append(_error("approval-admit", section, f"{row['file']}: admission of {identity[:12]} requires a changed "
                                     "mapping that preserves every layer rank and known layer and an exact base indeterminate edge"))
                continue
            accepted["indeterminate_dependencies"] = [r for r in accepted["indeterminate_dependencies"] if r["id"] != identity]
            index["indeterminate_dependencies"].pop(identity, None)
            record = {k: row[k] for k in ("id", "source", "target", "kind")} | {"count": current}
            accepted["reverse_dependencies"].append(record)
            index["reverse_dependencies"][identity] = record
            continue
        resolved = target_of.get((section, identity), identity)
        previous = old[section].get(resolved, {"count": 0})["count"]
        if current <= previous:
            errors.append(_error("approval-unused", section, f"{row['file']}: increase {identity[:12]} is unused "
                                 f"(base {previous}, head {current})"))
        elif current > ceiling:
            errors.append(_error("approval-exceeded", section, f"{row['file']}: increase {identity[:12]} head {current} "
                                 f"exceeds max_count {ceiling}"))
        elif section == "reverse_dependencies" and resolved in index["indeterminate_dependencies"]:
            errors.append(_error("approval-admit", section, f"{row['file']}: {identity[:12]} was indeterminate on the base; "
                                 "use admit-classification"))
        elif resolved in index[section]:
            index[section][resolved]["count"] = current
        else:
            record = ({k: row[k] for k in ("id", "source", "target", "kind")} if section in EDGE_SECTIONS
                      else {"id": identity}) | {"count": current}
            accepted[section].append(record)
            index[section][identity] = record
    for section in SECTIONS:
        accepted[section].sort(key=lambda r: r["id"])
    accepted["indeterminate_edges"] = len(accepted["indeterminate_dependencies"])
    accepted["indeterminate_occurrences"] = sum(r["count"] for r in accepted["indeterminate_dependencies"])
    return accepted, {"schema_version": 1, "moves": moves}, errors


def moved_hidden_reverse(accepted, head, moves, sources, comparison):
    """Re-judge shrunk moved reverse debt on its candidate endpoints; compare() keeps the base endpoints.

    Errors when an unresolved reference in the moved source still names the moved target, and promotes
    uncertainty growth in that source to an error, as compare() does for unmoved shrunk sources."""
    errors = []
    limits = {row["id"]: row["count"] for row in accepted["reverse_dependencies"]}
    rows = {row["id"]: row for row in head["reverse_dependencies"]}
    moved_uncertainty = {m["old_id"]: m["new_id"] for m in moves["moves"] if m["section"] == "uncertainties"}
    for move in (m for m in moves["moves"] if m["section"] == "reverse_dependencies"):
        row = rows.get(move["new_id"])
        if row is None or row["count"] >= limits.get(move["old_id"], 0):
            continue
        for uncertainty in head["uncertainties"]:
            evidence = sources.get(uncertainty["id"], {})
            if evidence.get("source") == row["source"] and row["target"] in _path_targets(row["source"], evidence.get("path")):
                errors.append(_error("hidden-reverse", "reverse_dependencies", f"hidden-reverse: moved reverse_dependencies "
                                     f"{row['id']} shrank while unresolved {evidence.get('path')} still names its target",
                                     id=row["id"]))
                break
        for warning in list(comparison["warnings"]):
            if (warning["section"] == "uncertainties"
                    and sources.get(moved_uncertainty.get(warning.get("id"), warning.get("id")), {}).get("source") == row["source"]):
                comparison["warnings"].remove(warning)
                errors.append(warning)
    return errors


def layer_rules(base, head, base_layers, head_layers, ranks, measured_edges, sources, accepted, moves, prefixes):
    """Mapping-only reductions, declassification and reverse-shaped unresolved references."""
    errors, used = [], set()
    if base["mapping_sha256"] != head["mapping_sha256"]:
        current = {row["id"] for row in head["reverse_dependencies"]}
        moved = {m["old_id"]: m["new_id"] for m in moves["moves"] if m["section"] == "reverse_dependencies"}
        edges = {row["id"]: row for row in measured_edges}
        for row in base["reverse_dependencies"]:
            edge = edges.get(moved.get(row["id"], row["id"]))
            if moved.get(row["id"], row["id"]) in current or edge is None:
                continue
            match = {p for p in prefixes if any(_matches_prefix(edge[k], p) for k in ("source", "target"))}
            used |= match
            if not match:
                errors.append(_error("mapping-reduction", "reverse_dependencies",
                                     f"mapping-only reverse reduction requires reclassify approval: {row['id']}",
                                     id=row["id"], source=edge["source"], target=edge["target"]))
    for module in sorted(base_layers.keys() & head_layers.keys()):
        if base_layers[module] is not None and head_layers[module] is None:
            match = {p for p in prefixes if _matches_prefix(module, p)}
            used |= match
            if not match:
                errors.append(_error("declassified", "classification", f"declassified: {module} was "
                                     f"{base_layers[module]} on the base and is unclassified now", module=module))
    for prefix in sorted(set(prefixes) - used):
        errors.append(_error("approval-unused", "reclassify", f"reclassify {prefix} explains no mapping reduction or declassification"))
    limits = {row["id"]: row["count"] for row in accepted["uncertainties"]}
    moved = {m["new_id"]: m["old_id"] for m in moves["moves"] if m["section"] == "uncertainties"}
    for row in head["uncertainties"]:
        if row["count"] <= limits.get(moved.get(row["id"], row["id"]), 0):
            continue
        evidence = sources.get(row["id"], {})
        layer = head_layers.get(evidence.get("source"))
        if layer not in ranks:
            continue
        for target in _path_targets(evidence.get("source"), evidence.get("path")):
            if head_layers.get(target) in ranks and ranks[layer] < ranks[head_layers[target]]:
                errors.append(_error("unresolved-reverse", "uncertainties",
                                     f"unresolved-reverse: {evidence.get('source')} names higher-layer {target} "
                                     f"through {evidence.get('path')} ({row['count']} > {limits.get(row['id'], 0)})",
                                     id=row["id"], count=row["count"]))
                break
    return errors


def analyzer_delta(by_base_code, by_head_code):
    old = {r["id"]: r["count"] for r in by_base_code["reverse_dependencies"]}
    new = {r["id"]: r["count"] for r in by_head_code["reverse_dependencies"]}
    diffs = sorted((i, old.get(i, 0), new.get(i, 0)) for i in old.keys() | new.keys() if old.get(i, 0) != new.get(i, 0))
    return {"digest": sha256(_json(diffs).encode()).hexdigest() if diffs else None,
            "reverse_removed": sum(1 for _, a, b in diffs if b < a),
            "reverse_added": sum(1 for _, a, b in diffs if b > a)}


def evaluate(repo, env):
    """Return the gate verdict; unexpected failures become a blocking ``gate-unavailable`` error."""
    repo = Path(repo)
    try:
        return _evaluate(repo, env)
    except (GateError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as exc:
        message = f"module layer gate unavailable: {exc}"
        return {"summary": message, "errors": [_error("gate-unavailable", "configuration", message)],
                "warnings": [], "decreases": [], "extra": {}, "draft": {}}


def _evaluate(repo, env):
    from audit_maintainability.checks.module_layers import analyze
    chosen = resolve_base(repo, env)
    base_sha, head_sha, mode = chosen["base"], chosen["head"], chosen["mode"]
    dirty = _dirty(repo)
    if dirty and env.get("GITHUB_ACTIONS") == "true":
        raise GateError("CI candidate tree differs from HEAD in measured paths")
    local = mode == "local"
    errors = []
    if (repo / BASELINE).exists() and _git(repo, "cat-file", "-t", f"{base_sha}:{BASELINE}", check=False) != "blob":
        errors.append(_error("tombstone", "configuration", f"{BASELINE} was removed from main and must not come back"))
    analyzer_changed = code_hash(repo, base_sha) != code_hash(repo, None)
    with tempfile.TemporaryDirectory(prefix="module-layer-gate-") as temporary:
        workdir = Path(temporary)
        pending, base_side, cache = (None, None, None)
        if not (base_sha == head_sha and not dirty):
            pending, base_side, cache = _base_measurement(repo, base_sha, "head", workdir, local)
        old_code = None
        if analyzer_changed:
            scripts = workdir / "base-code"
            scripts.mkdir()
            _materialize(repo, base_sha, scripts, ("scripts",))
            old_code = _base_measurement(repo, base_sha, scripts / "scripts", workdir, False, "base-by-base-code")[0]
        occurrences = []
        graph = analyze(repo, occurrence_projection=occurrences)
        mapping = load_mapping(repo)
        head = baseline(graph, mapping, head_sha, occurrences)
        projection = project(graph, mapping, occurrences)
        head_layers = {r["module"]: r["layer"] for r in projection["classifications"]}
        ranks = mapping.get("layers", {}) if isinstance(mapping.get("layers"), dict) else {}
        sources = uncertainty_sources(graph, mapping, occurrences)
        if pending:
            base_side = _finish(*pending, "base")
            if cache is not None:
                cache.parent.mkdir(parents=True, exist_ok=True)
                temporary_cache = cache.with_name(cache.name + f".{os.getpid()}")
                temporary_cache.write_text(json.dumps(base_side))
                os.replace(temporary_cache, cache)
        elif base_side is None:
            base_side = {"payload": deepcopy(head), "layers_of": dict(head_layers), "ranks": ranks}
        old_side = _finish(*old_code, "base-by-base-code") if old_code else None
    base = base_side["payload"]
    for side, payload in (("base", base), ("head", head)):
        if payload["configuration_errors"]:
            errors.append(_error("invalid-input", "configuration", f"{side} measurement has configuration errors: "
                                 f"{payload['configuration_errors']}"))
    entries, approval_files, approval_errors = _load_approvals(repo, base_sha)
    errors.extend(approval_errors)
    accepted, moves, apply_errors = apply_approvals(base, head, entries, base_side["layers_of"], head_layers,
                                                    base_side.get("ranks", {}), ranks)
    errors.extend(apply_errors)
    comparison = compare(accepted, head, moves, sources)
    errors.extend(comparison["errors"])
    errors.extend(moved_hidden_reverse(accepted, head, moves, sources, comparison))
    errors.extend(layer_rules(base, head, base_side["layers_of"], head_layers, ranks, _edges(graph, occurrences),
                              sources, accepted, moves, [e["prefix"] for e in entries["reclassify"]]))
    delta = analyzer_delta(old_side["payload"], base) if old_side else {"digest": None, "reverse_removed": 0, "reverse_added": 0}
    approved = [e for e in entries["analyzer"] if (e["digest"], e["reverse_removed"], e["reverse_added"])
                == (delta["digest"], delta["reverse_removed"], delta["reverse_added"])]
    if delta["digest"] and not approved:
        errors.append(_error("analyzer-change", "reverse_dependencies", f"measuring code changes reverse debt on the base "
                             f"(-{delta['reverse_removed']}/+{delta['reverse_added']} ids, digest {delta['digest']}); "
                             "add an analyzer approval", **delta))
    for entry in entries["analyzer"]:
        if entry not in approved:
            errors.append(_error("approval-unused", "analyzer", f"{entry['file']}: analyzer approval does not match "
                                 f"the measured change ({delta['digest'] or 'no change'})"))
    decreases = compare(base, head, moves, sources)["decreases"]
    drop = {section: [0, 0] for section in SECTIONS}
    for row in decreases:
        drop[row["section"]][0] += 1
        drop[row["section"]][1] += row["baseline_count"] - row["current_count"]
    analyzer = ("unchanged" if not analyzer_changed else
                f"changed(reverse -{delta['reverse_removed']}/+{delta['reverse_added']})")
    summary = (f"module_layers mode={mode} base={base_sha[:10]} head={head_sha[:10]}{' dirty' if dirty else ''} "
               f"errors={len(errors)} warnings={len(comparison['warnings'])}; decreases: "
               + ", ".join(f"{s.split('_')[0]} {drop[s][0]} id/{drop[s][1]} occ" for s in SECTIONS)
               + f"; approvals used: {len(approval_files)}; analyzer: {analyzer}")
    extra = {"measurement": graph, "reference_occurrences": occurrences, "layer_analysis": projection,
             "ratchet": comparison, "gate": {"mode": mode, "base": base_sha, "head": head_sha, "dirty": dirty,
                                             "approval_files": approval_files, "analyzer": delta}}
    draft = {"head": head, "base": base, "errors": errors, "delta": delta, "moves": moves["moves"]}
    return {"summary": summary, "errors": errors, "warnings": comparison["warnings"], "decreases": decreases,
            "extra": extra, "draft": draft}


def _logical(module):
    return module.partition("::crate::")[2]


def draft_approval(result, name):
    """Approval entries for the current blocking increases; reasons stay TODO until reviewed."""
    if not NAME.fullmatch(name + ".toml"):
        raise GateError("draft name must look like <issue>-<slug>")
    head, base = result["draft"]["head"], result["draft"]["base"]
    rows = _tables(head)
    destination = {(m["section"], m["old_id"]): m["new_id"] for m in result["draft"].get("moves", [])}
    increases, reclassify, analyzer = [], [], []
    for error in result["errors"]:
        code, section, identity = error.get("code"), error.get("section"), error.get("id")
        identity = destination.get((section, identity), identity)
        if code in ("new-id", "increase", "zero-rule", "unresolved-reverse") and identity in rows.get(section, {}):
            row = rows[section][identity]
            admit = (section == "reverse_dependencies" and base["mapping_sha256"] != head["mapping_sha256"]
                     and _tables(base)["indeterminate_dependencies"].get(identity, {}).get("count") == row["count"])
            entry = {"section": section, "id": identity}
            if section in EDGE_SECTIONS:
                entry |= {k: row[k] for k in ("source", "target", "kind")}
            increases.append(entry | {"max_count": row["count"],
                                      "reason": "admit-classification" if admit else "accepted-debt"})
        elif code == "mapping-reduction":
            reclassify.append({"prefix": _logical(error["source"]), "reason": "TODO: 재분류 근거"})
        elif code == "declassified":
            reclassify.append({"prefix": _logical(error["module"]), "reason": "TODO: 분류 해제 근거"})
        elif code == "analyzer-change":
            analyzer.append({k: error[k] for k in ("digest", "reverse_removed", "reverse_added")}
                            | {"reason": "TODO: 측정 코드 변경 근거"})
    lines = ["schema_version = 1", f"issue = {int(name.split('-', 1)[0])}",
             'summary = "TODO: 승인 사유를 리뷰받을 문장으로 쓴다"',
             "# move = [ { section = \"reverse_dependencies\", old_id = \"<base id>\", new_id = \"<head id>\", reason = \"...\" } ]"]
    for kind, entries in (("increase", increases), ("reclassify", reclassify), ("analyzer", analyzer)):
        if entries:
            unique = list({_json(e): e for e in entries}.values())
            lines.append(f"{kind} = [")
            lines.extend("  { " + ", ".join(f"{k} = {json.dumps(v, ensure_ascii=False)}" for k, v in e.items()) + " },"
                         for e in unique)
            lines.append("]")
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[3])
    parser.add_argument("--draft-approval", metavar="ISSUE-SLUG",
                        help=f"write {APPROVALS}/<ISSUE-SLUG>.toml for the current blocking increases")
    args = parser.parse_args(argv)
    result = evaluate(args.repo, os.environ)
    print(result["summary"])
    for row in result["warnings"]:
        print(f"warning: {row['message']}", file=sys.stderr)
    for row in result["errors"]:
        print(f"error: {row['message']}", file=sys.stderr)
    if args.draft_approval:
        if any(e["code"] == "gate-unavailable" for e in result["errors"]):
            return 2
        target = args.repo / APPROVALS / f"{args.draft_approval}.toml"
        if target.exists():
            print(f"refusing to overwrite {target}", file=sys.stderr)
            return 2
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(draft_approval(result, args.draft_approval), encoding="utf-8")
        print(f"wrote {target}; fill summary and every TODO reason, then rerun")
        return 0
    return 1 if result["errors"] else 0


if __name__ == "__main__":
    sys.exit(main())
