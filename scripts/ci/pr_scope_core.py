"""Resolve the existing PR contract selectors against runnable nextest IDs."""

import json
from pathlib import Path
import re

from nextest_required import load_selectors


_SELECTOR = re.compile(r"[A-Za-z0-9_:.\-]+")


class CoreScopeError(ValueError):
    """The core contract inputs cannot be interpreted for scope selection."""


class MissingCoreTests(CoreScopeError):
    """A required core contract has too few runnable IDs in the inventory."""


def _test_name(value):
    if not isinstance(value, str) or not _SELECTOR.fullmatch(value):
        raise ValueError(f"invalid test selector {value!r}")
    return value


def _manifest(path):
    doc = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(doc, dict) or type(doc.get("schema_version")) is not int or doc["schema_version"] != 1:
        raise ValueError("expected schema_version 1")
    return doc


def _rows(doc, key):
    rows = doc[key]
    if not isinstance(rows, list) or not rows or not all(isinstance(row, dict) for row in rows):
        raise ValueError(f"{key} must be a nonempty array of objects")
    return rows


def _relay(path):
    doc = _manifest(path)
    exact = {_test_name(row["target"]) for row in _rows(doc, "condition3_mutations")}
    for row in _rows(doc, "authority_surface"):
        guards = row["guards"]
        if not isinstance(guards, list) or not guards:
            raise ValueError("authority surface needs a guard list")
        for guard in guards:
            kind, _, value = guard.partition(":")
            if kind == "entry_test":
                exact.add(_test_name(value))
            elif kind not in {"mutation_row", "named_target", "known_gap"} or not value:
                raise ValueError(f"invalid relay guard {guard!r}")

    lanes = []
    for lane in _rows(doc, "lanes"):
        if lane["status"] == "gap":
            continue
        command = lane["command"]
        floor = lane["minimum"]
        if (lane["status"] != "active" or not isinstance(command, list)
                or command[:3] != ["cargo", "test", "--lib"] or len(command) != 4
                or type(floor) is not int or floor < 1):
            raise ValueError(f"unsupported relay lane {lane['name']}")
        needle = _test_name(command[3])
        if needle.startswith("-"):
            raise ValueError(f"unsupported relay lane option {needle!r}")
        lanes.append((needle, floor))
    if not lanes:
        raise ValueError("no active relay lane")
    return exact, lanes


def _census(path):
    doc = _manifest(path)
    exact = set()
    for group in ("probes", "transport_probes", "claim_probes"):
        for row in _rows(doc, group):
            names = row["verified_by"]
            if not isinstance(names, list) or any("::" not in _test_name(name) for name in names):
                raise ValueError(f"{group} needs qualified verified_by names")
            if "recording_test" in row and not any(name.rsplit("::", 1)[-1] == row["recording_test"] for name in names):
                raise ValueError(f"{group} recording_test must name one of its verified_by tests")
            exact.update(names)
    if not exact:
        raise ValueError("no census verified_by names")
    return exact


def _load_core(repo):
    source = "targeted selectors"
    try:
        selectors = load_selectors(repo / "scripts/ci/targeted-test-selectors.txt")
        if not selectors:
            raise ValueError("no targeted selectors")
        targeted = {_test_name(sel[1:]) for _, sel in selectors if sel.startswith("=")}
        targeted_filters = [(_test_name(sel), 1) for _, sel in selectors if not sel.startswith("=")]

        source = "PG15 exact IDs"
        pg15 = [name for lane in ("pg15_required_ids.txt", "pg15_required_runtime_ids.txt")
                for name in (repo / "scripts/ci" / lane).read_text(encoding="utf-8").split()]
        if not pg15:
            raise ValueError("no PG15 exact IDs")
        exact = {_test_name(name) for name in pg15}

        source = "relay authority"
        relay, lanes = _relay(repo / "scripts/relay_authority_contract_targets.json")
        exact.update(relay)
        source = "census"
        exact.update(_census(repo / "scripts/tui_o_body_probes.json"))
        return (targeted, targeted_filters), (exact, lanes)
    except (OSError, ValueError, TypeError, KeyError, AttributeError) as exc:
        raise CoreScopeError(f"cannot read {source}: {exc}") from exc


def _match(ids, exact, filters):
    names = {name for _, name in ids}
    missing = sorted(exact - names)
    selected = set(exact)
    for needle, floor in filters:
        matches = {name for name in names if needle in name}
        if len(matches) < floor:
            missing.append(f"{needle} ({len(matches)}/{floor})")
        selected.update(matches)
    return {test_id for test_id in ids if test_id[1] in selected}, missing


def select_core_ids(repo: Path, universe: set[tuple[str, str]]) -> set[tuple[str, str]]:
    """Keep all matches as binary-qualified IDs. A missing contract cannot pass."""
    targeted, owner_contracts = _load_core(repo)
    selected, missing = _match(universe, *targeted)
    # Owner proofs and census evidence are defined against the root and runtime lib binaries.
    owner_ids = {test_id for test_id in universe if test_id[0] in ("agentdesk", "adk-runtime")}
    owners, owner_missing = _match(owner_ids, *owner_contracts)
    missing.extend(owner_missing)
    if missing:
        raise MissingCoreTests("required core tests missing: " + ", ".join(missing))
    return selected | owners
