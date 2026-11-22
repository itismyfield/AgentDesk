"""Map changed Rust files and named direct callers to library test names."""

import re
import subprocess

import rust_test_sources


TESTISH = re.compile(r"(?:^|::)(?:tests?|[a-z0-9_]+_tests?)(?:::|$)")


class MappingError(ValueError):
    pass


class GitQueryError(ValueError):
    pass


def git(repo, *args):
    """Git grep's no-match status is the only accepted nonzero result."""
    command = ["git", "-C", str(repo), *args]
    try:
        result = subprocess.run(command, capture_output=True, timeout=30)
        if result.returncode and (args[0], result.returncode) != ("grep", 1):
            raise subprocess.CalledProcessError(result.returncode, command)
        return result.stdout.decode("utf-8")
    except (OSError, subprocess.SubprocessError, UnicodeError) as exc:
        raise GitQueryError(args[0]) from exc


def own_modules(module):
    """A production module and its conventional sibling test module, or vice versa."""
    base = re.sub(r"_tests?$", "", module)
    return {module, base if base != module else module + "_tests"}


def matcher(changed, callers):
    roots = sorted(changed | {m for m in callers if re.search(r"_tests?$", m)})
    calls = sorted(callers - set(roots))
    alternatives = [re.escape(m) + "::" for m in roots]
    alternatives += [re.escape(m) + r"::(?:tests|[a-z0-9_]+_tests?)::" for m in calls]
    return re.compile("^(?:" + "|".join(alternatives or ["(?!)"]) + ")")


def _starts_item(tokens, index):
    while index >= 2 and [t.value for t in tokens[index - 2:index]] == [":", ":"]:
        index -= 2
        if index and tokens[index - 1].kind == "ident":
            index -= 1
    return not index or tokens[index - 1].value in {"{", "}", ";", "]"}


def _macro_or_feature(path, chains):
    if any("feature" in cfg for chain in chains for cfg in chain):
        return True
    tokens = rust_test_sources.rust_tokens(path.read_text(encoding="utf-8"), extended=True)
    depth, modules = 0, {0}
    for index, token in enumerate(tokens):
        if token.kind == "ident":
            if token.value == "macro":
                return True
            if index + 1 < len(tokens) and tokens[index + 1].value == "!":
                if token.value in {"macro_rules", "include", "paste"} or (
                    depth in modules and _starts_item(tokens, index)
                ):
                    return True
        elif token.kind == "punct":
            if token.value in {"(", "[", "{"}:
                depth += 1
                if token.value == "{" and index >= 2 and tokens[index - 2].value == "mod":
                    modules.add(depth)
            elif token.value in {")", "]", "}"}:
                modules.discard(depth)
                depth -= 1
        span = rust_test_sources.attribute_span(tokens, index)
        if span:
            start, end = span
            attr = [item.value for item in tokens[start + 1:end - 1] if item.kind == "ident"]
            if attr and (attr[0] in {"macro_export", "proc_macro", "proc_macro_attribute", "proc_macro_derive"}
                         or attr[0] in {"cfg", "cfg_attr"} and "feature" in attr[1:]):
                return True
    return False


def select_modules(repo, head, changed_paths, runnable_names):
    """Select names from one archive using its checkout's Rust module declarations."""
    try:
        libraries = rust_test_sources.workspace_lib_roots(repo)
        manifest, root = libraries.pop("agentdesk")
        inventory = rust_test_sources.walk(root, repo, manifest_dir=manifest.parent)
        workspace = [rust_test_sources.walk(lib, repo, manifest_dir=member.parent)
                     for member, lib in libraries.values()]
    except (KeyError, OSError, ValueError) as exc:
        raise MappingError("unmapped-module") from exc
    if any(inv.errors or inv.unreadable for inv in (inventory, *workspace)):
        raise MappingError("unmapped-module")
    workspace_paths = set().union(*(inv.reached for inv in workspace))

    changed, names = set(), set()
    for path in changed_paths:
        source = repo / path
        if source in workspace_paths:
            raise MappingError(f"workspace-dependency:{path!r}")
        modules = inventory.modules.get(source, set())
        if len(modules) != 1 or not next(iter(modules)):
            raise MappingError(f"unmapped-module:{path!r}")
        if _macro_or_feature(source, inventory.file_cfgs[source]):
            raise MappingError(f"macro-feature:{path!r}")
        parts = next(iter(modules))
        module = "::".join(parts)
        changed.update(own_modules(module))
        if not TESTISH.search(module):
            names.add(parts[-1])

    # A whole Rust name also finds grouped and relative imports. Homonyms may add tests.
    args = [arg for name in sorted(names) for arg in ("-e", name)]
    caller_paths = (
        {item.split(":", 1)[1] for item in git(repo, "grep", "-l", "-z", "-w", "-F",
                                              *args, head, "--", ":(glob)src/**/*.rs").split("\0") if item}
        if args else set()
    )
    for path in sorted(caller_paths):
        if repo / path in workspace_paths:
            raise MappingError(f"workspace-dependency:{path!r}")
        if repo / path not in inventory.modules and path != "src/main.rs" and not path.startswith("src/bin/"):
            raise MappingError(f"unmapped-module:{path!r}")
    callers = {
        own for path in caller_paths for module in inventory.modules.get(repo / path, ())
        if module for own in own_modules("::".join(module))
    }
    selects = matcher(changed, callers).match
    touched = caller_paths | set(changed_paths)
    return {name for name in runnable_names if selects(name) or any(
        site.rsplit(":", 1)[0] in touched for site, _ in inventory.tests.get(name, ())
    )}
