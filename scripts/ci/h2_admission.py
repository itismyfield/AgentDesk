#!/usr/bin/env python3
"""H2 admission gate: one-shot admissions, R-W, R-E inventory and the 0-rules.

Inert until the baseline lands: without scripts/ci/h2_baseline_*.toml it does nothing.
"""

from __future__ import annotations

import argparse
import collections
import importlib.util
import re
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import h2_measure as m  # noqa: E402
import rust_lex  # noqa: E402  (h2_measure put scripts/ on sys.path)

ADMISSIONS_FILE = "scripts/ci/h2_admissions.toml"
ADMISSION_LANES = (*m.LANES, "both")
REQUIRED_FIELDS = frozenset({"file", "item", "callee", "old", "new", "lane", "issue"})
OPTIONAL_FIELDS = frozenset({"base_sha", "lines"})
# R-O: every file under an owner path; adding one means editing this roster in review.
OWNER_GLOBS = ("src/services/platform/tmux*", "src/services/session_host*")
OWNER_ROSTER = frozenset({
    "src/services/platform/tmux.rs",
    "src/services/platform/tmux/availability.rs",
    "src/services/session_host.rs",
    *(f"src/services/session_host/{name}.rs"
      for name in ("legacy_collapse", "model", "process_host", "resolve", "tmux_host", "traits")),
})
# R-E: low-level tmux owner API inventory; each pub fn is EXEC (clippy.toml) or a non-exec helper.
INVENTORY_FILES = ("src/services/platform/tmux.rs", "src/services/platform/tmux/availability.rs")
NONEXEC = frozenset(f"agentdesk::services::platform::tmux::availability::{name}" for name in (
    "mark_available_from_live_session", "invalidate_cache", "cached_unavailable_due_to_missing"))
PS = frozenset(f"agentdesk::services::platform::tmux::{name}" for name in ("read_process_args", "process_start_time"))
SUBPROC_PATHS = frozenset({"std::process::Command::new", "tokio::process::Command::new"})
W_TYPES = frozenset({
    "agentdesk::services::codex_tui::input::TmuxTuiActionExecutor",
    "agentdesk::services::claude_tui::tui_relay::TmuxSendBackend",
})
# Hand-kept entries allowed to have no diagnostic in a lane; the data PR pins these.
KNOWN_UNREFERENCED: dict[str, frozenset[str]] = {lane: frozenset() for lane in m.LANES}
# H9: files no measured lane compiles; they must not mention tmux at all.
WINDOWS_ONLY_FILES = ("src/runtime_layout/windows_links.rs",)

# Visibility left of an item's `fn` token: pub / pub(crate) / pub(super) plus qualifiers.
PUB_PREFIX_RE = re.compile(r"\bpub(?:\s*\([^)]*\))?\s+(?:(?:async|const|unsafe|extern)\s+)*$")
# R-C (r6): a file pairing a `Command` token (a `use .. Command as X` alias line included) with a
# "tmux" / "tmux .." / "../tmux" literal. Pre-existing pairs are pinned per file by literal count.
TMUX_LITERAL_RE = re.compile(r'b?r?#*"(?:[^"\s]*/)?tmux(?:"|\s)')
R_C_GRANDFATHERED = {
    "src/cli/dcserver.rs": 2,
    "src/cli/doctor/orchestrator.rs": 13,
    "src/engine/ops/exec_ops.rs": 1,
    "src/services/claude.rs": 3,
    "src/services/codex.rs": 2,
    "src/services/codex_tmux_wrapper.rs": 1,
    "src/services/discord/idle_recap/scrollback.rs": 1,
    "src/services/discord/recovery_engine.rs": 1,
    "src/services/discord/recovery_engine/restore_inflight/output_paths.rs": 1,
    "src/services/discord/tmux_reaper.rs": 4,
    "src/services/qwen/session_lifecycle.rs": 1,
    "src/services/qwen_tmux_wrapper.rs": 1,
}
RC2_RE = re.compile(r"\b(?:const|static)\s+(?:mut\s+)?\w+\s*:\s*&\s*(?:'\w+\s+)?str\s*=\s*b?r?#*\"tmux")
RF_RE = re.compile(r"\blibc::(?:exec\w*|posix_spawn\w*)|\bposix_spawnp?\b|\bnix::unistd::exec\w*|\.exec\s*\(\s*\)")
LOCK_NAME_RE = re.compile(r'^name = "([^"]+)"', re.M)
LOCK_BANNED_RE = re.compile(r"tmux|(?:^|[-_])pty(?:[-_]|$)")

class AdmissionError(RuntimeError):
    pass

def _load_lexer():
    """Reuse the landed call-site gate's cfg(test) masking (same loader as the destructive ratchet)."""
    path = m.REPO_ROOT / "scripts/check_durable_frontier_writer_call_sites.py"
    spec = importlib.util.spec_from_file_location("h2_admission_rust_lexer", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module

LEXER = _load_lexer()

def production_views(path: Path) -> tuple[str, str, list[str]]:
    """Non-test text as (code only, code + literals, literals); comments and test lines are blanked."""
    countable = {lineno for lineno, _code, keep in LEXER.production_lines(path) if keep}
    state = rust_lex.StripState()
    code, mixed, literals = [], [], []
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        segments = rust_lex.lex_segments(line, state)
        if lineno not in countable:
            code.append("")
            mixed.append("")
            continue
        literals += [t for k, t in segments if k == rust_lex.LITERAL]
        code.append("".join(t if k == rust_lex.CODE else " " * len(t) for k, t in segments))
        mixed.append("".join(t if k in (rust_lex.CODE, rust_lex.LITERAL) else " " * len(t) for k, t in segments))
    return "\n".join(code), "\n".join(mixed), literals

def owner_pub_fns(root: Path, rel: str, modpath: str) -> set[str]:
    """Every pub fn of an owner file (free, inherent-impl, nested-module), via the measurer's brace walk."""
    code = production_views(root / rel)[0]
    src = m.SourceFile(code)
    found = set()
    for start, _end, names, registrable in src.items:
        line_start = code.rfind("\n", 0, start) + 1
        if names[-1].startswith(("const ", "static ")) or not PUB_PREFIX_RE.search(code[line_start:start]):
            continue
        # trait-impl methods have no registrable path; a `pub` one cannot exist, so this keeps a marker
        found.add("::".join([modpath, *(registrable or names)]))
    return found

def _is_owner(rel: str) -> bool:
    return rel in m.OWNER_FILES or rel.startswith(m.OWNER_PREFIXES)

def zero_rules(root: Path) -> list[str]:
    """R-C, R-C2, R-F over non-owner prod Rust; R-O roster, Cargo.lock (H4) and H9 files."""
    problems, rc_found = [], {}
    for path in sorted((root / "src").rglob("*.rs")):
        rel = path.relative_to(root).as_posix()
        if _is_owner(rel) or LEXER.is_test_file(path.name) or rel in LEXER.PINNED_TEST_ONLY_MODULE_FILES:
            continue
        code, mixed, literals = production_views(path)
        tmux_literals = sum(1 for lit in literals if TMUX_LITERAL_RE.match(lit))
        if tmux_literals and re.search(r"\bCommand\b", code):
            rc_found[rel] = tmux_literals
        if RC2_RE.search(mixed):
            problems.append(f"R-C2: {rel} binds a \"tmux\" const/static outside the owner")
        if RF_RE.search(code):
            problems.append(f"R-F: {rel} calls exec/posix_spawn directly")
    for rel in sorted(set(rc_found) | set(R_C_GRANDFATHERED)):
        got, pinned = rc_found.get(rel, 0), R_C_GRANDFATHERED.get(rel, 0)
        if got > pinned:
            problems.append(f"R-C: {rel} pairs `Command` with {got} tmux literal(s) outside the owner (pinned {pinned})")
        elif got < pinned:
            problems.append(f"R-C: {rel} now has {got} tmux literal(s); lower its R_C_GRANDFATHERED pin from {pinned}")
    found = {p.relative_to(root).as_posix() for pattern in OWNER_GLOBS for hit in root.glob(pattern)
             for p in ([hit] if hit.is_file() else hit.rglob("*")) if p.is_file()}
    for rel in sorted(found ^ OWNER_ROSTER):
        problems.append(f"R-O: owner roster mismatch: {rel} ({'unlisted' if rel in found else 'missing'})")
    lock = root / "Cargo.lock"
    if lock.exists():
        problems += [f"R-O: Cargo.lock brings in `{name}` (tmux/pty crate, H4)"
                     for name in LOCK_NAME_RE.findall(lock.read_text(encoding="utf-8")) if LOCK_BANNED_RE.search(name)]
    for rel in WINDOWS_ONLY_FILES:
        if (root / rel).exists() and "tmux" in (root / rel).read_text(encoding="utf-8").lower():
            problems.append(f"R-O: {rel} is unmeasured (H9) and must not mention tmux")
    return problems

def inventory(root: Path, config: dict, lane: str, lines: list[str], result: dict) -> list[str]:
    """R-E: owner inventory, fixed SUBPROC/TYPES sets, lane W*/SUBPROC_W fixpoint equality, dead entries."""
    problems = []
    table = m._module_table(root)
    owner_pub = {path for rel in INVENTORY_FILES if rel in table for path in owner_pub_fns(root, rel, table[rel])}
    by_set = collections.defaultdict(set)
    for path, (set_name, lanes) in config.items():
        by_set[set_name].add(path)
    for path in sorted(owner_pub ^ (by_set["EXEC"] | NONEXEC | PS)):
        problems.append(f"R-E: owner pub fn inventory mismatch: {path} "
                        f"({'unclassified' if path in owner_pub else 'not an owner pub fn'})")
    for path in sorted(by_set["EXEC"] & (NONEXEC | PS)):
        problems.append(f"R-E: {path} is both EXEC and a non-exec helper")
    for set_name, want in (("SUBPROC", SUBPROC_PATHS), ("TYPES", W_TYPES)):
        for path in sorted(by_set[set_name] ^ want):
            problems.append(f"R-E: {set_name} must be exactly {sorted(want)}; differs at {path}")
    registered_w = {p for p, (s, lanes) in config.items() if s == "W" and lane in lanes}
    for set_name in m.DERIVED_SETS:
        registered = {p for p, (s, lanes) in config.items() if s == set_name and lane in lanes}
        derived = result["derived"][set_name] - (registered_w if set_name == "SUBPROC_W" else set())
        problems += [f"R-E: {path} must be registered as {set_name} ({lane}); run h2_measure.py --regen"
                     for path in sorted(derived - registered)]
        problems += [f"R-E: stale {set_name} ({lane}) entry {path}; run h2_measure.py --regen"
                     for path in sorted(registered - derived)]
    problems += [f"R-E: {item} cannot be registered; move the call into a fn" for item in sorted(result["derived"]["unregistrable"])]
    seen = {callee for *_, callee in m.diagnostics(lines)}
    dead = {p for p, (s, lanes) in config.items() if s not in m.DERIVED_SETS and lane in lanes and p not in seen}
    for path in sorted(dead ^ KNOWN_UNREFERENCED[lane]):
        problems.append(f"R-E: {path} " + (f"is registered but has no {lane} diagnostic" if path in dead
                        else f"is pinned in KNOWN_UNREFERENCED[{lane}] but is referenced or unregistered"))
    return problems

def untagged_entries(clippy_toml: Path) -> list[str]:
    """Every disallowed-* entry must carry an H2 tag; --regen would drop anything else."""
    import tomllib
    data = tomllib.loads(clippy_toml.read_text(encoding="utf-8")) if clippy_toml.exists() else {}
    return [f"R-E: clippy.toml entry without an `H2 <SET> <lane>` reason: {entry}"
            for key in ("disallowed-methods", "disallowed-types") for entry in data.get(key, [])
            if not (isinstance(entry, dict) and str(entry.get("reason", "")).startswith("H2 "))]

def parse_admissions(text: str | None) -> list[dict]:
    import tomllib
    rows = tomllib.loads(text).get("admission", []) if text else []
    for index, row in enumerate(rows):
        where = f"{ADMISSIONS_FILE} admission #{index + 1}"
        if not isinstance(row, dict) or not REQUIRED_FIELDS <= set(row) <= REQUIRED_FIELDS | OPTIONAL_FIELDS:
            raise AdmissionError(f"{where}: fields must be {sorted(REQUIRED_FIELDS)} plus optional {sorted(OPTIONAL_FIELDS)}")
        if row["lane"] not in ADMISSION_LANES:
            raise AdmissionError(f"{where}: lane must be one of {ADMISSION_LANES}")
        ints = [row["old"], row["new"], row["issue"], *row.get("lines", [])]
        if not all(type(v) is int for v in ints) or not 0 <= row["old"] < row["new"] or row["issue"] <= 0:
            raise AdmissionError(f"{where}: need integers 0 <= old < new and issue > 0")
        if not all(isinstance(row[k], str) for k in ("file", "item", "callee")) or not isinstance(row.get("lines", []), list):
            raise AdmissionError(f"{where}: file/item/callee must be strings, lines a list")
    return rows

def _totals(baseline: dict) -> dict:
    """Sum a key's lane counts across sections; a callee that changed set is still one site."""
    totals = collections.defaultdict(lambda: dict.fromkeys(m.LANES, 0))
    for section in baseline.values():
        for key, counts in section.items():
            for lane in m.LANES:
                totals[key][lane] += counts.get(lane, 0)
    return totals

def rw_problem(root: Path, config: dict, key: tuple[str, str, str]) -> str | None:
    """R-W: an item gaining an EXEC/W/TYPES reference must itself be a registered W* fn."""
    file, item, callee = key
    if config.get(callee, ("",))[0] not in ("EXEC", "W", "TYPES") or item == "<module>":
        return None
    trait_impl = re.match(r"<(\w+) as ", item)
    if trait_impl and any(s == "TYPES" and p.rsplit("::", 1)[-1] == trait_impl.group(1) for p, (s, _) in config.items()):
        return None  # a registered Self type covers its trait-impl bodies (same rule as the measurer)
    modpath, parts = m._module_table(root).get(file), item.split("::")
    registered = {p for p, (s, _) in config.items() if s == "W"}
    # nested fns register as their outermost fn, so any prefix of the item path counts
    if modpath and any("::".join([modpath, *parts[:n]]) in registered for n in range(1, len(parts) + 1)):
        return None
    return f"R-W: {file} :: {item} gained {callee} but is not a registered W* fn; run h2_measure.py --regen"

def check_admissions(root: Path, base_rows: list[dict], head_rows: list[dict], base: dict, head: dict,
                     config: dict, lane: str, sites: dict) -> list[str]:
    """Suffix one-shot admissions must cover exactly the grown keys, with old/new == base/head."""
    if head_rows[:len(base_rows)] != base_rows:
        return [f"admission: entries already on base were edited or removed in {ADMISSIONS_FILE}"]
    problems = []
    suffix = head_rows[len(base_rows):]
    before, after = _totals(base), _totals(head)
    for t in m.LANES:
        grown = {k for k in after if after[k][t] > before[k][t]}
        claimed = collections.Counter((a["file"], a["item"], a["callee"]) for a in suffix if a["lane"] in (t, "both"))
        problems += [f"admission: {t} {k[0]} :: {k[1]} -> {k[2]} is claimed {n} times" for k, n in claimed.items() if n > 1]
        problems += [f"admission: {t} {k[0]} :: {k[1]} -> {k[2]} grew {before[k][t]} -> {after[k][t]} without an admission"
                     for k in sorted(grown - set(claimed))]
        problems += [f"admission: {t} {k[0]} :: {k[1]} -> {k[2]} is admitted but did not grow"
                     for k in sorted(set(claimed) - grown)]
    for a in suffix:
        key = (a["file"], a["item"], a["callee"])
        for t in (m.LANES if a["lane"] == "both" else (a["lane"],)):
            if (a["old"], a["new"]) != (before[key][t], after[key][t]):
                problems.append(f"admission: {key} {t} says {a['old']}->{a['new']}, "
                                f"base/head are {before[key][t]}->{after[key][t]}")
        if (problem := rw_problem(root, config, key)) is not None:
            problems.append(problem)
        if a["lane"] in (lane, "both"):  # H8: folded anonymous items must name their site lines
            if key in sites and a.get("lines") != sites[key]:
                problems.append(f"admission: {key} folds several items (H8); set lines = {sites[key]}")
            elif key not in sites and "lines" in a:
                problems.append(f"admission: {key} is unambiguous; drop lines")
    return problems

def git_show(root: Path, rev: str, rel: str) -> str | None:
    proc = subprocess.run(["git", "show", f"{rev}:{rel}"], cwd=root, capture_output=True, text=True)
    return proc.stdout if proc.returncode == 0 else None

def base_state(root: Path, rev: str) -> tuple[dict | None, dict, str | None]:
    """(baseline, clippy config, admissions text) at `rev`, read through the measurer's loaders."""
    with tempfile.TemporaryDirectory() as tmp:
        for rel in (*m.BASELINE_FILES, "clippy.toml"):
            if (text := git_show(root, rev, rel)) is not None:
                (Path(tmp) / rel).parent.mkdir(parents=True, exist_ok=True)
                (Path(tmp) / rel).write_text(text, encoding="utf-8")
        return m.load_baseline(Path(tmp)), m.load_config(Path(tmp) / "clippy.toml"), git_show(root, rev, ADMISSIONS_FILE)

def evaluate(root: Path, lane: str, base_rev: str, lines: list[str]) -> list[str]:
    head = m.load_baseline(root)
    base, base_config, base_admissions = base_state(root, base_rev)
    if base is None:
        return [f"base {base_rev} has no scripts/ci/h2_baseline_*.toml; rebase onto a main that has it"]
    if not any(s == "W" for s, _ in base_config.values()):
        return [f"base {base_rev} clippy.toml has no H2 W entries; rebase onto a main that has them"]
    config = m.load_config(root / "clippy.toml")
    result = m.measure(root, lines, config)
    problems = zero_rules(root) + untagged_entries(root / "clippy.toml")
    problems += m.compare(result["rows"], head, lane)
    if result["total"] < m.LIVENESS_FLOOR:
        problems.append(f"only {result['total']} H2 diagnostics (< liveness floor {m.LIVENESS_FLOOR})")
    problems += inventory(root, config, lane, lines, result)
    head_text = (root / ADMISSIONS_FILE).read_text(encoding="utf-8") if (root / ADMISSIONS_FILE).exists() else None
    try:
        base_rows, head_rows = parse_admissions(base_admissions), parse_admissions(head_text)
    except AdmissionError as exc:
        return problems + [str(exc)]
    sites = {key: found for section in result["sites"].values() for key, found in section.items()}
    return problems + check_admissions(root, base_rows, head_rows, base, head, config, lane, sites)

def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--lane", choices=m.LANES, required=True)
    parser.add_argument("--repo", type=Path, default=m.REPO_ROOT)
    parser.add_argument("--base", help="base commit (PR base SHA or merge-base with main)")
    parser.add_argument("--json", type=Path, help="read clippy JSON from a file instead of running cargo")
    parser.add_argument("--inert", action="store_true", help="no-op without a baseline; report without failing")
    args = parser.parse_args(argv)
    root = args.repo.resolve()
    if m.load_baseline(root) is None:
        if args.inert:
            print("h2-admission: no baseline committed; inert no-op")
            return 0
        print("h2-admission: baseline missing (scripts/ci/h2_baseline_*.toml)", file=sys.stderr)
        return 2
    if not args.base:
        parser.error("--base is required once a baseline exists")
    try:
        lines = args.json.read_text(encoding="utf-8").splitlines() if args.json else m.run_clippy(root, None)
        problems = evaluate(root, args.lane, args.base, lines)
    except (m.MeasureError, AdmissionError) as exc:
        problems = [str(exc)]
    for problem in problems:
        print(("::warning::h2-admission: " if args.inert else "h2-admission: ") + problem, file=sys.stderr)
    if problems:
        return 0 if args.inert else 1
    print(f"h2-admission: {args.lane} admissions, R-W, R-E and 0-rules hold")
    return 0

if __name__ == "__main__":
    sys.exit(main())
