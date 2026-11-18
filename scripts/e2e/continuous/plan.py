"""Occurrences for one runner request, read from the bundle manifest.

Rotation runs one R1-R5 slice on one cell and profile per run; the cursor then moves to the next
step. O and Legacy profiles never share a run because a profile change needs a new boot.
A scenario whose capability is missing is recorded BLOCKED_CAPABILITY and never executed.
"""

from pathlib import Path

from .store import read_json, write_json

MANIFEST = Path(__file__).resolve().parents[3] / "docs/e2e-continuous/bundles.schema1.json"
SLICES = ("R1", "R2", "R3", "R4", "R5")
# The initial TUI profile; pipe cells stay outside it (E-13 gap).
CELLS = ("claude-tui", "codex-tui", "claude-herdr", "codex-herdr")
# New scenarios and the managed control need the E1 adapters that are not landed yet.
NEEDS = {"R-marker": {"E1a", "E1-ingress", "E1b"}, "R-inject": {"E1a", "E1-ingress", "E1b"},
         "R-inject-managed": {"E1a", "E1-ingress", "E1b"}, "R-fresh": {"E1c"}, "R-host": {"E1c"}}


def load(path=MANIFEST):
    manifest = read_json(path, None)
    if not isinstance(manifest, dict) or manifest.get("schema_version") != 1:
        raise ValueError("bundle manifest is missing or not schema 1")
    return manifest


def cell_of(row):
    return row.get("cell") or f"{row['provider']}-{'tui' if row['host'] == 'tmux' else 'herdr'}"


def profile_of(manifest, scenario):
    return manifest["execution_contract"]["profiles"].get(scenario, "default")


def rotation(manifest):
    catalog, steps = manifest["scenario_catalog"], []
    for name in SLICES:
        for cell in CELLS:
            profiles = [profile_of(manifest, s) for s in manifest["bundles"][name]["scenarios"]
                        if cell in catalog[s]["cells"]]
            steps += [(name, cell, profile) for profile in dict.fromkeys(profiles)]
    return steps


def step_occurrences(manifest, name, cell, profile):
    bundle, catalog = manifest["bundles"][name], manifest["scenario_catalog"]
    first = next(step for step in rotation(manifest) if step[0] == name) == (name, cell, profile)
    rows = [{"scenario": s, "cell": cell} for s in bundle["scenarios"]
            if cell in catalog[s]["cells"] and profile_of(manifest, s) == profile]
    # Cell-less scenarios and controls are recorded once per slice rotation, on its first step.
    rows += [{"scenario": s, "cell": None} for s in bundle["scenarios"] if first and not catalog[s]["cells"]]
    rows += [{"scenario": c, "cell": cell} for c in bundle.get("control_occurrences", [])
             if profile == "default" and cell_of(manifest["control_occurrences"][c]) == cell]
    return rows


def deploy_occurrences(manifest):
    return [{"scenario": row["scenario"], "cell": cell_of(row)} for row in manifest["bundles"]["P"]["occurrences"]]


def gate(manifest, row, capabilities):
    """Return (status, reason) when the occurrence cannot run here, else None."""
    gaps, scenario, cell = manifest["capability_gaps"], row["scenario"], row["cell"]
    keys = [scenario]
    if cell is not None:
        provider, host = cell.split("-")
        host = "tmux" if host == "tui" else host
        keys += [f"{scenario}/{host}", f"{provider}/{host}"]
    for key in keys:
        if key in gaps and key not in capabilities:
            return gaps[key]["status"], f"{key}: {gaps[key]['reason']}"
    if cell is None:
        return "BLOCKED_CAPABILITY", f"{scenario}: no runnable cell"
    missing = sorted(NEEDS.get(scenario, set()) - set(capabilities))
    if missing:
        return "BLOCKED_CAPABILITY", f"{scenario}: needs {', '.join(missing)}"
    return None


def cursor(state_dir):
    return read_json(state_dir / "cursor.json", {"index": 0, "last_scheduled_at": None})


def save_cursor(state_dir, value):
    write_json(state_dir / "cursor.json", value)
