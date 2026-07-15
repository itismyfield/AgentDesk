#!/usr/bin/env python3
"""Declarative external-tool inventory for the AgentDesk update routine."""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path


DEFAULT_MEMENTO_URL = "http://100.123.183.105:57332"


@dataclass(frozen=True)
class ToolSpec:
    key: str
    display_name: str
    method: str
    tier: str
    current_kind: str
    current_value: tuple[str, ...] | str
    latest_kind: str
    latest_value: tuple[str, ...] | str
    update_kind: str | None
    update_value: tuple[str, ...] | str | None
    smoke_profile: str | None
    risk: str
    changelog_url: str
    report_only: bool = False


@dataclass(frozen=True)
class CommandResult:
    returncode: int
    stdout: str
    stderr: str


@dataclass(frozen=True)
class ValueProbe:
    ok: bool
    value: str
    detail: str


@dataclass(frozen=True)
class ToolCheck:
    key: str
    display_name: str
    method: str
    tier: str
    current: str
    latest: str
    decision: str
    current_detail: str
    latest_detail: str
    risk: str
    changelog_url: str
    report_only: bool


@dataclass(frozen=True)
class SmokeResult:
    check: str
    ok: bool
    detail: str


def tool_inventory() -> tuple[ToolSpec, ...]:
    """Return the exhaustive inventory from AgentDesk issue #4555."""

    home = Path.home()
    brew_tools = (
        ("tmux", "tmux", "approval", "tmux", "tmux/tmux", "tmux capture/paste is load-bearing"),
        ("gh", "gh", "hygiene", "gh", "cli/cli", "GitHub CLI compatibility"),
        ("node", "node", "approval", "node", "nodejs/node", "major upgrades can break every global npm CLI"),
        (
            "python-3-14",
            "python@3.14",
            "hygiene",
            "python@3.14",
            "python/cpython",
            "automation interpreter compatibility",
        ),
        ("uv", "uv", "hygiene", "uv", "astral-sh/uv", "Python tool installer compatibility"),
        ("pipx", "pipx", "hygiene", "pipx", "pypa/pipx", "isolated Python CLI management"),
        ("jq", "jq", "hygiene", "jq", "jqlang/jq", "JSON command-line parsing"),
        ("ripgrep", "ripgrep", "hygiene", "ripgrep", "BurntSushi/ripgrep", "repository search tooling"),
        ("ffmpeg", "ffmpeg", "hygiene", "ffmpeg", "FFmpeg/FFmpeg", "voice media processing"),
        (
            "whisper-cpp",
            "whisper-cpp",
            "hygiene",
            "whisper-cpp",
            "ggml-org/whisper.cpp",
            "voice transcription runtime",
        ),
        (
            "postgresql-17",
            "postgresql@17",
            "approval",
            "postgresql@17",
            "postgres/postgres",
            "client major must remain compatible with the server",
        ),
    )
    specs: list[ToolSpec] = [
        ToolSpec(
            "claude",
            "claude",
            "native",
            "approval",
            "command",
            ("claude", "--version"),
            "npm",
            "@anthropic-ai/claude-code",
            "command",
            ("claude", "update"),
            "claude",
            "auth/session behavior can desynchronize cswap",
            "https://docs.anthropic.com/en/release-notes/claude-code",
        ),
        ToolSpec(
            "codex",
            "codex (@openai/codex)",
            "npm-g",
            "approval",
            "command",
            ("codex", "--version"),
            "npm",
            "@openai/codex",
            "npm-exact",
            "@openai/codex",
            "codex",
            "version parsing and all Codex agent sessions depend on it",
            "https://github.com/openai/codex/releases",
        ),
        ToolSpec(
            "ocx",
            "ocx (@bitkyc08/opencodex)",
            "npm-g",
            "approval",
            "command",
            ("ocx", "--version"),
            "npm",
            "@bitkyc08/opencodex",
            "npm-exact",
            "@bitkyc08/opencodex",
            "ocx",
            "proxy lifecycle and agent routing depend on it",
            "https://www.npmjs.com/package/@bitkyc08/opencodex",
        ),
        ToolSpec(
            "claude-e",
            "claude-e",
            "npm-g",
            "hygiene",
            "command",
            ("claude-e", "--version"),
            "npm",
            "claude-e",
            "npm-exact",
            "claude-e",
            "claude-e",
            "claude-e wrapper compatibility",
            "https://www.npmjs.com/package/claude-e",
        ),
        ToolSpec(
            "cswap",
            "cswap (claude-swap)",
            "uv-tool",
            "approval",
            "command",
            ("cswap", "--version"),
            "pypi",
            "claude-swap",
            "uv-exact",
            "claude-swap",
            "cswap",
            "cswap 0.17 changed the --list --json shape",
            "https://pypi.org/project/claude-swap/",
        ),
        ToolSpec(
            "cargo-rustc",
            "cargo/rustc",
            "rustup",
            "hygiene",
            "command",
            ("rustup", "run", "stable", "rustc", "--version"),
            "rustup",
            ("rustup", "check"),
            "command",
            ("rustup", "update", "stable", "--no-self-update"),
            "rust",
            "the repository MSRV and pinned toolchain must still compile",
            "https://blog.rust-lang.org/releases/",
        ),
    ]
    for key, display, tier, formula, upstream, risk in brew_tools:
        specs.append(
            ToolSpec(
                key,
                display,
                "homebrew",
                tier,
                "brew-current",
                formula,
                "brew-latest",
                formula,
                "brew-upgrade",
                formula,
                key,
                risk,
                f"https://formulae.brew.sh/formula/{formula.replace('@', '%40')}",
            )
        )
    specs.extend(
        [
            ToolSpec(
                "edge-tts",
                "edge-tts",
                "pipx",
                "hygiene",
                "command",
                ("edge-tts", "--version"),
                "pypi",
                "edge-tts",
                "pipx-exact",
                "edge-tts",
                "edge-tts",
                "voice synthesis CLI compatibility",
                "https://pypi.org/project/edge-tts/",
            ),
            ToolSpec(
                "opencode",
                "opencode",
                "installer",
                "hygiene",
                "command",
                ("opencode", "--version"),
                "npm",
                "opencode-ai",
                "command",
                ("opencode", "upgrade"),
                "opencode",
                "only update hosts where the optional provider is installed",
                "https://github.com/anomalyco/opencode/releases",
            ),
            ToolSpec(
                "memento-mcp",
                "memento MCP",
                "remote-service",
                "report-only",
                "memento-health",
                os.environ.get("AGENTDESK_TOOLCHAIN_MEMENTO_URL", DEFAULT_MEMENTO_URL),
                "remote-managed",
                "mac-mini service owner",
                None,
                None,
                None,
                "remote mac-mini service; local mutation is forbidden",
                "http://100.123.183.105:57332/health",
                report_only=True,
            ),
            ToolSpec(
                "brave-search-mcp",
                "brave-search MCP",
                "npx-always-latest",
                "approval",
                "literal",
                "npx -y @modelcontextprotocol/server-brave-search (unpinned)",
                "npm",
                "@modelcontextprotocol/server-brave-search",
                None,
                None,
                None,
                "pinning @x.y.z is the update-management decision",
                "https://www.npmjs.com/package/@modelcontextprotocol/server-brave-search",
                report_only=True,
            ),
            ToolSpec(
                "sidecar-launcher",
                "SidecarLauncher",
                "manual",
                "report-only",
                "manual-command",
                (str(home / "bin" / "SidecarLauncher"), "devices"),
                "manual",
                "manual receipt/upstream check",
                None,
                None,
                None,
                "manually installed binary; do not mutate it",
                "https://github.com/Ocasio-J/SidecarLauncher",
                report_only=True,
            ),
            ToolSpec(
                "playwright-chromium",
                "playwright + Chromium",
                "manual",
                "report-only",
                "command",
                ("playwright", "--version"),
                "manual",
                "inspect /receipt when installed",
                None,
                None,
                None,
                "installation is on-demand and receipt-managed",
                "https://playwright.dev/docs/browsers",
                report_only=True,
            ),
        ]
    )
    return tuple(specs)
