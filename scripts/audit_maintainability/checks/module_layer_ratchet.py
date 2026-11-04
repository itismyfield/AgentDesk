"""Compare individual layer debts; reductions never purchase unrelated growth."""
from __future__ import annotations

from copy import deepcopy
import json
import re
import tomllib

from .module_layer_baseline import BASELINE, _id

MOVES = "scripts/audit_maintainability/layer_moves.toml"
SECTIONS = ("reverse_dependencies", "indeterminate_dependencies", "uncertainties")
TOTALS = ("unclassified_modules", "indeterminate_edges", "indeterminate_occurrences")


def load_inputs(repo):
    return (json.loads((repo / BASELINE).read_bytes()),
            tomllib.loads((repo / MOVES).read_text("utf-8")))


def _record(row, section, zero=False):
    if not isinstance(row, dict) or not re.fullmatch(r"[0-9a-f]{64}", str(row.get("id", ""))):
        raise ValueError("record requires a SHA256 identifier")
    count = row.get("count")
    if type(count) is not int or (count != 0 if zero else count < 1):
        raise ValueError("record count must be zero for a zero-rule, otherwise positive")
    if section != "uncertainties":
        if not all(isinstance(row.get(key), str) and row[key] for key in ("source", "target", "kind")):
            raise ValueError("edge requires source, target and reference kind")
        if row["id"] != _id("edge", row["source"], row["target"], row["kind"]):
            raise ValueError("edge identifier does not match its endpoints and kind")


def _validate(payload):
    if (type(payload.get("schema_version")) is not int or payload["schema_version"] != 3
            or type(payload.get("count_unit_version")) is not int or payload["count_unit_version"] != 1
            or payload.get("source_grain") != "module"
            or payload.get("edge_count_unit") != "static reference occurrences"):
        raise ValueError("unsupported baseline schema or count unit; explicit record upgrade is required")
    tables = {}
    for section in SECTIONS:
        if not isinstance(payload.get(section), list):
            raise ValueError(f"missing record list: {section}")
        tables[section] = {}
        for row in payload[section]:
            _record(row, section)
            if row["id"] in tables[section]:
                raise ValueError(f"duplicate identifier in {section}")
            tables[section][row["id"]] = row
    if tables[SECTIONS[0]].keys() & tables[SECTIONS[1]].keys():
        raise ValueError("an edge cannot be both classified debt and indeterminate")
    for key in TOTALS:
        if type(payload.get(key)) is not int or payload[key] < 0:
            raise ValueError(f"invalid nonnegative total: {key}")
    if (payload["indeterminate_edges"] != len(tables[SECTIONS[1]])
            or payload["indeterminate_occurrences"] != sum(r["count"] for r in tables[SECTIONS[1]].values())):
        raise ValueError("indeterminate totals do not match individual records")
    if payload.get("configuration_errors") != []:
        raise ValueError("measurement has configuration errors")
    if not isinstance(payload.get("zero_rules"), list):
        raise ValueError("missing zero-rule list")
    zeros = {section: {} for section in SECTIONS}
    for row in payload["zero_rules"]:
        section = row.get("section")
        if section not in SECTIONS:
            raise ValueError("unknown zero-rule section")
        _record(row, section, zero=True)
        if row["id"] in zeros[section] or row["id"] in tables[section]:
            raise ValueError("duplicate or positive zero-rule identifier")
        zeros[section][row["id"]] = row
    return tables, zeros


def _moves(manifest, tables, zeros):
    if manifest.get("schema_version") != 1 or not isinstance(manifest.get("moves"), list):
        raise ValueError("invalid movement mapping schema")
    result = {section: {} for section in SECTIONS}
    targets = {section: set() for section in SECTIONS}
    for row in manifest["moves"]:
        section, old, new = row.get("section"), row.get("old_id"), row.get("new_id")
        if section not in SECTIONS or not all(isinstance(x, str) and re.fullmatch(r"[0-9a-f]{64}", x) for x in (old, new)):
            raise ValueError("mapping requires a section and old/new SHA256 identifiers")
        known = tables[section].keys() | zeros[section].keys()
        if old not in known:
            raise ValueError("mapping old ID is absent from the accepted baseline")
        if old == new or old in result[section] or new in targets[section] or new in known:
            raise ValueError("mapping is not a collision-free one-to-one movement")
        result[section][old] = new
        targets[section].add(new)
    return result


def _normalized(accepted, candidate, manifest):
    old, zeros = _validate(accepted)
    new, _ = _validate(candidate)
    moves = _moves(manifest, old, zeros)
    normalized = {}
    for section in SECTIONS:
        reverse = {target: source for source, target in moves[section].items()}
        normalized[section] = {}
        for identity, row in new[section].items():
            if identity in moves[section]:
                raise ValueError("mapping source still exists in the current measurement")
            identity = reverse.get(identity, identity)
            if identity in normalized[section]:
                raise ValueError("mapping merges distinct current debts")
            normalized[section][identity] = row
    return old, zeros, normalized


def compare(accepted, candidate, moves):
    errors, decreases = [], []
    try:
        old, zeros, current = _normalized(accepted, candidate, moves)
        unknown_ids = {row["id"] for row in candidate["indeterminate_dependencies"]}
        moved_reverse = {row["old_id"]: row["new_id"] for row in moves["moves"]
                         if row["section"] == "reverse_dependencies"}
        for section in SECTIONS:
            for identity, row in sorted(current[section].items()):
                previous = old[section].get(identity, {"count": 0})["count"]
                if row["count"] > previous:
                    code = "zero-rule" if identity in zeros[section] else "increase" if previous else "new-id"
                    errors.append({"code": code, "section": section, "id": identity,
                                   "baseline_count": previous, "current_count": row["count"],
                                   "message": f"{code}: {section} {identity} {previous} -> {row['count']}"})
            for identity, row in sorted(old[section].items()):
                if section == "reverse_dependencies" and moved_reverse.get(identity, identity) in unknown_ids:
                    continue
                count = current[section].get(identity, {"count": 0})["count"]
                if count < row["count"]:
                    decreases.append({"section": section, "id": identity,
                                      "baseline_count": row["count"], "current_count": count})
        for key in TOTALS:
            if candidate[key] > accepted[key]:
                errors.append({"code": "total-increase", "section": key,
                               "baseline_count": accepted[key], "current_count": candidate[key],
                               "message": f"{key} increased: {accepted[key]} -> {candidate[key]}"})
    except (KeyError, TypeError, ValueError, AttributeError) as exc:
        errors.append({"code": "invalid-input", "section": "configuration", "message": str(exc)})
    return {"errors": sorted(errors, key=lambda r: (r["section"], r.get("id", ""), r["code"])),
            "decreases": sorted(decreases, key=lambda r: (r["section"], r["id"]))}


def reduced_baseline(accepted, candidate, moves):
    result = compare(accepted, candidate, moves)
    if result["errors"]:
        raise ValueError("baseline reduction refused: " + "; ".join(r["message"] for r in result["errors"]))
    old, zeros, current = _normalized(accepted, candidate, moves)
    output = deepcopy(candidate)
    output["zero_rules"] = deepcopy(accepted["zero_rules"])
    for section in SECTIONS:
        output[section] = []
        for identity, row in sorted(old[section].items()):
            count = current[section].get(identity, {"count": 0})["count"]
            if count:
                output[section].append({**row, "count": count})
            else:
                output["zero_rules"].append({**row, "section": section, "count": 0})
    output["zero_rules"].sort(key=lambda r: (r["section"], r["id"]))
    return output
