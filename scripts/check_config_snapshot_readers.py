#!/usr/bin/env python3
"""Pin disk-reader function scopes and APIs; runtime A/B tests prove lazy snapshot use."""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import subprocess
from collections import Counter, defaultdict
from pathlib import Path

try:
    from scripts.rust_lex import StripState, strip_line
    from scripts.rust_production import _production_text
    from scripts import test_only_module_skip_pin as skip_pin
    from scripts.ci import rust_test_sources
except ModuleNotFoundError:
    from rust_lex import StripState, strip_line
    from rust_production import _production_text
    import test_only_module_skip_pin as skip_pin
    from ci import rust_test_sources

REGISTRY = 'scripts/config_snapshot_reader_exceptions.json'
ANCHORS = ('src/lib.rs', 'src/config.rs', 'src/config/config_path.rs', 'src/config_live_reload.rs')
NAMES = {'load', 'load_graceful', 'load_from_path', 'resolved_config_path', 'config_path_in',
         'load_config_file', 'load_runtime_config_file'}
CONFIG_APIS = {'crate::config::' + name for name in NAMES}
CONFIG_APIS |= {'crate::config::config_path::' + name for name in NAMES}
EXPORT = ('src/config.rs', 'pub use config_path::resolved_config_path;')
PATH = re.compile(r'(?<![\w#])(?:::)?(?:r#)?[A-Za-z_]\w*(?:\s*::\s*(?:r#)?[A-Za-z_]\w*)*')
USE = re.compile(r'\b(?P<public>pub(?:\([^)]*\))?\s+)?use\s+(?P<tree>[^;]+);')
FN = re.compile(r'\bfn\s+(?P<name>[A-Za-z_]\w*)\s*(?:<[^;{}]*?>\s*)?\(')
KINDS = {'producer', 'boot', 'standalone', 'editor', 'migration', 'shared-fallback', 'path-only', 'snapshot-consumer'}


def normalized(text):
    return re.sub(r'\s+', '', text)


def digest(text):
    return hashlib.sha256(normalized(text).encode()).hexdigest()


def brace_end(code, start):
    depth = 0
    for at in range(start, len(code)):
        depth += (code[at] == '{') - (code[at] == '}')
        if not depth:
            return at + 1
    raise ValueError('unbalanced function body')


def functions(code):
    items = []
    for match in FN.finditer(code):
        nest = 0
        for at in range(match.end() - 1, len(code)):
            char = code[at]
            nest += (char in '([') - (char in ')]')
            if not nest and char == ';':
                break
            if not nest and char == '{':
                end = brace_end(code, at)
                items.append((match.group('name'), match.start(), end,
                              normalized(code[match.start():at])))
                break
    return items


def split_tree(tree):
    depth, start, items = 0, 0, []
    for at, char in enumerate(tree):
        depth += (char == '{') - (char == '}')
        if char == ',' and depth == 0:
            items.append(tree[start:at].strip())
            start = at + 1
    items.append(tree[start:].strip())
    return [item for item in items if item]


def expand_use(tree, prefix=''):
    result = []
    for part in split_tree(tree):
        if '{' in part:
            opening = part.index('{')
            if not part.endswith('}'):
                raise ValueError('unbalanced use tree')
            result.extend(expand_use(part[opening + 1:-1], prefix + part[:opening]))
        else:
            original, _, alias = part.partition(' as ')
            original = re.sub(r'\s*::\s*', '::', prefix + original.strip())
            if original.endswith('::self'):
                original = original[:-6]
            result.append((original, alias.strip() or original.split('::')[-1]))
    return result


def module_path(path):
    parts = list(Path(path).with_suffix('').parts[1:])
    if parts[-1] in {'lib', 'main', 'mod'}:
        parts.pop()
    return 'crate' + ''.join('::' + part for part in parts)


def resolve(path, aliases, module):
    parts = re.sub(r'\s*::\s*', '::', path).removeprefix('::').split('::')
    parts = [part.removeprefix('r#') for part in parts]
    seen = set()
    while parts[0] in aliases and parts[0] not in seen:
        seen.add(parts[0])
        parts = aliases[parts[0]].split('::') + parts[1:]
    if parts[0] in {'self', 'super'}:
        base = module.split('::')
        while parts and parts[0] == 'super':
            parts.pop(0)
            base = base[:-1]
        if parts and parts[0] == 'self':
            parts.pop(0)
        parts = base + parts
    target = '::'.join(parts)
    api = re.search(r'(?:^|::)(?:config(?P<resolver>::config_path)?|(?P<path_only>config_path))::(?P<name>' + '|'.join(sorted(NAMES)) + r')$', target)
    if api:
        return 'crate::config::' + ('config_path::' if api.group('resolver') or api.group('path_only') else '') + api.group('name')
    if target.startswith('crate::') and target.endswith('::Config'):
        return 'crate::config::Config'
    return target


def scan_text(path, code):
    """Return API-reference tuples and structural errors for stripped production text."""
    errors, refs = [], []
    if re.search(r'\$\s*\w+\s*::\s*(?:' + '|'.join(sorted(NAMES)) + r')\b', code):
        errors.append(f'{path}: dynamic disk API namespace is unclassified')
    if re.search(r'\bextern\s+crate\s+(?:self|serde_yaml)\b', code):
        errors.append(f'{path}: extern namespace alias is unclassified')
    module = module_path(path)
    aliases = {'config': 'crate::config', 'config_path': 'crate::config::config_path',
               'Config': 'crate::config::Config'}
    if path == 'src/config.rs' or path.startswith('src/config/'):
        aliases.update({name: 'crate::config::' + name for name in NAMES})
    if path == 'src/config/config_path.rs':
        aliases.update({name: 'crate::config::config_path::' + name for name in NAMES})
    uses = list(USE.finditer(code))
    import_names = defaultdict(set)
    for use in uses:
        try:
            expanded = expand_use(use.group('tree'))
        except ValueError as error:
            errors.append(f'{path}: {error}')
            continue
        for original, local in expanded:
            target = resolve(original, aliases, module)
            if target in {'serde_yaml', 'serde_yaml::Deserializer', 'serde_yaml::*'} or re.fullmatch(r'serde_yaml::from_(?:str|slice|reader|value)', target):
                errors.append(f'{path}: YAML parser imports require fully qualified references')
            if target in {'crate', 'crate::config', 'crate::config::config_path', 'crate::config::Config'} and local != target.split('::')[-1]:
                errors.append(f'{path}: cross-module config namespace alias is unclassified: {local}')
            if local == '*' and target.startswith('crate::config') and original != 'super::*':
                errors.append(f'{path}: config glob import is unclassified')
            if target.startswith('crate::config') and local != '*' and not re.fullmatch(r'[A-Za-z_][A-Za-z_0-9]*', local):
                errors.append(f'{path}: unsupported config import alias {local}')
            if local not in {'*', '_'}:
                import_names[local].add(target)
                if len(import_names[local]) != 1 and any(
                        value.startswith('crate::config') for value in import_names[local]):
                    errors.append(f'{path}: ambiguous import alias {local}')
                aliases[local] = target
            if use.group('public') and target in {'crate::config', 'crate::config::config_path', 'crate::config::Config'}:
                errors.append(f'{path}: config module re-export is unclassified')
            if target in CONFIG_APIS:
                if path == EXPORT[0] and normalized(use.group()) == normalized(EXPORT[1]):
                    continue
                refs.append((use.start(), target))
    for alias in re.finditer(r'\btype\s+(\w+)\s*=\s*([\w: ]+)\s*;', code):
        target = resolve(alias.group(2).strip(), aliases, module)
        if target == 'crate::config::Config':
            if alias.group(1) != 'Config':
                errors.append(f'{path}: cross-module Config type alias is unclassified')
            aliases[alias.group(1)] = target
    body = code
    for use in reversed(uses):
        body = body[:use.start()] + ' ' * (use.end() - use.start()) + body[use.end():]
    definition_names = {m.start('name') for m in FN.finditer(body)}
    yaml_parsers = []
    for match in PATH.finditer(body):
        if match.start() in definition_names or (
                match.start() > 0 and body[match.start() - 1] == '.'):
            continue
        target = resolve(match.group(), aliases, module)
        if target in {'crate::config', 'crate::config::config_path'} and '::' in match.group():
            errors.append(f'{path}: bare config module reference is unclassified')
        if target in CONFIG_APIS:
            refs.append((match.start(), target))
        if re.fullmatch(r'serde_yaml::(?:Deserializer::)?from_(?:str|slice|reader|value)', target):
            yaml_parsers.append(match.start())
    fn_items = functions(code) if refs or yaml_parsers else []
    for _name, start, end, _signature in fn_items:
        fn_code = code[start:end]
        parses = [offset for offset in yaml_parsers if start <= offset < end]
        if parses and any(resolve(m.group(), aliases, module) == 'crate::config::Config'
                          for m in PATH.finditer(fn_code)):
            refs.extend((offset, 'raw-config-yaml') for offset in parses)
    if any(resolve(match.group(), aliases, module) == 'crate::config::Config'
           for match in PATH.finditer(code)):
        refs.extend((offset, 'raw-config-yaml') for offset in yaml_parsers
                    if not any(start <= offset < end for _name, start, end, _sig in fn_items))
    return refs, errors, fn_items


def lexical_errors(path, raw):
    state = StripState()
    for line in raw.splitlines():
        strip_line(line, state)
    if state.in_string or state.raw_hashes is not None or state.block_depth:
        return [f'{path}: unterminated Rust literal/comment']
    return []


def audit(sources, entries, *, required=ANCHORS):
    """Audit an already classified production map, including stale registry entries."""
    errors = [f'{path}: required source missing' for path in required if path not in sources]
    if not sources or not any(code.strip() for code in sources.values()):
        errors.append('production scan target count is zero')
    rows, seen_rows, reference_count = {}, set(), 0
    for row in entries:
        key = row.get('file'), row.get('function'), row.get('signature')
        if (not all(isinstance(value, str) and value for value in key)
                or '*' in str(key) or row.get('kind') not in KINDS
                or not row.get('reason') or not row.get('tests')
                or not re.fullmatch(r'[a-f0-9]{64}', row.get('sha256', ''))
                or not row.get('signature') or not isinstance(row.get('apis'), dict)
                or (not row['apis'] and row.get('kind') != 'snapshot-consumer')
                or (row.get('kind') == 'snapshot-consumer' and row['apis'])
                or any(not isinstance(count, int) or count < 1
                                          for count in row['apis'].values())):
            errors.append(f'{key}: invalid function exception')
            continue
        if key in rows:
            errors.append(f'{key}: duplicate function exception')
        rows[key] = row
    for path, code in sources.items():
        try:
            refs, issues, items = scan_text(path, code)
            if not items and any(file == path for file, _name, _sig in rows):
                items = functions(code)
        except ValueError as error:
            errors.append(f'{path}: {error}')
            continue
        errors.extend(issues)
        reference_count += len(refs)
        owned = defaultdict(Counter)
        for offset, api in refs:
            owner = [item for item in items if item[1] <= offset < item[2]]
            owner.sort(key=lambda item: item[2] - item[1])
            if not owner:
                errors.append(f'{path}:{code[:offset].count(chr(10)) + 1}: unowned {api}')
                continue
            name, _start, _end, _signature = owner[0]
            owned[(name, _signature)][api] += 1
            if (path, name, _signature) not in rows:
                errors.append(f'{path}:{code[:offset].count(chr(10)) + 1}:{name}: unapproved {api}')
        for (file, name, registered_signature), row in rows.items():
            if file != path:
                continue
            matching = [item for item in items
                        if item[0] == name and item[3] == registered_signature]
            if len(matching) != 1:
                errors.append(f'{path}:{name}: expected one function, found {len(matching)}')
                continue
            _, start, end, signature = matching[0]
            seen_rows.add((path, name, registered_signature))
            if signature != row['signature'] or digest(code[start:end]) != row['sha256']:
                errors.append(f'{path}:{name}: function signature/body changed')
            if dict(owned[(name, signature)]) != row['apis']:
                errors.append(f'{path}:{name}: API count mismatch: {dict(owned[(name, signature)])}')
    errors.extend(f'{file}:{name}: stale exception' for file, name, signature in rows if (file, name, signature) not in seen_rows)
    if reference_count == 0:
        errors.append('disk API census is empty; producer/exception coverage disappeared')
    return errors, {'production_files': len(sources), 'disk_references': reference_count,
                    'function_registry_entries': len(rows),
                    'function_exceptions': sum(row['kind'] != 'snapshot-consumer' for row in rows.values()),
                    'consumer_pins': sum(row['kind'] == 'snapshot-consumer' for row in rows.values()),
                    'unapproved_runtime_references': sum('unapproved ' in error or 'unowned ' in error
                                                         for error in errors)}


def proof_errors(root, entries):
    errors, inventories = [], {}
    for row in entries:
        for proof in row.get('tests', []):
            file, separator, name = proof.partition('::')
            path = Path(file)
            if (not separator or not file.startswith(('src/', 'tests/'))
                    or '..' in path.parts or path.suffix != '.rs'
                    or not re.fullmatch(r'\w+', name)):
                errors.append(f'{proof}: invalid test proof selector')
                continue
            try:
                if (root / path).is_symlink() or not (root / path).resolve().is_relative_to(root.resolve()):
                    raise OSError('test proof must remain inside the checkout')
                text = (root / path).read_text('utf-8')
            except OSError:
                errors.append(f'{proof}: test proof file missing')
                continue
            if path not in inventories:
                inventories[path] = rust_test_sources.walk(root / path, root)
            inventory = inventories[path]
            declarations = [site for test_id, sites in inventory.tests.items()
                            if test_id.split('::')[-1] == name
                            for site, _cfgs in sites if site.rpartition(':')[0] == file]
            if inventory.errors or inventory.unreadable or len(declarations) != 1:
                errors.append(f'{proof}: expected one declared test function')
    return errors


def tracked_missing(root, files):
    done = subprocess.run(['git', 'ls-files', '-z', '--', 'src'], cwd=root,
                          check=True, capture_output=True, text=True)
    tracked = {name for name in done.stdout.split('\0') if name.endswith('.rs')}
    if not tracked:
        raise RuntimeError('tracked Rust source census is empty')
    actual = {path.relative_to(root).as_posix() for path in files}
    return [f'{name}: tracked source missing from scan' for name in sorted(tracked - actual)]


def check(root):
    root = root.resolve()
    entries = json.loads((root / REGISTRY).read_text())
    if not isinstance(entries, list) or any(not isinstance(row, dict) for row in entries):
        raise ValueError('function registry must be an array of objects')
    files, skipped = skip_pin.validated_scan_files(
        root, Path('src'), skip_pin.is_basename_test_file)
    errors = tracked_missing(root, files) + proof_errors(root, entries)
    sources = {}
    for path in files:
        raw = path.read_text('utf-8')
        errors.extend(lexical_errors(str(path.relative_to(root)), raw))
        if path not in skipped:
            sources[path.relative_to(root).as_posix()] = _production_text(path)
    issues, counts = audit(sources, entries)
    return errors + issues, {**counts, 'enumerated_files': len(files), 'test_only_files': len(skipped)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path.cwd())
    args = parser.parse_args()
    try:
        errors, counts = check(args.root)
    except (OSError, ValueError, RuntimeError, TypeError, subprocess.SubprocessError) as error:
        errors, counts = [str(error)], {}
    print(json.dumps({'config_snapshot_readers': 'FAIL' if errors else 'PASS', **counts,
                      'failures': len(errors)}, sort_keys=True))
    for error in errors:
        print(error)
    return bool(errors)


if __name__ == '__main__':
    sys.exit(main())
