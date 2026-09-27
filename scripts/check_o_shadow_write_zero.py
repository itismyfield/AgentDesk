#!/usr/bin/env python3
"""Write-zero census for the TUI output shadow (src/services/tui_o/shadow/).

Exit 1 when non-test code names Discord HTTP, tmux mutation, process spawn,
mailbox/queue/inflight/checkpoint writers, database handles or deletes, or
writes files outside root.rs, the only owner of `o_shadow`. Test code must sit
in inline `#[cfg(test)] mod` blocks, which are skipped. Self-test runs first.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SHADOW_DIR = Path("src/services/tui_o/shadow")
WRITE_OWNER = SHADOW_DIR / "root.rs"
FORBIDDEN = [
    ("discord http", r"\bserenity::http\b|\bHttp\b|\.http\b|\b(Create|Edit)Message\b"
                     r"|\b(send|edit|delete)_message\b|\breqwest\b|\bSharedData\b"),
    ("tmux mutation", r"\b(send_keys|(paste|load)_buffer|kill_(session|server|pane))\b"
                      r"|\brespawn_pane\b|send-keys|(paste|load)-buffer|kill-(session|server|pane)"),
    ("process spawn", r"\bCommand::new\b|\b(std|tokio)::process\b"),
    ("mailbox/queue/inflight/checkpoint", r"(?i)mailbox|inflight|\bsave_channel_queue\b"
                                          r"|\bqueue_io\b|\badvance_last_message_checkpoint\b"),
    ("database", r"\bsqlx\b|\bPgPool\b|\bmessage_outbox\b"),
    ("delete", r"\bremove_(file|dir|dir_all)\b"),
]
OUTSIDE_ROOT = ("write outside ShadowRoot",
                r"\bOpenOptions\b|\bFile::(create|options)\b|\bset_len\b|\bsymlink\b"
                r"|\bfs::(write|rename|copy|create_dir(_all)?|set_permissions|hard_link)\b")
TOKEN = re.compile(r'//[^\n]*|/\*.*?\*/|r(#*)".*?"\1|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\\n])\'', re.S)
TEST_MOD = re.compile(r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")


def scan_text(name: str, text: str, writes_allowed: bool) -> list[str]:
    """Blank comments (and, for brace matching, literals) without moving line numbers."""
    def blank(match: re.Match, keep_literal: bool) -> str:
        token = match.group(0)
        return token if keep_literal and not token.startswith("/") else re.sub(r"[^\n]", " ", token)
    code = TOKEN.sub(lambda m: blank(m, True), text)
    bare = TOKEN.sub(lambda m: blank(m, False), text)
    for match in TEST_MOD.finditer(bare):
        depth, end = 0, match.end() - 1
        while end < len(bare):
            depth += {"{": 1, "}": -1}.get(bare[end], 0)
            if depth == 0:
                break
            end += 1
        body = re.sub(r"[^\n]", " ", code[match.start():end + 1])
        code = code[:match.start()] + body + code[end + 1:]
    rules = FORBIDDEN + ([] if writes_allowed else [OUTSIDE_ROOT])
    return [f"{name}:{number}: {label}: {line.strip()}"
            for number, line in enumerate(code.split("\n"), 1)
            for label, pattern in rules if re.search(pattern, line)]


def self_test() -> list[str]:
    cases = [
        ("fn f(ctx: &Ctx) { ctx.http.say(1); }", False, 1),
        ('fn f() { Command::new("tmux").arg("send-keys"); }', False, 2),
        ("fn f() { mailbox_try_enqueue(); }\nfn g() { save_channel_queue(); }", False, 2),
        ("fn f() { std::fs::write(p, b); }", False, 1),
        ("fn f() { std::fs::create_dir_all(p); OpenOptions::new(); }", True, 0),
        ("fn f() { std::fs::remove_file(p); }", True, 1),
        ("// Http and mailbox appear only in comments\nfn f() {}", False, 0),
        ('#[cfg(test)]\nmod tests {\n    fn t() { std::fs::write(p, "}"); }\n}\n', False, 0),
        ("#[cfg(test)]\nmod tests {\n}\nfn f() { std::fs::write(p, b); }\n", False, 1),
    ]
    return [f"self-test case {index}: expected {expected}, got {hits}"
            for index, (text, writes_allowed, expected) in enumerate(cases)
            if len(hits := scan_text("case.rs", text, writes_allowed)) != expected]


def main() -> int:
    failures = self_test()
    for path in sorted((REPO_ROOT / SHADOW_DIR).rglob("*.rs")):
        relative = path.relative_to(REPO_ROOT)
        failures += scan_text(str(relative), path.read_text("utf-8"), relative == WRITE_OWNER)
    if not (REPO_ROOT / WRITE_OWNER).is_file():
        failures.append(f"{WRITE_OWNER} missing")
    print("\n".join(failures + [f"o-shadow write-zero: {len(failures)} failure(s)"]))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
