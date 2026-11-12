"""Private instance files, with no legacy credential migration or link following."""

from __future__ import annotations

import json
import os
import stat
import subprocess
import uuid
from pathlib import Path


CREDENTIAL_NAMES = frozenset({"claude_bot_token", "codex_bot_token", "driver_bot_token"})
LAYOUT_DIRS = (
    "config", "runtime", "data", "logs", "outputs", "policies", "workspaces",
    "credential", "postgres", "postgres/data", "postgres/socket",
    "routines", "config/agents", "workspaces/claude", "workspaces/codex",
    "data/memory", "data/memory/shared", "data/memory/long-term", "data/memory/auto",
)


class InstanceError(ValueError):
    """A declaration or instance file violates the isolated-instance contract."""


class BlockedBinary(InstanceError):
    """The lane binary needed for shared credential storage is unavailable."""


def safe_absolute(path: str | Path) -> Path:
    candidate = Path(path)
    if not candidate.is_absolute() or ".." in candidate.parts:
        raise InstanceError("path must be absolute and contain no parent traversal")
    current = Path(candidate.anchor)
    for part in candidate.parts[1:]:
        current /= part
        try:
            mode = current.lstat().st_mode
        except FileNotFoundError:
            continue
        except OSError:
            raise InstanceError("path ancestor is not a readable directory") from None
        if stat.S_ISLNK(mode):
            raise InstanceError("symlink paths are not allowed")
    if any(a.casefold() == ".adk" and b.casefold() == "release" for a, b in zip(candidate.parts, candidate.parts[1:])):
        raise InstanceError("release subtree is not allowed")
    return candidate


def instance_root(base: str | Path, instance_id: str) -> Path:
    try:
        valid = str(uuid.UUID(instance_id)) == instance_id
    except (ValueError, TypeError, AttributeError):
        valid = False
    if not valid:
        raise InstanceError("instance ID must be a canonical UUID")
    return safe_absolute(safe_absolute(base) / instance_id)


def _open_dir(path: Path) -> int:
    safe_absolute(path)
    descriptor = os.open(path.anchor, os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in path.parts[1:]:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        return descriptor
    except OSError:
        os.close(descriptor)
        raise InstanceError("instance directory cannot be opened without following links") from None


def _child_parts(relative: str | Path) -> tuple[str, ...]:
    path = Path(relative)
    if path.is_absolute() or not path.parts or ".." in path.parts:
        raise InstanceError("instance child path must be relative without parent traversal")
    return path.parts


def _open_parent(root: Path, relative: str | Path) -> tuple[int, str]:
    parts = _child_parts(relative)
    descriptor = _open_dir(root)
    try:
        for part in parts[:-1]:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        return descriptor, parts[-1]
    except OSError:
        os.close(descriptor)
        raise InstanceError("instance child directory is invalid") from None


def validate_tree(root: str | Path, require_credentials: bool = False) -> None:
    root = safe_absolute(root)
    descriptor = _open_dir(root)
    try:
        if stat.S_IMODE(os.fstat(descriptor).st_mode) != 0o700:
            raise InstanceError("instance root permissions must be 0700")
        _validate_directory(descriptor, ())
        if require_credentials:
            credentials = _open_dir(root / "credential")
            try:
                if set(os.listdir(credentials)) != CREDENTIAL_NAMES:
                    raise InstanceError("all three root credentials are required")
                values = []
                for name in sorted(CREDENTIAL_NAMES):
                    leaf = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=credentials)
                    with os.fdopen(leaf, "rb") as source:
                        values.append(_token_bytes(source.read(4098)))
                if len(set(values)) != 3:
                    raise InstanceError("credentials must be distinct")
            finally:
                os.close(credentials)
    finally:
        os.close(descriptor)


def _validate_directory(descriptor: int, relative: tuple[str, ...]) -> None:
    for name in os.listdir(descriptor):
        child = relative + (name,)
        metadata = os.stat(name, dir_fd=descriptor, follow_symlinks=False)
        if stat.S_ISLNK(metadata.st_mode):
            raise InstanceError("instance tree contains a symlink")
        if name.casefold() in {"credential", "credentials"} and child != ("credential",):
            raise InstanceError("legacy credential subtree is not allowed")
        if relative == ("credential",):
            if name not in CREDENTIAL_NAMES or not stat.S_ISREG(metadata.st_mode):
                raise InstanceError("unknown credential entry is not allowed")
            if stat.S_IMODE(metadata.st_mode) != 0o600 or metadata.st_nlink != 1 or metadata.st_size == 0:
                raise InstanceError("credential must be a nonempty private regular file")
        elif stat.S_ISDIR(metadata.st_mode):
            if child == ("credential",) and stat.S_IMODE(metadata.st_mode) != 0o700:
                raise InstanceError("credential directory permissions must be 0700")
            nested = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            try:
                _validate_directory(nested, child)
            finally:
                os.close(nested)
        elif not stat.S_ISREG(metadata.st_mode):
            raise InstanceError("instance tree contains a non-regular entry")
        elif child == ("credential",):
            raise InstanceError("credential directory is not a directory")


def create_layout(root: str | Path) -> None:
    root = safe_absolute(root)
    descriptor = _open_dir(root.parent)
    try:
        os.mkdir(root.name, 0o700, dir_fd=descriptor)
    except OSError:
        raise InstanceError("instance root must be fresh") from None
    finally:
        os.close(descriptor)
    descriptor = _open_dir(root)
    try:
        os.fchmod(descriptor, 0o700)
        for relative in LAYOUT_DIRS:
            parent, name = _open_parent(root, relative)
            try:
                os.mkdir(name, 0o700, dir_fd=parent)
                child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
                try:
                    os.fchmod(child, 0o700)
                finally:
                    os.close(child)
            finally:
                os.close(parent)
    finally:
        os.close(descriptor)


def _write_private(root: Path, relative: str | Path, data: bytes) -> None:
    if _child_parts(relative)[0].casefold() == "credential":
        raise InstanceError("credentials require the shared Rust writer")
    validate_tree(root)
    parent, name = _open_parent(root, relative)
    try:
        descriptor = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=parent)
        try:
            os.fchmod(descriptor, 0o600)
            with os.fdopen(descriptor, "wb", closefd=False) as output:
                output.write(data)
                output.flush()
                os.fsync(descriptor)
        finally:
            os.close(descriptor)
    except OSError:
        raise InstanceError("private instance file must be new and writable") from None
    finally:
        os.close(parent)


def write_private_json(root: str | Path, relative: str | Path, data: object) -> None:
    _write_private(safe_absolute(root), relative, (json.dumps(data, indent=2, sort_keys=True) + "\n").encode())


def write_private_text(root: str | Path, relative: str | Path, text: str) -> None:
    _write_private(safe_absolute(root), relative, text.encode())


def read_regular_text(path: str | Path) -> str:
    path = safe_absolute(path)
    parent = _open_dir(path.parent)
    try:
        descriptor = os.open(path.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
        with os.fdopen(descriptor, "rb") as source:
            if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
                raise InstanceError("declaration must be a regular file")
            data = source.read(1024 * 1024 + 1)
            if len(data) > 1024 * 1024:
                raise InstanceError("declaration file is too large")
            return data.decode("utf-8")
    finally:
        os.close(parent)


def _token_bytes(value) -> bytes:
    if isinstance(value, str):
        value = value.encode()
    if not isinstance(value, bytes):
        raise InstanceError("credential input must be text or bytes")
    value = value.removesuffix(b"\n")
    if not value or len(value) > 4096 or any(c in value for c in (b"\n", b"\r", b"\x00")) or value.strip() != value:
        raise InstanceError("credential input must be one nonempty bounded line")
    return value


def write_credential(root: str | Path, name: str, stream) -> None:
    root = safe_absolute(root)
    if name not in CREDENTIAL_NAMES:
        raise InstanceError("unknown credential name")
    validate_tree(root)
    first_path = os.environ.get("PATH", "").split(os.pathsep)[0]
    binary = Path(first_path) / "agentdesk"
    if not first_path or not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise BlockedBinary("lane binary is unavailable")
    value = _token_bytes(stream.read(4098))
    parent = _open_dir(root / "credential")
    try:
        for existing in os.listdir(parent):
            descriptor = os.open(existing, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=parent)
            with os.fdopen(descriptor, "rb") as source:
                if source.read(4097) == value:
                    raise InstanceError("credentials must be distinct")
    finally:
        os.close(parent)
    try:
        result = subprocess.run(
            [str(binary), "dev-instance", "credential-write", "--root", str(root), "--name", name],
            input=value, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"}, timeout=10, check=False,
        )
    except OSError:
        raise BlockedBinary("lane binary cannot be executed") from None
    except subprocess.TimeoutExpired:
        raise InstanceError("shared credential writer timed out") from None
    if result.returncode != 0:
        raise InstanceError("shared credential writer failed")
