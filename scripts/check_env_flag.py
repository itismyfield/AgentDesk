#!/usr/bin/env python3
"""Reject growth of production env reads compared directly with boolean tokens."""
from __future__ import annotations

import argparse
from collections import Counter
import os
from pathlib import Path
import re
import subprocess

from generate_inventory_docs import cfg_requires_test, is_test_file, scan_balanced
from rust_lex import StripState, cfg_item_end, strip_line

ROOT = Path(__file__).resolve().parents[1]
READ = re.compile(r'\b(?:std\s*::\s*)?env\s*::\s*var(?:_os)?\s*\(')
LITERAL = re.compile(r"""//[^\n]*|/\*.*?\*/|(?:br|r)(?P<hashes>\#*)".*?"(?P=hashes)|b?"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])'""", re.S)
BOOL_VALUES = {'0', '1', 'true', 'false', 'yes', 'no', 'on', 'off'}
CFG = re.compile(r'#\[\s*(?:cfg\s*\((.*?)\)|(?:[\w:]+::)?test(?:\([^]]*\))?)\s*\]', re.S)
FUNCTION = re.compile(r'\bfn\s+(\w+)[^{;]*\{')
BIND = re.compile(r'\blet\s+(?:mut\s+)?(?:(?:Ok|Some)\s*\(\s*(?:mut\s+)?(\w+)\s*\)|(\w+))\s*(?::[^=;]+)?=')


def production_code(text):
    # Keep bool literal positions while the shared lexer removes prose and other strings.
    def mark(match):
        literal = match[0]
        if literal.startswith(('//', '/*', "'")):
            return literal
        content = literal[literal.index('"') + 1:literal.rindex('"')]
        if content.strip().lower() not in BOOL_VALUES:
            return literal
        return 'B' + ''.join('\n' if c == '\n' else ' ' for c in literal[1:])
    marked = LITERAL.sub(mark, text)
    state = StripState()
    code = '\n'.join(strip_line(line, state).ljust(len(line)) for line in marked.split('\n'))
    blanked = list(code)
    for match in CFG.finditer(code):
        if match[1] is not None and not cfg_requires_test(match[1]):
            continue
        end = cfg_item_end(code, match.end(), scan_balanced)
        blanked[match.start():end] = ['\n' if c == '\n' else ' ' for c in code[match.start():end]]
    return ''.join(blanked)


def read_pattern(code):
    aliases = {"std": "std"}
    patterns = [READ.pattern]

    def visit(prefix, item):
        item = item.strip()
        if not item:
            return
        if "{" in item:
            head, tail = item.split("{", 1)
            body = tail.rsplit("}", 1)[0]
            depth, start = 0, 0
            for index, char in enumerate(body + ","):
                depth += (char == "{") - (char == "}")
                if char == "," and depth == 0:
                    visit(prefix + head, body[start:index])
                    start = index + 1
            return
        parts = re.split(r"\s+as\s+", item)
        path = re.sub(r"\s+", "", prefix + parts[0]).lstrip(":")
        if path.endswith("::self"):
            path = path[:-6]
        first, *rest = path.split("::")
        path = "::".join([aliases.get(first, first), *rest])
        name = parts[-1].strip() if len(parts) > 1 else path.split("::")[-1]
        if path in ("std", "std::env"):
            aliases[name] = path
            suffix = r"\s*::\s*env" if path == "std" else ""
            patterns.append(r"\b" + re.escape(name) + suffix + r"\s*::\s*var(?:_os)?\s*\(")
        elif path in ("std::env::var", "std::env::var_os"):
            patterns.append(r"(?<![\w:.])" + re.escape(name) + r"\s*\(")

    for imported in re.finditer(r"\buse\s+([^;]+);", code):
        visit("", imported[1])
    return re.compile("|".join(patterns))


def sites(text):
    if not READ.search(text) and not re.search(r'\buse\s+(?:::)?std[^;]*\benv\b', text):
        return []
    code = production_code(text)
    reads_pattern = read_pattern(code)
    bodies = []
    outside = list(code)
    for match in FUNCTION.finditer(code):
        start = match.end() - 1
        end = scan_balanced(code, start, '{', '}')[1]
        bodies.append((match[1], start, code[start:end]))
        outside[match.start():end] = ['\n' if c == '\n' else ' ' for c in code[match.start():end]]
    bodies.append(('<module>', 0, ''.join(outside)))
    found = {}
    for function, offset, body in bodies:
        tainted = {}
        cursor = 0
        for statement in body.split(';'):
            reads = set()
            for match in reads_pattern.finditer(statement):
                pos = offset + cursor + match.start()
                arg_start = offset + cursor + match.end() - 1
                _, arg_end = scan_balanced(code, arg_start)
                argument = re.sub(r'\s+', '', text[arg_start + 1:arg_end - 1])
                reads.add((function, argument, text.count('\n', 0, pos) + 1, pos))
            for name in re.findall(r'\b\w+\b', statement):
                reads.update(tainted.get(name, ()))
            comparison = (re.search(r'\bB\b', statement) and re.search(r'==|!=|\bmatches\s*!|\bmatch\b|\.\s*(?:eq(?:_ignore_ascii_case)?|contains)\s*\(', statement)) or re.search(r'\.\s*parse\s*::\s*<\s*bool\s*>', statement)
            if reads and comparison:
                for site in reads:
                    found[site[3]] = site[:3]
            for binding in BIND.finditer(statement):
                name = binding[1] or binding[2]
                if reads:
                    tainted[name] = reads.copy()
                else:
                    tainted.pop(name, None)
            cursor += len(statement) + 1
    return sorted(found.values())


def collect(files):
    found = Counter()
    locations = {}
    for path, text in files:
        if is_test_file(Path(path)) or 'tests' in Path(path).parts:
            continue
        try:
            matches = sites(text)
        except Exception as error:
            raise ValueError(f'{path}: {error}') from error
        for function, argument, line in matches:
            key = (path, function, argument)
            found[key] += 1
            locations.setdefault(key, []).append(line)
    return found, locations


def git(root, *args):
    return subprocess.check_output(['git', '-C', str(root), *args], text=True, stderr=subprocess.PIPE)


def check(root, base):
    files = sorted((root / 'src').rglob('*.rs'))
    if not files:
        raise ValueError('missing Rust source')
    actual, locations = collect((p.relative_to(root).as_posix(), p.read_text()) for p in files)
    previous = Counter()
    if base:
        commit = git(root, 'rev-parse', '--verify', '--end-of-options', f'{base}^{{commit}}').strip()
        paths = git(root, 'ls-tree', '-r', '--name-only', commit, '--', 'src').splitlines()
        # Read the base once, instead of starting git show for each source file.
        import tarfile
        import io
        archive = subprocess.check_output(['git', '-C', str(root), 'archive', commit, 'src'])
        with tarfile.open(fileobj=io.BytesIO(archive)) as tree:
            wanted = {p for p in paths if p.endswith('.rs')}
            previous, _ = collect((m.name, tree.extractfile(m).read().decode()) for m in tree if m.name in wanted and m.isfile())
    added = actual - previous
    for key in sorted(added):
        print(f'{key[0]}:{locations[key][0]} {key[1]}: use env_flag(name, default) for {key[2]}')
    print(f'ENV_FLAG {"FAIL" if added else "PASS"} direct={sum(actual.values())} base={sum(previous.values())} added={sum(added.values())}')
    return bool(added)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base-ref', default=os.environ.get('TEST_LANE_BASELINE_REF', 'origin/main'))
    parser.add_argument('--root', type=Path, default=ROOT)
    args = parser.parse_args(argv)
    try:
        return check(args.root, args.base_ref)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f'ENV_FLAG ERROR: {error}')
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
