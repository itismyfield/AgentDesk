#!/usr/bin/env python3
"""Pin disk-reader function scopes, APIs and approved-reader callers; runtime A/B tests prove lazy snapshot use."""
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
# Approved fresh readers whose callers are pinned too, so reuse cannot add a new runtime reader.
WRAPPER_KINDS = {'boot', 'standalone', 'editor', 'migration'}
CALLER = re.compile(r'src/[\w./-]+\.rs(?:::[A-Za-z_]\w*)+')
INCLUDE = re.compile(r'\binclude\s*!\s*[(\[{]')
PATH_ATTR = re.compile(r'#\s*\[\s*path\s*=')


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


def scope_spans(code):
    """Brace spans labelled ('mod'|'impl'|'trait', name) for item scopes and None for bodies/blocks."""
    spans, stack, last = [], [], -1
    for match in re.finditer(r'[{};]', code):
        at = match.start()
        if match.group() == '{':
            header, label = code[last + 1:at], None
            if re.search(r'\bfn\b', header):
                pass
            elif named := re.search(r'\bmod\s+(?:r#)?([A-Za-z_]\w*)\s*$', header):
                label = ('mod', named.group(1))
            elif named := re.search(r'\btrait\s+([A-Za-z_]\w*)', header):
                label = ('trait', named.group(1))
            elif named := re.search(r'\bimpl\b', header):
                rest = re.sub(r'^\s*<(?:[^<>]|<[^<>]*>)*>', '', header[named.end():])
                rest = re.split(r'\bfor\b', rest)[-1]
                typed = re.match(r'\s*(?:&\s*(?:mut\s+)?)?(?:dyn\s+)?(?:[A-Za-z_]\w*\s*::\s*)*([A-Za-z_]\w*)', rest)
                label = ('impl', typed.group(1) if typed else '?')
            stack.append((at, label))
        elif match.group() == '}' and stack:
            start, label = stack.pop()
            spans.append((start, at + 1, label))
        last = at
    return sorted(spans)


def enclosing(spans, offset):
    return [label for start, end, label in spans if start < offset < end]


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
    if target.startswith('crate::') and (target.endswith('::Config') or '::Config::' in target):
        return 'crate::config::Config'  # Associated paths such as Config::deserialize use the type.
    return target


def wrapper_target(target, wrappers):
    """Map a resolved path (or re-export alias) to its approved wrapper, tolerating the binary crate prefix."""
    if target in wrappers:
        return wrappers[target]
    return next((wrappers[key] for key in wrappers if target.endswith('::' + key.removeprefix('crate::'))), None)


def inline_module(code, module, offset, spans):
    return module + ''.join('::' + label[1] for start, end, label in spans
                            if start < offset < end and label and label[0] == 'mod')


def reexport_aliases(sources, wrappers):
    """Extend wrapper paths with `pub use` re-exports until a fixpoint; module re-exports stay errors."""
    wrappers = dict(wrappers)
    for _round in range(8):
        added = False
        for path, code in sources.items():
            module, spans = module_path(path), None
            for use in USE.finditer(code):
                if not use.group('public'):
                    continue
                spans = scope_spans(code) if spans is None else spans
                here = inline_module(code, module, use.start(), spans)
                try:
                    expanded = expand_use(use.group('tree'))
                except ValueError:
                    continue
                for original, local in expanded:
                    target = resolve(original, {}, here)
                    wrapper = wrapper_target(target, wrappers) or (
                        not target.startswith('crate::') and wrapper_target(here + '::' + target, wrappers))
                    if wrapper and local != '*' and here + '::' + local not in wrappers:
                        wrappers[here + '::' + local] = wrapper
                        added = True
        if not added:
            return wrappers
    raise ValueError('approved reader re-export chain does not converge')


def locally_bound(name, fn_code):
    return re.search(r'(?<![\w.])(?:let\s+(?:mut\s+)?|mut\s+)?' + name + r'\s*:(?!:)|\blet\s+(?:mut\s+)?'
                     + name + r'\b|\|[^|]*\b' + name + r'\b[^|]*\||\bfor\s+' + name + r'\b', fn_code)


def scan_text(path, code, wrappers=None):
    """Return API-reference tuples and structural errors for stripped production text."""
    errors, refs, wrappers = [], [], wrappers or {}
    wrapper_modules = {key.rpartition('::')[0] for key in wrappers}
    wrapper_names = {key.rpartition('::')[2] for key in wrappers}
    spans = None

    def module_at(offset):
        # Inline `mod` blocks change what `self`/`super` and bare names refer to.
        nonlocal spans
        spans = scope_spans(code) if spans is None else spans
        return inline_module(code, module, offset, spans)

    def candidates(target, offset):
        # Rust 2018 paths may start at a child module (`node::run`), so also try the scope-relative path.
        return [target] + ([module_at(offset) + '::' + target] if not target.startswith('crate') else [])

    def wrapper_of(target, offset):
        # Bare names are handled below so a local binding with the same name is not a reader call.
        return '::' in target and next(filter(None, (wrapper_target(item, wrappers)
                                                     for item in candidates(target, offset))), None)

    def resolve_at(text, offset):
        relative = re.match(r'\s*(?:r#)?(?:self|super)\b', text)
        return resolve(text, aliases, module_at(offset) if relative else module)
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
    import_names, globs = defaultdict(set), set()
    for use in uses:
        try:
            expanded = expand_use(use.group('tree'))
        except ValueError as error:
            errors.append(f'{path}: {error}')
            continue
        for original, local in expanded:
            target = resolve_at(original, use.start())
            if local == '*':
                globs.update(item.removesuffix('::*') for item in candidates(target, use.start()))
            if use.group('public') and (
                    any(item.removesuffix('::*') in wrapper_modules for item in candidates(target, use.start()))
                    or (wrapper_of(target, use.start()) and module_at(use.start()) + '::' + local not in wrappers)):
                errors.append(f'{path}: approved reader re-export is unclassified')
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
    yaml_parsers, bare_wrappers = [], []
    for match in PATH.finditer(body):
        at = match.start()
        # A single `.` is member access; `..` is a range operator followed by a fresh path.
        if at in definition_names or (at > 0 and body[at - 1] == '.' and body[at - 2:at] != '..'):
            continue
        target = resolve_at(match.group(), at)
        wrapper = wrapper_of(target, at)
        if wrapper:
            refs.append((at, 'wrapper:' + wrapper))
        elif '::' not in match.group() and target in wrapper_names:
            local = [wrappers[key] for key in wrappers if key.rpartition('::')[2] == target
                     and key.rpartition('::')[0] in {module_at(at)} | globs]
            if local:
                called = re.match(r'\s*(?:\(|::\s*<)', body[match.end():])
                bare_wrappers.append((at, 'wrapper:' + local[0], bool(called), target))
        if target in {'crate::config', 'crate::config::config_path'} and '::' in match.group():
            errors.append(f'{path}: bare config module reference is unclassified')
        if target in CONFIG_APIS:
            refs.append((match.start(), target))
        if re.fullmatch(r'serde_yaml::(?:Deserializer::)?from_(?:str|slice|reader|value)', target):
            yaml_parsers.append(match.start())
    fn_items = functions(code) if refs or yaml_parsers or bare_wrappers else []
    for at, api, called, name in bare_wrappers:
        owner = sorted((item for item in fn_items if item[1] <= at < item[2]), key=lambda item: item[2] - item[1])
        if called or not owner or not locally_bound(name, code[owner[0][1]:owner[0][2]]):
            refs.append((at, api))
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


def mount_errors(root, path, raw, production, skipped, wrapper_files):
    """Reject production include!/#[path] that could hide a skipped file or rename a wrapper module."""
    errors, lines = [], raw.splitlines()
    starts = [0]
    for line in production.split('\n'):
        starts.append(starts[-1] + len(line) + 1)
    for pattern, kind in ((INCLUDE, 'include!'), (PATH_ATTR, '#[path]')):
        for match in pattern.finditer(production):
            row = next(index for index in range(len(starts)) if starts[index + 1] > match.start())
            column = match.start() - starts[row]
            literal = re.match(r'(?:include\s*!\s*[(\[{]|#\s*\[\s*path\s*=)\s*"([^"\\]+)"', lines[row][column:])
            where = f'{path.relative_to(root).as_posix()}:{row + 1}'
            if not literal:
                errors.append(f'{where}: unresolved production {kind} target')
                continue
            target = (path.parent / literal.group(1)).resolve()
            if not target.is_relative_to((root / 'src').resolve()):
                errors.append(f'{where}: production {kind} target is outside src/')
            elif target in skipped:
                errors.append(f'{where}: production {kind} reuses a test-only file {literal.group(1)}')
            elif kind == '#[path]' and target in wrapper_files:
                errors.append(f'{where}: #[path] remounts approved reader file {literal.group(1)}')
    return errors


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
        callers = row.get('callers')
        if (not all(isinstance(value, str) and value for value in key)
                or (row.get('kind') in WRAPPER_KINDS) != isinstance(callers, dict)
                or any(not isinstance(caller, str) or not CALLER.fullmatch(caller)
                       or not isinstance(count, int) or count < 1
                       for caller, count in (callers or {}).items())
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
    wrappers, wrapper_rows, observed_callers = {}, {}, defaultdict(Counter)
    for (file, name, signature), row in rows.items():
        if row['kind'] not in WRAPPER_KINDS or file not in sources:
            continue
        code = sources[file]
        spans = scope_spans(code)
        for item in functions(code):
            if item[0] == name and item[3] == signature:
                labels = enclosing(spans, item[1])
                if any(not label or label[0] != 'mod' for label in labels):
                    errors.append(f'{file}:{name}: approved reader must be a module-level function')
                inline = ''.join('::' + label[1] for label in labels if label and label[0] == 'mod')
                key = module_path(file) + inline + '::' + name
                if key in wrapper_rows:
                    errors.append(f'{file}:{name}: duplicate approved reader path {key}')
                wrappers[key], wrapper_rows[key] = key, (file, name)
    try:
        wrappers = reexport_aliases(sources, wrappers)
    except ValueError as error:
        errors.append(str(error))
    for path, code in sources.items():
        try:
            refs, issues, items = scan_text(path, code, wrappers)
            if not items and any(file == path for file, _name, _sig in rows):
                items = functions(code)
        except ValueError as error:
            errors.append(f'{path}: {error}')
            continue
        errors.extend(issues)
        reference_count += sum(not api.startswith('wrapper:') for _offset, api in refs)
        owned, spans = defaultdict(Counter), None
        for offset, api in refs:
            owner = [item for item in items if item[1] <= offset < item[2]]
            owner.sort(key=lambda item: item[2] - item[1])
            if not owner:
                errors.append(f'{path}:{code[:offset].count(chr(10)) + 1}: unowned {api}')
                continue
            name, _start, _end, _signature = owner[0]
            if api.startswith('wrapper:'):
                spans = scope_spans(code) if spans is None else spans
                scope = lambda item: ''.join(label[1] + '::' for label in enclosing(spans, item[1]) if label)
                caller = f'{path}::{scope(owner[0])}{name}'
                if sum(f'{path}::{scope(item)}{item[0]}' == caller for item in items) != 1:
                    errors.append(f'{caller}: ambiguous caller of {api[8:]}')
                observed_callers[api[8:]][caller] += 1
                continue
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
    for key, (file, name) in wrapper_rows.items():
        approved = next(row['callers'] for (f, n, _s), row in rows.items() if (f, n) == (file, name))
        actual = observed_callers.get(key, Counter())
        for caller in sorted(set(actual) | set(approved)):
            if caller not in approved:
                errors.append(f'{caller}: unapproved caller of approved reader {file}:{name}')
            elif actual[caller] != approved[caller]:
                errors.append(f'{caller}: caller count mismatch for {file}:{name}: {actual[caller]}')
    if reference_count == 0:
        errors.append('disk API census is empty; producer/exception coverage disappeared')
    return errors, {'production_files': len(sources), 'disk_references': reference_count,
                    'function_registry_entries': len(rows),
                    'function_exceptions': sum(row['kind'] != 'snapshot-consumer' for row in rows.values()),
                    'wrapper_callers': sum(len(row['callers']) for row in rows.values() if row['kind'] in WRAPPER_KINDS),
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
    sources, issues = production_sources(root, files, skipped, entries)
    errors.extend(issues)
    issues, counts = audit(sources, entries)
    return errors + issues, {**counts, 'enumerated_files': len(files), 'test_only_files': len(skipped)}


def production_sources(root, files, skipped, entries):
    """Strip production files and reject mounts that would let a whole-file skip hide code."""
    errors, sources = [], {}
    skipped_targets = {path.resolve() for path in skipped}
    wrapper_files = {(root / row['file']).resolve() for row in entries if row.get('kind') in WRAPPER_KINDS
                     and isinstance(row.get('file'), str)}
    for path in files:
        raw = path.read_text('utf-8')
        errors.extend(lexical_errors(str(path.relative_to(root)), raw))
        if path not in skipped:
            production = sources[path.relative_to(root).as_posix()] = _production_text(path)
            errors.extend(mount_errors(root, path, raw, production, skipped_targets, wrapper_files))
    return sources, errors


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
