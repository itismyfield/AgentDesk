#!/usr/bin/env python3
"""Check every docs/statecharts TLA+ config with plain TLC against expected.tsv.

Each config is run until no rule breaks: a broken rule is recorded, dropped from
the config and the model is run again. TLC stops at the first violation and
`-continue` still stops at a deadlock, so this is the only way to get the full
set. Any TLC end that is not a named rule breaking or a clean finish fails.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_ROOT = REPO_ROOT / "docs/statecharts"
DEADLOCK = "NoDeadlock"

# tlc2.output.EC message codes and tlc2.TLC exit codes (tla2tools 1.7.4).
MSG_INVARIANT_INITIAL = 2107
MSG_INVARIANT_BEHAVIOR = 2110
MSG_DEADLOCK = 2114
MSG_SUCCESS = 2193
EXIT_OK, EXIT_DEADLOCK, EXIT_SAFETY = 0, 11, 12

CFG_KEYWORDS = {
    "CONSTANT", "CONSTANTS", "CONSTRAINT", "CONSTRAINTS", "ACTION_CONSTRAINT",
    "ACTION_CONSTRAINTS", "INVARIANT", "INVARIANTS", "INIT", "NEXT", "VIEW",
    "SYMMETRY", "SPECIFICATION", "PROPERTY", "PROPERTIES", "TYPE",
    "TYPE_CONSTRAINT", "CHECK_DEADLOCK", "POSTCONDITION", "ALIAS",
}
CFG_TOKEN = re.compile(
    r'(?P<comment>\\\*[^\n]*)|(?P<block>\(\*)|(?P<string>"(?:[^"\\\n]|\\[^\n])*")|(?P<quote>")'
    r"|(?P<word>[A-Za-z_][A-Za-z0-9_]*)|(?P<other>\S)"
)
# Characters refused outside comments: TLC 1.7.4 reads the rest of the file as the end at `;`, `?`,
# a backtick, control characters and non-ASCII, so only printable ASCII without those is accepted.
UNREADABLE = re.compile(r"[^\t\n\x20-\x3a\x3c-\x3e\x40-\x5f\x61-\x7e]")
# Inside a block comment TLC sees neither strings nor line comments, only these marks.
BLOCK_MARK = re.compile(r"\(\*|\*\)")
TOOL_MESSAGE = re.compile(r"@!@!@STARTMSG (\d+):\d+ @!@!@\n(.*?)@!@!@ENDMSG \1 @!@!@", re.S)
VIOLATED = re.compile(r"Invariant (\w+) is violated")


class CheckError(Exception):
    """A config or a TLC run that cannot give a trustworthy rule set."""


@dataclass
class Config:
    text: str
    invariants: list[str]
    check_deadlock: bool
    cut: list[tuple[int, int]]

    def rules(self) -> list[str]:
        return self.invariants + ([DEADLOCK] if self.check_deadlock else [])

    def render(self, invariants: list[str], check_deadlock: bool) -> str:
        """The config with only the given rules checked."""
        parts, start = [], 0
        for begin, end in self.cut:
            parts.append(self.text[start:begin])
            start = end
        parts.append(self.text[start:])
        body = "".join(parts).rstrip() + "\n"
        if invariants:
            body += f"INVARIANT {' '.join(invariants)}\n"
        return body + f"CHECK_DEADLOCK {'TRUE' if check_deadlock else 'FALSE'}\n"


def cfg_tokens(text: str) -> list[re.Match[str]]:
    """Tokens outside comments, refusing what TLC would read past without a word.

    TLC nests block comments, and both an unclosed comment and an unclosed string
    silently swallow the rest of the file, so rewriting such a config could drop
    every rule it checks. A character TLC cannot lex and any backslash in a string
    are refused too: TLC reads the rest of the file as the end at them.
    """
    tokens, pos = [], 0
    while match := CFG_TOKEN.search(text, pos):
        comment = match.group("comment") or match.group("block")
        if bad := UNREADABLE.search(text, pos, match.start() if comment else match.end()):
            raise CheckError(f"character {bad.group()!r} outside a comment is not supported")
        pos = match.end()
        if match.group("quote"):
            raise CheckError("string not closed on its line")
        if "\\" in (match.group("string") or ""):
            raise CheckError("escaped string literals are not supported")
        if match.group("block"):
            mark = BLOCK_MARK.search(text, pos)
            if not mark:
                raise CheckError("block comment not closed")
            if mark.group() == "(*":
                raise CheckError("nested block comments are not supported")
            pos = mark.end()
        elif not match.group("comment"):
            tokens.append(match)
    if bad := UNREADABLE.search(text, pos):
        raise CheckError(f"character {bad.group()!r} outside a comment is not supported")
    return tokens


def parse_config(text: str) -> Config:
    tokens = cfg_tokens(text)
    invariants: list[str] = []
    check_deadlock, seen_check_deadlock = True, False
    cut: list[tuple[int, int]] = []
    i = 0
    while i < len(tokens):
        word = tokens[i].group("word")
        if word not in CFG_KEYWORDS:
            i += 1
            continue
        j = i + 1
        while j < len(tokens) and tokens[j].group("word") not in CFG_KEYWORDS:
            j += 1
        args = tokens[i + 1:j]
        end = tokens[j].start() if j < len(tokens) else len(text)
        if word in ("PROPERTY", "PROPERTIES"):
            raise CheckError("PROPERTY is not supported; this checker reads invariants and deadlock only")
        if word in ("INVARIANT", "INVARIANTS"):
            if not args or any(not t.group("word") for t in args):
                raise CheckError(f"{word} must list invariant names")
            invariants += [t.group("word") for t in args]
            cut.append((tokens[i].start(), end))
        elif word == "CHECK_DEADLOCK":
            if seen_check_deadlock:
                raise CheckError("CHECK_DEADLOCK appears twice; TLC refuses that config")
            seen_check_deadlock = True
            values = [t.group(0) for t in args]
            if values not in (["TRUE"], ["FALSE"]):
                raise CheckError(f"CHECK_DEADLOCK must be TRUE or FALSE, got {values}")
            check_deadlock = values == ["TRUE"]
            cut.append((tokens[i].start(), end))
        i = j
    if len(set(invariants)) != len(invariants) or DEADLOCK in invariants:
        raise CheckError(f"invariant names must be unique and not {DEADLOCK}: {invariants}")
    return Config(text, invariants, check_deadlock, cut)


def classify(status: int, output: str, invariants: list[str], check_deadlock: bool) -> str | None:
    """None when every checked rule holds, the broken rule's name, or CheckError."""
    messages = [(int(code), body) for code, body in TOOL_MESSAGE.findall(output)]
    violated = [name for code, body in messages if code in (MSG_INVARIANT_INITIAL, MSG_INVARIANT_BEHAVIOR)
                for name in VIOLATED.findall(body)]
    deadlocks = sum(code == MSG_DEADLOCK for code, _ in messages)
    success = any(code == MSG_SUCCESS for code, _ in messages)
    if status == EXIT_OK and success and not violated and not deadlocks:
        return None
    if status == EXIT_SAFETY and not success and not deadlocks and len(violated) == 1 and violated[0] in invariants:
        return violated[0]
    if status == EXIT_DEADLOCK and not success and not violated and deadlocks == 1 and check_deadlock:
        return DEADLOCK
    tail = "\n".join(output.strip().splitlines()[-25:])
    raise CheckError(f"TLC ended without a clean finish or one named rule breaking (exit {status}):\n{tail}")


@dataclass
class Result:
    cfg: str
    expected: set[str]
    broken: set[str] = field(default_factory=set)
    rules: list[str] = field(default_factory=list)
    runs: int = 0
    seconds: float = 0.0
    error: str | None = None

    def problems(self) -> list[str]:
        if self.error:
            return [f"{self.cfg}: {self.error}"]
        unknown = self.expected - set(self.rules)
        if unknown:
            return [f"{self.cfg}: expected.tsv names rules the config does not check: {', '.join(sorted(unknown))}"]
        out = [f"{self.cfg}: {rule}: newly broken (not in expected.tsv)" for rule in sorted(self.broken - self.expected)]
        out += [f"{self.cfg}: {rule}: now holds but expected.tsv still lists it (remove it)"
                for rule in sorted(self.expected - self.broken)]
        return out


def run_tlc(java: str, jar: Path, workdir: Path, module: str, cfg_name: str, deadline: float) -> tuple[int, str]:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise CheckError("timed out")
    jtmp = workdir / f"{cfg_name}.tmp"
    jtmp.mkdir()
    cmd = [java, "-XX:+UseParallelGC", f"-Djava.io.tmpdir={jtmp}", "-cp", str(jar), "tlc2.TLC",
           "-tool", "-workers", "1", "-fp", "0", "-metadir", str(workdir / f"{cfg_name}.meta"),
           "-config", cfg_name, f"{module}.tla"]
    proc = subprocess.Popen(cmd, cwd=workdir, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            text=True, start_new_session=True)
    try:
        output, _ = proc.communicate(timeout=remaining)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.communicate()
        raise CheckError("timed out") from None
    return proc.returncode, output


def check_cfg(root: Path, cfg: str, module: str, expected: set[str], java: str, jar: Path,
              timeout: float) -> Result:
    result = Result(cfg, expected)
    started = time.monotonic()
    deadline = started + timeout
    path = root / cfg
    try:
        config = parse_config(path.read_text(encoding="utf-8"))
        result.rules = config.rules()
        if not (path.parent / f"{module}.tla").is_file():
            raise CheckError(f"module {module}.tla not found next to the config")
        with tempfile.TemporaryDirectory(prefix="statecharts-tlc-") as tmp:
            workdir = Path(tmp)
            for spec in path.parent.glob("*.tla"):
                shutil.copy(spec, workdir)
            invariants, check_deadlock = list(config.invariants), config.check_deadlock
            while True:
                result.runs += 1
                name = f"run{result.runs}.cfg"
                (workdir / name).write_text(config.render(invariants, check_deadlock), encoding="utf-8")
                status, output = run_tlc(java, jar, workdir, module, name, deadline)
                broke = classify(status, output, invariants, check_deadlock)
                if broke is None:
                    break
                result.broken.add(broke)
                if broke == DEADLOCK:
                    check_deadlock = False
                else:
                    invariants.remove(broke)
    except (CheckError, OSError) as error:
        result.error = f"{error} (limit {timeout:g} s per config)" if str(error) == "timed out" else str(error)
    result.seconds = time.monotonic() - started
    return result


def read_expected(path: Path) -> dict[str, tuple[str, set[str]]]:
    rows: dict[str, tuple[str, set[str]]] = {}
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) != 3 or not fields[0] or not fields[1]:
            raise CheckError(f"{path.name}:{number}: want 3 tab-separated fields: cfg, module, rules")
        rules = set() if fields[2].strip() in ("", "-") else {r.strip() for r in fields[2].split(",")}
        if "" in rules or fields[0] in rules:
            raise CheckError(f"{path.name}:{number}: bad rule list {fields[2]!r}")
        if fields[0] in rows:
            raise CheckError(f"{path.name}:{number}: {fields[0]} listed twice")
        rows[fields[0]] = (fields[1], rules)
    return rows


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--java", default=os.environ.get("JAVA", "java"))
    parser.add_argument("--jar", default=os.environ.get("TLA2TOOLS"), help="tla2tools.jar 1.7.4 (env TLA2TOOLS)")
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument("--timeout", type=float, default=300.0, help="seconds per config")
    args = parser.parse_args(argv)
    if not args.jar or not Path(args.jar).is_file():
        parser.error("tla2tools.jar not found: pass --jar or set TLA2TOOLS")
    root = args.root.resolve()
    started = time.monotonic()
    try:
        expected = read_expected(root / "expected.tsv")
    except (CheckError, OSError) as error:
        print(f"FAIL {error}", file=sys.stderr)
        return 1
    on_disk = {p.relative_to(root).as_posix() for p in root.rglob("*.cfg")}
    problems = [f"{cfg}: config not listed in expected.tsv" for cfg in sorted(on_disk - set(expected))]
    problems += [f"{cfg}: listed in expected.tsv but the file is gone" for cfg in sorted(set(expected) - on_disk)]
    results = []
    for cfg in sorted(set(expected) & on_disk):
        module, rules = expected[cfg]
        result = check_cfg(root, cfg, module, rules, args.java, Path(args.jar).resolve(), args.timeout)
        status = "ok" if not result.problems() else "FAIL"
        print(f"{status:4} {cfg} runs={result.runs} {result.seconds:.1f}s broken={','.join(sorted(result.broken)) or '-'}",
              flush=True)
        results.append(result)
        problems += result.problems()
    print("\n| cfg | expected to break | broken | seconds |\n|---|---|---|---|")
    for r in results:
        actual = "ERROR" if r.error else (", ".join(sorted(r.broken)) or "-")
        print(f"| {r.cfg} | {', '.join(sorted(r.expected)) or '-'} | {actual} | {r.seconds:.1f} |")
    print(f"\ntotal {time.monotonic() - started:.1f}s, {len(results)} configs")
    for problem in problems:
        print(f"FAIL {problem}", file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
