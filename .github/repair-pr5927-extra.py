from pathlib import Path
import json
import sys

sys.path.insert(0, 'scripts')
import check_destructive_call_site_ratchet as ratchet

path = 'src/services/discord/tmux_watcher/cancel_handoff/interrupted_adoption_tests.rs'
text = ratchet._stripped_text(Path(path))
count = len(ratchet.ALL_SOURCE_PATTERNS['watcher_cancel'].findall(text))
assert count == 1
baseline = Path('scripts/destructive_call_site_baseline.json')
original = baseline.read_text()
payload = json.loads(original)
files = payload['categories']['watcher_cancel']['files']
assert path not in files
anchor = '        "src/services/discord/tmux_watcher/placeholder_reclaim.rs": 1,'
start = original.index('    "watcher_cancel":')
assert original[start:].count(anchor) == 1
updated = original[:start] + original[start:].replace(anchor, f'        "{path}": {count},\n' + anchor, 1)
files[path] = count
assert json.loads(updated) == payload
baseline.write_text(updated)
