#!/usr/bin/env python3
"""Read-only staged prompt path lint against this checkout's endpoint inventory."""
import argparse
import json
from pathlib import Path
import re
from urllib.parse import urlsplit

URL = re.compile(r'''(?<![^\s"'`(<])https?://(?:127\.0\.0\.1|localhost|\[::1\]):[0-9]+/api/[^\s"'`()<>\\]*''')
API_ENDPOINTS_JSON = Path('docs/data/api-endpoints.json')
JOIN = re.compile(r'''\s*\+|[/${"']''')

def joined(line, match):
    """True when a path continues past the matched URL token's own closing quote."""
    rest, quote = line[match.end():], line[match.end():match.end() + 1]
    # A quote right after the token closes that token's own span when it also opened
    # the token or is still open by parity; stray apostrophes must not decide this.
    if quote in ('"', "'") and (line[match.start() - 1:match.start()] == quote
                                or line[:match.start()].count(quote) % 2 == 1):
        rest = rest[1:]
    return bool(JOIN.match(rest))

def load_endpoint_pairs(catalog: Path) -> list[tuple[str, str]]:
    entries = json.loads(catalog.read_text(encoding='utf-8'))
    if not isinstance(entries, list) or not entries:
        raise ValueError('expected a non-empty endpoint inventory')
    pairs = []
    for entry in entries:
        if not isinstance(entry, dict):
            raise ValueError('expected an endpoint object')
        method, path = entry.get('method'), entry.get('path')
        if (not isinstance(method, str) or not method
                or not isinstance(path, str) or not path.startswith('/')):
            raise ValueError('invalid endpoint method or path')
        pairs.append((method, path))
    return pairs

def inventory(repo):
    paths = {pair[1] for pair in load_endpoint_pairs(repo / API_ENDPOINTS_JSON)}
    return [
        re.compile(''.join(
            '.+' if token.startswith('{*') else '[^/]+' if token.startswith('{') else re.escape(token)
            for token in re.split(r'(\{\*?[^{}]+\})', path)
        ))
        for path in sorted(paths)
    ]

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
            # Prose, language and client names never decide a join: a complete URL
            # token counts unless a path continues past its closing quote.
            matches = (match for match in URL.finditer(line)
                       if '$' not in urlsplit(match[0]).path and not joined(line, match))
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
            source = prompt.read_text(encoding='utf-8')
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
    if not inspected:
        print('inspection_unavailable: no_prompt_files')
        return 2
    if not candidates_count:
        print('no_applicable_urls')
    return int(unknown > 0)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', type=Path, required=True)
    parser.add_argument('--prompts', type=Path, required=True)
    args = parser.parse_args()
    raise SystemExit(check(args.repo, args.prompts))
