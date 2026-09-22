#!/usr/bin/env python3
"""Exercise native provider wrapper entrypoints without credentials or provider calls."""
from __future__ import annotations

import argparse
from pathlib import Path
import subprocess
import tempfile


def verify(binary: Path) -> None:
    binary = binary.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="agentdesk-wrapper-smoke-") as temporary:
        root = Path(temporary)
        # Every wrapper must reach its own prompt reader. A missing command,
        # rejected pipe argument or startup crash must not pass as exit code 1.
        # The deliberately absent prompt stops before credentials/provider IO.
        for command, extra in (
            ("tmux-wrapper", ["--", "unused-provider"]),
            ("codex-tmux-wrapper", ["--codex-bin", "unused-provider"]),
            ("qwen-tmux-wrapper", ["--qwen-bin", "unused-provider"]),
        ):
            result = subprocess.run(
                [str(binary), command, "--output-file", str(root / "output.jsonl"),
                 "--input-fifo", str(root / "unused-fifo"), "--prompt-file",
                 str(root / "missing.prompt"), "--cwd", str(root),
                 "--input-mode", "pipe", *extra],
                stdin=subprocess.DEVNULL, capture_output=True, timeout=15,
            )
            stderr = result.stderr.decode("utf-8", "replace")
            if result.returncode != 1 or "Error reading prompt file:" not in stderr:
                raise RuntimeError(
                    f"{command} did not reach its pipe entrypoint: "
                    f"exit={result.returncode}; {stderr[:800]}"
                )
            print(f"PASS {command}: native pipe entrypoint")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    verify(args.binary)


if __name__ == "__main__":
    main()
