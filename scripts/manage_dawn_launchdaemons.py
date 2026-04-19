#!/usr/bin/env python3
"""Manage dawn LaunchDaemon jobs for observability skills on a macOS host.

Typical flow:
1. `preflight` to verify python path, skill roots, and sudo readiness.
2. `bootstrap` once under sudo to install the sudoers drop-in and the plists.
3. `status` later as an unprivileged operator command.
"""

from __future__ import annotations

import argparse
import os
import plistlib
import pwd
import shutil
import stat
import subprocess
import sys
import tempfile
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator, Optional, Sequence


SCRIPT_PATH = Path(__file__).resolve()
REPO_ROOT = SCRIPT_PATH.parents[1]
SUDOERS_TARGET = Path("/etc/sudoers.d/agentdesk-dawn-manager")


@dataclass(frozen=True)
class DawnJobSpec:
    skill_name: str
    manager_relpath: Path
    daemon_plist_relpath: Path


@dataclass(frozen=True)
class ResolvedDawnJob:
    name: str
    skill_root: Path
    manager_script: Path
    daemon_plist: Path

    @property
    def installed_target(self) -> Path:
        return Path("/Library/LaunchDaemons") / self.daemon_plist.name


JOB_SPECS = {
    "memory-dream": DawnJobSpec(
        skill_name="memory-dream",
        manager_relpath=Path("scripts/manage_memory_dream_launchd.py"),
        daemon_plist_relpath=Path("launchd/com.agentdesk.memory-dream-dawn.plist"),
    ),
    "service-monitoring": DawnJobSpec(
        skill_name="service-monitoring",
        manager_relpath=Path("scripts/manage_service_monitoring_launchd.py"),
        daemon_plist_relpath=Path("launchd/com.agentdesk.service-monitoring-dawn.plist"),
    ),
    "version-watch": DawnJobSpec(
        skill_name="version-watch",
        manager_relpath=Path("scripts/manage_version_watch_launchd.py"),
        daemon_plist_relpath=Path("launchd/com.agentdesk.version-watch-dawn.plist"),
    ),
    "hardware-audit": DawnJobSpec(
        skill_name="hardware-audit",
        manager_relpath=Path("scripts/manage_hardware_audit_launchd.py"),
        daemon_plist_relpath=Path("launchd/com.agentdesk.hardware-audit-dawn.plist"),
    ),
}


def run_command(command: Sequence[str]) -> subprocess.CompletedProcess:
    return subprocess.run(
        list(command),
        capture_output=True,
        text=True,
        check=False,
    )


def preferred_python_bin() -> Path:
    homebrew_python = Path("/opt/homebrew/bin/python3")
    if homebrew_python.exists():
        return homebrew_python
    python3_path = shutil.which("python3")
    if python3_path:
        return Path(python3_path)
    return Path(sys.executable)


def current_user_name() -> str:
    sudo_user = os.environ.get("SUDO_USER")
    if sudo_user:
        return sudo_user
    try:
        return pwd.getpwuid(os.getuid()).pw_name
    except Exception:
        return "agentdesk"


def home_for_user(user_name: str) -> Optional[Path]:
    try:
        return Path(pwd.getpwnam(user_name).pw_dir)
    except Exception:
        return None


def invoking_home() -> Path:
    sudo_user = os.environ.get("SUDO_USER")
    if sudo_user:
        home = home_for_user(sudo_user)
        if home is not None:
            return home
    return Path.home()


def split_env_roots(raw: str | None) -> list[Path]:
    if not raw:
        return []
    roots: list[Path] = []
    for item in raw.split(os.pathsep):
        item = item.strip()
        if item:
            roots.append(Path(item).expanduser())
    return roots


def unique_paths(paths: Sequence[Path]) -> list[Path]:
    unique: list[Path] = []
    seen: set[Path] = set()
    for path in paths:
        resolved = path.expanduser()
        if resolved in seen:
            continue
        seen.add(resolved)
        unique.append(resolved)
    return unique


def default_skills_roots() -> list[Path]:
    # Search the most common local runtime layouts before falling back to repo-local skills.
    roots: list[Path] = []
    roots.extend(split_env_roots(os.environ.get("AGENTDESK_SKILLS_ROOT")))
    codex_home = os.environ.get("CODEX_HOME")
    if codex_home:
        roots.append(Path(codex_home).expanduser() / "skills")
    home = invoking_home()
    roots.append(home / ".codex/skills")
    roots.append(home / ".adk/release/skills")
    roots.append(REPO_ROOT / "skills")
    return unique_paths(roots)


def trusted_root_python_bin() -> Path:
    candidates = unique_paths(
        [
            preferred_python_bin().expanduser().resolve(),
            Path("/usr/bin/python3"),
            Path("/usr/bin/python"),
            Path(sys.executable).expanduser().resolve(),
        ]
        + [
            Path(candidate).expanduser().resolve()
            for candidate in (shutil.which("python3"), shutil.which("python"))
            if candidate
        ]
    )
    for candidate in candidates:
        if path_is_root_owned_and_locked(candidate):
            return candidate
    return candidates[0]


def path_is_root_owned_and_locked(path: Path) -> bool:
    try:
        st = path.stat()
    except OSError:
        return False
    return st.st_uid == 0 and not (st.st_mode & (stat.S_IWGRP | stat.S_IWOTH))


def privileged_root_requested(args: argparse.Namespace) -> bool:
    if args.action == "status" and args.as_root and os.geteuid() == 0:
        return True
    return action_needs_privileged_reexec(args.action) and (args.as_root or os.geteuid() == 0)


def effective_manager_python(args: argparse.Namespace) -> Path:
    if privileged_root_requested(args):
        return trusted_root_python_bin()
    return Path(args.python_bin).expanduser()


def validate_privileged_entrypoint(python_bin: Path) -> None:
    for label, path in (
        ("manager_python", python_bin),
        ("script_path", SCRIPT_PATH),
    ):
        if not path_is_root_owned_and_locked(path):
            raise PermissionError(
                f"{label} must be root-owned and not group/other-writable for privileged actions: {path}"
            )


def validate_privileged_job_artifacts(job: ResolvedDawnJob, python_bin: Path) -> None:
    validate_privileged_entrypoint(python_bin)
    for label, path in (
        ("skill_root", job.skill_root),
        ("manager_script", job.manager_script),
        ("daemon_plist", job.daemon_plist),
    ):
        if not path_is_root_owned_and_locked(path):
            raise PermissionError(
                f"{job.name} {label} must be root-owned and not group/other-writable for privileged actions: {path}"
            )


def candidate_skills_roots(args: argparse.Namespace) -> list[Path]:
    if args.skills_root:
        return unique_paths([Path(item).expanduser() for item in args.skills_root])
    return default_skills_roots()


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Single entrypoint for dawn LaunchDaemon install/status/bootstrap operations.",
        epilog=(
            "Examples:\n"
            "  python3 scripts/manage_dawn_launchdaemons.py preflight\n"
            "  sudo python3 scripts/manage_dawn_launchdaemons.py bootstrap\n"
            "  python3 scripts/manage_dawn_launchdaemons.py status --job memory-dream\n"
            "  python3 scripts/manage_dawn_launchdaemons.py install --hour 5 --minute 30\n"
            "  python3 scripts/manage_dawn_launchdaemons.py preflight --skills-root ~/.codex/skills\n"
            "  python3 scripts/manage_dawn_launchdaemons.py sudoers\n\n"
            "Action summary:\n"
            "  bootstrap  install sudoers drop-in and run install for the selected jobs\n"
            "  install    install or refresh LaunchDaemon plists\n"
            "  status     inspect configured jobs without requiring root by default\n"
            "  uninstall  remove installed LaunchDaemon plists\n"
            "  preflight  verify python path, skill roots, plist validity, and sudo readiness\n"
            "  sudoers    print the exact sudoers drop-in content for manual review"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "action",
        choices=["bootstrap", "install", "status", "uninstall", "preflight", "sudoers"],
        help="operation to run for the selected dawn jobs",
    )
    parser.add_argument(
        "--job",
        action="append",
        choices=sorted(JOB_SPECS),
        help="limit to one or more named jobs; defaults to all supported dawn jobs",
    )
    parser.add_argument(
        "--hour",
        type=int,
        help="override install schedule hour (0-23); requires --minute and only works with install/bootstrap",
    )
    parser.add_argument(
        "--minute",
        type=int,
        help="override install schedule minute (0-59); requires --hour and only works with install/bootstrap",
    )
    parser.add_argument(
        "--python-bin",
        default=str(preferred_python_bin()),
        help="python binary used for sudo re-exec and downstream manager scripts",
    )
    parser.add_argument(
        "--sudoers-user",
        default=current_user_name(),
        help="user name to emit in the sudoers example and bootstrap drop-in",
    )
    parser.add_argument(
        "--skills-root",
        action="append",
        help="explicit skills root to search; pass multiple times to search more than one location",
    )
    parser.add_argument("--as-root", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args(list(argv) if argv is not None else None)
    validate_schedule_args(args)
    return args


def validate_schedule_args(args: argparse.Namespace) -> None:
    if (args.hour is None) != (args.minute is None):
        raise SystemExit("--hour and --minute must be provided together")
    if args.hour is None:
        return
    if args.action not in {"install", "bootstrap"}:
        raise SystemExit("schedule override is only supported for install/bootstrap")
    if not 0 <= args.hour <= 23:
        raise SystemExit("--hour must be between 0 and 23")
    if not 0 <= args.minute <= 59:
        raise SystemExit("--minute must be between 0 and 59")


def action_needs_privileged_reexec(action: str) -> bool:
    return action in {"bootstrap", "install", "uninstall"}


def selected_job_specs(names: Optional[Sequence[str]]) -> list[tuple[str, DawnJobSpec]]:
    if not names:
        return list(JOB_SPECS.items())
    return [(name, JOB_SPECS[name]) for name in names]


def resolve_job_artifacts(
    job_name: str,
    spec: DawnJobSpec,
    *,
    skills_roots: Sequence[Path],
) -> ResolvedDawnJob:
    searched: list[str] = []
    for root in skills_roots:
        skill_root = root / spec.skill_name
        manager_script = skill_root / spec.manager_relpath
        daemon_plist = skill_root / spec.daemon_plist_relpath
        searched.append(str(skill_root))
        if manager_script.exists() and daemon_plist.exists():
            return ResolvedDawnJob(
                name=job_name,
                skill_root=skill_root,
                manager_script=manager_script,
                daemon_plist=daemon_plist,
            )
    raise FileNotFoundError(
        f"could not resolve `{job_name}` under any skills root: {', '.join(searched) or '(none)'}"
    )


def build_schedule_override(source_plist: Path, *, hour: int, minute: int) -> Path:
    with source_plist.open("rb") as handle:
        plist = plistlib.load(handle)
    plist["StartCalendarInterval"] = {"Hour": hour, "Minute": minute}
    tmp = tempfile.NamedTemporaryFile(prefix="dawn-launchd-", suffix=".plist", delete=False)
    try:
        with open(tmp.name, "wb") as handle:
            plistlib.dump(plist, handle)
    finally:
        tmp.close()
    return Path(tmp.name)


@contextmanager
def schedule_override_path(
    daemon_plist: Path,
    *,
    hour: Optional[int],
    minute: Optional[int],
) -> Iterator[Optional[Path]]:
    if hour is None or minute is None:
        yield None
        return

    override_path = build_schedule_override(daemon_plist, hour=hour, minute=minute)
    try:
        yield override_path
    finally:
        override_path.unlink(missing_ok=True)


def run_manager(
    job: ResolvedDawnJob,
    action: str,
    *,
    python_bin: Path,
    hour: Optional[int],
    minute: Optional[int],
) -> subprocess.CompletedProcess:
    command = [str(python_bin), str(job.manager_script), action, "--scope", "daemon"]
    with schedule_override_path(job.daemon_plist, hour=hour, minute=minute) as override_path:
        if override_path is not None:
            command.extend(["--source", str(override_path)])
        return run_command(command)


def summarize_result(job_name: str, result: subprocess.CompletedProcess) -> list[str]:
    lines = [f"## {job_name}"]
    status = "ok" if result.returncode == 0 else "needs-attention"
    lines.append(f"- status: `{status}`")
    lines.append(f"- exit_code: `{result.returncode}`")
    stdout = (result.stdout or "").strip()
    stderr = (result.stderr or "").strip()
    if stdout:
        for line in stdout.splitlines()[:14]:
            lines.append(f"- output: `{line}`")
    if stderr:
        for line in stderr.splitlines()[:8]:
            lines.append(f"- stderr: `{line}`")
    return lines


def summarize_resolution_error(job_name: str, exc: Exception) -> list[str]:
    return [
        f"## {job_name}",
        "- status: `needs-attention`",
        "- exit_code: `1`",
        f"- stderr: `{str(exc)}`",
    ]


def render_batch_summary(args: argparse.Namespace, jobs: Sequence[tuple[str, DawnJobSpec]]) -> int:
    all_ok = True
    skills_roots = candidate_skills_roots(args)
    manager_python = effective_manager_python(args)
    summary_lines = [
        "# Dawn LaunchDaemons",
        "",
        f"- action: `{args.action}`",
        f"- job_count: `{len(jobs)}`",
        f"- execution_user: `{current_user_name()}`",
        f"- execution_uid: `{os.geteuid()}`",
        f"- python: `{sys.executable}`",
        f"- manager_python: `{manager_python}`",
        f"- schedule: `{args.hour:02d}:{args.minute:02d}`" if args.hour is not None else "- schedule: `default`",
        f"- skills_roots: `{', '.join(str(path) for path in skills_roots) or '(none)'}`",
        "",
    ]

    for index, (job_name, spec) in enumerate(jobs):
        try:
            resolved = resolve_job_artifacts(job_name, spec, skills_roots=skills_roots)
        except FileNotFoundError as exc:
            all_ok = False
            summary_lines.extend(summarize_resolution_error(job_name, exc))
        else:
            if privileged_root_requested(args):
                try:
                    validate_privileged_job_artifacts(resolved, manager_python)
                except PermissionError as exc:
                    all_ok = False
                    summary_lines.extend(summarize_resolution_error(job_name, exc))
                    if index != len(jobs) - 1:
                        summary_lines.append("")
                    continue
            result = run_manager(
                resolved,
                args.action,
                python_bin=manager_python,
                hour=args.hour,
                minute=args.minute,
            )
            if result.returncode != 0:
                all_ok = False
            summary_lines.extend(summarize_result(job_name, result))
        if index != len(jobs) - 1:
            summary_lines.append("")

    print("\n".join(summary_lines))
    return 0 if all_ok else 1


def build_self_command(
    args: argparse.Namespace,
    *,
    action: Optional[str] = None,
    as_root: bool = False,
    job_names: Optional[Sequence[str]] = None,
) -> list[str]:
    command = [str(Path(args.python_bin)), str(SCRIPT_PATH)]
    if as_root:
        command.append("--as-root")
    command.append(action or args.action)
    for job_name in (job_names if job_names is not None else args.job or []):
        command.extend(["--job", job_name])
    if args.hour is not None:
        command.extend(["--hour", str(args.hour), "--minute", str(args.minute)])
    command.extend(["--python-bin", str(Path(args.python_bin))])
    command.extend(["--sudoers-user", args.sudoers_user])
    for skills_root in args.skills_root or []:
        command.extend(["--skills-root", skills_root])
    return command


def print_subprocess_output(result: subprocess.CompletedProcess) -> None:
    stdout = (result.stdout or "").strip()
    stderr = (result.stderr or "").strip()
    if stdout:
        print(stdout)
    if stderr:
        print(stderr, file=sys.stderr)


def run_via_sudo(args: argparse.Namespace) -> int:
    # Keep the public operator entrypoint stable; privilege escalation only re-enters this script.
    forwarded = build_self_command(args, as_root=True)
    forwarded[0] = str(trusted_root_python_bin())
    for index, token in enumerate(forwarded[:-1]):
        if token == "--python-bin":
            forwarded[index + 1] = str(trusted_root_python_bin())
    if not args.skills_root:
        forwarded.extend(
            token
            for path in candidate_skills_roots(args)
            for token in ("--skills-root", str(path))
        )
    command = ["sudo", "-n"] + forwarded
    result = run_command(command)
    print_subprocess_output(result)
    return result.returncode


def build_preflight_probe_command(args: argparse.Namespace, probe_job_name: str) -> list[str]:
    command = [
        "sudo",
        "-n",
        str(trusted_root_python_bin()),
        str(SCRIPT_PATH),
        "--as-root",
        "status",
        "--python-bin",
        str(trusted_root_python_bin()),
        "--sudoers-user",
        args.sudoers_user,
        "--job",
        probe_job_name,
    ]
    for path in candidate_skills_roots(args):
        command.extend(["--skills-root", str(path)])
    return command


def plist_valid(path: Path) -> bool:
    if not path.exists():
        return False
    return run_command(["plutil", "-lint", str(path)]).returncode == 0


def sudoers_text(*, user_name: str, python_bin: Path, script_path: Path) -> str:
    return "\n".join(
        [
            "# /etc/sudoers.d/agentdesk-dawn-manager",
            "# Install with: sudo visudo -f /etc/sudoers.d/agentdesk-dawn-manager",
            "",
            f"User_Alias AGENTDESK_RUNTIME = {user_name}",
            "",
            "Cmnd_Alias AGENTDESK_DAWN_MANAGER = \\",
            f"    {python_bin} {script_path} *",
            "",
            "AGENTDESK_RUNTIME ALL = (root) NOPASSWD: AGENTDESK_DAWN_MANAGER",
        ]
    )


def visudo_bin() -> Path:
    candidate = shutil.which("visudo")
    if candidate:
        return Path(candidate)
    return Path("/usr/sbin/visudo")


def install_sudoers_dropin(*, user_name: str, python_bin: Path, script_path: Path) -> tuple[bool, str]:
    target_dir = SUDOERS_TARGET.parent
    target_dir.mkdir(parents=True, exist_ok=True)
    tmp = tempfile.NamedTemporaryFile(prefix="agentdesk-dawn-sudoers-", dir=target_dir, delete=False)
    tmp_path = Path(tmp.name)
    tmp.close()

    try:
        tmp_path.write_text(
            sudoers_text(user_name=user_name, python_bin=python_bin, script_path=script_path) + "\n",
            encoding="utf-8",
        )
        os.chmod(tmp_path, 0o440)

        validator = visudo_bin()
        validation = run_command([str(validator), "-cf", str(tmp_path)])
        if validation.returncode != 0:
            detail = (validation.stderr or validation.stdout or "visudo validation failed").strip()
            return False, detail

        tmp_path.replace(SUDOERS_TARGET)
        os.chmod(SUDOERS_TARGET, 0o440)
        return True, f"installed sudoers drop-in: {SUDOERS_TARGET}"
    finally:
        if tmp_path.exists():
            tmp_path.unlink(missing_ok=True)


def access_denied(stderr: str) -> bool:
    lowered = stderr.lower()
    fragments = [
        "a password is required",
        "is not in the sudoers file",
        "not allowed to run sudo",
        "may not run sudo",
        "user is not allowed",
    ]
    return any(fragment in lowered for fragment in fragments)


def render_preflight(args: argparse.Namespace, jobs: Sequence[tuple[str, DawnJobSpec]]) -> int:
    python_bin = Path(args.python_bin).expanduser()
    skills_roots = candidate_skills_roots(args)
    manager_python = effective_manager_python(args)
    if python_bin.exists():
        version_result = run_command([str(python_bin), "--version"])
        invocation_python_version = version_result.stdout.strip() or version_result.stderr.strip() or "unknown"
    else:
        invocation_python_version = "missing"
    lines = [
        "# Dawn LaunchDaemon Preflight",
        "",
        f"- script_path: `{SCRIPT_PATH}`",
        f"- invocation_python: `{python_bin}`",
        f"- manager_python: `{manager_python}`",
        f"- runtime_python: `{sys.executable}`",
        f"- invocation_python_exists: `{python_bin.exists()}`",
        f"- invocation_python_version: `{invocation_python_version}`",
        f"- sudoers_user: `{args.sudoers_user}`",
        f"- skills_roots: `{', '.join(str(path) for path in skills_roots) or '(none)'}`",
        "",
    ]

    all_ok = python_bin.exists()
    for job_name, spec in jobs:
        try:
            resolved = resolve_job_artifacts(job_name, spec, skills_roots=skills_roots)
        except FileNotFoundError as exc:
            lines.extend(
                [
                    f"## {job_name}",
                    "- manager_exists: `False`",
                    "- source_exists: `False`",
                    f"- detail: `{str(exc)}`",
                    "",
                ]
            )
            all_ok = False
            continue

        manager_exists = resolved.manager_script.exists()
        source_exists = resolved.daemon_plist.exists()
        source_valid = plist_valid(resolved.daemon_plist) if source_exists else False
        target_exists = resolved.installed_target.exists()
        trusted_for_privileged_actions = True
        if action_needs_privileged_reexec("install"):
            try:
                validate_privileged_job_artifacts(resolved, manager_python)
            except PermissionError:
                trusted_for_privileged_actions = False
        lines.extend(
            [
                f"## {job_name}",
                f"- skill_root: `{resolved.skill_root}`",
                f"- manager_exists: `{manager_exists}`",
                f"- manager_path: `{resolved.manager_script}`",
                f"- source_exists: `{source_exists}`",
                f"- source_valid: `{source_valid}`",
                f"- source_path: `{resolved.daemon_plist}`",
                f"- installed_target_exists: `{target_exists}`",
                f"- installed_target_path: `{resolved.installed_target}`",
                f"- privileged_trusted: `{trusted_for_privileged_actions}`",
                "",
            ]
        )
        all_ok = all_ok and manager_exists and source_exists and source_valid and trusted_for_privileged_actions

    probe_job_name = jobs[0][0]
    probe_command = build_preflight_probe_command(args, probe_job_name)
    probe_result = run_command(probe_command)
    probe_access = probe_result.returncode == 0 and not access_denied(probe_result.stderr or "")
    lines.extend(
        [
            "## sudo_probe",
            f"- command: `{' '.join(probe_command)}`",
            f"- access_ready: `{probe_access}`",
            f"- exit_code: `{probe_result.returncode}`",
        ]
    )
    if probe_result.stderr:
        for line in probe_result.stderr.strip().splitlines()[:8]:
            lines.append(f"- stderr: `{line}`")
    if probe_result.stdout:
        for line in probe_result.stdout.strip().splitlines()[:8]:
            lines.append(f"- stdout: `{line}`")

    lines.extend(
        [
            "",
            "## next_steps",
            f"- install sudoers file with `python3 {SCRIPT_PATH} sudoers` output",
            f"- or run `sudo {python_bin} {SCRIPT_PATH} bootstrap` once",
            f"- status can be checked with `python3 {SCRIPT_PATH} status`",
        ]
    )
    print("\n".join(lines))
    return 0 if (all_ok and probe_access) else 1


def namespace_for_action(args: argparse.Namespace, action: str) -> argparse.Namespace:
    return argparse.Namespace(
        action=action,
        job=args.job,
        hour=args.hour,
        minute=args.minute,
        python_bin=args.python_bin,
        sudoers_user=args.sudoers_user,
        skills_root=args.skills_root,
        as_root=True,
    )


def run_bootstrap(args: argparse.Namespace, jobs: Sequence[tuple[str, DawnJobSpec]]) -> int:
    python_bin = effective_manager_python(args)
    # Bootstrap is the one-time setup path: install sudoers, then install the selected plists.
    try:
        validate_privileged_entrypoint(python_bin)
    except PermissionError as exc:
        print(
            "\n".join(
                [
                    "# Dawn LaunchDaemon Bootstrap",
                    "",
                    f"- sudoers_target: `{SUDOERS_TARGET}`",
                    f"- sudoers_user: `{args.sudoers_user}`",
                    f"- invocation_python: `{python_bin}`",
                    "- sudoers_installed: `False`",
                    f"- detail: `{str(exc)}`",
                ]
            )
        )
        return 1
    sudoers_ok, sudoers_message = install_sudoers_dropin(
        user_name=args.sudoers_user,
        python_bin=python_bin,
        script_path=SCRIPT_PATH,
    )

    lines = [
        "# Dawn LaunchDaemon Bootstrap",
        "",
        f"- sudoers_target: `{SUDOERS_TARGET}`",
        f"- sudoers_user: `{args.sudoers_user}`",
        f"- invocation_python: `{python_bin}`",
        f"- sudoers_installed: `{sudoers_ok}`",
        f"- detail: `{sudoers_message}`",
        "",
    ]
    print("\n".join(lines))
    if not sudoers_ok:
        return 1

    return render_batch_summary(namespace_for_action(args, "install"), jobs)


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    jobs = selected_job_specs(args.job)

    if args.action == "sudoers":
        print(
            sudoers_text(
                user_name=args.sudoers_user,
                python_bin=effective_manager_python(args),
                script_path=SCRIPT_PATH,
            )
        )
        return 0

    if args.action == "preflight":
        return render_preflight(args, jobs)

    if action_needs_privileged_reexec(args.action) and not args.as_root and os.geteuid() != 0:
        return run_via_sudo(args)

    if args.action == "bootstrap":
        return run_bootstrap(args, jobs)

    return render_batch_summary(args, jobs)


if __name__ == "__main__":
    raise SystemExit(main())
