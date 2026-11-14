#!/usr/bin/env python3
"""Generate declared feature coverage; live execution evidence is separate."""

from __future__ import annotations

import argparse
import ast
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

import yaml

try:
    from scripts.e2e.tui_relay.assertion_registry import ASSERTION_OPTIONS
except ModuleNotFoundError:
    from e2e.tui_relay.assertion_registry import ASSERTION_OPTIONS

ROWS = Path("docs/user-feature-map.rows.yaml")
MARKDOWN = Path("docs/user-feature-map.md")
REPORT = Path("docs/generated/user-feature-coverage.json")
SCENARIOS = Path("tests/e2e/tui_relay/scenarios")
FEATURE_IDS = (
    "message-delivery", "busy-input", "reaction-display", "cancel-stop",
    "reset-clear", "threads", "dm", "routines", "herdr-handoff",
    "effort-model", "campaigns",
)
AXES = {"host": {"tmux", "herdr"}, "provider": {"claude", "codex"},
        "surface": {"channel", "thread", "dm"}, "state": {"idle", "busy"}}
SOURCES = {"human", "allowed_bot", "legacy_automation"}
QUALIFIERS = {"turn_origin": {"managed", "native"},
              "output_path": {"legacy", "tui_o", "unspecified"}, "input_source": SOURCES,
              "execution_node": {"default", "preferred"}, "ownership": {"unowned", "bound"}}
CELLS = {"claude-tui": ("tmux", "claude"), "claude-herdr": ("herdr", "claude"),
         "codex-tui": ("tmux", "codex"), "codex-herdr": ("herdr", "codex")}
CLASSES = {"live", "fixture", "unsupported-known-gap", "retired"}


class MapError(ValueError):
    """Invalid input or generated output."""


def require(ok, message):
    if not ok:
        raise MapError(message)


def nonempty(value):
    return isinstance(value, str) and bool(value.strip())


def string_list(value, label, empty=False):
    require(isinstance(value, list) and (empty or bool(value)), f"schema: nonempty {label} list required")
    require(all(nonempty(v) for v in value) and len(set(value)) == len(value),
            f"schema: duplicate/invalid {label}")
    return set(value)


def tuple_key(app):
    return json.dumps({k: app[k] for k in (*AXES, "qualifiers")},
                      ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def applicable_key(feature_id, app):
    return f"applicable:{feature_id}:{tuple_key(app)}"


def required_key(feature_id, app, operation):
    return f"required:{feature_id}:{tuple_key(app)}:{operation}"


def required_assertions(app):
    result = set(app["required_assertion_ids"])
    if app["surface"] == "thread":
        result.add("surface_thread_observed")
    if app["state"] == "busy":
        result.add("state_busy_observed")
    return result


def validate_tuple(app):
    require(isinstance(app, dict), "schema: applicable/claim mapping required")
    for axis, values in AXES.items():
        require(isinstance(app.get(axis), str) and app[axis] in values, f"schema: invalid {axis}")
    qualifiers = app.get("qualifiers")
    require(isinstance(qualifiers, dict) and {"turn_origin", "output_path", "input_source"} <= qualifiers.keys(),
            "schema: turn_origin/output_path/input_source qualifiers required")
    for name, value in qualifiers.items():
        require(name in QUALIFIERS and isinstance(value, str) and value in QUALIFIERS[name],
                f"schema: invalid qualifier/source {name}")


def validate_rows(rows):
    require(isinstance(rows, dict) and type(rows.get("schema")) is int and rows["schema"] == 1,
            "schema: rows.schema must be 1")
    features = rows.get("features")
    require(isinstance(features, list) and bool(features), "schema: nonempty feature inventory required")
    index = {}
    for feature in features:
        require(isinstance(feature, dict), "schema: feature mapping required")
        fid = feature.get("feature_id")
        require(nonempty(fid) and fid not in index, "schema: duplicate/invalid feature ID")
        for field in ("title", "visible_contract", "verification", "owner", "chart"):
            require(nonempty(feature.get(field)), f"schema: {fid}.{field} required")
        require(type(feature.get("needs_live", True)) is bool, "needs_live: boolean required")
        string_list(feature.get("test_names", []), "test_names", empty=True)
        apps = feature.get("applicable")
        require(isinstance(apps, list) and bool(apps), f"schema: nonempty applicable inventory for {fid}")
        pending = feature.get("pending_assertions", [])
        require(isinstance(pending, list), "schema: pending_assertions list required")
        pending_ids = set()
        for entry in pending:
            require(isinstance(entry, dict) and all(nonempty(entry.get(k)) for k in ("assertion_id", "issue", "reason")),
                    "schema: pending assertion needs ID/issue/reason")
            require(entry["assertion_id"] not in pending_ids, "schema: duplicate pending assertion")
            pending_ids.add(entry["assertion_id"])
            require(string_list(entry.get("input_sources"), "pending input_sources") <= SOURCES,
                    "schema: invalid pending source")
        source_rules = feature.get("assertion_sources", {})
        require(isinstance(source_rules, dict), "schema: assertion_sources mapping required")
        for operation, sources in source_rules.items():
            require(nonempty(operation) and string_list(sources, "assertion sources") <= SOURCES,
                    "schema: invalid assertion source")
        tuples = set()
        for app in apps:
            validate_tuple(app)
            key = tuple_key(app)
            require(key not in tuples, "schema: duplicate applicable tuple/qualifiers")
            tuples.add(key)
            string_list(app.get("required_assertion_ids"), "required assertion IDs")
            missing = required_assertions(app) - ASSERTION_OPTIONS.keys()
            require(missing <= pending_ids, f"schema: unsupported required assertion needs pending reason: {sorted(missing - pending_ids)}")
        index[fid] = feature
    require(set(FEATURE_IDS) <= index.keys(), "schema: required initial feature inventory incomplete")
    return index


def load_yaml(path):
    return yaml.safe_load(path.read_text(encoding="utf-8"))


def test_sources(root, index):
    sources, definitions = set(), {}
    for feature in index.values():
        for coordinate in feature.get("test_names", []):
            relative, separator, name = coordinate.partition("::")
            path = Path(relative)
            require(separator and not path.is_absolute() and ".." not in path.parts,
                    "test: repo-relative file::test_name required")
            source = (root / path).resolve()
            require(source.is_relative_to(root) and source.is_file() and path.suffix in {".py", ".rs"},
                    f"test: missing or invalid source {relative}")
            if source not in definitions:
                text = source.read_text(encoding="utf-8")
                if path.suffix == ".py":
                    names = set()
                    def collect(node, prefix=""):
                        for child in node.body:
                            if isinstance(child, ast.ClassDef):
                                collect(child, prefix + child.name + ".")
                            elif isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)) and child.name.startswith("test_"):
                                names.add(prefix + child.name)
                    try:
                        collect(ast.parse(text))
                    except SyntaxError as exc:
                        raise MapError(f"test: invalid Python source {relative}") from exc
                else:
                    names = set(re.findall(
                        r"(?m)^\s*#\[(?:\w+::)*test(?:\([^\]]*\))?\]\s*"
                        r"(?:#\[[^\]]*\]\s*|//[^\n]*\n\s*)*(?:async\s+)?fn\s+(\w+)\s*\(", text))
                definitions[source] = names
            require(name in definitions[source], f"test: definition not found: {coordinate}")
            sources.add(source)
    return sorted(sources)


def git(root, *args):
    result = subprocess.run(["git", "-C", str(root), *args], text=True, capture_output=True)
    require(result.returncode == 0, f"base: git {' '.join(args[:1])} failed: {result.stderr.strip()}")
    return result.stdout


def base_rows(root, base_ref):
    require(nonempty(base_ref), "base: explicit base ref required")
    commit = git(root, "rev-parse", "--verify", f"{base_ref}^{{commit}}").strip()
    present = git(root, "ls-tree", "--name-only", commit, "--", str(ROWS)).strip()
    if not present:
        return None
    return yaml.safe_load(git(root, "show", f"{commit}:{ROWS}"))


def inventory_keys(index):
    result = set()
    for fid, feature in index.items():
        result.add(f"feature:{fid}")
        for app in feature["applicable"]:
            result.add(applicable_key(fid, app))
            result.update(required_key(fid, app, op) for op in required_assertions(app))
    return result


def check_ratchet(rows, index, baseline):
    reasons = rows.get("removal_reasons", [])
    require(isinstance(reasons, list), "ratchet: removal_reasons list required")
    by_key = {}
    for entry in reasons:
        require(isinstance(entry, dict) and nonempty(entry.get("key")) and nonempty(entry.get("reason")),
                "ratchet: exact removal key and reason required")
        require(entry["key"] not in by_key, "ratchet: duplicate removal key")
        by_key[entry["key"]] = entry["reason"]
    if baseline is None:
        return
    removed = inventory_keys(validate_rows(baseline)) - inventory_keys(index)
    old = baseline.get("removal_reasons", [])
    old_keys = {entry["key"] for entry in old}
    old_reasons = {entry["reason"] for entry in old}
    for key in sorted(removed):
        require(key in by_key and key not in old_keys and by_key[key] not in old_reasons,
                f"ratchet: new removal_reason required for {key}")


def validate_claim(claim, scenario, index):
    validate_tuple(claim)
    fid = claim.get("feature_id")
    require(isinstance(fid, str) and fid in index, "schema: unknown feature ID in claim")
    key = tuple_key(claim)
    feature = index[fid]
    apps = {tuple_key(app): app for app in feature["applicable"]}
    require(key in apps, f"schema: claim outside applicable tuple: {fid}")
    cells = scenario.get("cells", [])
    require(isinstance(cells, list) and all(isinstance(c, str) for c in cells), "cells: list required")
    pairs = {CELLS[c] for c in cells if c in CELLS}
    require((claim["host"], claim["provider"]) in pairs, "cells: claim host/provider outside explicit TUI cells")
    operations = string_list(claim.get("assertion_ids"), "claim assertion IDs")
    assertions = scenario.get("assertions", [])
    require(isinstance(assertions, list) and all(isinstance(a, dict) for a in assertions), "assertion: mappings required")
    actions = set()
    for entry in assertions:
        keys = set(entry) - {"requires_feature"}
        require(len(keys) == 1, "assertion: exactly one action per entry required")
        actions.update(keys)
    require(operations <= actions & ASSERTION_OPTIONS.keys(),
            "assertion: claim IDs outside YAML actions / exported registry intersection")
    required = required_assertions(apps[key])
    require(required <= operations, f"required witness/assertion missing: {sorted(required - operations)}")
    source = claim["qualifiers"]["input_source"]
    rules = feature.get("assertion_sources", {}).copy()
    for pending in feature.get("pending_assertions", []):
        op = pending["assertion_id"]
        rules[op] = set(rules.get(op, SOURCES)) & set(pending["input_sources"])
    require(all(source in rules.get(op, SOURCES) for op in required), "source: assertion is inapplicable to input_source")
    return fid, key


def scenario_claims(root, index):
    claims = {}
    references = {}
    ids = set()
    counts = {"active": 0, "live": 0, "fixture": 0, "unsupported-known-gap": 0, "retired": 0}
    paths = sorted((root / SCENARIOS).glob("*.yaml")) + sorted((root / SCENARIOS / "retired").glob("*.yaml"))
    for path in paths:
        scenario = load_yaml(path)
        require(isinstance(scenario, dict) and nonempty(scenario.get("id")), "schema: scenario ID required")
        sid = scenario["id"]
        require(sid not in ids, "schema: duplicate scenario ID")
        ids.add(sid)
        category = scenario.get("coverage_class")
        retired = path.parent.name == "retired" or category == "retired"
        require(isinstance(category, str) and category in CLASSES, "schema: invalid scenario coverage_class")
        if retired:
            counts["retired"] += 1
        else:
            counts["active"] += 1
            counts[category] += 1
        metadata = scenario.get("feature_coverage")
        if metadata is None:
            continue
        require(isinstance(metadata, dict) and type(metadata.get("schema")) is int and metadata["schema"] == 1,
                "schema: feature_coverage.schema must be 1")
        entries = metadata.get("claims", [])
        refs = metadata.get("references", [])
        require(isinstance(entries, list), "schema: claims list required")
        require(isinstance(refs, list), "reference: list required")
        require(bool(entries or refs), "schema: nonempty claims or references required")
        referenced = set()
        for reference in refs:
            require(isinstance(reference, dict), "reference: mapping required")
            fid = reference.get("feature_id")
            require(isinstance(fid, str) and fid in index and fid not in referenced,
                    "reference: unknown or duplicate feature")
            require(nonempty(reference.get("scope")), "reference: explicit partial scope required")
            require(reference.get("needs_live") is True, "reference: needs_live must be true")
            referenced.add(fid)
            references.setdefault(fid, []).append({"scenario_id": sid, "scope": reference["scope"], "needs_live": True})
        seen = set()
        for claim in entries:
            fid, key = validate_claim(claim, scenario, index)
            require((fid, key) not in seen, "schema: duplicate feature claim")
            seen.add((fid, key))
            live = category == "live" and not retired
            if live:
                require(scenario.get("agent_mode") == "real_live", "class: live claims need real_live agent_mode")
                require(not scenario.get("skip_reason"), "class: skipped/known-gap scenario cannot cover live")
                steps = scenario.get("steps", [])
                require(isinstance(steps, list) and all(isinstance(step, dict) for step in steps),
                        "class: steps must be a list of mappings")
                fixture = str(scenario.get("execution") or "").strip().lower() in {"fixture", "local_fixture"}
                fixture = fixture or any("replay_fixture" in step or "fixture_followup_probe" in step for step in steps)
                require(not fixture, "class: fixture execution cannot cover live")
                input_steps = {"send_prompt", "send_discord_prompt", "deliver_prompt", "send_provider_hold_prompt",
                               "send_timed_response_prompt", "send_prompts_concurrent", "send_keys", "local_control_then_prompt"}
                contact = any(input_steps & step.keys() or ("send_keys_sequence" in step and
                              isinstance(step["send_keys_sequence"], dict) and
                              step["send_keys_sequence"].get("mark_prompt_sent", True)) for step in steps)
                require(contact, "class: live claim needs an executable provider input step")
                require(not any(op.startswith("fixture_") or op == "no_duplicate_marker_with_known_gap"
                                for op in claim["assertion_ids"]), "class: fixture/known-gap assertion cannot cover live")
            claims.setdefault((fid, key), []).append((sid, live))
    return claims, counts, paths, references


def source_hash(root, paths, tests=()):
    digest = hashlib.sha256()
    sources = [(str(ROWS), root / ROWS)] + [(str(p.relative_to(root)), p) for p in paths]
    sources += [("generator", Path(__file__)),
                ("assertion_registry", Path(__file__).parent / "e2e/tui_relay/assertion_registry.py")]
    sources += [(str(path.relative_to(root)), path) for path in tests]
    for label, path in sources:
        data = path.read_bytes()
        digest.update(label.encode() + b"\0" + str(len(data)).encode() + b"\0" + data)
    return digest.hexdigest()


def generate(root: Path, base_ref: str):
    root = Path(root).resolve()
    baseline = base_rows(root, base_ref)
    rows = load_yaml(root / ROWS)
    index = validate_rows(rows)
    check_ratchet(rows, index, baseline)
    tests = test_sources(root, index)
    claims, counts, paths, references = scenario_claims(root, index)
    report = {"schema_version": 1, "applicable": 0, "covered": 0, "missing": 0,
              "unsupported": 0, "by_feature": {}, "scenario_counts": counts,
              "source_hash": source_hash(root, paths, tests)}
    for fid, feature in sorted(index.items()):
        entries = []
        for app in sorted(feature["applicable"], key=tuple_key):
            key = tuple_key(app)
            matches = claims.get((fid, key), [])
            live = sorted(sid for sid, supported in matches if supported)
            unsupported = bool(required_assertions(app) - ASSERTION_OPTIONS.keys())
            entries.append({"key": applicable_key(fid, app), **app,
                            "required_assertion_ids": sorted(required_assertions(app)),
                            "scenario_ids": live, "non_live_scenario_ids": sorted(sid for sid, supported in matches if not supported),
                            "status": "covered" if live else "unsupported" if unsupported else "gap"})
        covered = sum(bool(entry["scenario_ids"]) for entry in entries)
        summary = {"applicable": len(entries), "covered": covered, "missing": len(entries) - covered,
                   "unsupported": sum(entry["status"] == "unsupported" for entry in entries),
                   "tuples": entries, "pending_assertions": feature.get("pending_assertions", []),
                   "needs_live": feature.get("needs_live", True), "test_names": sorted(feature.get("test_names", [])),
                   "scenario_references": sorted(references.get(fid, []), key=lambda ref: ref["scenario_id"])}
        report["by_feature"][fid] = summary
        for field in ("applicable", "covered", "missing", "unsupported"):
            report[field] += summary[field]
    return render(index, report), report


def cell(value):
    return str(value).replace("|", "\\|").replace("\n", "<br>")


def render(index, report):
    lines = ["# 사용자 기능 지도", "", "생성: `python3 scripts/generate_user_feature_map.py --write --base-ref origin/main`", "",
             "수기 정본: [user-feature-map.rows.yaml](user-feature-map.rows.yaml). 이 지도는 live **명세** 연결이며 실행 PASS·최근성 증거가 아니다.",
             "fixture·known-gap·retired와 미구현 witness는 live coverage로 세지 않는다. `unspecified` output은 실제 출력 경로를 증명하지 않는다.", "",
             "기존 시험 이름은 소스 정의 참조이며 실행/PASS가 아니다. 부분 시나리오 references와 needs_live는 covered 수를 늘리지 않는다.", "",
             f"적용 {report['applicable']} · covered {report['covered']} · 빈 칸 {report['missing']} · 미지원 단언 칸 {report['unsupported']}",
             f"시나리오 분류: {json.dumps(report['scenario_counts'], ensure_ascii=False, sort_keys=True)}", "",
             "| ID / 기능 | 사용자에게 보이는 계약 | 확인 명령 | 적용 / covered / 빈 칸 | 담당 / 차트 |",
             "| --- | --- | --- | --- | --- |"]
    for fid, feature in sorted(index.items()):
        summary = report["by_feature"][fid]
        lines.append("| " + " | ".join(cell(v) for v in (
            f"{fid} / {feature['title']}", feature["visible_contract"], feature["verification"],
            f"{summary['applicable']} / {summary['covered']} / {summary['missing']}", f"{feature['owner']} / {feature['chart']}")) + " |")
    for fid, summary in report["by_feature"].items():
        lines += ["", f"## {fid}", "", f"needs_live: `{str(summary['needs_live']).lower()}`", ""]
        for name in summary["test_names"]:
            lines.append(f"- 기존 시험 정의: `{name}`")
        for reference in summary["scenario_references"]:
            lines.append(f"- 부분 시나리오 `{reference['scenario_id']}` (needs_live): {reference['scope']}")
        lines += ["", "| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |", "| --- | --- | --- | --- |"]
        for entry in summary["tuples"]:
            lines.append("| " + " | ".join(cell(v) for v in (tuple_key(entry), ", ".join(entry["required_assertion_ids"]),
                          ", ".join(entry["scenario_ids"]) or "—", entry["status"])) + " |")
        if summary["pending_assertions"]:
            lines.append("")
        for pending in summary["pending_assertions"]:
            lines.append(f"- `{pending['assertion_id']}` ({pending['issue']}; {', '.join(pending['input_sources'])}): {pending['reason']}")
    lines += ["", f"source_hash: `{report['source_hash']}`", ""]
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    parser.add_argument("--base-ref", required=True)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args(argv)
    try:
        markdown, report = generate(args.root, args.base_ref)
        if base_rows(args.root, args.base_ref) is None:
            print("bootstrap: valid base commit has no feature rows; full inventory validated")
        outputs = {MARKDOWN: markdown.encode(),
                   REPORT: (json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode()}
        for path, content in outputs.items():
            target = args.root / path
            if args.write:
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(content)
            else:
                require(target.exists() and target.read_bytes() == content, f"drift: regenerate {path}")
        print(f"user feature map: applicable={report['applicable']} covered={report['covered']} missing={report['missing']}")
        return 0
    except (MapError, OSError, yaml.YAMLError) as exc:
        print(f"user feature map: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
