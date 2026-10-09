#!/usr/bin/env python3
"""Fail when non-test shadow code can reach an effect outside o_shadow; see CRATE_ALLOW, ROOT_ONLY, DENIED, CLI_DENIED."""

import re
import sys
from itertools import accumulate
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SHADOW_DIR = Path("src/services/tui_o/shadow")
SHADOW_MODULE = "crate::services::tui_o::shadow"
# Crate items outside the shadow whose whole call tree was audited as mutation-free.
CRATE_ALLOW = {
    "crate::services::agent_protocol::RuntimeHandoffKind",
    "crate::services::discord::DISCORD_MSG_LIMIT",
    "crate::services::discord::formatting::split_for_shadow",
    "crate::services::provider::ProviderKind::Claude",
    "crate::services::provider::ProviderKind::Codex",
    "crate::services::tui_o::repost::config::RepostConfig",
    "crate::services::tui_o::turn_mode::TurnConfig",
    "crate::services::tui_prompt_dedupe::TuiRuntimeBinding",
    "crate::services::tui_prompt_dedupe::peek_tmux_runtime_binding",
    "crate::services::tui_turn_state::envelope_is_turn_end_terminator",
}
EXTERNAL_ROOTS = {"std", "core", "alloc", "chrono", "serde", "serde_json", "sha2", "hex", "tokio", "tracing", "libc"}
PRIMITIVE = re.compile(r"[iu](8|16|32|64|128|size)|f32|f64|bool|char|str")
READ_ONLY = {"std::fs::File", "std::fs::File::open", "std::fs::Metadata", "std::fs::metadata", "std::fs::symlink_metadata",
             "std::os::unix::fs::MetadataExt", "std::os::unix::fs::MetadataExt::nlink"}
# Writer items allowed only in root.rs; their constructors must sit inside ROOT_WRITERS functions.
ROOT_ONLY = ("std::fs::OpenOptions", "std::fs::DirBuilder", "std::os::unix::fs::OpenOptionsExt", "libc::O_NOFOLLOW", "std::io::Write")
ROOT_WRITERS = {"under", "open", "claim_report_attempt"}
DENIED = ("std::net", "std::process", "std::env::set_var", "std::env::remove_var", "std::io::Write", "std::fs", "std::os", "libc",
          "tokio::fs", "tokio::net", "tokio::process", "tokio::io")
TOKENS = [
    ("discord http", r"\.http\b|\b(send|edit|delete)_message\b|\bSharedData\b"),
    ("tmux mutation", r"send-keys|(paste|load)-buffer|kill-(session|server|pane)"),
    ("relay state", r"(?i)mailbox|inflight|\bsave_channel_queue\b|\badvance_last_message_checkpoint\b"),
    ("unchecked escape", r"\bunsafe\b|\bextern\b|#\[path\b|\binclude!|::\s*$|^\s*::|\bset_(len|permissions|modified|times)\b"),
]
TOKEN = re.compile(r'//[^\n]*|/\*.*?\*/|r(#*)".*?"\1|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\\n])\'', re.S)
TEST_MOD = re.compile(r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")
USE = re.compile(r"\buse\s+([^;]+);")
USE_GROUP = re.compile(r"((?:\w+::)*)\{([^{}]*)\}")
PATH = re.compile(r"(?<![\w:])(?:::)?(?:[A-Za-z_]\w*::)+[A-Za-z_*]\w*")

# The report CLI reads the server config; server loaders tighten a secret-bearing file's mode.
CLI_FILE = Path("src/cli/o_shadow.rs")
CLI_DENIED = re.compile(r"\b(load_from_path|load_graceful|save_to_path|audit_or_harden\w*|set_permissions|config::load)\b")

blank = lambda text: re.sub(r"[^\n]", " ", text)  # noqa: E731
def brace_end(text: str, open_at: int) -> int:
    depths = accumulate({"{": 1, "}": -1}.get(char, 0) for char in text[open_at:])
    return open_at + next((index for index, depth in enumerate(depths) if depth == 0), len(text) - open_at - 1)

def expand(tree: str) -> list[tuple[str, str]]:
    """(path, local name) pairs of one `use` tree, flattened innermost braces first."""
    tree = re.sub(r"\s*::\s*", "::", re.sub(r"\s+", " ", tree.strip()))
    while "{" in tree:
        tree = USE_GROUP.sub(lambda m: ", ".join(m.group(1) + part.strip()
                                                 for part in m.group(2).split(",") if part.strip()), tree)
    pairs = [(item.strip().partition(" as ")[0].removesuffix("::self"), item.strip().partition(" as ")[2])
             for item in tree.split(",") if item.strip()]
    return [(path, alias or path.split("::")[-1]) for path, alias in pairs]

def resolve(path: str, module: str, aliases: dict[str, str]) -> str:
    segments = path.split("::")
    if segments[0] in aliases:
        segments = aliases[segments[0]].split("::") + segments[1:]
    base = module.split("::") if segments[0] in ("self", "super") else []
    while segments[:1] == ["super"]:
        base, segments = base[:-1], segments[1:]
    return "::".join(base + (segments[1:] if segments[:1] == ["self"] else segments))

def verdict(path: str, in_root: bool) -> str | None:
    # Only the root owner may flush its receipt directory entries through the canonical helper.
    if in_root and path == "crate::services::discord::runtime_store::fsync_parent_dir":
        return None
    if path == SHADOW_MODULE or path.startswith(SHADOW_MODULE + "::"):
        return None
    root = path.split("::")[0]
    if root == "crate":
        allowed = any(path == item or path.startswith(item + "::") for item in CRATE_ALLOW)
        return None if allowed else "unaudited crate item"
    if root not in EXTERNAL_ROOTS:
        return "unknown crate root"
    path = re.sub(r"^(core|alloc)::", "std::", path)
    if path in READ_ONLY or (in_root and any(path == p or path.startswith(p + "::") for p in ROOT_ONLY)):
        return None
    denied = any(path == prefix or path.startswith(prefix + "::") for prefix in DENIED)
    return "effect-capable path" if denied or path.endswith("::*") else None

def scan_text(name: str, text: str, module: str, in_root: bool) -> list[str]:
    code = TOKEN.sub(lambda m: m.group(0) if not m.group(0).startswith("/") else blank(m.group(0)), text)
    bare = TOKEN.sub(lambda m: blank(m.group(0)), text)
    for match in TEST_MOD.finditer(bare):
        end = brace_end(bare, match.end() - 1) + 1
        code = code[:match.start()] + blank(code[match.start():end]) + code[end:]
        bare = bare[:match.start()] + blank(bare[match.start():end]) + bare[end:]
    line = lambda offset: bare.count("\n", 0, offset) + 1  # noqa: E731
    hits = [f"{name}:{number}: {label}: {text_line.strip()}"
            for number, text_line in enumerate(code.split("\n"), 1)
            for label, pattern in TOKENS if re.search(pattern, text_line)]
    aliases = {mod: f"{module}::{mod}" for mod in re.findall(r"\bmod\s+(\w+)", bare)}
    uses = []
    for match in USE.finditer(bare):
        for path, local in expand(match.group(1)):
            aliases[local] = resolve(path, module, aliases)
            uses.append((match.start(), aliases[local]))
    body = re.sub(r"[ \t]*::[ \t]*", "::", USE.sub(lambda m: blank(m.group(0)), bare))
    for match in PATH.finditer(body):
        path = match.group(0).removeprefix("::")
        if PRIMITIVE.fullmatch(first := path.split("::")[0]) or first[0].isupper() and first not in aliases:
            continue
        uses.append((match.start(), resolve(path, module, aliases)))
    hits += [f"{name}:{line(offset)}: {reason}: {path}"
             for offset, path in uses if (reason := verdict(path, in_root))]
    if in_root:
        writers = [(m.start(), brace_end(bare, m.end() - 1))
                   for m in re.finditer(r"\bfn\s+(\w+)[^{;]*\{", bare) if m.group(1) in ROOT_WRITERS]
        hits += [f"{name}:{line(m.start())}: writer outside {sorted(ROOT_WRITERS)}: {m.group(0)}"
                 for m in re.finditer(r"\b(OpenOptions|DirBuilder)::new\b", bare)
                 if not any(start < m.start() < end for start, end in writers)]
    return hits

def scan_cli(name: str, text: str) -> list[str]:
    code = TOKEN.sub(lambda m: blank(m.group(0)), text)
    for match in TEST_MOD.finditer(code):
        end = brace_end(code, match.end() - 1) + 1
        code = code[:match.start()] + blank(code[match.start():end]) + code[end:]
    return [f"{name}:{number}: config mode change: {line.strip()}"
            for number, line in enumerate(code.split("\n"), 1) if CLI_DENIED.search(line)]

BAD = [
    ("fn f() { crate::services::tui_o::turn_mode::confirm(1); }", False),
    ("fn f() { crate::services::discord::runtime_store::fsync_parent_dir(p); }", False),
    ("fn f(ctx: &Ctx) { ctx.http.say(1); }", False),
    ("fn f() { use std::fs::{write}; let _ = write(p, b); }", False),
    ("fn f() { let _ = crate::services::platform::tmux::send_literal(a, b); }", False),
    ("fn f() { let _ = crate::services::tmux_common::write_tmux_channel_binding(a, 1); }", False),
    ("fn f() { let _ = std::fs::write(p, b); }", True),
    ("fn f() { use std::{process::Command as Proc}; let _ = Proc::new(a).spawn(); }", False),
    ("async fn f() { let _ = crate::db::cancel_tombstones::prune_expired_cancel_tombstones(p).await; }", False),
    ("fn f() { let s = std::net::TcpStream::connect(a); }", False),
    ("use crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority;", False),
    ("fn f() { let _ = reqwest::get(u); }", False),
    ("fn f() { let _ = File::create(p); }\nuse std::fs::File;", False),
    ("use std::fs::OpenOptions;\nfn append() { let _ = OpenOptions::new(); }", True),
    ("fn f() { save_channel_queue(); }", False),
    ("fn f() { let _ = ::std::fs::write(p, b); }", False),
    ("fn f() { let _ = std :: fs :: write(p, b); }", False),
    ('include!("../../elsewhere.rs");', False),
    ("#[cfg(test)]\nmod tests {\n}\nfn f() { std::fs::write(p, b); }\n", False),
]
GOOD = [
    ("struct C { turn: crate::services::tui_o::turn_mode::TurnConfig }", False),
    ("fn claim_report_attempt() { let _ = std::fs::OpenOptions::new(); crate::services::discord::runtime_store::fsync_parent_dir(p); }", True),
    ("use std::fs::OpenOptions;\nfn open() { let _ = OpenOptions::new(); }", True),
    ("use crate::services::tui_prompt_dedupe::{TuiRuntimeBinding, peek_tmux_runtime_binding as peek};\n"
     "use super::root::file_identity;\nuse std::io;\nfn f() { let _ = std::fs::File::open(p); let _ = io::Error::other(e); }", False),
    ("// Http, mailbox and std::fs::write appear only in comments\nfn f() {}", False),
    ('#[cfg(test)]\nmod tests {\n    fn t() { std::fs::write(p, "}"); }\n}\n', False),
]

CLI_BAD = ["fn f() { let _ = crate::config::load_from_path(p); }", "fn f() { let _ = config::load(); }",
           "fn f() { std::fs::set_permissions(p, m).ok(); }"]
CLI_GOOD = ["fn f() { let c: Config = serde_yaml::from_slice(&b)?; }\n"
            "#[cfg(test)]\nmod tests {\n    fn t() { std::fs::set_permissions(p, m).ok(); }\n}\n"]

def self_test() -> list[str]:
    run = lambda text, in_root: scan_text("case.rs", text, f"{SHADOW_MODULE}::case", in_root)  # noqa: E731
    return ([f"self-test missed: {text!r}" for text, in_root in BAD if not run(text, in_root)]
            + [f"self-test flagged: {hits}" for text, in_root in GOOD if (hits := run(text, in_root))]
            + [f"self-test missed: {text!r}" for text in CLI_BAD if not scan_cli("case.rs", text)]
            + [f"self-test flagged: {hits}" for text in CLI_GOOD if (hits := scan_cli("case.rs", text))])

def main() -> int:
    failures = self_test()
    for path in sorted((REPO_ROOT / SHADOW_DIR).rglob("*.rs")):
        relative = path.relative_to(REPO_ROOT)
        parts = list(relative.relative_to(SHADOW_DIR).with_suffix("").parts)
        module = "::".join([SHADOW_MODULE] + (parts[:-1] if parts[-1] == "mod" else parts))
        failures += scan_text(str(relative), path.read_text("utf-8"), module, relative == SHADOW_DIR / "root.rs")
    failures += scan_cli(str(CLI_FILE), (REPO_ROOT / CLI_FILE).read_text("utf-8"))
    print("\n".join(failures + [f"o-shadow write-zero: {len(failures)} failure(s)"]))
    return 1 if failures else 0

if __name__ == "__main__":
    sys.exit(main())
