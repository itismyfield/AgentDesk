#!/usr/bin/env python3
"""Keep `runtime_store::fsync_parent_dir` the only path-based directory fsync.

Windows cannot open a directory with `File::open`, so an inline copy fails on
every call there. Lexical scan, one function at a time (a function runs from
its `fn` header to the next): a read-only `File::open(..)` / `.open(..)` is a
directory sync when a sync call follows it in the same `;` statement, or when
it is bound with `let` and that binding is synced (`name.sync_all()`, or passed
to a `*sync*(..)` call) anywhere later in the function. Write-mode opens are
files (a directory cannot be opened for write) and `.join(..)` arguments name an
entry inside a directory, so both are skipped; that includes `options.open(..)`
when the function set `options.write(true)` in an earlier statement. A file
opened in one function and synced in another is not followed.

Raw sync APIs (`libc`/`nix`/`rustix` fsync, `FlushFileBuffers`) bypass the
helper entirely and are denied outside the reviewed `RAW_SYNC_REVIEWED` sites.
Only the `//` suffix of each line is ignored.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

OPEN = re.compile(r"(?:File::open|\.open)\(")
WRITE_MODE = re.compile(r"\.(?:write|append|create|create_new|truncate)\(\s*true\s*\)")
SYNC = re.compile(r"\w*sync\w*\(")
WRITE_BUILDER = re.compile(
    r"\b(\w+)\s*(?:\.\s*\w+\([^()]*\)\s*)*"
    r"\.\s*(?:write|append|create|create_new|truncate)\(\s*true\s*\)"
)
RECEIVER = re.compile(r"(\w+)\s*$")
BINDING = re.compile(r"\blet\s+(?:mut\s+)?(\w+)\s*(?::[^=;]*)?=(?!=)")
FN_HEADER = re.compile(r"\bfn\s+(\w+)")
RAW_SYNC = re.compile(
    r"\b(?:libc|nix|rustix)(?:::\w+)*::f(?:data)?sync\b|\bFlushFileBuffers\b"
)
CANONICAL = {Path("src/services/discord/runtime_store.rs"): 1}
# The restart-v2 backend syncs a parent it holds only by descriptor, so its
# fd-relative confinement cannot go through a path-based helper.
_PROTOCOL_V2_FS = Path("src/services/discord/restart_mode/protocol_v2/fs/unix.rs")
RAW_SYNC_REVIEWED = {
    (_PROTOCOL_V2_FS, "fsync_fd"): 1,
    (_PROTOCOL_V2_FS, "open_or_create_child"): 1,
}


def open_argument(text: str, start: int) -> str:
    depth = 1
    for index in range(start, len(text)):
        depth += {"(": 1, ")": -1}.get(text[index], 0)
        if depth == 0:
            return text[start:index]
    return text[start:]


def directory_opens(statement: str, write_builders: set[str]) -> list[int]:
    if WRITE_MODE.search(statement):
        return []
    opens = []
    for match in OPEN.finditer(statement):
        if ".join(" in open_argument(statement, match.end()):
            continue
        receiver = RECEIVER.search(statement, 0, match.start())
        if match.group().startswith(".") and receiver and receiver.group(1) in write_builders:
            continue
        opens.append(match.start())
    return opens


def strip_comments(text: str) -> str:
    return "\n".join(line.split("//", 1)[0] for line in text.splitlines())


def functions(code: str) -> list[tuple[int, str, str]]:
    """Split `code` into `(offset, name, body)` runs, one per `fn` header."""
    starts = [(0, "")] + [(match.start(), match.group(1)) for match in FN_HEADER.finditer(code)]
    ends = [start for start, _ in starts[1:]] + [len(code)]
    return [(start, name, code[start:end]) for (start, name), end in zip(starts, ends)]


def binding_synced(name: str, later: list[str]) -> bool:
    synced = re.compile(
        rf"\b{name}\s*\.\s*sync_(?:all|data)\s*\(|\w*sync\w*\([^;]*?(?<![\w.]){name}\b"
    )
    return any(synced.search(statement) for statement in later)


def directory_syncs(text: str) -> list[int]:
    code = strip_comments(text)
    lines: list[int] = []
    for base, _, body in functions(code):
        statements, offset = [], base
        for statement in body.split(";"):
            statements.append((offset, statement))
            offset += len(statement) + 1
        write_builders: set[str] = set()
        for index, (start, statement) in enumerate(statements):
            later = [following for _, following in statements[index + 1 :]]
            for opened in directory_opens(statement, write_builders):
                bindings = [m for m in BINDING.finditer(statement) if m.end() <= opened]
                if SYNC.search(statement[opened:]) or (
                    bindings and binding_synced(bindings[-1].group(1), later)
                ):
                    lines.append(code.count("\n", 0, start + opened) + 1)
            write_builders.update(match.group(1) for match in WRITE_BUILDER.finditer(statement))
    return lines


def raw_syncs(text: str) -> list[tuple[str, int]]:
    code = strip_comments(text)
    return [
        (name, code.count("\n", 0, base + match.start()) + 1)
        for base, name, body in functions(code)
        for match in RAW_SYNC.finditer(body)
    ]


def audit(root: Path) -> list[str]:
    findings: list[str] = []
    for path in sorted((root / "src").rglob("*.rs")):
        relative = path.relative_to(root)
        text = path.read_text(encoding="utf-8")
        hits = directory_syncs(text)
        expected = CANONICAL.get(relative, 0)
        if len(hits) != expected:
            where = ", ".join(f"{relative}:{number}" for number in hits) or str(relative)
            findings.append(
                f"{where}: expected {expected} directory fsync opens, found {len(hits)}; "
                "call runtime_store::fsync_parent_dir instead"
            )
        by_function: dict[str, list[int]] = {}
        for name, number in raw_syncs(text):
            by_function.setdefault(name, []).append(number)
        for name, numbers in by_function.items():
            expected = RAW_SYNC_REVIEWED.get((relative, name), 0)
            if len(numbers) != expected:
                where = ", ".join(f"{relative}:{number}" for number in numbers)
                findings.append(
                    f"{where}: expected {expected} raw sync calls in fn {name or '<top level>'}, "
                    f"found {len(numbers)}; call runtime_store::fsync_parent_dir instead"
                )
    for relative in CANONICAL:
        if not (root / relative).is_file():
            findings.append(f"{relative}: canonical fsync_parent_dir owner is missing")
    return findings


def main() -> int:
    findings = audit(Path.cwd())
    if findings:
        print("directory fsync single-path audit failed:", file=sys.stderr)
        for finding in findings:
            print(f"  {finding}", file=sys.stderr)
        return 1
    print("directory fsync single-path audit passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
