"""Command doubles for the reviewed release-script fixture, never a host PATH fallback."""

import fcntl
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile

ROOT = Path(os.environ["DRY_ROOT"]).resolve()
NAME = Path(sys.argv[0]).name
ARGS = sys.argv[1:]
SOURCE_PROBE = ["-c", "core.fsmonitor=false", "-c", "core.trustctime=true", "-c", "core.checkStat=default"]


def blocked(reason):
    with (ROOT / "blocked").open("a") as log:
        log.write(reason + "\n")
    raise SystemExit(97)


def local(raw):
    path = Path(raw).resolve()
    if not path.is_relative_to(ROOT):
        blocked(f"path outside dry root: {raw}")
    return path


def token_held(marker):
    try:
        dev, ino, pid, start = marker.split(":", 3)
        with (ROOT / "token").open("r+") as token:
            info = os.fstat(token.fileno())
            if (info.st_dev, info.st_ino, os.getppid()) != (int(dev), int(ino), int(pid)) or not start:
                return False
            try:
                fcntl.flock(token, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return True
            return False
    except (OSError, ValueError):
        return False


event = {"command": NAME, "argv": ARGS}
if NAME == "cargo":
    profile = next((arg.split("=", 1)[1] for arg in ARGS if arg.startswith("--profile=")), "")
    if "--profile" in ARGS:
        profile = ARGS[ARGS.index("--profile") + 1]
    marker = os.environ.get("ADK_BUILD_TOKEN_HOLDER", "")
    event.update(holder=marker, held=token_held(marker),
                 release="--release" in ARGS or "-r" in ARGS or profile.startswith("release"),
                 checkout_sha_in_cargo="ADK_RELEASE_CHECKOUT_SHA" in os.environ,
                 in_target_checkout=Path.cwd() == ROOT / "repo")
elif NAME == "git" and ARGS[2:8] == SOURCE_PROBE:
    event.update(source_probe=True, held=token_held(os.environ.get("ADK_BUILD_TOKEN_HOLDER", "")))
with (ROOT / "events.jsonl").open("a") as log:
    log.write(json.dumps(event) + "\n")

if NAME == "cargo":
    if ARGS[0] == "metadata":
        print(json.dumps({"target_directory": str(ROOT / "target")}))
    elif ARGS[0] == "build":
        profile = profile or "release"
        target = ARGS[ARGS.index("--target") + 1] if "--target" in ARGS else ""
        name = "agentdesk.exe" if target.endswith("windows-msvc") else "agentdesk"
        binary = local(ROOT / "target" / target / profile / name)
        binary.parent.mkdir(parents=True, exist_ok=True)
        binary.write_text('#!/bin/bash\nprintf \'{"checks":[{"id":"postgres_connection","status":"pass"}]}\\n\'\n')
        binary.chmod(0o700)
elif NAME == "python3":
    if ARGS and Path(ARGS[0]).name in ("build_token.py", "release_source.py"):
        path = local(ARGS[0])
        if path.parent not in (ROOT / "repo/scripts", ROOT / "tools/scripts"):
            blocked("unexpected release helper")
        driver = "import runpy, sys; from pathlib import Path; sys.argv.pop(0); " \
                 "sys.path.insert(0, str(Path(sys.argv[0]).parent)); runpy.run_path(sys.argv[0], run_name='__main__')"
        os.execv(sys.executable, [sys.executable, "-I", "-c", driver, str(path), *ARGS[1:]])
    elif ARGS and Path(ARGS[0]).name == "package_release.py":
        if "--source-record" in ARGS:
            path = local(ARGS[ARGS.index("--source-record") + 1])
            if path.is_file():
                with (ROOT / "events.jsonl").open("a") as log:
                    log.write(json.dumps({"command": "package_witness",
                                          "record": json.loads(path.read_text())}) + "\n")
    elif ARGS and (ARGS[0] in ("-c", "-") or Path(ARGS[0]).name == "check_postgres_migration_checksums.py"):
        pass
    else:
        blocked(f"unexpected Python call: {ARGS}")
elif NAME == "env":
    shell = ["/bin/bash", "--noprofile", "--norc", "-s", "--"]
    keys = ("HOME", "PATH", "AGENTDESK_ROOT_DIR", "AGENTDESK_DCSERVER_LABEL")
    if (len(ARGS) != 16 or ARGS[0] != "-i" or ARGS[5:10] != shell
            or tuple(arg.partition("=")[0] for arg in ARGS[1:5]) != keys):
        blocked(f"unexpected env call: {ARGS}")
    child_env = dict(arg.split("=", 1) for arg in ARGS[1:5])
    if (local(child_env["HOME"]) != ROOT / "home"
            or local(child_env["AGENTDESK_ROOT_DIR"]) != ROOT / "release"
            or local(ARGS[10]) not in (ROOT / "repo/scripts", ROOT / "tools/scripts")
            or local(ARGS[11]) != ROOT / "release/config/launchd.env"
            or ARGS[12] not in ("planned", "tmux")
            or ARGS[13:] != ["com.agentdesk.release", "_check_deploy_role", ARGS[12]]):
        blocked(f"unexpected isolated role paths: {ARGS}")
    child_env.update(PATH=str(ROOT / "bin"), DRY_ROOT=str(ROOT), BASH_ENV=str(ROOT / "shell-env"))
    os.execve(shell[0], ARGS[5:], child_env)
elif NAME == "bash":
    if ARGS[:2] == ["-c", '. "$1"; shift; _preflight_resource_contention || exit 1; exec "$@"']:
        os.execv("/bin/bash", ["/bin/bash", *ARGS])
    elif ARGS in ([str(ROOT / "repo/scripts/resolve-config-path.sh"), str(ROOT / "release")],
                  [str(ROOT / "tools/scripts/resolve-config-path.sh"), str(ROOT / "release")]):
        print(ROOT / "release/config/agentdesk.yaml")
    elif ARGS and Path(ARGS[0]).name in ("verify-dashboard.sh", "check-dashboard-toolchain.sh"):
        pass
    else:
        blocked(f"unexpected Bash call: {ARGS}")
elif NAME == "git":
    # Dashboard build-commit probe: a clean fixture checkout at a fixed commit.
    if len(ARGS) < 3 or ARGS[0] != "-C":
        blocked(f"unexpected git: {ARGS}")
    local(ARGS[1])
    tail = ARGS[2:]
    if tail[:6] == SOURCE_PROBE:
        tail = tail[6:]
        if tail == ["rev-parse", "--show-toplevel"]:
            print(ROOT / "repo")
        elif tail == ["rev-parse", "--verify", "HEAD^{commit}"]:
            print("a" * 40)
        elif tail == ["ls-files", "--stage", "-v", "-z"]:
            sys.stdout.write("H 100644 " + "b" * 40 + " 0\tbuild.rs\0")
        elif tail != ["status", "--porcelain=v1", "--untracked-files=all", "--ignore-submodules=none"]:
            blocked(f"unexpected source probe: {tail}")
    elif tail == ["rev-parse", "HEAD"]:
        print("0" * 40)
    elif tail[:2] != ["status", "--porcelain"]:
        blocked(f"unexpected git: {ARGS}")
elif NAME == "dirname":
    print(os.path.dirname(ARGS[0]) or ".")
elif NAME == "uname":
    print("Darwin")
elif NAME == "rustc":
    print("host: aarch64-apple-darwin")
elif NAME == "sed":
    if ARGS == ["-n", "s/^host: //p"]:
        print(sys.stdin.read().removeprefix("host: ").strip())
    else:
        blocked(f"unexpected sed: {ARGS}")
elif NAME == "ps":
    print("dry-start" if "lstart=" in ARGS else "")
elif NAME == "curl":
    raise SystemExit(7)
elif NAME == "jq":
    print(ROOT / "target")
elif NAME == "mkdir":
    for arg in ARGS:
        if not arg.startswith("-"):
            local(arg).mkdir(parents=True, exist_ok=True)
elif NAME == "cp":
    source, destination = map(local, ARGS[-2:])
    if source.is_dir():
        shutil.copytree(source, destination, dirs_exist_ok=True)
    else:
        shutil.copy2(source, destination)
elif NAME == "touch":
    for arg in ARGS:
        local(arg).touch()
elif NAME == "mktemp":
    template = local(ARGS[-1])
    fd, path = tempfile.mkstemp(dir=template.parent, prefix=template.name.replace("XXXXXX", ""))
    os.close(fd)
    print(path)
elif NAME in ("rm", "rsync", "chmod", "xattr", "codesign", "npm", "node"):
    pass
else:
    blocked(f"unexpected external command: {NAME} {ARGS}")
