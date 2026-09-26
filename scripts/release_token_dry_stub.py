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
                 release="--release" in ARGS or "-r" in ARGS or profile.startswith("release"))
with (ROOT / "events.jsonl").open("a") as log:
    log.write(json.dumps(event) + "\n")

if NAME == "cargo":
    if ARGS[0] == "metadata":
        print(json.dumps({"target_directory": str(ROOT / "target")}))
    elif ARGS[0] == "build":
        profile = profile or "release"
        target = ARGS[ARGS.index("--target") + 1] if "--target" in ARGS else ""
        binary = local(ROOT / "target" / target / profile / "agentdesk")
        binary.parent.mkdir(parents=True, exist_ok=True)
        binary.write_text('#!/bin/bash\nprintf \'{"checks":[{"id":"postgres_connection","status":"pass"}]}\\n\'\n')
        binary.chmod(0o700)
elif NAME == "python3":
    if ARGS and Path(ARGS[0]).name == "build_token.py":
        path = local(ARGS[0])
        if path != ROOT / "repo/scripts/build_token.py":
            blocked("unexpected token helper")
        os.execv(sys.executable, [sys.executable, "-I", str(path), *ARGS[1:]])
    elif ARGS and (ARGS[0] in ("-c", "-") or Path(ARGS[0]).name in (
            "check_postgres_migration_checksums.py", "package_release.py")):
        pass
    else:
        blocked(f"unexpected Python call: {ARGS}")
elif NAME == "bash":
    if ARGS[:2] == ["-c", '. "$1"; shift; _preflight_resource_contention || exit 1; exec "$@"']:
        os.execv("/bin/bash", ["/bin/bash", *ARGS])
    elif ARGS and Path(ARGS[0]).name in ("verify-dashboard.sh", "check-dashboard-toolchain.sh"):
        pass
    else:
        blocked(f"unexpected Bash call: {ARGS}")
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
