"""Publish statechart index rows and links for the reported commits."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import posixpath
import sys
from urllib.parse import quote, urlunsplit

from scripts.ci import statechart_index as index
from scripts.ci.generated_artifact_report import git, publish, resolve_comparison


CHARTS = Path("docs/statecharts")
README = CHARTS / "README.md"
FRAGMENTS = CHARTS / "index.d"


class Snapshot:
    def __init__(self, root: Path, sha: str, repository: str):
        self.root = root
        self.sha = sha
        self.repository = repository
        self.paths = {}
        for entry in git(root, "ls-tree", "-rtz", sha).decode().split("\0"):
            if entry:
                metadata, path = entry.split("\t", 1)
                self.paths[path] = metadata.split()[1]

    def read(self, path: Path) -> bytes:
        return git(self.root, "show", f"{self.sha}:{path.as_posix()}")

    def resolve(self, path: str) -> str:
        return posixpath.normpath(f"{CHARTS.as_posix()}/{path}") if path else README.as_posix()

    def exists(self, path: str) -> bool:
        resolved = self.resolve(path)
        return resolved == "." or resolved in self.paths

    def url(self, path: str) -> str:
        kind = "tree" if path == "." or self.paths.get(path) == "tree" else "blob"
        suffix = "" if path == "." else f"/{quote(path)}"
        return f"{self.repository}/{kind}/{self.sha}{suffix}"

    def rows(self):
        files = sorted(Path(path) for path in self.paths if Path(path).parent == FRAGMENTS
                       and path.endswith(".md"))
        return index.read_rows(FRAGMENTS, ((path, self.read(path)) for path in files), self.exists)

    def render(self, table: bytes) -> bytes:
        def qualify(row: str) -> str:
            code = [span.span() for span in index.CODE.finditer(row)]

            def replace(match):
                if any(start <= match.start() < end for start, end in code):
                    return match[0]
                target = index.link_target(match[1])
                if target is None:
                    return match[0]
                path, query, fragment = target
                url = self.url(self.resolve(path)) + urlunsplit(("", "", "", query, fragment))
                return match[0][:match.start(1) - match.start()] + url + ")"

            return index.LINK.sub(replace, row)

        return "".join(qualify(row) for row in table.decode("utf-8").splitlines(keepends=True)).encode()


def report(root: Path, directory: Path, env) -> None:
    comparison = resolve_comparison(root, env)
    repository = env.get("GITHUB_REPOSITORY") or env.get("GFP_REPOSITORY") or "itismyfield/AgentDesk"
    repository = f"{env.get('GITHUB_SERVER_URL', 'https://github.com')}/{repository}"
    head = Snapshot(root, comparison.head, repository)
    index.validate_readme(head.read(README), README)
    after = head.rows()
    head_table = head.render(index.render_table(after))
    before_table = head_table
    lines = ["## Statechart index", "", comparison.describe(), ""]
    if comparison.mode != "self":
        base = Snapshot(root, comparison.base, repository)
        if FRAGMENTS.as_posix() not in base.paths:
            before_table = base.render(index.legacy_table(base.read(README), README, base.exists))
            lines.append("Keyed comparison unavailable. The base artifact contains the legacy table.")
        else:
            before = base.rows()
            before_table = base.render(index.render_table(before))
            groups = (("Added", after.keys() - before.keys(), head),
                      ("Removed", before.keys() - after.keys(), base),
                      ("Changed", {key for key in before.keys() & after.keys()
                                   if before[key] != after[key]}, head))
            for label, keys, snapshot in groups:
                links = [f"[`{key}`]({snapshot.url(f'{FRAGMENTS.as_posix()}/{key}.md')})"
                         for key in sorted(keys)]
                lines.append(f"- {label} ({len(keys)}): {', '.join(links) or '—'}")
    publish(directory, comparison, "\n".join(lines) + "\n",
            {"base.md": before_table, "head.md": head_table}, env)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        report(Path.cwd(), args.output, os.environ)
    except (OSError, ValueError) as error:
        print(f"statechart index: {error}\nReproduce: {index.RECOVERY}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
