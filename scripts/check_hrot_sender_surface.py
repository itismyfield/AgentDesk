#!/usr/bin/env python3
"""Fail unless the operator route is the transition cleanup's only production sender: it alone looks
up the port, builds an operation, Apply included, sends it and consumes the reply; `request_as`
stays in the port. Test files and `#[cfg(test)]` items are left out."""
import functools
import re
import sys
from pathlib import Path

from check_o_shadow_write_zero import TOKEN, blank, brace_end

ROOT = Path(__file__).resolve().parent.parent
PORT = "src/services/tui_o/writer/transition_port.rs"
ROUTE = "src/server/routes/o_transition.rs"
HOST = "src/services/tui_o/writer/host/transition.rs"
WRITER = "src/services/tui_o/writer/"
# Outside the writer only the route and the context it sends name the port's module.
IMPORTERS = {ROUTE, "src/services/discord/health/o_transition.rs"}
CFG_TEST = re.compile(r"#\[cfg\((?:test|all\(test[^\]]*)\)\]")
ATTR = re.compile(r"\s*#\[[^\]]*\]")
BUILT = re.compile(r"Operation::(Inspect|Apply)\s*\(")
READINESS = re.compile(r"tui_o::writer::host\b[^;]*\b(?:Readiness|process_readiness)\b")
# A port send in either call form: a method call, or a path to the method however it is named.
SEND = re.compile(r"\.\s*request(?P<method_as>_as)?\s*\(|::\s*request(?P<path_as>_as)?\b")


def paren_end(code, start):
    """Index of the parenthesis closing the one at `start`, or the end of `code`."""
    depth = 0
    for index in range(start, len(code)):
        depth += {"(": 1, ")": -1}.get(code[index], 0)
        if depth == 0:
            return index
    return len(code)


@functools.lru_cache(maxsize=None)
def production(source):
    """`source` with comments, literals and `#[cfg(test)]` items blanked; empty for a test file."""
    code = TOKEN.sub(lambda m: blank(m.group(0)), source)
    if re.match(r"\s*#!\[cfg\((?:test|all\(test)", code):
        return ""
    for gate in reversed(list(CFG_TEST.finditer(code))):
        start = gate.end()
        while attr := ATTR.match(code, start):
            start = attr.end()
        brace, semi = code.find("{", start), code.find(";", start)
        end = brace_end(code, brace) + 1 if brace != -1 and (semi == -1 or brace < semi) else semi + 1
        code = code[:gate.start()] + blank(code[gate.start():end]) + code[end:]
    return code


def file_errors(path, code):
    errors = []
    names_port = "transition_port" in code or "TransitionPort" in code
    if names_port and not path.startswith(WRITER) and path not in IMPORTERS:
        errors.append("names the transition port outside the writer and its route")
    if names_port and re.search(r"\bOperation\s+as\b", code):
        errors.append("renames the transition operation")
    sends = list(SEND.finditer(code))
    if any(send["method_as"] or send["path_as"] for send in sends) and path != PORT:
        errors.append("calls request_as outside the port")
    if (path.startswith(WRITER) or names_port) and path not in (ROUTE, PORT) and sends:
        errors.append("sends on a transition port outside the route")
    if re.search(r"(?<!fn )\bwith_transition_reply\s*\(", code) and path != ROUTE:
        errors.append("consumes a transition reply outside the route")
    if re.search(r"\bTransitionPort::new\s*\(", code) and path != HOST:
        errors.append("opens a transition port outside the host")
    if READINESS.search(code) and re.search(r"\.transition\s*\(", code) and path != ROUTE:
        errors.append("looks up a transition port outside the route")
    for built in BUILT.finditer(code):
        # A match arm or or-pattern destructures an operation rather than building one.
        after = code[paren_end(code, built.end() - 1) + 1:].lstrip()
        if after.startswith(("=>", "|")):
            continue
        if path != ROUTE:
            errors.append(f"builds Operation::{built.group(1)} outside the route")
    if path == ROUTE and len(sends) != 1:
        errors.append("the route must send exactly once")
    return [f"{path}: {error}" for error in errors]


def errors(sources):
    found = []
    for path, source in sorted(sources.items()):
        name = path.rsplit("/", 1)[-1]
        if name.endswith("tests.rs") or name == "test_support.rs" or "/tests/" in path:
            continue
        found.extend(file_errors(path, production(source)))
    if ROUTE not in sources:
        found.append(f"{ROUTE}: missing")
    return found


def tree():
    return {str(p.relative_to(ROOT)): p.read_text() for p in (ROOT / "src").rglob("*.rs")}


def main():
    found = errors(tree())
    print("\n".join(found) if found else "hrot sender surface: PASS")
    return bool(found)


if __name__ == "__main__":
    sys.exit(main())
