"""Layer projections and reproducible snapshots of the unchanged source graph."""
from __future__ import annotations

import argparse
from collections import Counter, defaultdict
from hashlib import sha256
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tarfile
import tomllib

MAPPING = "scripts/audit_maintainability/layers.toml"
BASELINE = "scripts/audit_maintainability/baselines/generated/module_layers.json"
GENERATOR = "python3 scripts/audit_maintainability/checks/module_layer_baseline.py"


def _json(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":"))


def _ordered(value):
    if isinstance(value, dict):
        return {key: _ordered(child) for key, child in sorted(value.items())}
    if isinstance(value, (list, tuple)):
        return sorted((_ordered(child) for child in value), key=_json)
    return value


def _id(*parts):
    return sha256(_json(parts).encode()).hexdigest()


def _relative(path):
    path = path.replace("\\", "/")
    for anchor in ("src/", "scripts/"):
        if path.startswith(anchor):
            return path
        if "/" + anchor in path:
            return anchor + path.rsplit("/" + anchor, 1)[1]
    return "<outside>/" + path.rsplit("/", 1)[-1] if path.startswith("/") or re.match(r"^[A-Za-z]:/", path) else path


def _location(text):
    match = re.match(r"(.+?):(\d+)(?:\s+(.*))?$", text)
    if match:
        file, line, reason = match.groups()
        return _relative(file), f"{_relative(file)}:{line}", reason or ""
    path, _, reason = text.partition(": ")
    return _relative(path), _relative(path), reason


def load_mapping(repo):
    try:
        return tomllib.loads((repo / MAPPING).read_text("utf-8"))
    except (OSError, UnicodeError, ValueError) as exc:
        return {"schema_version": 1, "layers": {}, "_errors": [f"layers.toml unavailable: {type(exc).__name__}"]}


def _classify(node, mapping, layers):
    root, marker, logical = node.partition("::crate")
    if not marker:
        return {"module": node, "layer": None, "reason": "unrecognized module identifier"}
    logical = logical.removeprefix("::")
    matches = []
    for category in ("rules", "unclassified"):
        for rule in mapping.get(category, []):
            prefix = rule.get("prefix")
            if not isinstance(prefix, str) or rule.get("root", root) != root:
                continue
            if logical != prefix and not (prefix and logical.startswith(prefix + "::")):
                continue
            layer = rule.get("layer") if category == "rules" else None
            reason = rule.get("reason", "no classification rationale")
            if layer is not None and layer not in layers:
                reason, layer = f"unknown layer {layer}: {reason}", None
            matches.append(((len(prefix.split("::")) if prefix else 0, bool(rule.get("root"))), layer, reason))
    if not matches:
        return {"module": node, "layer": None, "reason": "no reviewed responsibility mapping"}
    score = max(row[0] for row in matches)
    best = [row for row in matches if row[0] == score]
    choices = {row[1] for row in best}
    if len(choices) != 1:
        return {"module": node, "layer": None, "reason": "conflicting responsibility mappings"}
    return {"module": node, "layer": best[0][1], "reason": "; ".join(sorted({row[2] for row in best}))}


def _edges(graph, occurrence_projection):
    grouped, counts = {}, {}
    for edge in graph["edges"]:
        key = (edge["source"], edge["target"], edge["kind"])
        grouped.setdefault(key, set()).update(_location(site)[1] for site in edge["sites"])
    for row in occurrence_projection:
        key = (row["source"], row["target"], row["kind"])
        if key in counts or type(row["count"]) is not int or row["count"] < 1:
            raise ValueError("occurrence projection must contain one positive count per edge")
        counts[key] = row["count"]
    if counts.keys() != grouped.keys():
        raise ValueError("occurrence projection does not match measurement edges")
    return [{"id": _id("edge", *key), "source": key[0], "target": key[1], "kind": key[2],
             "count": counts[key], "sites": sorted(sites)} for key, sites in sorted(grouped.items())]


def _mapping_errors(mapping):
    diagnostics = mapping.get("_errors", [])
    errors = list(diagnostics) if isinstance(diagnostics, list) and all(isinstance(item, str) for item in diagnostics) else ["invalid mapping diagnostics"]
    layers = mapping.get("layers", {})
    if not isinstance(layers, dict):
        errors.append("layers must be a table of integer ranks")
        layers = {}
    if type(mapping.get("schema_version")) is not int or mapping["schema_version"] != 1 or any(type(rank) is not int for rank in layers.values()):
        errors.append("unsupported mapping schema or non-integer layer rank")
    elif len(set(layers.values())) != len(layers):
        errors.append("layer ranks must be unique")
    for category in ("rules", "unclassified"):
        rules = mapping.get(category, [])
        if not isinstance(rules, list):
            errors.append(f"{category} must be an array")
            continue
        for rule in rules:
            if (not isinstance(rule, dict) or not isinstance(rule.get("prefix"), str)
                    or not isinstance(rule.get("root", ""), str)
                    or not isinstance(rule.get("reason", ""), str)
                    or not isinstance(rule.get("evidence", []), list)
                    or any(not isinstance(site, str) for site in rule.get("evidence", []))):
                errors.append(f"invalid {category} record")
            elif category == "rules" and (not isinstance(rule.get("layer"), str) or rule["layer"] not in layers):
                errors.append("rule names an unknown layer")
    return layers, errors


def _reason(text):
    def source_path(value):
        normalized = value.replace("\\", "/")
        return _relative(value) if any(anchor in normalized for anchor in ("/src/", "/scripts/")) else value

    text = re.sub(r"""(['"])(.*?)\1""", lambda match: match[1] + source_path(match[2]) + match[1], text)
    text = re.sub(r"""(?:[A-Za-z]:[\\/]|/)[^\s'"(),]+""", lambda match: source_path(match[0]), text)
    return re.sub(r"\b(offset|position|line|column) \d+(?:-\d+)?", r"\1 <position>", text)


def project(graph, mapping, occurrence_projection):
    """Assess known endpoints; missing or conflicting classifications remain indeterminate."""
    layers, errors = _mapping_errors(mapping)
    classifications = [_classify(node, {} if errors else mapping, layers) for node in sorted(set(graph["nodes"]))]
    assigned = {row["module"]: row["layer"] for row in classifications}
    counts = {layer: 0 for layer in sorted(layers)}
    for layer in assigned.values():
        if layer is not None:
            counts[layer] += 1
    reverse, indeterminate, pairs = [], [], defaultdict(lambda: {"edges": 0, "sites": 0, "occurrences": 0})
    for edge in _edges(graph, occurrence_projection):
        source, target = assigned.get(edge["source"]), assigned.get(edge["target"])
        if source is None or target is None:
            indeterminate.append({**edge, "reason": "unclassified source or target"})
            continue
        pair = pairs[(source, target)]
        pair["edges"] += 1
        pair["sites"] += len(edge["sites"])
        pair["occurrences"] += edge["count"]
        if layers[source] < layers[target]:
            reverse.append(edge)
    unknown = [row for row in classifications if row["layer"] is None]
    raw = graph["coverage"]
    uncertainty_count = sum(len(raw.get(key, [])) for key in ("unresolved_references", "cfg_uncertainties", "read_failures", "module_errors"))
    coverage = {"complete": not (unknown or indeterminate or uncertainty_count or errors) and raw.get("complete", False),
                "unclassified_modules": len(unknown), "indeterminate_edges": len(indeterminate),
                "uncertainty_rows": uncertainty_count, "configuration_errors": sorted(errors),
                "cfg_scope": raw.get("cfg_scope"), "target_os": raw.get("target_os")}
    return {"classifications": classifications, "layer_counts": counts, "unclassified": unknown,
            "reverse_dependencies": reverse, "indeterminate_edges": indeterminate, "coverage": coverage,
            "layer_pairs": [{"source_layer": source, "target_layer": target, **value,
                              "reverse": layers[source] < layers[target]} for (source, target), value in sorted(pairs.items())],
            "sccs": graph["sccs"], "uncertainties": _uncertainties(graph)}


def _uncertainties(graph):
    grouped = {}

    def add(category, source, path, reason, site):
        key = (category, source, path, _reason(reason))
        row = grouped.setdefault(key, {"count": 0, "sites": set()})
        row["count"] += 1
        if site:
            row["sites"].add(_location(site)[1])

    raw = graph["coverage"]
    for row in raw.get("unresolved_references", []):
        add("reference", row["source"], row["path"], row["reason"], row["site"])
    for category in ("cfg_uncertainties", "read_failures", "module_errors"):
        for text in raw.get(category, []):
            file, site, reason = _location(text)
            add(category, "file:" + file, file, reason, site)
    return [{"id": _id("uncertainty", *key), "category": key[0], "source": key[1], "path": key[2], "reason": key[3],
             "count": value["count"], "sites": sorted(value["sites"])} for key, value in sorted(grouped.items())]


def uncertainty_sources(graph, mapping, occurrence_projection):
    return {row["id"]: row["source"] for row in project(graph, mapping, occurrence_projection)["uncertainties"]}


def baseline(graph, mapping, input_sha, occurrence_projection):
    result = project(graph, mapping, occurrence_projection)
    return {"schema_version": 3, "count_unit_version": 1, "input_sha": input_sha, "source_grain": "module",
            "edge_count_unit": "static reference occurrences",
            "generation_command": f"{GENERATOR} --input-sha {input_sha} --output {BASELINE}",
            "mapping_sha256": _id(_ordered(mapping)),
            "reverse_dependencies": [{key: row[key] for key in ("id", "source", "target", "kind", "count")}
                                     for row in sorted(result["reverse_dependencies"], key=lambda row: row["id"])],
            "indeterminate_dependencies": [{key: row[key] for key in ("id", "source", "target", "kind", "count")}
                                           for row in sorted(result["indeterminate_edges"], key=lambda row: row["id"])],
            "uncertainties": [{"id": row["id"], "count": row["count"]}
                              for row in sorted(result["uncertainties"], key=lambda row: row["id"])],
            "zero_rules": [],
            "unclassified_modules": len(result["unclassified"]), "indeterminate_edges": len(result["indeterminate_edges"]),
            "indeterminate_occurrences": sum(row["count"] for row in result["indeterminate_edges"]),
            "configuration_errors": result["coverage"]["configuration_errors"]}


def encode_baseline(payload):
    """Write each debt/uncertainty record on its own deterministic JSON line."""
    lines = ["{"]
    keys = sorted(payload)
    for index, key in enumerate(keys):
        suffix = "," if index + 1 < len(keys) else ""
        if key in {"reverse_dependencies", "indeterminate_dependencies", "uncertainties", "zero_rules"}:
            rows = sorted(payload[key], key=lambda row: (row.get("section", ""), row["id"]))
            lines.append("  " + _json(key) + ":[")
            lines.extend("    " + _json(row) + ("," if n + 1 < len(rows) else "") for n, row in enumerate(rows))
            lines.append("  ]" + suffix)
        else:
            lines.append("  " + _json(key) + ":" + _json(payload[key]) + suffix)
    return "\n".join([*lines, "}", ""])


def write_baseline(path, payload):
    data = encode_baseline(payload).encode("utf-8")
    if path.exists():
        if path.read_bytes() == data:
            return
        raise ValueError("baseline already exists with different content; explicit debt migration review is required")
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("xb") as output:
        output.write(data)


def update_baseline(path, payload, moves, *, upgrade=False, legacy_records=None, sources=None):
    """Only the explicit reduction command may replace an accepted snapshot."""
    from audit_maintainability.checks.module_layer_ratchet import compare, reduced_baseline, _validate
    lock = path.with_name(path.name + ".lock")
    temporary = None
    with lock.open("xb") as held_lock:
        try:
            original = path.read_bytes()
            accepted = json.loads(original)
            if upgrade:
                _validate(payload)
                if accepted.get("schema_version") != 2 or moves != {"schema_version": 1, "moves": []}:
                    raise ValueError("record upgrade requires schema 2 and an empty movement mapping")
                if legacy_records is None:
                    raise ValueError("record upgrade requires replayed historical records")
                _validate(legacy_records)
                for key in ("reverse_dependencies", "uncertainties", "unclassified_modules",
                            "indeterminate_edges", "indeterminate_occurrences", "configuration_errors",
                            "count_unit_version", "source_grain", "edge_count_unit", "mapping_sha256"):
                    if (_ordered(accepted[key]) != _ordered(payload[key])
                            or _ordered(accepted[key]) != _ordered(legacy_records[key])):
                        raise ValueError(f"record upgrade changed existing debt or measurement: {key}")
                upgrade_diff = compare(legacy_records, payload, moves)
                if upgrade_diff["errors"] or upgrade_diff["warnings"]:
                    raise ValueError("record upgrade changed historical individual debts")
                result = payload
            else:
                result = reduced_baseline(accepted, payload, moves, sources)
            result["generation_command"] += " --upgrade-records" if upgrade else " --update-reductions"
            data = encode_baseline(result).encode("utf-8")
            if original == data:
                return
            descriptor, name = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
            temporary = Path(name)
            with os.fdopen(descriptor, "wb") as output:
                output.write(data)
                output.flush()
                os.fsync(output.fileno())
            if path.read_bytes() != original:
                raise ValueError("accepted baseline changed during reduction")
            os.replace(temporary, path)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
            held_lock.close()
            lock.unlink()


def historical_baseline(repo, input_sha, mapping):
    """Replay tracked source at the original SHA before extending its record schema."""
    from audit_maintainability.checks.module_layers import analyze
    listing = subprocess.check_output(["git", "-C", str(repo), "ls-tree", "-r", "--name-only",
                                       input_sha, "--", "src", "Cargo.toml"], text=True)
    paths = listing.splitlines()
    if not paths or any(Path(p).name in {"agentdesk.yaml", "launchd.env"} for p in paths):
        raise ValueError("historical source is unavailable")
    archive = subprocess.check_output(["git", "-C", str(repo), "archive", input_sha, "--",
                                       *sorted({p.split("/", 1)[0] for p in paths})])
    with tempfile.TemporaryDirectory(prefix="module-layer-history-") as temporary:
        root = Path(temporary)
        with tarfile.open(fileobj=io.BytesIO(archive)) as source:
            for entry in source:
                target = root / entry.name
                if not target.resolve().is_relative_to(root.resolve()):
                    raise ValueError("historical source escapes its replay directory")
                if entry.isfile():
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_bytes(source.extractfile(entry).read())
                elif not entry.isdir():
                    raise ValueError("historical replay requires regular source files")
        occurrences = []
        graph = analyze(root, occurrence_projection=occurrences)
        return baseline(graph, mapping, input_sha, occurrences)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[3])
    parser.add_argument("--input-sha", required=True)
    parser.add_argument("--output", type=Path, default=Path(BASELINE))
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--update-reductions", action="store_true")
    mode.add_argument("--upgrade-records", action="store_true")
    args = parser.parse_args()
    revision = subprocess.run(["git", "-C", str(args.repo), "rev-parse", "--verify", args.input_sha + "^{commit}"], capture_output=True, text=True)
    if revision.returncode:
        parser.error("input SHA must resolve to a commit")
    args.input_sha = revision.stdout.strip()
    verified = subprocess.run(["git", "-C", str(args.repo), "diff", "--quiet", args.input_sha, "--", "src", "Cargo.toml"])
    if verified.returncode:
        parser.error("input SHA does not match the working source tree (src and Cargo.toml)")
    untracked = subprocess.run(["git", "-C", str(args.repo), "ls-files", "--others", "--", "src"], capture_output=True, text=True, check=True)
    if untracked.stdout:
        parser.error("untracked source files are outside the recorded input SHA")
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
    from audit_maintainability.checks.module_layers import analyze
    mapping = load_mapping(args.repo)
    if _mapping_errors(mapping)[1]:
        parser.error("invalid or missing layer mapping")
    occurrences = []
    graph = analyze(args.repo, occurrence_projection=occurrences)
    payload = baseline(graph, mapping, args.input_sha, occurrences)
    if args.update_reductions or args.upgrade_records:
        from audit_maintainability.checks.module_layer_ratchet import MOVES
        moves = tomllib.loads((args.repo / MOVES).read_text("utf-8"))
        legacy_records = historical_baseline(args.repo, json.loads(args.output.read_bytes())["input_sha"], mapping) if args.upgrade_records else None
        update_baseline(args.output, payload, moves, upgrade=args.upgrade_records, legacy_records=legacy_records,
                        sources=uncertainty_sources(graph, mapping, occurrences))
    else:
        write_baseline(args.output, payload)
    print(f"wrote {args.output} input_sha={args.input_sha}")


if __name__ == "__main__":
    main()
