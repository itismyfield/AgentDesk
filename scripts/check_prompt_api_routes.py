#!/usr/bin/env python3
"""Read-only staged prompt path lint against this checkout's endpoint inventory."""
import argparse
from pathlib import Path
import re
from urllib.parse import urlsplit

URL = re.compile(r'''(?<![^\s"'`(<])https?://(?:127\.0\.0\.1|localhost|\[::1\]):[0-9]+/api/[^\s"'`()<>\\]*''')

def inventory(repo):
    root = repo / 'src/server/routes/docs/inventory/endpoints'
    parts = re.findall(r'^mod (part_\d+);$', (root / 'mod.rs').read_text(), re.M)
    if not parts:
        raise ValueError('empty inventory')
    paths = set()
    for part in parts:
        source = (root / (part + '.rs')).read_text()
        entries = re.findall(r'^\s*ep\(\s*"[^"\\]+"\s*,\s*"([^"\\]+)"\s*,', source, re.M)
        if len(entries) != len(re.findall(r'^\s*ep\(', source, re.M)):
            raise ValueError('unparsed endpoint')
        paths.update(entries)
    if not paths:
        raise ValueError('empty inventory')
    return [re.compile(''.join('.+' if token.startswith('{*') else '[^/]+' if token.startswith('{') else re.escape(token)
                              for token in re.split(r'(\{\*?[^{}]+\})', path))) for path in paths]

def candidates(source):
    source = re.sub(r'<!--.*?(?:-->|\Z)', lambda m: '\n' * m[0].count('\n'), source, flags=re.S)
    fence = None
    for number, line in enumerate(source.splitlines(), 1):
        if line.lstrip().startswith('>'):
            continue
        marker = re.match(r'^\s{0,3}(`{3,}|~{3,})(.*)$', line)
        if marker:
            run, info = marker.groups()
            if fence is None:
                fence = (run[0], len(run), info.strip() in ('sh', 'bash', 'shell'))
            elif run[0] == fence[0] and len(run) >= fence[1] and not info.strip():
                fence = None
            continue
        if fence is None or fence[2]:
            # Only ASCII shell joins are excluded; Korean prose suffixes remain candidates.
            shell = bool(fence or re.search(r'\bcurl\b|\$[A-Za-z_{]|`', line))
            matches = (match for match in URL.finditer(line)
                       if not (shell and (re.search(r'''[A-Za-z0-9_/.:=$}\"']["']$''', line[:match.start()])
                                          or re.match(r'''["'][A-Za-z0-9_/.$\"'-]''', line[match.end():])))
                       and '$' not in urlsplit(match[0]).path and not re.match(r'''["']?(?:\s*\+|\$)''', line[match.end():]))
            for path in sorted({urlsplit(match[0]).path for match in matches}):
                yield number, path

def check(repo, prompts):
    inspected = candidates_count = unknown = skipped = 0
    try:
        routes = inventory(repo)
        for prompt in sorted(prompts.iterdir()):
            if not prompt.name.endswith('.prompt.md'):
                continue
            if prompt.is_symlink():
                skipped += 1
                print(f'{prompt.name}: skipped_symlink')
                continue
            if not prompt.is_file():
                continue
            source = prompt.read_text()
            inspected += 1
            for number, path in candidates(source):
                candidates_count += 1
                if not any(route.fullmatch(path) for route in routes):
                    unknown += 1
                    print(f'{prompt.name}:{number}: unknown_path {path}')
    except (OSError, UnicodeError, ValueError):
        print('inspection_unavailable')
        return 2
    print(f'inspected_files={inspected} candidate_urls={candidates_count} unknown_paths={unknown} skipped_files={skipped}')
    if not candidates_count:
        print('no_applicable_urls')
    return int(unknown > 0)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', type=Path, required=True)
    parser.add_argument('--prompts', type=Path, required=True)
    args = parser.parse_args()
    raise SystemExit(check(args.repo, args.prompts))
