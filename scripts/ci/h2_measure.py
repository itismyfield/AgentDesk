#!/usr/bin/env python3
"""H2 tmux-boundary measurer: one sealed Clippy session's lib diagnostics -> (file, item, callee) rows.

Rows are classified by the `reason = "H2 <SET> <lanes>"` tag of the matching
clippy.toml entry; W* and SUBPROC_W are the compiler item paths of the same session's sites.
"""

from __future__ import annotations

import argparse
import collections
import hashlib
import itertools
import json
import os
import re
import sys
import uuid
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "scripts"))
import rust_lex  # noqa: E402
import h2_env  # noqa: E402

CRATE = "agentdesk"
LANES = ("linux", "macos")
OWNER_FILES = frozenset({
    "src/services/platform/tmux/liveness.rs",
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
# R-O lints forced in the same pass; diagnostics() skips them so they never become measured rows.
RO_LINTS = ("clippy::duplicate_mod",)
# Raised by the activation PR; 0 keeps the measurer inert until then.
LIVENESS_FLOOR = 0
MAX_REGEN_ITERATIONS = 20
# Run, config and Cargo target stay apart: a session seals its config's parent directories.
SESSIONS, SESSION_TARGET = Path("target/h2/sessions"), Path("target/h2/target")
MODULE_LEVEL = "<module>"
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
H2_PATH_RE = re.compile(r"[A-Za-z_]\w*(::[A-Za-z_]\w*)+")
H2_CRATES = frozenset({"agentdesk", "std", "core", "alloc", "tokio"})
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
        self.item_names = collections.Counter("::".join(names) for _, _, names in self.items)

    def ambiguous(self, item: str) -> bool:
        """H8: `<module>` or a name shared by several items (`const _`, cfg twins) folds sites."""
        return item == "<module>" or self.item_names[item] > 1

    def offset(self, line: int, column: int) -> int:
        return self.line_offsets[line - 1] + column - 1

    def enclosing(self, pos: int) -> tuple[str, tuple[int, int]]:
        """Innermost item containing `pos`: (row name, range)."""
        hits = [item for item in self.items if item[0] <= pos <= item[1]]
        if not hits:
            return "<module>", (0, len(self.stripped))
        start, end, names = max(hits, key=lambda item: item[0])
        return "::".join(names), (start, end)

def _impl_name(header: str) -> str:
    """`impl<T> Tr for Ty<T> where ..` -> '<Ty as Tr>'; inherent -> 'Ty'."""
    header = re.split(r"\bwhere\b", header)[0].replace("->", " ")
    while re.search(r"<[^<>]*>", header):  # drop generics innermost-first
        header = re.sub(r"<[^<>]*>", "", header)
    names = [re.sub(r"\b(?:dyn|mut)\b|[&!]|'\w+", "", part).strip().split("::")[-1].strip() or "?"
             for part in re.split(r"\bfor\b", header, maxsplit=1)]
    return names[0] if len(names) == 1 else f"<{names[1]} as {names[0]}>"

def _item_ranges(text: str) -> list[tuple[int, int, tuple[str, ...]]]:
    """Ranges of fn / const / static items with qualified names from the brace walk."""
    items = []
    stack: list[tuple[str, str | None, int]] = []  # kind, name, header start
    pending = None  # (kind, name, start, paren depth)
    const_pending = None  # (name, start, stack depth, paren depth)
    parens = 0
    def scope() -> tuple[str, ...]:
        return tuple(n for k, n, _ in stack if k in ("mod", "impl", "trait", "fn"))
    for match in ITEM_TOKEN_RE.finditer(text):
        token, pos = match.group(0), match.start()
        if token in "([":
            parens += 1
        elif token in ")]":
            parens -= 1
        elif token == "{":
            if pending is not None and pending[3] == parens:
                kind, name, start, _ = pending
                if kind == "impl":
                    name = _impl_name(text[start + 4:pos])
                stack.append((kind, name, start))
                pending = None
            else:
                stack.append(("block", None, pos))
        elif token == "}":
            if not stack:
                continue
            kind, name, start = stack.pop()
            if kind == "fn":
                items.append((start, pos, scope() + (name,)))
        elif token == ";":
            if pending is not None and pending[3] == parens:
                pending = None
            if const_pending is not None and const_pending[2] == len(stack) and const_pending[3] == parens:
                items.append((const_pending[1], pos, scope() + (const_pending[0],)))
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

_MODULE_TABLES: dict[Path, tuple[dict[str, str], list[Path]]] = {}
MOD_DECL_RE = re.compile(r"^([ \t]*)(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+([A-Za-z_]\w*)[ \t]*(;|\{)", re.M)
PATH_ATTR_RE = re.compile(r"#\[path\s*=\s*\"([^\"]+)\"\]\s*$")

def _module_table(root: Path) -> dict[str, str]:
    """{src file: crate module path}, following `mod` / `#[path]` from src/lib.rs."""
    return _module_walk(root)[0]

def _module_walk(root: Path) -> tuple[dict[str, str], list[Path]]:
    """(module table keyed by `..`-collapsed path, module files as opened). Files are opened and searched
    at the joined path so `..` after a directory symlink resolves on disk, as it does for rustc."""
    if root in _MODULE_TABLES:
        return _MODULE_TABLES[root]
    table: dict[str, str] = {}
    opened: list[Path] = []
    queue = [(root / "src/lib.rs", CRATE, False)]
    while queue:
        path, modpath, owned = queue.pop()
        rel = Path(os.path.normpath(path)).relative_to(root).as_posix()
        if rel in table or not path.exists():
            continue
        table[rel] = modpath
        opened.append(path)
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
                child = (base if chain else path.parent) / attr
            else:
                child = next((c for c in (base / f"{name}.rs", base / name / "mod.rs") if c.exists()), base / f"{name}.rs")
            queue.append((child, "::".join([modpath, *chain, name]), bool(attr)))
    _MODULE_TABLES[root] = table, opened
    return table, opened

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
            if not H2_PATH_RE.fullmatch(entry["path"]) or entry["path"].split("::", 1)[0] not in H2_CRATES:
                raise MeasureError(f"invalid H2 path in {clippy_toml}: {entry['path']!r}")
            if entry["path"] in config:  # a later duplicate would silently override the first
                raise MeasureError(f"duplicate H2 path in {clippy_toml}: {entry['path']}")
            config[entry["path"]] = tag
    return config

def lane_config(config: dict, lane: str) -> dict:
    """Only paths registered for this lane are passed to Clippy and measured."""
    return {path: entry for path, entry in config.items() if lane in entry[1]}

def seed_config(config: dict, lane: str) -> dict:
    """Start from hand-kept seeds while preserving the other lane's derived registrations."""
    seeded = {path: (set_name, lanes - {lane} if set_name in DERIVED_SETS else lanes)
              for path, (set_name, lanes) in config.items()}
    return {path: entry for path, entry in seeded.items() if entry[1]}

def _events(lines):
    for raw in (line.strip() for line in lines):
        if raw.startswith("{"):
            yield json.loads(raw)

def _check_config(event: dict) -> None:
    message = event.get("message") or {}
    if event.get("reason") == "compiler-message" and any(
            Path(span.get("file_name", "")).name == "clippy.toml" for span in message.get("spans", [])):
        raise MeasureError(f"clippy could not use an H2 path: {message.get('message', '')}; "
                           "run --regen for stale derived paths")

def _sites(messages) -> dict[tuple[str, int, int, str, str], dict]:
    """{(file, line, col, lint, callee): primary span} per H2 lint site, the outermost macro call site once."""
    seen = {}
    for message in messages:
        code = (message.get("code") or {}).get("code")
        if code not in LINTS:
            continue
        callee = CALLEE_RE.search(message.get("message", ""))
        primary = next((s for s in message.get("spans", []) if s.get("is_primary")), None)
        if callee is None or primary is None:
            raise MeasureError(f"unparseable {code} diagnostic: {message.get('rendered', '')[:200]}")
        site = primary
        while site.get("expansion"):  # attribute macro output to the outermost call site
            site = site["expansion"]["span"]
        if site["file_name"].startswith("src/"):
            seen.setdefault((site["file_name"], site["line_start"], site["column_start"], code, callee.group(1)), primary)
    return seen

def diagnostics(lines) -> list[tuple[str, int, int, str, str]]:
    """Deduped (file, line, col, lint, callee) from cargo `--message-format=json` lines."""
    messages = []
    for event in _events(lines):
        _check_config(event)
        if "lib" in (event.get("target") or {}).get("kind", []):
            messages.append(event.get("message") or {})
    return sorted(_sites(messages))

def session_runner(root: Path, lane: str, driver: Path | None = None):
    """Build the driver once; each call runs one sealed lib Clippy session and loads the items it produced.
    `--cap-lints warn` and the force-warn lints come from the session's CLIPPY_ARGS."""
    import h2_items, h2_modmap, h2_session  # they import this module
    try:
        driver = driver or h2_modmap.build_driver(root)
    except h2_modmap.ModmapError as exc:
        raise MeasureError(f"modmap-driver build: {exc}") from exc
    def run(conf: Path, run_dir: Path):
        manifest = h2_session.session(root, root, run_dir, conf, lane, driver=driver,
                                      extra=("--locked", "--target-dir", str(root / SESSION_TARGET)))
        return h2_items.load(Path(manifest["manifest"]), crate=root)
    return run

def bind(items, root: Path, run: Path, lane: str, conf_text: str):
    """Only this run's session, of this repo and lane, with exactly this lane configuration, is measured."""
    request = items.manifest["request"]
    if (items.manifest["run_dir"], request["repo"], request["lane"], request["source"]["config"]) != (
            str(run), str(root), lane, hashlib.sha256(conf_text.encode()).hexdigest()):
        raise MeasureError(f"session {items.manifest['run_dir']} is not the {lane} run {run} of {root} "
                           "with this lane configuration")
    return items

def load_session(root: Path, run: Path, config: dict, lane: str):
    """A sealed session produced elsewhere; an unsealed, unfenced or other-lane run is refused."""
    import h2_items
    run = run.resolve()
    return bind(h2_items.load(run / "manifest.json", crate=root), root, run, lane, render_clippy_toml(lane_config(config, lane)))

def write_conf(conf: Path, config: dict, lane: str) -> str:
    text = render_clippy_toml(lane_config(config, lane))
    (conf / "clippy.toml").write_text(text, encoding="utf-8")
    return text

def new_run(root: Path) -> Path:
    base = root / SESSIONS / uuid.uuid4().hex
    (base / "conf").mkdir(parents=True)
    return base

def lane_session(root: Path, config: dict, lane: str, runner=None):
    """(items, lane clippy.toml) of one session with the lane configuration; the stored union is untouched."""
    base = new_run(root)
    text = write_conf(base / "conf", config, lane)
    items = (runner or session_runner(root, lane))(base / "conf", base / "check")
    return bind(items, root, base / "check", lane, text), base / "conf/clippy.toml"

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

def measure(root: Path, items, config) -> dict:
    """Rows per section, the derived W* / SUBPROC_W paths and each R-W key's site paths for one lane session."""
    import h2_items
    for event in _events(items.lines):
        _check_config(event)
    sources: dict[str, SourceFile] = {}
    rows = {section: collections.Counter() for section in SECTIONS}
    derived = {"W": set(), "SUBPROC_W": set(), "unregistrable": set()}
    sites = {section: collections.defaultdict(list) for section in SECTIONS}  # H8 aux, key unchanged
    reg = collections.defaultdict(list)  # R-W: each site's path, None when unregistrable
    total = 0
    for (file, line, col, code, callee), span in sorted(_sites(items.messages).items()):
        if callee not in config:
            continue
        total += 1
        if file in OWNER_FILES or file.startswith(OWNER_PREFIXES):
            continue
        src = sources.get(file) or sources.setdefault(file, SourceFile((root / file).read_text(encoding="utf-8")))
        pos = src.offset(line, col)
        item, item_range = src.enclosing(pos)
        set_name = config[callee][0]
        key = (file, item, callee)
        rows[SET_SECTION[set_name]][key] += 1
        if src.ambiguous(item):
            sites[SET_SECTION[set_name]][key].append(line)
        target = "W" if set_name in ("EXEC", "W", "TYPES") else None
        if set_name == "SUBPROC" and subproc_seed(src, pos, item_range):
            target = "SUBPROC_W"
        if target is None:
            continue
        path, reason = h2_items.resolve(items, span)
        if reason == "module-level" and code == "clippy::disallowed_types":
            reg[key].append(MODULE_LEVEL)  # a type named outside any body, like a field: nothing to register
            continue
        reg[key].append(path)
        if path is not None:
            derived[target].add(path)
        else:
            derived["unregistrable"].add(f"{file}::{item} ({'module-call' if reason == 'module-level' else reason})")
    sites = {section: {key: sorted(v) for key, v in found.items()} for section, found in sites.items()}
    return {"rows": rows, "derived": derived, "total": total, "sites": sites, "reg": dict(reg)}

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

def regen(root: Path, lane: str, runner=None) -> dict:
    """Iterate W* / SUBPROC_W for `lane` to a fixpoint, then rewrite clippy.toml and the baseline."""
    clippy_toml = root / "clippy.toml"
    if clippy_toml.exists() and re.search(
            r"^(?!disallowed-(?:methods|types)\b)[A-Za-z]", clippy_toml.read_text(encoding="utf-8"), re.M):
        raise MeasureError("clippy.toml has keys --regen cannot preserve; extend render_clippy_toml")
    config = seed_config(load_config(clippy_toml), lane)
    if not any(set_name == "EXEC" for set_name, _ in config.values()):
        raise MeasureError("clippy.toml has no H2 EXEC entries; nothing to regenerate from")
    runner, base = runner or session_runner(root, lane), new_run(root)
    for n in range(1, MAX_REGEN_ITERATIONS + 1):
        run = base / f"pass-{n}"
        text = write_conf(base / "conf", config, lane)
        result = measure(root, bind(runner(base / "conf", run), root, run, lane, text), lane_config(config, lane))
        (run / "items.jsonl").unlink(missing_ok=True)  # mapped: no later pass may reuse these items
        if result["derived"]["unregistrable"]:
            raise MeasureError("cannot register (reason): " + ", ".join(sorted(result["derived"]["unregistrable"]))
                               + "; restructure the call site")
        updated = seed_config(config, lane)
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
    parser.add_argument("--session", type=Path, help="measure this sealed h2_session run instead of running one")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="compare with the baseline lane column")
    mode.add_argument("--regen", action="store_true", help="fixpoint W*/SUBPROC_W, rewrite clippy.toml + baseline")
    parser.add_argument("--inert", action="store_true", help="no-op without a baseline; report drift without failing")
    args = parser.parse_args(argv)
    if args.session and args.regen:
        parser.error("--regen runs its own session per pass; --session is for --check and the report")
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
        config = load_config(root / "clippy.toml")
        items = load_session(root, args.session, config, args.lane) if args.session else lane_session(root, config, args.lane)[0]
        result = measure(root, items, lane_config(config, args.lane))
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
    import h2_measure  # session modules raise the imported module's MeasureError, not __main__'s
    sys.exit(h2_measure.main())
