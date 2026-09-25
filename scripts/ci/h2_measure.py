#!/usr/bin/env python3
"""H2 tmux-boundary measurer: lib-target clippy JSON -> (file, item, callee) rows.

Rows are classified by the `reason = "H2 <SET> <lanes>"` tag of the matching
clippy.toml entry; W* and SUBPROC_W are derived from the same pass.
"""

from __future__ import annotations

import argparse
import collections
import itertools
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "scripts"))
import rust_lex  # noqa: E402

CRATE = "agentdesk"
LANES = ("linux", "macos")
OWNER_FILES = frozenset({
    "src/services/platform/tmux.rs",
    "src/services/platform/tmux/availability.rs",
    "src/services/session_host.rs",
})
OWNER_PREFIXES = ("src/services/session_host/",)
BASELINE_FILES = ("scripts/ci/h2_baseline_services.toml", "scripts/ci/h2_baseline_others.toml")
SECTIONS = ("exec", "w", "types", "subproc", "subproc_w_callers")
SET_SECTION = {"EXEC": "exec", "W": "w", "TYPES": "types", "SUBPROC": "subproc", "SUBPROC_W": "subproc_w_callers"}
DERIVED_SETS = ("W", "SUBPROC_W")
LINTS = ("clippy::disallowed_methods", "clippy::disallowed_types")
# Raised by the activation PR; 0 keeps the measurer inert until then.
LIVENESS_FLOOR = 0
MAX_REGEN_ITERATIONS = 20
# R-D: a literal program in this list plus any non-literal argument seeds SUBPROC_W.
DISPATCHERS = (
    "bash", "sh", "zsh", "dash", "ksh", "fish",
    "cmd", "cmd.exe", "pwsh", "powershell", "powershell.exe",
    "env", "nohup", "sudo", "doas", "xargs", "timeout", "nice", "ionice", "stdbuf",
    "setsid", "caffeinate", "arch", "script", "time", "chroot", "su",
    "ssh", "launchctl", "osascript", "open", "docker", "kubectl",
    "nix-shell", "uv", "uvx", "npx", "pnpm", "bunx", "cargo", "python3", "python", "node",
    "ruby", "perl",
)

CALLEE_RE = re.compile(r"`([^`]+)`")
ITEM_TOKEN_RE = re.compile(
    r"\bfn\s+([A-Za-z_]\w*)|\bimpl\b|\bmod\s+([A-Za-z_]\w*)|\btrait\s+([A-Za-z_]\w*)"
    r"|(?m:^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?(const|static)[ \t]+(?:mut[ \t]+)?(?!fn\b)([A-Za-z_]\w*))"
    r"|[{}();\[\]]"
)
ARG_CALL_RE = re.compile(r"\.\s*args?\s*\(")
SCALAR_TOKEN_RE = re.compile(r"-?\d[\d_]*(?:[iu](?:8|16|32|64|128|size))?|true|false")

class MeasureError(RuntimeError):
    pass

class SourceFile:
    """Raw and literal-blanked text of one Rust file plus its item ranges."""

    def __init__(self, text: str) -> None:
        self.raw = text
        state = rust_lex.StripState()
        lines = text.split("\n")
        self.stripped = "\n".join(rust_lex.strip_line(line, state).ljust(len(line)) for line in lines)
        self.line_offsets = [0, *itertools.accumulate(len(line) + 1 for line in lines)]
        self.items = _item_ranges(self.stripped)
        self.item_names = collections.Counter("::".join(names) for _, _, names, _ in self.items)

    def ambiguous(self, item: str) -> bool:
        """H8: `<module>` or a name shared by several items (`const _`, cfg twins) folds sites."""
        return item == "<module>" or self.item_names[item] > 1

    def offset(self, line: int, column: int) -> int:
        return self.line_offsets[line - 1] + column - 1

    def enclosing(self, pos: int) -> tuple[str, tuple[str, ...], tuple[int, int]]:
        """Innermost item containing `pos`: (row name, registrable path parts, range)."""
        hits = [item for item in self.items if item[0] <= pos <= item[1]]
        if not hits:
            return "<module>", (), (0, len(self.stripped))
        start, end, names, registrable = max(hits, key=lambda item: item[0])
        return "::".join(names), registrable, (start, end)

def _impl_name(header: str) -> tuple[str, bool]:
    """`impl<T> Tr for Ty<T> where ..` -> ('<Ty as Tr>', False); inherent -> ('Ty', True)."""
    header = re.split(r"\bwhere\b", header)[0].replace("->", " ")
    while re.search(r"<[^<>]*>", header):  # drop generics innermost-first
        header = re.sub(r"<[^<>]*>", "", header)
    names = [re.sub(r"\b(?:dyn|mut)\b|[&!]|'\w+", "", part).strip().split("::")[-1].strip() or "?"
             for part in re.split(r"\bfor\b", header, maxsplit=1)]
    return (names[0], True) if len(names) == 1 else (f"<{names[1]} as {names[0]}>", False)

def _item_ranges(text: str) -> list[tuple[int, int, tuple[str, ...], tuple[str, ...]]]:
    """Ranges of fn / const / static items with qualified names from the brace walk."""
    items = []
    stack: list[tuple[str, str | None, int, bool]] = []  # kind, name, header start, registrable
    pending = None  # (kind, name, start, paren depth)
    const_pending = None  # (name, start, stack depth, paren depth)
    parens = 0
    def scope() -> tuple[str, ...]:
        return tuple(n for k, n, _, _ in stack if k in ("mod", "impl", "trait", "fn"))
    for match in ITEM_TOKEN_RE.finditer(text):
        token, pos = match.group(0), match.start()
        if token in "([":
            parens += 1
        elif token in ")]":
            parens -= 1
        elif token == "{":
            if pending is not None and pending[3] == parens:
                kind, name, start, _ = pending
                registrable = True
                if kind == "impl":
                    name, registrable = _impl_name(text[start + 4:pos])
                stack.append((kind, name, start, registrable))
                pending = None
            else:
                stack.append(("block", None, pos, True))
        elif token == "}":
            if not stack:
                continue
            kind, name, start, _ = stack.pop()
            if kind == "fn":
                items.append((start, pos, scope() + (name,), _registrable(stack, name)))
        elif token == ";":
            if pending is not None and pending[3] == parens:
                pending = None
            if const_pending is not None and const_pending[2] == len(stack) and const_pending[3] == parens:
                items.append((const_pending[1], pos, scope() + (const_pending[0],), ()))
                const_pending = None
        elif match.group(1):
            pending = ("fn", match.group(1), pos, parens)
        elif token == "impl" and pending is None:
            pending = ("impl", None, pos, parens)
        elif match.group(2) or match.group(3):
            pending = ("mod" if match.group(2) else "trait", match.group(2) or match.group(3), pos, parens)
        elif match.group(5) and const_pending is None and pending is None:
            const_pending = (f"{match.group(4)} {match.group(5)}", pos, len(stack), parens)
    return items

def _registrable(stack, name: str) -> tuple[str, ...]:
    """Path parts clippy can resolve: nested fns collapse to the outermost fn; trait impls none."""
    parts: list[str] = []
    for kind, frame_name, _, registrable in stack:
        if kind == "fn":
            return tuple(parts + [frame_name])
        if kind in ("mod", "impl", "trait"):
            if not registrable:
                return ()
            parts.append(frame_name)
    return tuple(parts + [name])

_MODULE_TABLES: dict[Path, dict[str, str]] = {}
MOD_DECL_RE = re.compile(r"^([ \t]*)(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+([A-Za-z_]\w*)[ \t]*(;|\{)", re.M)
PATH_ATTR_RE = re.compile(r"#\[path\s*=\s*\"([^\"]+)\"\]\s*$")

def _module_table(root: Path) -> dict[str, str]:
    """{src file: crate module path}, following `mod` / `#[path]` from src/lib.rs."""
    if root in _MODULE_TABLES:
        return _MODULE_TABLES[root]
    table: dict[str, str] = {}
    queue = [(root / "src/lib.rs", CRATE, False)]
    while queue:
        path, modpath, owned = queue.pop()
        rel = path.relative_to(root).as_posix()
        if rel in table or not path.exists():
            continue
        table[rel] = modpath
        lines = path.read_text(encoding="utf-8").splitlines()
        inline: list[tuple[str, str]] = []
        for index, line in enumerate(lines):
            while inline and line.startswith(inline[-1][0] + "}"):
                inline.pop()
            match = MOD_DECL_RE.match(line)
            if match is None:
                continue
            indent, name, opener = match.groups()
            if opener == "{":
                inline.append((indent, name))
                continue
            chain = [n for _, n in inline]
            run = itertools.takewhile(lambda l: l.startswith("#["), (l.strip() for l in reversed(lines[:index])))
            attr = next((m.group(1) for m in map(PATH_ATTR_RE.search, run) if m), None)  # nearest #[path]
            own_dir = path.parent if owned or path.stem in ("mod", "lib", "main") else path.parent / path.stem
            base = own_dir.joinpath(*chain)
            if attr:
                child = Path(os.path.normpath((base if chain else path.parent) / attr))
            else:
                child = next((c for c in (base / f"{name}.rs", base / name / "mod.rs") if c.exists()), base / f"{name}.rs")
            queue.append((child, "::".join([modpath, *chain, name]), bool(attr)))
    _MODULE_TABLES[root] = table
    return table

def h2_tag(entry, key: str) -> tuple[str, frozenset[str]] | None:
    """(SET, lanes) of an `H2 <SET> <lane>` reason; None when not H2-tagged, error when malformed."""
    reason = entry.get("reason", "") if isinstance(entry, dict) else ""
    if not str(reason).startswith("H2"):
        return None
    parts = str(reason).split()
    lanes = frozenset(LANES) if parts[2:] == ["both"] else frozenset(parts[2:])
    if (len(parts) != 3 or parts[0] != "H2" or parts[1] not in SET_SECTION or not lanes <= set(LANES)
            or (key == "disallowed-types") != (parts[1] == "TYPES") or not isinstance(entry.get("path"), str)):
        raise MeasureError(f"bad H2 entry in clippy.toml {key}: {entry}")
    return parts[1], lanes

def load_config(clippy_toml: Path) -> dict[str, tuple[str, frozenset[str]]]:
    """{path: (SET, lanes)} for every H2-tagged disallowed-methods/types entry."""
    if not clippy_toml.exists():
        return {}
    import tomllib  # Python >= 3.11; imported lazily so the inert path needs nothing.
    data = tomllib.loads(clippy_toml.read_text(encoding="utf-8"))
    config = {}
    for key in ("disallowed-methods", "disallowed-types"):
        for entry in data.get(key, []):
            if (tag := h2_tag(entry, key)) is None:
                continue
            if entry["path"] in config:  # a later duplicate would silently override the first
                raise MeasureError(f"duplicate H2 path in {clippy_toml}: {entry['path']}")
            config[entry["path"]] = tag
    return config

def diagnostics(lines) -> list[tuple[str, int, int, str, str]]:
    """Deduped (file, line, col, lint, callee) from cargo `--message-format=json` lines."""
    seen = set()
    for raw in (line.strip() for line in lines):
        if not raw.startswith("{"):
            continue
        event = json.loads(raw)
        message = event.get("message") or {}
        code = (message.get("code") or {}).get("code")
        if code not in LINTS or "lib" not in (event.get("target") or {}).get("kind", []):
            continue
        callee = CALLEE_RE.search(message.get("message", ""))
        primary = next((s for s in message.get("spans", []) if s.get("is_primary")), None)
        if callee is None or primary is None:
            raise MeasureError(f"unparseable {code} diagnostic: {message.get('rendered', '')[:200]}")
        site = primary
        while site.get("expansion"):  # attribute macro output to the outermost call site
            site = site["expansion"]["span"]
        if site["file_name"].startswith("src/"):
            seen.add((site["file_name"], site["line_start"], site["column_start"], code, callee.group(1)))
    return sorted(seen)

def run_clippy(root: Path, conf_dir: Path | None) -> list[str]:
    """Lint only the lib target. Touching lib.rs forces a re-lint instead of a cache replay;
    `--cap-lints warn` stops unrelated deny lints from aborting it (force-warn is uncapped)."""
    (root / "src/lib.rs").touch()
    env = dict(os.environ, CARGO_INCREMENTAL="0", **({"CLIPPY_CONF_DIR": str(conf_dir)} if conf_dir else {}))
    command = ["cargo", "clippy", "--lib", "--message-format=json", "--", "--cap-lints", "warn",
               *itertools.chain.from_iterable(("--force-warn", lint) for lint in LINTS)]
    proc = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True)
    if proc.returncode != 0:
        raise MeasureError(f"cargo clippy failed ({proc.returncode}):\n{proc.stderr[-4000:]}")
    return proc.stdout.splitlines()

def _arg_text(src: SourceFile, open_paren: int) -> str:
    depth = 0
    for index in range(open_paren, len(src.stripped)):
        depth += (src.stripped[index] in "([{") - (src.stripped[index] in ")]}")
        if depth == 0:
            return src.raw[open_paren + 1:index]
    return src.raw[open_paren + 1:]

def _literal_value(text: str) -> str | None:
    """The string value when `text` is exactly one string literal, else None."""
    state = rust_lex.StripState()
    segments = [seg for line in text.split("\n") for seg in rust_lex.lex_segments(line, state)]
    kinds = [kind for kind, _ in segments if kind != rust_lex.SPACE]
    if kinds != [rust_lex.LITERAL]:
        return None
    literal = next(t for k, t in segments if k == rust_lex.LITERAL)
    match = re.fullmatch(r'b?r(#*)"(.*)"\1|b?"(.*)"', literal, re.S)
    return None if match is None else (match.group(2) if match.group(2) is not None else match.group(3))

def _all_scalar_literals(text: str) -> bool:
    state = rust_lex.StripState()
    code = " ".join(t for line in text.split("\n") for k, t in rust_lex.lex_segments(line, state) if k == rust_lex.CODE)
    tokens = [tok for tok in re.split(r"[\s&\[\](),]+|vec!", code) if tok]
    return all(SCALAR_TOKEN_RE.fullmatch(tok) for tok in tokens)

def subproc_seed(src: SourceFile, pos: int, item_range: tuple[int, int]) -> bool:
    """R-D plus the non-literal-program rule for one `Command::new` site."""
    match = re.compile(r"[\w:\s<>]*?\bnew\b\s*").match(src.stripped, pos)
    if match is None or match.end() >= len(src.stripped) or src.stripped[match.end()] != "(":
        return True  # used as a value (fn pointer): the program is not visible here
    program = _literal_value(_arg_text(src, match.end()))
    if program is None:
        return True
    if program.rsplit("/", 1)[-1] not in DISPATCHERS:
        return False
    calls = ARG_CALL_RE.finditer(src.stripped, item_range[0], item_range[1])
    return any(not _all_scalar_literals(_arg_text(src, call.end() - 1)) for call in calls)

def measure(root: Path, lines, config) -> dict:
    """Rows per section plus the derived W* / SUBPROC_W sets for one lane."""
    sources: dict[str, SourceFile] = {}
    rows = {section: collections.Counter() for section in SECTIONS}
    derived = {"W": set(), "SUBPROC_W": set(), "unregistrable": set()}
    sites = {section: collections.defaultdict(list) for section in SECTIONS}  # H8 aux, key unchanged
    total = 0
    type_names = {p.rsplit("::", 1)[-1] for p, (s, _) in config.items() if s == "TYPES"}
    for file, line, col, _code, callee in diagnostics(lines):
        if callee not in config:
            continue
        total += 1
        if file in OWNER_FILES or file.startswith(OWNER_PREFIXES):
            continue
        src = sources.get(file) or sources.setdefault(file, SourceFile((root / file).read_text(encoding="utf-8")))
        pos = src.offset(line, col)
        item, parts, item_range = src.enclosing(pos)
        set_name = config[callee][0]
        rows[SET_SECTION[set_name]][(file, item, callee)] += 1
        if src.ambiguous(item):
            sites[SET_SECTION[set_name]][(file, item, callee)].append(line)
        target = "W" if set_name in ("EXEC", "W", "TYPES") else None
        if set_name == "SUBPROC" and subproc_seed(src, pos, item_range):
            target = "SUBPROC_W"
        if target is None or item == "<module>":
            continue
        modpath = _module_table(root).get(file)
        if parts and modpath:
            derived[target].add("::".join([modpath, *parts]))
        elif target == "W" and (set_name == "TYPES" or (m := re.search(r"<(\w+) as ", item)) and m.group(1) in type_names):
            continue  # a registered Self type already covers its trait-impl bodies
        else:
            derived["unregistrable"].add(f"{file}::{item}")
    sites = {section: {key: sorted(v) for key, v in found.items()} for section, found in sites.items()}
    return {"rows": rows, "derived": derived, "total": total, "sites": sites}

def load_baseline(root: Path) -> dict | None:
    present = [root / rel for rel in BASELINE_FILES if (root / rel).exists()]
    if not present:
        return None
    import tomllib
    merged = {section: {} for section in SECTIONS}
    for path in present:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
        for section in SECTIONS:
            for row in data.get(section, {}).get("rows", []):
                key = (row["file"], row["item"], row["callee"])
                if key in merged[section]:
                    raise MeasureError(f"duplicate baseline row {key} in [{section}]")
                merged[section][key] = {lane: int(row.get(lane, 0)) for lane in LANES}
    return merged

_toml_str = json.dumps  # a JSON string is a valid TOML basic string

def write_baseline(root: Path, baseline: dict) -> None:
    """Rewrite both split files, one row per line, sorted; `src/services/**` vs the rest."""
    for rel in BASELINE_FILES:
        services = rel == BASELINE_FILES[0]
        out = ["# Generated by scripts/ci/h2_measure.py --regen; edit only through --regen.", ""]
        for section in SECTIONS:
            out += [f"[{section}]", "rows = ["]
            for (file, item, callee), counts in sorted(baseline[section].items()):
                if file.startswith("src/services/") != services or not any(counts.values()):
                    continue
                cells = ", ".join(f"{lane} = {counts.get(lane, 0)}" for lane in LANES)
                out.append(f"  {{ file = {_toml_str(file)}, item = {_toml_str(item)}, callee = {_toml_str(callee)}, {cells} }},")
            out += ["]", ""]
        (root / rel).write_text("\n".join(out), encoding="utf-8")

def compare(measured: dict, baseline: dict, lane: str) -> list[str]:
    return [f"[{section}] {lane} {key[0]} :: {key[1]} -> {key[2]}: measured {got}, baseline {want}"
            for section in SECTIONS for key in sorted(set(measured[section]) | set(baseline[section]))
            for got, want in [(measured[section].get(key, 0), baseline[section].get(key, {}).get(lane, 0))]
            if got != want]

def render_clippy_toml(config: dict[str, tuple[str, frozenset[str]]]) -> str:
    def entries(types: bool) -> list[str]:
        out = []
        for path, (set_name, lanes) in sorted(config.items(), key=lambda kv: (list(SET_SECTION).index(kv[1][0]), kv[0])):
            if (set_name == "TYPES") == types:
                tag = "both" if lanes == frozenset(LANES) else next(iter(lanes))
                out.append(f"  {{ path = {_toml_str(path)}, reason = \"H2 {set_name} {tag}\" }},")
        return out
    return "\n".join([
        "# H2 tmux boundary sets. EXEC/SUBPROC/TYPES are hand-kept; W/SUBPROC_W are",
        "# rewritten by `scripts/ci/h2_measure.py --regen`.",
        "disallowed-methods = [", *entries(False), "]",
        "disallowed-types = [", *entries(True), "]", "",
    ])

def regen(root: Path, lane: str, runner=run_clippy) -> dict:
    """Iterate W* / SUBPROC_W for `lane` to a fixpoint, then rewrite clippy.toml and the baseline."""
    clippy_toml = root / "clippy.toml"
    if clippy_toml.exists() and re.search(
            r"^(?!disallowed-(?:methods|types)\b)[A-Za-z]", clippy_toml.read_text(encoding="utf-8"), re.M):
        raise MeasureError("clippy.toml has keys --regen cannot preserve; extend render_clippy_toml")
    config = load_config(clippy_toml)
    if not any(set_name == "EXEC" for set_name, _ in config.values()):
        raise MeasureError("clippy.toml has no H2 EXEC entries; nothing to regenerate from")
    with tempfile.TemporaryDirectory() as conf_dir:
        for _ in range(MAX_REGEN_ITERATIONS):
            Path(conf_dir, "clippy.toml").write_text(render_clippy_toml(config), encoding="utf-8")
            result = measure(root, runner(root, Path(conf_dir)), config)
            if result["derived"]["unregistrable"]:
                raise MeasureError("items need a disallowed-types entry: " + ", ".join(sorted(result["derived"]["unregistrable"])))
            updated = {path: (set_name, lanes - {lane} if set_name in DERIVED_SETS else lanes)
                       for path, (set_name, lanes) in config.items()}
            updated = {path: entry for path, entry in updated.items() if entry[1]}
            for set_name in DERIVED_SETS:  # W first: a fn in both sets is tracked as W
                for path in result["derived"][set_name]:
                    prev_set, lanes = updated.get(path, (set_name, frozenset()))
                    if prev_set not in DERIVED_SETS or (prev_set == "W" and set_name != "W"):
                        continue
                    updated[path] = (set_name, lanes | {lane})
            if updated == config:
                break
            config = updated
        else:
            raise MeasureError(f"W*/SUBPROC_W did not converge in {MAX_REGEN_ITERATIONS} clippy passes")
    clippy_toml.write_text(render_clippy_toml(config), encoding="utf-8")
    baseline = load_baseline(root) or {section: {} for section in SECTIONS}
    for section in SECTIONS:
        for counts in baseline[section].values():
            counts[lane] = 0
        for key, count in result["rows"][section].items():
            baseline[section].setdefault(key, {other: 0 for other in LANES})[lane] = count
    write_baseline(root, baseline)
    return result

def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--lane", choices=LANES, required=True)
    parser.add_argument("--repo", type=Path, default=REPO_ROOT)
    parser.add_argument("--json", type=Path, help="read clippy JSON from a file instead of running cargo")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="compare with the baseline lane column")
    mode.add_argument("--regen", action="store_true", help="fixpoint W*/SUBPROC_W, rewrite clippy.toml + baseline")
    parser.add_argument("--inert", action="store_true", help="no-op without a baseline; report drift without failing")
    args = parser.parse_args(argv)
    root = args.repo.resolve()
    try:
        if args.regen:
            result = regen(root, args.lane)
            print(f"h2: regenerated {args.lane}: " + ", ".join(f"{s}={len(result['rows'][s])}" for s in SECTIONS))
            return 0
        if args.check and load_baseline(root) is None:
            if args.inert:
                print("h2: no baseline committed; inert no-op")
                return 0
            print("h2: baseline missing (scripts/ci/h2_baseline_*.toml); rebase onto a main that has it", file=sys.stderr)
            return 2
        # Same lookup as clippy itself: CLIPPY_CONF_DIR, else the repo root.
        config = load_config(Path(os.environ.get("CLIPPY_CONF_DIR", root)) / "clippy.toml")
        lines = args.json.read_text(encoding="utf-8").splitlines() if args.json else run_clippy(root, None)
        result = measure(root, lines, config)
        if not args.check:
            rows = {s: [dict(zip(("file", "item", "callee"), k), count=v,
                             **({"lines": result["sites"][s][k]} if k in result["sites"][s] else {}))
                        for k, v in sorted(r.items())] for s, r in result["rows"].items()}
            derived = {k: sorted(v) for k, v in result["derived"].items()}
            print(json.dumps({"lane": args.lane, "total": result["total"], "rows": rows, "derived": derived}, indent=1))
            return 0
        problems = compare(result["rows"], load_baseline(root), args.lane)
        if result["total"] < LIVENESS_FLOOR:
            problems.append(f"only {result['total']} H2 diagnostics (< liveness floor {LIVENESS_FLOOR})")
    except MeasureError as exc:
        print(f"h2: {exc}", file=sys.stderr)
        return 0 if args.inert else 1
    for problem in problems:
        print(("::warning::h2: " if args.inert else "h2: ") + problem, file=sys.stderr)
    if problems:
        return 0 if args.inert else 1
    print(f"h2: {args.lane} matches the baseline ({result['total']} diagnostics)")
    return 0

if __name__ == "__main__":
    sys.exit(main())
