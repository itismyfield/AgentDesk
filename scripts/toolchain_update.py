#!/usr/bin/env python3
"""Draft, approve, apply, and smoke-gate AgentDesk's external toolchain.

The scheduled operation is read-only; apply is separately human-gated.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import secrets
import subprocess
import sys
import urllib.error
import urllib.request
from dataclasses import asdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Mapping, Sequence

from toolchain_manifest import (
    CommandResult, SmokeResult, ToolCheck, ToolSpec, ValueProbe, tool_inventory
)


SCHEMA_VERSION = 2
APPROVAL_CONFIRMATION = "approve-exact-toolchain-draft"
SAFE_WINDOW_CONFIRMATION = "no-active-turns-or-deploys"
APPROVAL_REQUIRED_METHODS = frozenset({"installer", "native", "npm-g", "rustup"})
DEFAULT_TIMEOUT_SECONDS = 20
DEFAULT_UPDATE_TIMEOUT_SECONDS = 1800
BUILD_UPDATE_TIMEOUT_SECONDS = 7200
IDLE_QUEUE_STATUSES = frozenset({"canceled", "cancelled", "completed", "failed", "idle"})
_SEMVER_RE = re.compile(
    r"(?<![0-9A-Za-z])v?(\d+)\.(\d+)\.(\d+)(?:[-+][0-9A-Za-z.-]+)?(?![0-9A-Za-z])"
)
_SAFE_VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$")
_BREW_VERSION_RE = re.compile(r"^[0-9][0-9A-Za-z._+-]*$")


class ToolchainError(RuntimeError):
    """Base class for an operator-actionable toolchain error."""


class ApprovalError(ToolchainError):
    """Raised when an exact, required approval marker is absent or stale."""


class Runner:
    """Bounded subprocess and HTTP runner used by probes and smoke checks."""

    def run(
        self,
        argv: Sequence[str],
        *,
        timeout: int = DEFAULT_TIMEOUT_SECONDS,
        env: Mapping[str, str] | None = None,
    ) -> CommandResult:
        process_env = os.environ.copy()
        if env:
            process_env.update(env)
        try:
            completed = subprocess.run(
                list(argv),
                check=False,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=timeout,
                env=process_env,
            )
        except FileNotFoundError as error:
            return CommandResult(127, "", str(error))
        except subprocess.TimeoutExpired:
            return CommandResult(124, "", f"timed out after {timeout}s")
        except OSError as error:
            return CommandResult(126, "", str(error))
        return CommandResult(completed.returncode, completed.stdout, completed.stderr)

    def get_json(
        self,
        url: str,
        *,
        timeout: int = 5,
        headers: Mapping[str, str] | None = None,
    ) -> Any:
        request_headers = {
            "Accept": "application/json",
            "User-Agent": "AgentDesk-toolchain-update/1",
        }
        if headers:
            request_headers.update(headers)
        request = urllib.request.Request(url, headers=request_headers)
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return json.load(response)

def highest_semver(text: str) -> str | None:
    """Return the numerically highest semantic version found in free-form text."""

    matches = [
        ((int(match[0]), int(match[1]), int(match[2])), ".".join(match[:3]))
        for match in _SEMVER_RE.findall(text)
    ]
    return max(matches, default=((), None), key=lambda item: item[0])[1]


def _compact_detail(text: str, limit: int = 240) -> str:
    return " ".join(text.split())[:limit]


def _command_probe(
    runner: Runner,
    argv: Sequence[str],
    *,
    env: Mapping[str, str] | None = None,
    allow_unparsed: bool = False,
) -> ValueProbe:
    result = runner.run(argv, env=env)
    if result.returncode != 0:
        detail = _compact_detail(result.stderr or result.stdout or f"exit {result.returncode}")
        return ValueProbe(False, "unavailable", detail)
    combined = f"{result.stdout}\n{result.stderr}".strip()
    version = highest_semver(combined)
    if version:
        return ValueProbe(True, version, _compact_detail(combined))
    if allow_unparsed and combined:
        return ValueProbe(True, _compact_detail(combined, 80), _compact_detail(combined))
    return ValueProbe(False, "unknown", "command succeeded but no semantic version was found")


def _memento_probe(runner: Runner, base_url: str) -> ValueProbe:
    headers: dict[str, str] = {}
    token = os.environ.get("MEMENTO_ACCESS_KEY", "").strip()
    if token:
        headers["Authorization"] = f"Bearer {token}"
    try:
        payload = runner.get_json(f"{base_url.rstrip('/')}/health", timeout=3, headers=headers)
    except (OSError, ValueError, urllib.error.URLError) as error:
        return ValueProbe(False, "unreachable", _compact_detail(str(error)))
    if not isinstance(payload, dict):
        return ValueProbe(False, "invalid-health-shape", "health response is not a JSON object")
    healthy = payload.get("ok", payload.get("healthy", payload.get("status") in {"ok", "healthy"}))
    version = next(
        (
            value
            for key in ("version", "serviceVersion", "buildVersion")
            if isinstance((value := payload.get(key)), str) and value.strip()
        ),
        None,
    )
    if healthy is not True:
        return ValueProbe(False, version or "unhealthy", _compact_detail(json.dumps(payload)))
    return ValueProbe(True, version or "healthy/version-unreported", "remote health probe succeeded")


def probe_current(spec: ToolSpec, runner: Runner) -> ValueProbe:
    if spec.current_kind == "literal":
        return ValueProbe(True, str(spec.current_value), "configured behavior")
    if spec.current_kind == "memento-health":
        return _memento_probe(runner, str(spec.current_value))
    if spec.current_kind == "manual-command":
        argv = tuple(spec.current_value)
        result = runner.run(argv)
        if result.returncode == 0:
            return ValueProbe(True, "installed/manual-version", _compact_detail(result.stdout))
        return ValueProbe(False, "manual-check-required", _compact_detail(result.stderr))
    if spec.current_kind == "brew-current":
        formula = str(spec.current_value)
        result = runner.run(
            ("brew", "list", "--versions", formula),
            env={"HOMEBREW_NO_AUTO_UPDATE": "1"},
        )
        if result.returncode != 0:
            return ValueProbe(False, "not-installed", _compact_detail(result.stderr))
        tokens = result.stdout.split()[1:]
        versions = [token for token in tokens if _BREW_VERSION_RE.fullmatch(token)]
        version = max(versions, key=_loose_version_key) if versions else None
        return ValueProbe(
            version is not None,
            version or "unknown",
            _compact_detail(result.stdout) or "formula present but version was not parseable",
        )
    if spec.current_kind == "command":
        return _command_probe(runner, tuple(spec.current_value))
    raise ToolchainError(f"unsupported current probe kind for {spec.key}: {spec.current_kind}")


def _npm_latest(runner: Runner, package: str) -> ValueProbe:
    result = runner.run(("npm", "view", package, "version", "--json"))
    if result.returncode != 0:
        return ValueProbe(False, "unavailable", _compact_detail(result.stderr))
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        return ValueProbe(False, "invalid-registry-response", str(error))
    value = payload if isinstance(payload, str) else None
    version = highest_semver(value or "")
    return ValueProbe(
        version is not None,
        version or "invalid-registry-response",
        f"npm package {package}",
    )


def _pypi_latest(runner: Runner, package: str) -> ValueProbe:
    try:
        payload = runner.get_json(f"https://pypi.org/pypi/{package}/json")
        value = payload["info"]["version"]
    except (KeyError, TypeError, ValueError, OSError, urllib.error.URLError) as error:
        return ValueProbe(False, "unavailable", _compact_detail(str(error)))
    version = highest_semver(str(value))
    return ValueProbe(version is not None, version or "invalid-registry-response", f"PyPI {package}")


def _brew_latest(runner: Runner, formula: str) -> ValueProbe:
    result = runner.run(
        ("brew", "info", "--json=v2", formula),
        env={"HOMEBREW_NO_AUTO_UPDATE": "1"},
    )
    if result.returncode != 0:
        return ValueProbe(False, "unavailable", _compact_detail(result.stderr))
    try:
        payload = json.loads(result.stdout)
        formula_payload = payload["formulae"][0]
        stable = formula_payload["versions"]["stable"]
        revision = formula_payload.get("revision", 0)
        if isinstance(revision, bool) or not isinstance(revision, int) or revision < 0:
            raise TypeError("formula revision must be a non-negative integer")
    except (KeyError, IndexError, TypeError, json.JSONDecodeError) as error:
        return ValueProbe(False, "invalid-brew-response", str(error))
    stable_text = str(stable).strip()
    expected_text = f"{stable_text}_{revision}" if revision else stable_text
    version = expected_text if _BREW_VERSION_RE.fullmatch(expected_text) else None
    return ValueProbe(version is not None, version or "invalid-brew-response", f"brew formula {formula}")


def probe_latest(spec: ToolSpec, runner: Runner, *, offline: bool) -> ValueProbe:
    if offline:
        return ValueProbe(False, "offline/not-queried", "latest-version probe explicitly skipped")
    if spec.latest_kind == "npm":
        return _npm_latest(runner, str(spec.latest_value))
    if spec.latest_kind == "pypi":
        return _pypi_latest(runner, str(spec.latest_value))
    if spec.latest_kind == "brew-latest":
        return _brew_latest(runner, str(spec.latest_value))
    if spec.latest_kind == "rustup":
        result = runner.run(tuple(spec.latest_value))
        if result.returncode != 0:
            return ValueProbe(False, "unavailable", _compact_detail(result.stderr))
        stable_line = next(
            (line for line in result.stdout.splitlines() if "stable" in line.lower()),
            result.stdout,
        )
        version = highest_semver(stable_line)
        return ValueProbe(version is not None, version or "unknown", _compact_detail(stable_line))
    if spec.latest_kind in {"manual", "remote-managed"}:
        return ValueProbe(True, str(spec.latest_value), "no local update query or mutation")
    raise ToolchainError(f"unsupported latest probe kind for {spec.key}: {spec.latest_kind}")


def _version_tuple(value: str) -> tuple[int, int, int] | None:
    match = _SEMVER_RE.search(value)
    return tuple(map(int, match.groups())) if match else None


def _loose_version_identity(value: str) -> tuple[tuple[int, ...], str] | None:
    match = re.search(r"(?<!\d)(\d+(?:\.\d+){1,2})([0-9A-Za-z._+-]*)(?!\d)", value)
    if not match:
        return None
    numeric = tuple(int(part) for part in match.group(1).split("."))
    return numeric + (0,) * (3 - len(numeric)), match.group(2)


def _loose_version_key(value: str) -> tuple[tuple[int, ...], str, int] | None:
    identity = _loose_version_identity(value)
    if identity is None:
        return None
    numeric, suffix = identity
    revision_match = re.fullmatch(r"(.*)_([0-9]+)", suffix)
    if revision_match is None:
        # Preserve the prior lexical ordering for non-Homebrew or malformed suffixes.
        return numeric, suffix, 0
    try:
        revision = int(revision_match.group(2))
    except ValueError:
        # Extremely long numeric-looking suffixes can exceed Python's conversion limit.
        return numeric, suffix, 0
    return numeric, revision_match.group(1), revision


def decide_check(spec: ToolSpec, current: ValueProbe, latest: ValueProbe) -> str:
    if spec.report_only:
        if spec.key == "brave-search-mcp" and latest.ok:
            return "approval-required-pin-decision"
        return "report-only"
    if not current.ok:
        return "not-installed-or-current-probe-failed"
    if not latest.ok:
        return "latest-probe-failed"
    current_version = _loose_version_key(current.value)
    latest_version = _loose_version_key(latest.value)
    if current_version is None or latest_version is None:
        return "manual-version-review"
    if latest_version > current_version:
        return "update-available"
    if latest_version == current_version:
        return "current"
    return "installed-newer-than-registry"


def collect_checks(runner: Runner, *, offline: bool = False) -> list[ToolCheck]:
    checks: list[ToolCheck] = []
    for spec in tool_inventory():
        current = (
            ValueProbe(False, "offline/not-queried", "remote health probe explicitly skipped")
            if offline and spec.current_kind == "memento-health"
            else probe_current(spec, runner)
        )
        latest = probe_latest(spec, runner, offline=offline)
        checks.append(
            ToolCheck(
                key=spec.key,
                display_name=spec.display_name,
                method=spec.method,
                tier=spec.tier,
                current=current.value,
                latest=latest.value,
                decision=decide_check(spec, current, latest),
                current_detail=current.detail,
                latest_detail=latest.detail,
                risk=spec.risk,
                changelog_url=spec.changelog_url,
                report_only=spec.report_only,
            )
        )
    return checks


def draft_basis(
    checks: Sequence[ToolCheck | Mapping[str, Any]],
    *,
    generated_at: str,
    draft_nonce: str,
) -> dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "generated_at": generated_at,
        "draft_nonce": draft_nonce,
        "checks": [dict(check) if isinstance(check, Mapping) else asdict(check) for check in checks],
    }


def compute_draft_id(basis: Mapping[str, Any]) -> str:
    canonical = json.dumps(basis, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def render_draft(checks: Sequence[ToolCheck], generated_at: str, draft_id: str) -> str:
    lines = [
        "# AgentDesk toolchain update draft",
        "",
        "> Evidence-only draft. No update command was executed while producing this report.",
        "> The scheduled launchd job is check-only and cannot enter the apply path.",
        "",
        f"- Generated: `{generated_at}`",
        f"- Draft ID: `{draft_id}`",
        f"- Inventory rows: `{len(checks)}`",
        "",
        "## Full inventory",
        "",
        "| Tool | Method | Tier | Current | Latest | Decision |",
        "|---|---|---|---|---|---|",
    ]
    for check in checks:
        values = (
            check.display_name,
            check.method,
            check.tier,
            check.current,
            check.latest,
            check.decision,
        )
        lines.append("| " + " | ".join(value.replace("|", "\\|") for value in values) + " |")

    approval_cards = [
        check
        for check in checks
        if check.tier == "approval"
        and check.decision in {"update-available", "approval-required-pin-decision"}
    ]
    hygiene = [
        check
        for check in checks
        if check.tier == "hygiene" and check.decision == "update-available"
    ]
    lines.extend(["", "## Approval cards", ""])
    if not approval_cards:
        lines.append("No approval-tier update or pin decision is currently pending.")
    for check in approval_cards:
        lines.extend(
            [
                f"### {check.display_name}",
                "",
                f"- Version: `{check.current}` → `{check.latest}`",
                f"- Risk: {check.risk}",
                f"- Changelog: {check.changelog_url}",
                "- Action: review this exact draft, then run the documented `approve` command.",
                "",
            ]
        )
    lines.extend(["## Hygiene candidates", ""])
    if not hygiene:
        lines.append("No hygiene-tier update is currently pending.")
    else:
        for check in hygiene:
            destructive = (
                " (per-tool approval also required: native/self-updater/rustup/npm mutation)"
                if check.method in APPROVAL_REQUIRED_METHODS
                else ""
            )
            lines.append(f"- `{check.display_name}`: `{check.current}` → `{check.latest}`{destructive}")

    exceptions = [
        check
        for check in checks
        if check.decision
        in {
            "not-installed-or-current-probe-failed",
            "latest-probe-failed",
            "manual-version-review",
            "report-only",
        }
    ]
    lines.extend(["", "## Explicit omissions and probe failures", ""])
    if not exceptions:
        lines.append("None.")
    for check in exceptions:
        lines.append(
            f"- `{check.display_name}` — {check.decision}; current: {check.current_detail or check.current}; "
            f"latest: {check.latest_detail or check.latest}"
        )
    lines.extend(
        [
            "",
            "## Gate contract",
            "",
            "Applying requires an explicit safe-window confirmation. Approval-tier tools and all native/self-updater/rustup/npm",
            "mutations require a per-tool approval marker bound to this draft ID. After mutation, exact-version verification",
            "runs first and may stop the batch before smoke. Otherwise the smoke profile runs immediately; failure stops the",
            "batch, emits an alert/rollback-or-pin plan, and exits non-zero.",
            "",
        ]
    )
    return "\n".join(lines)


def _atomic_write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(content, encoding="utf-8")
    temporary.replace(path)


def write_draft(
    checks: Sequence[ToolCheck],
    output_dir: Path,
    *,
    now: datetime | None = None,
) -> tuple[Path, Path, str]:
    generated = (now or datetime.now(timezone.utc)).astimezone(timezone.utc)
    generated_at = generated.isoformat(timespec="seconds").replace("+00:00", "Z")
    draft_nonce = secrets.token_hex(16)
    basis = draft_basis(checks, generated_at=generated_at, draft_nonce=draft_nonce)
    draft_id = compute_draft_id(basis)
    payload = {
        **basis,
        "draft_id": draft_id,
    }
    json_path = output_dir / "latest.json"
    markdown_path = output_dir / "latest.md"
    _atomic_write(json_path, json.dumps(payload, indent=2, sort_keys=True, ensure_ascii=False) + "\n")
    _atomic_write(markdown_path, render_draft(checks, generated_at, draft_id))
    return markdown_path, json_path, draft_id


def load_draft(path: Path) -> dict[str, Any]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ToolchainError(f"could not load draft {path}: {error}") from error
    if not isinstance(payload, dict) or payload.get("schema_version") != SCHEMA_VERSION:
        raise ToolchainError("unsupported or invalid toolchain draft schema")
    checks = payload.get("checks")
    if not isinstance(checks, list):
        raise ToolchainError("toolchain draft checks must be a list")
    generated_at = payload.get("generated_at")
    draft_nonce = payload.get("draft_nonce")
    if not isinstance(generated_at, str) or not generated_at:
        raise ToolchainError("toolchain draft generated_at must be a non-empty string")
    if not isinstance(draft_nonce, str) or not draft_nonce:
        raise ToolchainError("toolchain draft nonce must be a non-empty string")
    basis = draft_basis(checks, generated_at=generated_at, draft_nonce=draft_nonce)
    expected = compute_draft_id(basis)
    if payload.get("draft_id") != expected:
        raise ToolchainError("toolchain draft contents do not match its draft_id")
    return payload


def _approval_dir(draft_path: Path) -> Path:
    return draft_path.parent / f"{draft_path.stem}.approvals"


def approval_path(draft_path: Path, tool: str) -> Path:
    return _approval_dir(draft_path) / f"{tool}.json"


def approve_tool(draft_path: Path, tool: str, confirmation: str) -> Path:
    if confirmation != APPROVAL_CONFIRMATION:
        raise ApprovalError(f"confirmation must be exactly {APPROVAL_CONFIRMATION!r}")
    payload = load_draft(draft_path)
    check = next((item for item in payload["checks"] if item.get("key") == tool), None)
    if check is None:
        raise ApprovalError(f"tool {tool!r} is not present in the draft")
    marker = {
        "schema_version": SCHEMA_VERSION,
        "draft_id": payload["draft_id"],
        "generated_at": payload["generated_at"],
        "draft_nonce": payload["draft_nonce"],
        "tool": tool,
        "current": check.get("current"),
        "latest": check.get("latest"),
        "approved_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    }
    path = approval_path(draft_path, tool)
    _atomic_write(path, json.dumps(marker, indent=2, sort_keys=True) + "\n")
    return path


def _approval_matches(draft_path: Path, draft: Mapping[str, Any], check: Mapping[str, Any]) -> bool:
    path = approval_path(draft_path, str(check["key"]))
    try:
        marker = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return False
    return (
        marker.get("schema_version") == SCHEMA_VERSION
        and marker.get("draft_id") == draft.get("draft_id")
        and marker.get("generated_at") == draft.get("generated_at")
        and marker.get("draft_nonce") == draft.get("draft_nonce")
        and marker.get("tool") == check.get("key")
        and marker.get("current") == check.get("current")
        and marker.get("latest") == check.get("latest")
    )


def validate_cswap_shape(text: str) -> tuple[bool, str]:
    """Validate the required camelCase shape consumed by services/cswap.rs."""

    try:
        payload = json.loads(text)
    except json.JSONDecodeError as error:
        return False, f"invalid JSON: {error}"
    if not isinstance(payload, dict):
        return False, "top-level value must be an object"
    if payload.get("error") is not None:
        return False, "cswap returned an error payload"
    if "schemaVersion" in payload and (
        not isinstance(payload["schemaVersion"], int) or isinstance(payload["schemaVersion"], bool)
    ):
        return False, "schemaVersion must be an integer"
    if (
        "activeAccountNumber" in payload
        and payload["activeAccountNumber"] is not None
        and (
            not isinstance(payload["activeAccountNumber"], int)
            or isinstance(payload["activeAccountNumber"], bool)
        )
    ):
        return False, "activeAccountNumber must be an integer or null"
    if "accounts" not in payload or payload["accounts"] is None:
        return False, "accounts must be a present, non-null list"
    accounts = payload["accounts"]
    if not isinstance(accounts, list):
        return False, "accounts must be a list"
    for index, account in enumerate(accounts):
        if not isinstance(account, dict):
            return False, f"accounts[{index}] must be an object"
        if (
            "number" in account
            and account["number"] is not None
            and (not isinstance(account["number"], int) or isinstance(account["number"], bool))
        ):
            return False, f"accounts[{index}].number must be an integer or null"
        if "active" in account and not isinstance(account["active"], bool):
            return False, f"accounts[{index}].active must be a boolean"
        age = account.get("usageAgeSeconds")
        if age is not None and (not isinstance(age, (int, float)) or isinstance(age, bool)):
            return False, f"accounts[{index}].usageAgeSeconds must be numeric or null"
    return True, f"schemaVersion={payload.get('schemaVersion', 1)}, accounts={len(accounts)}"


def _smoke_version(runner: Runner, name: str, argv: Sequence[str]) -> SmokeResult:
    result = runner.run(argv)
    if result.returncode != 0:
        return SmokeResult(name, False, _compact_detail(result.stderr or result.stdout))
    combined = f"{result.stdout}\n{result.stderr}".strip()
    version = highest_semver(combined)
    return SmokeResult(
        name,
        version is not None,
        f"highest-semver={version}" if version else "no semantic version found",
    )


def _smoke_loose_version(runner: Runner, name: str, argv: Sequence[str]) -> SmokeResult:
    result = runner.run(argv)
    if result.returncode != 0:
        return SmokeResult(name, False, _compact_detail(result.stderr or result.stdout))
    combined = f"{result.stdout}\n{result.stderr}".strip()
    version = _loose_version_key(combined)
    return SmokeResult(
        name,
        version is not None,
        f"version={'.'.join(map(str, version[0]))}{version[1]}" if version else "no parseable version found",
    )


def _smoke_node(runner: Runner) -> list[SmokeResult]:
    results = [_smoke_version(runner, "node --version", ("node", "--version"))]
    npm_list = runner.run(("npm", "ls", "-g", "--depth=0", "--json"))
    if npm_list.returncode != 0:
        results.append(SmokeResult("node global CLI inventory", False, _compact_detail(npm_list.stderr)))
        return results
    try:
        payload = json.loads(npm_list.stdout)
        dependencies = payload.get("dependencies", {})
        if not isinstance(dependencies, dict):
            raise ValueError("dependencies is not an object")
    except (json.JSONDecodeError, ValueError, AttributeError) as error:
        results.append(SmokeResult("node global CLI inventory", False, str(error)))
        return results
    npm_global_specs = [spec for spec in tool_inventory() if spec.method == "npm-g"]
    invalid_specs = [
        spec.key
        for spec in npm_global_specs
        if not isinstance(spec.update_value, str) or isinstance(spec.current_value, str)
    ]
    if invalid_specs:
        results.append(
            SmokeResult(
                "node manifest npm global inventory",
                False,
                f"missing package/binary argv for {','.join(invalid_specs)}",
            )
        )
        return results
    package_binaries = {
        str(spec.update_value): tuple(spec.current_value) for spec in npm_global_specs
    }
    missing = sorted(package_binaries.keys() - dependencies.keys())
    results.append(
        SmokeResult(
            "node global CLI inventory",
            not missing,
            f"expected={len(package_binaries)}, packages={len(dependencies)}, "
            f"missing={','.join(missing) if missing else 'none'}",
        )
    )
    for package, argv in package_binaries.items():
        if package in dependencies:
            results.append(_smoke_version(runner, f"global {package} load", argv))
    return results


def _smoke_postgresql(runner: Runner, *, strict: bool) -> list[SmokeResult]:
    client = _smoke_loose_version(runner, "psql --version", ("psql", "--version"))
    results = [client]
    database_url = os.environ.get("AGENTDESK_DATABASE_URL") or os.environ.get("DATABASE_URL")
    if not database_url:
        results.append(
            SmokeResult(
                "psql server major comparison",
                not strict,
                "DATABASE_URL/AGENTDESK_DATABASE_URL is required for a strict post-update gate",
            )
        )
        return results
    server = runner.run(
        ("psql", "-Atqc", "SHOW server_version", database_url),
    )
    if server.returncode != 0:
        results.append(SmokeResult("psql server major comparison", False, _compact_detail(server.stderr)))
        return results
    client_version = _loose_version_key(client.detail)
    server_version = _loose_version_key(server.stdout)
    matches = (
        client_version is not None
        and server_version is not None
        and client_version[0][0] == server_version[0][0]
    )
    results.append(
        SmokeResult(
            "psql server major comparison",
            matches,
            f"client={client_version[0][0] if client_version else 'unknown'} server={server_version[0][0] if server_version else 'unknown'}",
        )
    )
    return results


def run_smoke_profile(profile: str, runner: Runner, *, strict: bool = True) -> list[SmokeResult]:
    simple = {
        "claude": ("claude", "--version"),
        "codex": ("codex", "--version"),
        "claude-e": ("claude-e", "--version"),
        "tmux": ("tmux", "-V"),
        "gh": ("gh", "--version"),
        "python-3-14": ("python3.14", "--version"),
        "uv": ("uv", "--version"),
        "pipx": ("pipx", "--version"),
        "jq": ("jq", "--version"),
        "ripgrep": ("rg", "--version"),
        "ffmpeg": ("ffmpeg", "-version"),
        "whisper-cpp": ("whisper-cli", "--version"),
        "edge-tts": ("edge-tts", "--version"),
        "opencode": ("opencode", "--version"),
    }
    if profile in simple:
        if profile == "tmux":
            return [_smoke_loose_version(runner, "tmux version", simple[profile])]
        return [_smoke_version(runner, f"{profile} version", simple[profile])]
    if profile == "ocx":
        version = _smoke_version(runner, "ocx version", ("ocx", "--version"))
        health = runner.run(("ocx", "health"), timeout=30)
        return [
            version,
            SmokeResult("ocx health", health.returncode == 0, _compact_detail(health.stdout or health.stderr)),
        ]
    if profile == "cswap":
        version = _smoke_version(runner, "cswap version", ("cswap", "--version"))
        listing = runner.run(("cswap", "--list", "--json"), timeout=60)
        if listing.returncode != 0:
            shape = SmokeResult("cswap --list --json shape", False, _compact_detail(listing.stderr))
        else:
            ok, detail = validate_cswap_shape(listing.stdout)
            shape = SmokeResult("cswap --list --json shape", ok, detail)
        return [version, shape]
    if profile == "rust":
        return [
            _smoke_version(
                runner,
                "stable rustc version",
                ("rustup", "run", "stable", "rustc", "--version"),
            ),
            _smoke_version(
                runner,
                "stable cargo version",
                ("rustup", "run", "stable", "cargo", "--version"),
            ),
        ]
    if profile == "node":
        return _smoke_node(runner)
    if profile == "postgresql-17":
        return _smoke_postgresql(runner, strict=strict)
    raise ToolchainError(f"unsupported smoke profile: {profile}")


def _update_argv(spec: ToolSpec, latest: str) -> tuple[str, ...]:
    if spec.update_kind is None or spec.update_value is None:
        raise ToolchainError(f"{spec.key} is report-only and has no local update command")
    if spec.update_kind in {"npm-exact", "uv-exact", "pipx-exact"} and not _SAFE_VERSION_RE.fullmatch(latest):
        raise ToolchainError(f"refusing unsafe or non-semver version for {spec.key}: {latest!r}")
    if spec.update_kind == "npm-exact":
        return ("npm", "install", "-g", f"{spec.update_value}@{latest}")
    if spec.update_kind == "uv-exact":
        return ("uv", "tool", "install", "--force", f"{spec.update_value}=={latest}")
    if spec.update_kind == "pipx-exact":
        return ("pipx", "install", "--force", f"{spec.update_value}=={latest}")
    if spec.update_kind == "brew-upgrade":
        return ("brew", "upgrade", str(spec.update_value))
    if spec.update_kind == "command":
        return tuple(spec.update_value)
    raise ToolchainError(f"unsupported update kind for {spec.key}: {spec.update_kind}")


def _rollback_or_pin_hint(spec: ToolSpec, current: str) -> str:
    if not _SAFE_VERSION_RE.fullmatch(current):
        return "No exact previous semantic version was recorded; stop and pin manually before retrying."
    if spec.update_kind == "npm-exact":
        return f"npm install -g {spec.update_value}@{current}"
    if spec.update_kind == "uv-exact":
        return f"uv tool install --force {spec.update_value}=={current}"
    if spec.update_kind == "pipx-exact":
        return f"pipx install --force {spec.update_value}=={current}"
    if spec.update_kind == "brew-upgrade":
        return f"brew pin {spec.update_value}; restore {current} from the approved Homebrew rollback procedure"
    return f"restore/pin {spec.display_name} to {current} using its native installer"


def _same_version(left: str, right: str) -> bool:
    left_key = _loose_version_identity(left)
    right_key = _loose_version_identity(right)
    return left_key == right_key if left_key is not None and right_key is not None else left == right


def _update_timeout(spec: ToolSpec) -> int:
    if spec.method == "homebrew":
        return BUILD_UPDATE_TIMEOUT_SECONDS
    return DEFAULT_UPDATE_TIMEOUT_SECONDS


def _write_apply_alert(
    draft_path: Path,
    draft_id: str,
    spec: ToolSpec,
    check: Mapping[str, Any],
    failures: Sequence[SmokeResult],
    *,
    stage: str,
) -> Path:
    path = draft_path.parent / "alerts" / f"{draft_id[:16]}-{spec.key}.md"
    failed = [result for result in failures if not result.ok]
    content = "\n".join(
        [
            f"# Toolchain apply failure: {spec.display_name}",
            "",
            f"- Draft ID: `{draft_id}`",
            f"- Attempted: `{check.get('current')}` → `{check.get('latest')}`",
            f"- Failure stage: {stage}",
            f"- Failed checks: {', '.join(result.check for result in failed)}",
            f"- Failure details: {'; '.join(result.detail for result in failed)}",
            f"- Rollback/pin plan: `{_rollback_or_pin_hint(spec, str(check.get('current', 'unknown')))}`",
            "",
            "The apply batch stopped. Do not restart dcserver or the ocx proxy until the pin/rollback is completed and the smoke profile passes.",
            "",
        ]
    )
    _atomic_write(path, content)
    return path


def assert_safe_window(runner: Runner) -> None:
    """Fail closed unless AgentDesk and local build activity are idle."""

    status = runner.run(("agentdesk", "status", "--json"), timeout=10)
    if status.returncode != 0:
        raise ToolchainError(
            "safe-window status is unavailable; refusing apply: "
            + _compact_detail(status.stderr or status.stdout)
        )
    try:
        payload = json.loads(status.stdout)
        sessions = payload["sessions"]
        queue = payload["queue"]
        working = int(sessions["working"])
        active_dispatches = int(sessions["with_active_dispatch"])
        queue_status = queue.get("status")
        if queue_status is not None and not isinstance(queue_status, str):
            raise TypeError("queue.status must be a string or null")
        queue_busy = queue_status is not None and queue_status not in IDLE_QUEUE_STATUSES
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        raise ToolchainError(f"safe-window status shape is invalid: {error}") from error
    if working or active_dispatches or queue_busy:
        raise ToolchainError(
            "safe window is busy: "
            f"working_sessions={working}, active_dispatches={active_dispatches}, "
            f"queue_status={queue_status or 'idle'}"
        )
    for process in ("cargo", "rustc"):
        running = runner.run(("pgrep", "-x", process), timeout=5)
        if running.returncode == 0:
            raise ToolchainError(f"safe window is busy: {process} process is running")
        if running.returncode != 1:
            raise ToolchainError(
                f"could not verify safe window for {process}: "
                + _compact_detail(running.stderr or running.stdout)
            )
    processes = runner.run(("ps", "-axo", "command="), timeout=5)
    if processes.returncode != 0:
        raise ToolchainError(
            "could not inspect the process table for deploy-release.sh: "
            + _compact_detail(processes.stderr or processes.stdout)
        )
    if any("deploy-release.sh" in line for line in processes.stdout.splitlines()):
        raise ToolchainError("safe window is busy: deploy-release.sh is running")


def apply_draft(
    draft_path: Path,
    *,
    requested_tools: Sequence[str],
    apply_hygiene: bool,
    safe_window_confirmation: str,
    runner: Runner,
) -> tuple[list[str], Path | None]:
    if safe_window_confirmation != SAFE_WINDOW_CONFIRMATION:
        raise ApprovalError(f"safe-window confirmation must be exactly {SAFE_WINDOW_CONFIRMATION!r}")
    assert_safe_window(runner)
    draft = load_draft(draft_path)
    specs = {spec.key: spec for spec in tool_inventory()}
    checks = {str(item.get("key")): item for item in draft["checks"] if isinstance(item, dict)}
    selected = set(requested_tools)
    unknown = selected - specs.keys()
    if unknown:
        raise ToolchainError(f"unknown tool(s): {', '.join(sorted(unknown))}")
    stale_manifest = checks.keys() - specs.keys()
    if stale_manifest:
        raise ToolchainError(
            "draft contains tool(s) absent from the current manifest: "
            f"{', '.join(sorted(stale_manifest))}; generate a fresh draft"
        )
    if apply_hygiene:
        selected.update(
            key
            for key, check in checks.items()
            if specs[key].tier == "hygiene" and check.get("decision") == "update-available"
        )
    if not selected:
        raise ToolchainError("no tools selected; use --tool and/or --apply-hygiene")

    ordered = [spec for spec in tool_inventory() if spec.key in selected]
    for spec in ordered:
        check = checks.get(spec.key)
        if check is None or check.get("decision") != "update-available":
            raise ToolchainError(f"{spec.key} is not an update-available row in this draft")
        if spec.report_only:
            raise ToolchainError(f"{spec.key} is report-only")
        needs_approval = spec.tier == "approval" or spec.method in APPROVAL_REQUIRED_METHODS
        if needs_approval and not _approval_matches(draft_path, draft, check):
            raise ApprovalError(
                f"{spec.key} requires a per-tool approval marker matching draft {draft['draft_id']}"
            )
        current = probe_current(spec, runner)
        latest = probe_latest(spec, runner, offline=False)
        if not current.ok or not _same_version(current.value, str(check["current"])):
            raise ToolchainError(
                f"stale draft for {spec.key}: expected current={check['current']}, observed={current.value}"
            )
        if not latest.ok or not _same_version(latest.value, str(check["latest"])):
            raise ToolchainError(
                f"stale draft for {spec.key}: expected latest={check['latest']}, observed={latest.value}"
            )

    applied: list[str] = []
    for spec in ordered:
        check = checks[spec.key]
        argv = _update_argv(spec, str(check["latest"]))
        assert_safe_window(runner)
        update_result = runner.run(
            argv,
            timeout=_update_timeout(spec),
            env={"HOMEBREW_NO_AUTO_UPDATE": "1"},
        )
        if update_result.returncode != 0:
            failure = SmokeResult(
                "update command",
                False,
                _compact_detail(
                    update_result.stderr
                    or update_result.stdout
                    or f"exit {update_result.returncode}"
                ),
            )
            alert = _write_apply_alert(
                draft_path,
                str(draft["draft_id"]),
                spec,
                check,
                [failure],
                stage="update command",
            )
            return applied, alert
        try:
            observed = probe_current(spec, runner)
        except Exception as error:
            failure = SmokeResult("post-update version probe", False, _compact_detail(str(error)))
            alert = _write_apply_alert(
                draft_path,
                str(draft["draft_id"]),
                spec,
                check,
                [failure],
                stage="post-update version probe",
            )
            return applied, alert
        exact_version = SmokeResult(
            "post-update exact approved version",
            observed.ok and _same_version(observed.value, str(check["latest"])),
            f"expected={check['latest']}, observed={observed.value}",
        )
        if not exact_version.ok:
            alert = _write_apply_alert(
                draft_path,
                str(draft["draft_id"]),
                spec,
                check,
                [exact_version],
                stage="post-update exact-version verification (smoke not run)",
            )
            return applied, alert
        try:
            smoke = run_smoke_profile(spec.smoke_profile or spec.key, runner, strict=True)
        except Exception as error:
            failure = SmokeResult("post-update smoke setup", False, _compact_detail(str(error)))
            alert = _write_apply_alert(
                draft_path,
                str(draft["draft_id"]),
                spec,
                check,
                [failure],
                stage="post-update smoke setup",
            )
            return applied, alert
        if any(not result.ok for result in smoke):
            alert = _write_apply_alert(
                draft_path,
                str(draft["draft_id"]),
                spec,
                check,
                smoke,
                stage="post-update smoke",
            )
            return applied, alert
        applied.append(spec.key)
    return applied, None


def _check_command(args: argparse.Namespace, runner: Runner) -> int:
    checks = collect_checks(runner, offline=args.offline)
    markdown, json_path, draft_id = write_draft(checks, args.output_dir)
    print(f"toolchain draft written: {markdown}")
    print(f"toolchain draft data: {json_path}")
    print(f"draft_id={draft_id}")
    print(f"inventory_rows={len(checks)} updates_executed=0")
    return 0


def _approve_command(args: argparse.Namespace) -> int:
    path = approve_tool(args.draft, args.tool, args.confirm)
    print(f"approval marker written: {path}")
    return 0


def _apply_command(args: argparse.Namespace, runner: Runner) -> int:
    applied, alert = apply_draft(
        args.draft,
        requested_tools=args.tool,
        apply_hygiene=args.apply_hygiene,
        safe_window_confirmation=args.confirm_safe_window,
        runner=runner,
    )
    if alert is not None:
        print(f"APPLY FAILED; apply batch stopped; alert={alert}", file=sys.stderr)
        return 3
    print(f"applied_and_smoked={','.join(applied)}")
    return 0


def _smoke_command(args: argparse.Namespace, runner: Runner) -> int:
    specs = {spec.key: spec for spec in tool_inventory()}
    selected = args.tool or [
        "claude",
        "codex",
        "ocx",
        "cswap",
        "tmux",
        "node",
        "postgresql-17",
    ]
    results: list[SmokeResult] = []
    for key in selected:
        spec = specs.get(key)
        if spec is None or spec.smoke_profile is None:
            raise ToolchainError(f"tool {key!r} has no smoke profile")
        results.extend(run_smoke_profile(spec.smoke_profile, runner, strict=not args.allow_server_skip))
    print(json.dumps([asdict(result) for result in results], indent=2, sort_keys=True))
    return 0 if all(result.ok for result in results) else 1


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    root = Path(os.environ.get("AGENTDESK_ROOT_DIR", Path.home() / ".adk" / "release"))
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    check = subparsers.add_parser("check", help="write a read-only current/latest draft")
    check.add_argument("--output-dir", type=Path, default=root / "runtime" / "toolchain-update")
    check.add_argument("--offline", action="store_true", help="skip all latest/network probes")

    approve = subparsers.add_parser("approve", help="approve one tool in an exact draft")
    approve.add_argument("--draft", type=Path, required=True)
    approve.add_argument("--tool", required=True)
    approve.add_argument("--confirm", required=True)

    apply = subparsers.add_parser("apply", help="apply selected approved/hygiene updates")
    apply.add_argument("--draft", type=Path, required=True)
    apply.add_argument("--tool", action="append", default=[])
    apply.add_argument("--apply-hygiene", action="store_true")
    apply.add_argument("--confirm-safe-window", required=True)

    smoke = subparsers.add_parser("smoke", help="run the post-update smoke gate")
    smoke.add_argument("--tool", action="append", default=[])
    smoke.add_argument(
        "--allow-server-skip",
        action="store_true",
        help="allow psql client-only diagnostics outside a post-update gate",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None, *, runner: Runner | None = None) -> int:
    args = parse_args(argv)
    active_runner = runner or Runner()
    try:
        if args.command == "check":
            return _check_command(args, active_runner)
        if args.command == "approve":
            return _approve_command(args)
        if args.command == "apply":
            return _apply_command(args, active_runner)
        if args.command == "smoke":
            return _smoke_command(args, active_runner)
    except ToolchainError as error:
        print(f"toolchain-update: ERROR: {error}", file=sys.stderr)
        return 2
    raise AssertionError(f"unhandled command: {args.command}")


if __name__ == "__main__":
    raise SystemExit(main())
