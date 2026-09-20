"""FILE-level ROOT/after-helper reference policy, not per-mutation lock ownership.
Qualified env writes, ROOT-bearing dynamic keys and explicit use paths only.
"""

from __future__ import annotations

import argparse
from bisect import bisect_right
from functools import lru_cache
import re
from pathlib import Path

try:
    from scripts.rust_lex import CODE, SPACE, LITERAL, StripState, lex_segments
except ModuleNotFoundError:
    from rust_lex import CODE, SPACE, LITERAL, StripState, lex_segments

ROOT = "__ROOT_LITERAL__"
CONFIG = "crate::config"
SUPPORT = "crate::services::turn_orchestrator::test_support"
OBS = "crate::services::observability"
CALL = re.compile(r"(?<![\w:])((?:[A-Za-z_]\w*::)*[A-Za-z_]\w*(?:\s*\.\s*lock)?)\s*\(")


def rust_text(source: str) -> str:
    state = StripState()
    out = []
    for line in source.splitlines(keepends=True):
        for kind, text in lex_segments(line, state):
            if kind in (CODE, SPACE):
                out.append(text)
            elif kind == LITERAL:
                out.append(ROOT if re.fullmatch(r'(?:r(#+)?)?"AGENTDESK_ROOT_DIR"\1?', text) else "__LITERAL__")
            else:
                out.append(" ")
    return "".join(out)


def block(text: str, pattern: str) -> str:
    match = re.search(pattern, text)
    if not match:
        raise ValueError(f"missing canonical definition: {pattern}")
    start = text.index("{", match.end())
    depth = 1
    for end in range(start + 1, len(text)):
        depth += (text[end] == "{") - (text[end] == "}")
        if depth == 0:
            return text[start + 1:end]
    raise ValueError("unclosed canonical definition")


def compact(text: str) -> str:
    return re.sub(r"\s+", "", text)


def function(text: str, name: str) -> str:
    return compact(block(text, rf"\bfn\s+{name}\s*\("))


@lru_cache(maxsize=8)
def module_contexts(path: Path, text: str) -> tuple[list[int], list[list[str]]]:
    parts = list(path.with_suffix("").parts[1:])
    if parts[-1] in ("mod", "lib", "main"):
        parts.pop()
    modules, stack = [], []
    offsets, contexts = [0], [["crate", *parts]]
    for token in re.finditer(r"\bmod\s+(\w+)\s*\{|[{}]", text):
        changed = False
        if token.group(1):
            modules.append(token.group(1))
            stack.append(True)
            changed = True
        elif token.group() == "{":
            stack.append(False)
        elif stack and stack.pop():
            modules.pop()
            changed = True
        if changed:
            offsets.append(token.end())
            contexts.append(["crate", *parts, *modules])
    return offsets, contexts


def module_path(path: Path, text: str, offset: int) -> list[str]:
    offsets, contexts = module_contexts(path, text)
    return contexts[bisect_right(offsets, offset) - 1]


def absolute(path: str, module: list[str]) -> str:
    parts = path.split("::")
    if parts[0] == "crate":
        return path
    base = module.copy()
    if parts[0] == "self":
        parts.pop(0)
    while parts and parts[0] == "super":
        parts.pop(0)
        if len(base) > 1:
            base.pop()
    return "::".join(base + parts)


def use_paths(tree: str, prefix: str = "") -> list[tuple[str, str]]:
    tokens = re.findall(r"\w+|::|[{},*]", tree)
    result = []

    def leaf(path: str) -> None:
        path = path.removesuffix("::self")
        result.append((path, path.split("::")[-1]))

    def visit(index: int, parent: str) -> int:
        path = parent
        while index < len(tokens):
            word = tokens[index]
            if word == "{":
                index = visit(index + 1, path)
                path = parent
            elif word == "}":
                if path != parent:
                    leaf(path)
                return index + 1
            elif word == ",":
                if path != parent:
                    leaf(path)
                path = parent
                index += 1
            elif word == "as":
                result.append((path, tokens[index + 1]))
                path = parent
                index += 2
            else:
                path += word
                index += 1
        if path != parent:
            leaf(path)
        return index

    visit(0, prefix)
    return result


def calls(path: Path, text: str) -> list[str]:
    imports = []
    for match in re.finditer(r"\buse\s+([^;]+);", text):
        module = module_path(path, text, match.start())
        for target, alias in use_paths(match.group(1)):
            imports.append((alias, absolute(target, module), module))
    aliases = {alias for alias, _, _ in imports}
    local_functions = set(re.findall(r"\bfn\s+(\w+)\s*\(", text))
    result = []
    for match in CALL.finditer(text):
        name = compact(match.group(1)).replace(".lock", "::lock")
        first, *rest = name.split("::")
        if not any(word in name for word in ("shared_test_env_lock", "set_path", "set_value", "set_agentdesk_root_for_test", "lock_test_env", "lock_env_then_runtime", "TestRuntimeRootGuard", "TEST_ENV_LOCK")) and first not in aliases:
            continue
        module = module_path(path, text, match.start())
        choices = [(target, scope) for alias, target, scope in imports
                   if alias == first and module[:len(scope)] == scope]
        if choices and first not in local_functions:
            target, _ = max(choices, key=lambda item: len(item[1]))
            result.append("::".join([target, *rest]))
        else:
            result.append(absolute(name, module))
    return result


def canonical_owners(sources: dict[Path, str]) -> set[str]:
    config = sources[Path("src/config.rs")]
    env = sources[Path("src/config/test_env.rs")]
    acquire = block(config, r"\bmod\s+test_env_lock\b")
    setter = block(env, r"\bimpl\s+TestEnvVarGuard\b")
    checks = [
        (function(config, "shared_test_env_lock"), "LOCK.get_or_init(||std::sync::Mutex::new(()))"),
        (function(acquire, "acquire_shared_test_env_lock"), "letmutex=super::shared_test_env_lock();"),
        (function(acquire, "acquire_shared_test_env_lock"), "mutex.lock()"),
        (function(setter, "set_path"), "letlock=super::test_env_lock::acquire_shared_test_env_lock();"),
        (function(setter, "set_path"), "_lock:Some(lock)"),
        (function(env, "set_agentdesk_root_for_test"), f"TestEnvVarGuard::set_path({ROOT},path)"),
        (compact(config), "usetest_env::{TestEnvVarGuard,TestRuntimeRootGuard,set_agentdesk_root_for_test}"),
    ]
    support = block(sources[Path("src/services/turn_orchestrator.rs")], r"\bmod\s+test_support\b")
    wrapper = block(support, r"\bimpl\s+SharedEnvLock\b")
    checks += [
        (function(wrapper, "lock"), "crate::config::shared_test_env_lock().lock()"),
        (compact(support), "staticTEST_ENV_LOCK:SharedEnvLock=SharedEnvLock;"),
        (function(support, "lock_test_env"), "TEST_ENV_LOCK.lock().unwrap_or_else("),
    ]
    if function(wrapper, "lock") != "crate::config::shared_test_env_lock().lock()":
        raise ValueError("SharedEnvLock must forward to the canonical mutex")
    if not re.fullmatch(r"TEST_ENV_LOCK\.lock\(\)\.unwrap_or_else\(\|(\w+)\|\1\.into_inner\(\)\)", function(support, "lock_test_env")):
        raise ValueError("lock_test_env must return the verified wrapper guard")
    if not all(expected in actual for actual, expected in checks):
        raise ValueError("canonical owner/turn_orchestrator forwarding contract changed")
    owners = {f"{CONFIG}::shared_test_env_lock", f"{CONFIG}::test_env_lock::acquire_shared_test_env_lock",
              f"{SUPPORT}::lock_test_env", f"{SUPPORT}::TEST_ENV_LOCK::lock"}
    for prefix in (CONFIG, f"{CONFIG}::test_env"):
        owners.update((f"{prefix}::TestEnvVarGuard::set_path", f"{prefix}::set_agentdesk_root_for_test"))
    runtime = block(env, r"\bimpl\s+TestRuntimeRootGuard\b")
    if "letenv=set_agentdesk_root_for_test(root.path());" not in function(runtime, "new") or "_env:env" not in function(runtime, "new"):
        raise ValueError("TestRuntimeRootGuard must retain the canonical ROOT setter")
    owners.update(f"{prefix}::TestRuntimeRootGuard::new" for prefix in (CONFIG, f"{CONFIG}::test_env"))
    obs_path = Path("src/services/observability/test_support.rs")
    if obs_path in sources:
        body = function(sources[obs_path], "lock_env_then_runtime")
        expected = "letenv=crate::config::test_env_lock::acquire_shared_test_env_lock();letruntime=test_runtime_lock();"
        exported = compact(sources[Path("src/services/observability/mod.rs")])
        if expected not in body or "_env:env" not in body or not re.search(r"usetest_support::\{[^}]*lock_env_then_runtime", exported):
            raise ValueError("observability env/runtime forwarding contract changed")
        owners.update((f"{OBS}::lock_env_then_runtime", f"{OBS}::test_support::lock_env_then_runtime"))
    return owners


def audit(root: Path) -> tuple[list[str], int, list[str]]:
    sources = {}
    for tree in ("src", "tests"):
        for path in sorted((root / tree).rglob("*.rs")):
            source = path.read_text()
            if any(word in source for word in ("AGENTDESK_ROOT_DIR", "shared_test_env_lock", "set_agentdesk_root_for_test", "set_var", "remove_var", "lock_env_then_runtime", "TestEnvVarGuard", "TestRuntimeRootGuard")):
                sources[path.relative_to(root)] = rust_text(source)
    try:
        owners = canonical_owners(sources)
    except (KeyError, ValueError) as error:
        return [f"UNKNOWN canonical source: {error}"], 0, []
    errors, dynamic = [], []
    candidates = 0
    for path, text in sources.items():
        constants = set(re.findall(rf"\bconst\s+(\w+)\s*:[^;=]+=\s*{ROOT}\s*;", text))
        constants.add("AGENTDESK_ROOT_DIR_ENV")
        keys = re.findall(r"\b(?:std::)?env::(?:set_var|remove_var)\s*\(\s*([^,)]*)", text)
        direct = any(key.strip() == ROOT or key.strip() in constants for key in keys)
        dynamic_key = any(key.strip() != "__LITERAL__" and key.strip() != ROOT and key.strip() not in constants for key in keys)
        root_evidence = ROOT in text or "AGENTDESK_ROOT_DIR_ENV" in text
        if dynamic_key and not root_evidence:
            dynamic.append(str(path))
        direct |= dynamic_key and root_evidence
        references = calls(path, text) if direct or "TestRuntimeRootGuard" in text or "TestEnvVarGuard" in text or "set_agentdesk_root_for_test" in text or "after_shared_test_env_lock" in text else []
        borrowed = "after_shared_test_env_lock" in text and any("after_shared_test_env_lock" in name for name in references)
        owning = any(name.endswith(("::set_agentdesk_root_for_test", "::TestEnvVarGuard::set_path", "::TestRuntimeRootGuard::new")) for name in references)
        if not (direct or borrowed or owning):
            continue
        candidates += 1
        if not owners.intersection(references):
            errors.append(f"UNKNOWN {path}: ROOT/after-lock candidate lacks a verified owner reference")
    return errors, candidates, dynamic


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    errors, count, dynamic = audit(args.root)
    print(f"ROOT file-reference policy: candidates={count}, unknown={len(errors)}; not an ownership proof")
    if dynamic:
        print(f"Outside discovery grammar: dynamic env keys in {len(dynamic)} files (not proved safe): " + ", ".join(dynamic))
    for error in errors:
        print(error)
    return bool(errors)


if __name__ == "__main__":
    raise SystemExit(main())
