#!/usr/bin/env python3
"""Generate ``docs/generated/env-reference.md`` from the Rust sources.

The README used to carry a hand-written environment-variable table that
drifted from the code (a dozen rows against 100+ variables actually read, plus
rows for variables nothing reads). This generator derives the table from
``src/`` so the reference cannot drift silently:

* ``env::var("NAME")`` / ``env::var_os("NAME")`` / ``std::env::var(...)`` read
  sites with a literal name.
* ``const FOO_ENV: &str = "AGENTDESK_..."`` style name constants (``AGENTDESK_``
  and ``ADK_`` prefixes), including the indirect reads that pass such a
  constant to ``env::var``.
* ``some_env_helper("AGENTDESK_...")`` calls — helper functions whose name
  contains ``env`` and that take the literal name as an argument.
* ``tracing_subscriber::EnvFilter::from_default_env()`` (reads ``RUST_LOG``)
  and ``EnvFilter::from_env("NAME")``.

The one-line description comes, in order of preference, from a comment in the
same file that names the variable (nearest to the site), the comment adjacent
to the site, or the doc comment of the enclosing ``fn``.

Test-only code is excluded: ``tests.rs`` / ``integration_tests.rs`` modules,
``src/**/tests/`` directories, and ``#[cfg(test)] mod ... { ... }`` blocks
(blanked in place so line numbers stay stable). ``target/`` is never scanned.

Output is deterministic: files are visited in sorted order, rows are sorted by
variable name, and the description is a single sentence chosen by a fixed
preference order. CI regenerates the document and fails on ``git diff`` drift
(see ``scripts/ci-script-checks.sh``).
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from generate_inventory_docs import (  # noqa: E402
    _CFG_MOD_RE,
    cfg_requires_test,
    is_test_file,
    scan_balanced,
)

REPO_ROOT = Path(__file__).resolve().parent.parent
SRC_ROOT = REPO_ROOT / "src"
OUTPUT_DOC = REPO_ROOT / "docs" / "generated" / "env-reference.md"
GENERATOR_COMMAND = "python3 scripts/generate_env_reference.py"

PROJECT_PREFIXES = ("AGENTDESK_", "ADK_")
ENV_NAME = r"[A-Z][A-Z0-9_]*"
MAX_DESCRIPTION_CHARS = 160

# `env::var("NAME")`, `env::var_os("NAME")`, `std::env::var("NAME")`.
_LITERAL_READ_RE = re.compile(
    r"(?<![A-Za-z0-9_])(?:std::)?env::var(?:_os)?\(\s*\"(?P<name>" + ENV_NAME + r")\""
)
# `env::var(SOME_CONST)` — resolved through the constant table below.
_CONST_READ_RE = re.compile(
    r"(?<![A-Za-z0-9_])(?:std::)?env::var(?:_os)?\(\s*(?P<ident>[A-Z][A-Z0-9_]*)\s*\)"
)
# `const FOO: &str = "AGENTDESK_..."` (also `pub`, `pub(crate)`, `static`,
# `&'static str`; the literal may sit on the next line after a rustfmt wrap).
_CONST_DEF_RE = re.compile(
    r"(?:pub(?:\([^)]*\))?\s+)?(?:const|static)\s+(?P<ident>[A-Z][A-Z0-9_]*)\s*:\s*&(?:'static\s+)?str\s*=\s*\"(?P<name>(?:AGENTDESK|ADK)_[A-Z0-9_]*)\""
)
# `explicit_env_path("AGENTDESK_X")`, `migrated_env_path("AGENTDESK_X")`, ...
_HELPER_READ_RE = re.compile(
    r"(?<![A-Za-z0-9_])(?P<helper>[a-z_]*env[a-z_]*)\(\s*[^()\"]*?\"(?P<name>(?:AGENTDESK|ADK)_[A-Z0-9_]*)\""
)
_HELPER_EXCLUDE = {"set_var", "remove_var", "env_remove", "env"}
# `tracing_subscriber::EnvFilter::from_default_env()` reads `RUST_LOG` without
# naming it; `EnvFilter::from_env("NAME")` names its variable explicitly.
_ENV_FILTER_DEFAULT_RE = re.compile(r"EnvFilter::(?:try_)?from_default_env\(")
_ENV_FILTER_NAMED_RE = re.compile(
    r"EnvFilter::(?:try_)?from_env\(\s*\"(?P<name>" + ENV_NAME + r")\""
)
ENV_FILTER_DEFAULT_VARIABLE = "RUST_LOG"


class ParseError(RuntimeError):
    pass


@dataclass(frozen=True, order=True)
class Site:
    path: str
    line: int
    kind: str  # "const" | "read" | "helper"
    comment: str = field(compare=False, default="")


@dataclass
class Variable:
    name: str
    sites: list[Site] = field(default_factory=list)

    @property
    def is_project_variable(self) -> bool:
        return self.name.startswith(PROJECT_PREFIXES)

    def primary_site(self) -> Site:
        consts = sorted(site for site in self.sites if site.kind == "const")
        if consts:
            return consts[0]
        return sorted(self.sites)[0]

    def description(self) -> str:
        ordered = sorted(self.sites)
        candidates = [site.comment for site in ordered if site.comment]
        if not candidates:
            return ""
        for comment in candidates:
            if self.name in comment:
                return comment
        for comment in candidates:
            if re.search(r"\benv(?:ironment)?\b", comment, re.IGNORECASE):
                return comment
        primary = self.primary_site()
        return primary.comment or candidates[0]


def rel_posix(path: Path) -> str:
    return path.relative_to(REPO_ROOT).as_posix()


def production_rust_files() -> list[Path]:
    files: list[Path] = []
    for path in sorted(SRC_ROOT.rglob("*.rs")):
        rel = path.relative_to(SRC_ROOT)
        if "target" in rel.parts:
            continue
        if "tests" in rel.parts[:-1]:
            continue
        if is_test_file(path):
            continue
        files.append(path)
    return files


def blank_test_modules(text: str) -> str:
    """Replace ``#[cfg(test)] mod x { ... }`` bodies with newlines.

    Line numbers of the surrounding code are preserved because every removed
    character that is not a newline is dropped while newlines are kept.
    """

    result: list[str] = []
    cursor = 0
    for match in _CFG_MOD_RE.finditer(text):
        if match.start() < cursor:
            continue
        if not cfg_requires_test(match.group("predicate")):
            continue
        open_index = match.end() - 1
        try:
            _body, close_index = scan_balanced(text, open_index, "{", "}")
        except Exception as error:  # pragma: no cover - defensive
            raise ParseError(f"unbalanced test module at offset {open_index}: {error}") from error
        result.append(text[cursor : match.start()])
        # ``scan_balanced`` returns the index just past the closing brace.
        removed = text[match.start() : close_index]
        result.append("\n" * removed.count("\n"))
        cursor = close_index
    result.append(text[cursor:])
    return "".join(result)


def _clean_comment_line(line: str) -> str:
    stripped = line.strip()
    for prefix in ("///", "//!", "//"):
        if stripped.startswith(prefix):
            stripped = stripped[len(prefix) :]
            break
    return " ".join(stripped.split())


_FN_LINE_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+\"C\"\s+)?fn\s")


def _comment_block_above(lines: list[str], index: int) -> str:
    """Join the contiguous ``//`` comment block ending right above ``index``.

    Attribute lines (``#[...]``) between the block and the statement are
    skipped so ``/// doc`` + ``#[arg]`` + ``field`` still resolves the doc.
    """

    cursor = index - 1
    while cursor >= 0 and lines[cursor].strip().startswith("#["):
        cursor -= 1
    block: list[str] = []
    while cursor >= 0:
        stripped = lines[cursor].strip()
        if stripped.startswith("//"):
            block.append(_clean_comment_line(stripped))
            cursor -= 1
            continue
        break
    block.reverse()
    return " ".join(text for text in block if text)


def _trailing_comment(line: str) -> str:
    code, _, trailing = line.partition("//")
    if trailing and '"' not in trailing and code.strip():
        return _clean_comment_line("//" + trailing)
    return ""


def _sentence_mentioning(text: str, name: str) -> str:
    for sentence in re.split(r"(?<=[.!?])\s+", text):
        if name in sentence:
            return sentence
    return first_sentence(text)


def _nearest_mention(lines: list[str], index: int, name: str) -> str:
    """Nearest comment line in the file that names the variable."""

    best: tuple[int, int] | None = None
    for cursor, line in enumerate(lines):
        stripped = line.strip()
        if not stripped.startswith("//"):
            continue
        if name not in _clean_comment_line(stripped):
            continue
        distance = abs(cursor - index)
        if best is None or distance < best[0]:
            best = (distance, cursor)
    if best is None:
        return ""
    # Expand to the contiguous comment block around the hit and pick the
    # sentence that mentions the variable.
    top = best[1]
    while top > 0 and lines[top - 1].strip().startswith("//"):
        top -= 1
    bottom = best[1]
    while bottom + 1 < len(lines) and lines[bottom + 1].strip().startswith("//"):
        bottom += 1
    joined = " ".join(
        text for text in (_clean_comment_line(lines[i]) for i in range(top, bottom + 1)) if text
    )
    return _sentence_mentioning(joined, name)


def _enclosing_fn_doc(lines: list[str], index: int) -> str:
    cursor = index
    while cursor >= 0:
        if _FN_LINE_RE.match(lines[cursor]):
            return _comment_block_above(lines, cursor)
        cursor -= 1
    return ""


def describe_site(lines: list[str], line_number: int, name: str) -> str:
    """Return a one-line description for the variable read at ``line_number``.

    Preference: a comment in the same file that names the variable (nearest to
    the site), then the trailing/preceding comment adjacent to the site, then
    the doc comment of the enclosing ``fn``. Only one sentence is kept so the
    table stays one row per variable.
    """

    index = line_number - 1
    mention = _nearest_mention(lines, index, name)
    if mention:
        return truncate(mention)
    trailing = _trailing_comment(lines[index])
    if trailing:
        return truncate(first_sentence(trailing))
    above = _comment_block_above(lines, index)
    if above:
        return truncate(first_sentence(above))
    fn_doc = _enclosing_fn_doc(lines, index)
    if fn_doc:
        return truncate(first_sentence(fn_doc))
    return ""


def first_sentence(text: str) -> str:
    match = re.search(r"[.!?](?:\s|$)", text)
    if match is None:
        return text
    return text[: match.start() + 1]


def truncate(text: str) -> str:
    if len(text) <= MAX_DESCRIPTION_CHARS:
        return text
    return text[: MAX_DESCRIPTION_CHARS - 1].rstrip() + "…"


def offset_to_line(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def collect_variables(files: list[Path]) -> dict[str, Variable]:
    variables: dict[str, Variable] = {}
    # Constant identifiers are resolved per file first (a const is almost
    # always used in the module that defines it), then crate-wide.
    per_file_consts: dict[Path, dict[str, str]] = {}
    global_consts: dict[str, str] = {}
    texts: dict[Path, str] = {}

    def add(name: str, site: Site) -> None:
        variables.setdefault(name, Variable(name)).sites.append(site)

    for path in files:
        text = blank_test_modules(path.read_text(encoding="utf-8"))
        texts[path] = text
        lines = text.splitlines()
        consts: dict[str, str] = {}
        rel = rel_posix(path)
        for match in _CONST_DEF_RE.finditer(text):
            line = offset_to_line(text, match.start())
            consts[match.group("ident")] = match.group("name")
            add(match.group("name"), Site(rel, line, "const", describe_site(lines, line, match.group("name"))))
        per_file_consts[path] = consts
        for ident, name in consts.items():
            global_consts.setdefault(ident, name)

    for path in files:
        text = texts[path]
        lines = text.splitlines()
        rel = rel_posix(path)
        for match in _LITERAL_READ_RE.finditer(text):
            line = offset_to_line(text, match.start())
            add(match.group("name"), Site(rel, line, "read", describe_site(lines, line, match.group("name"))))
        for match in _CONST_READ_RE.finditer(text):
            ident = match.group("ident")
            name = per_file_consts[path].get(ident) or global_consts.get(ident)
            if name is None:
                continue
            line = offset_to_line(text, match.start())
            add(name, Site(rel, line, "read", describe_site(lines, line, name)))
        for match in _HELPER_READ_RE.finditer(text):
            if match.group("helper") in _HELPER_EXCLUDE:
                continue
            line = offset_to_line(text, match.start())
            add(match.group("name"), Site(rel, line, "helper", describe_site(lines, line, match.group("name"))))
        for match in _ENV_FILTER_DEFAULT_RE.finditer(text):
            line = offset_to_line(text, match.start())
            add(
                ENV_FILTER_DEFAULT_VARIABLE,
                Site(rel, line, "helper", describe_site(lines, line, ENV_FILTER_DEFAULT_VARIABLE)),
            )
        for match in _ENV_FILTER_NAMED_RE.finditer(text):
            line = offset_to_line(text, match.start())
            add(match.group("name"), Site(rel, line, "helper", describe_site(lines, line, match.group("name"))))

    for variable in variables.values():
        variable.sites = sorted(set(variable.sites))
    return variables


def markdown_cell(text: str) -> str:
    return text.replace("|", "\\|").replace("\n", " ")


def render_table(variables: list[Variable]) -> list[str]:
    lines = ["| Variable | Defined at | Description |", "|---|---|---|"]
    for variable in variables:
        site = variable.primary_site()
        extra = len(variable.sites) - 1
        location = f"`{site.path}:{site.line}`"
        if extra > 0:
            location += f" (+{extra} more)"
        lines.append(
            f"| `{variable.name}` | {location} | {markdown_cell(variable.description())} |"
        )
    return lines


def render(variables: dict[str, Variable]) -> str:
    ordered = sorted(variables.values(), key=lambda variable: variable.name)
    project = [variable for variable in ordered if variable.is_project_variable]
    external = [variable for variable in ordered if not variable.is_project_variable]
    lines = [
        "# Environment Variable Reference",
        "",
        f"<!-- Generated by `{GENERATOR_COMMAND}`. Do not edit by hand. -->",
        "",
        "Environment variables read by the AgentDesk binary, derived from `src/`.",
        "Test-only modules and `#[cfg(test)]` blocks are excluded. `Defined at`",
        "points to the name constant when one exists, otherwise to the first read",
        "site; `(+N more)` counts additional read sites. `Description` is the",
        "comment adjacent to that site (blank when the code has none).",
        "",
        f"Regenerate with `{GENERATOR_COMMAND}`; CI fails when this file drifts.",
        "",
        f"- AgentDesk variables (`AGENTDESK_*`, `ADK_*`): {len(project)}",
        f"- Platform and third-party variables: {len(external)}",
        "",
        "## AgentDesk variables",
        "",
        *render_table(project),
        "",
        "## Platform and third-party variables",
        "",
        *render_table(external),
        "",
    ]
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--output",
        type=Path,
        default=OUTPUT_DOC,
        help="destination markdown path (default: docs/generated/env-reference.md)",
    )
    parser.add_argument(
        "--stdout",
        action="store_true",
        help="print the rendered document instead of writing it",
    )
    args = parser.parse_args(argv)

    variables = collect_variables(production_rust_files())
    rendered = render(variables)
    if args.stdout:
        sys.stdout.write(rendered)
        return 0
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(rendered, encoding="utf-8")
    project = sum(1 for variable in variables.values() if variable.is_project_variable)
    print(
        f"wrote {args.output.relative_to(REPO_ROOT).as_posix()}: "
        f"{project} AgentDesk variables, {len(variables) - project} other"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
