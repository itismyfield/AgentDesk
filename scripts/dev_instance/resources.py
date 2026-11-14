"""Foreground resources whose original handles remain in one keeper."""

import json
import os
from pathlib import Path
import signal
import socket
import stat
import struct
import subprocess
import sys
import time

from . import host
from .files import InstanceError, _open_parent
from .manifest import child_environment, resource_spec, validate


class CleanupPending(InstanceError):
    def __init__(self, owner):
        super().__init__("owned resources cleanup pending")
        self.owner = owner


def _peer_pid(stream):
    if sys.platform == "darwin":
        return struct.unpack("i", stream.getsockopt(0, 2, 4))[0]
    if sys.platform.startswith("linux"):
        return struct.unpack("3i", stream.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))[0]
    raise InstanceError("owned socket peer identity unavailable")


class ResourceOwner:
    def __init__(self, manifest, spec, deadline, ambient=None, *, live_sockets=None):
        self.spec = resource_spec(spec)
        self.root = Path(manifest["root"])
        self.sockets = {str(path): tuple(identity) for path, identity in (live_sockets or {}).items()}
        self.manifest = validate(self.root.parent, manifest["instance_id"], ambient or {},
                                 check_ports=False, live_sockets=self.sockets)
        if self.manifest != manifest:
            raise InstanceError("resource declaration changed")
        self.environment = child_environment(manifest, ambient or {})
        self.deadline, self.items, self._kinds = deadline, [], []
        self._herdr, self._helper = None, None
        self._commands, self._close_order, self._blocked = [], [], False
        self._closed, self._started, self.pg, self.herdr = False, False, {}, {}

    def _remaining(self):
        remaining = self._startup - time.monotonic()
        if remaining <= 0:
            raise InstanceError("resource startup deadline expired")
        return remaining

    def _spawn(self, kind, arguments, **options):
        self._remaining()
        if sum(child.poll() is None for child in self.items) >= self.spec["max_aux"]:
            raise InstanceError("resource child bound exceeded")
        child = subprocess.Popen(arguments, **options)
        # Register the original handle before any identity read or publication can fail.
        self.items.append(child)
        self._kinds.append(kind)
        return child

    def _stop(self, child, deadline, request=signal.SIGTERM):
        if child.poll() is not None:
            return
        child.send_signal(request)
        try:
            child.wait(timeout=max(0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            if request == signal.SIGINT:
                raise CleanupPending(self) from None
            child.kill()
            try:
                child.wait(timeout=max(0, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                raise CleanupPending(self) from None

    def _run(self, kind, arguments):
        self._remaining()
        if sum(child.poll() is None for child in self.items) >= self.spec["max_aux"]:
            raise InstanceError("resource helper bound exceeded")
        child = subprocess.Popen(arguments, env=self.environment, cwd=str(self.root),
                                 stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, text=True, close_fds=True)
        self._helper = child
        try:
            output, error = child.communicate(timeout=self._remaining())
        except BaseException:
            self._stop(child, min(self.deadline, time.monotonic() + self.spec["stop_grace"]))
            raise
        finally:
            if child.poll() is not None:
                self._helper = None
        self._commands.append({"kind": kind, "pid": child.pid, "exitcode": child.returncode, "argv": arguments,
                               "stdout": output[-1024:], "stderr": error[-512:]})
        self._commands = self._commands[-8:]
        if child.returncode:
            raise InstanceError("owned resource command failed: " + kind)
        return output.strip()

    def _listener(self, path, child):
        while True:
            remaining = self._remaining()
            if child.poll() is not None:
                raise InstanceError("owned resource exited before readiness")
            try:
                before = path.lstat()
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
                    stream.settimeout(min(remaining, 0.2))
                    parent, leaf = _open_parent(self.root, path.relative_to(self.root), private=True)
                    previous = os.open(".", os.O_RDONLY | os.O_DIRECTORY)
                    try:
                        # The keeper is single-threaded; relative socket addresses avoid sun_path truncation.
                        os.fchdir(parent)
                        stream.connect(leaf)
                    finally:
                        os.fchdir(previous)
                        os.close(previous)
                        os.close(parent)
                    if _peer_pid(stream) != child.pid:
                        self._blocked = True
                        raise InstanceError("resource socket belongs to a foreign process")
                after = path.lstat()
                if (not stat.S_ISSOCK(after.st_mode) or after.st_uid != os.geteuid()
                        or (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino)):
                    self._blocked = True
                    raise InstanceError("resource socket identity changed")
                self.sockets[str(path)] = (after.st_dev, after.st_ino)
                return child.pid
            except (FileNotFoundError, ConnectionRefusedError):
                time.sleep(min(0.05, self._remaining()))
            except (OSError, InstanceError):
                self._blocked = True
                raise

    def _sql(self, database, sql, kind):
        pg = self.manifest["postgres"]
        path = Path(pg["socket_dir"]) / (".s.PGSQL." + str(pg["port"]))
        metadata = path.lstat()
        alias = self.root / path.name
        alias_metadata = alias.lstat()
        if ((metadata.st_dev, metadata.st_ino) != self.sockets.get(str(path))
                or (alias_metadata.st_dev, alias_metadata.st_ino) != self.sockets.get(str(alias))
                or self.sockets.get(str(alias)) != self.sockets.get(str(path))):
            self._blocked = True
            raise InstanceError("owned PostgreSQL socket changed before SQL")
        output = self._run(kind, [self.spec["psql_bin"], "-X", "--no-password", "-v", "ON_ERROR_STOP=1",
                               "-h", str(self.root), "-p", str(pg["port"]), "-U", pg["role"],
                               "-d", database, "-At", "-c", sql])
        after = alias.lstat()
        if (after.st_dev, after.st_ino) != self.sockets[str(alias)]:
            self._blocked = True
            raise InstanceError("PostgreSQL alias changed during SQL")
        return output

    def start(self):
        if self._started:
            raise InstanceError("resources already started")
        self._startup = min(self.deadline, time.monotonic() + self.spec["startup_timeout"])
        self._remaining()
        pg = self.manifest["postgres"]
        alias = self.root / (".s.PGSQL." + str(pg["port"]))
        if (alias.exists() or alias.is_symlink() or any(Path(pg["data_dir"]).iterdir()) or Path(self.manifest["hosts"]["herdr"]["socket"]).exists()
                or (self.root / host.OWNER_FILE).exists()):
            self._blocked = True
            raise InstanceError("existing resources cannot be adopted")
        self._started = True
        try:
            self._run("initdb", [self.spec["initdb_bin"], "-D", pg["data_dir"], "-U", pg["role"],
                                 "--auth-local=trust", "--auth-host=trust", "--no-locale"])
            connections = self.spec["pg_pool"] + self.spec["pg_reserve"]
            child = self._spawn("postgres", [self.spec["postgres_bin"], "-D", pg["data_dir"],
                                "-h", "127.0.0.1", "-p", str(pg["port"]), "-k", "../socket",
                                "-N", str(connections), "-c", "superuser_reserved_connections=0",
                                "-c", "max_wal_senders=0"], env=self.environment, cwd=str(self.root),
                                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, close_fds=True, start_new_session=True)
            path = Path(pg["socket_dir"]) / (".s.PGSQL." + str(pg["port"]))
            peer = self._listener(path, child)
            # A private same-inode alias gives libpq a short absolute Unix address.
            alias = self.root / path.name
            os.link(path, alias, follow_symlinks=False)
            metadata = alias.lstat()
            if (not stat.S_ISSOCK(metadata.st_mode) or metadata.st_uid != os.geteuid()
                    or (metadata.st_dev, metadata.st_ino) != self.sockets[str(path)]):
                self._blocked = True
                raise InstanceError("PostgreSQL alias identity changed")
            self._listener(alias, child)
            query = ("SELECT json_build_object('data_directory',current_setting('data_directory'),"
                     "'port',current_setting('port')::int,'user',current_user,"
                     "'max_connections',current_setting('max_connections')::int)::text")
            while True:
                try:
                    observed = json.loads(self._sql("postgres", query, "pg-readiness"))
                    break
                except InstanceError:
                    time.sleep(min(0.05, self._remaining()))
            if observed != dict(data_directory=pg["data_dir"], port=pg["port"], user=pg["role"], max_connections=connections):
                raise InstanceError("actual PostgreSQL target differs from declaration")
            self._sql("postgres", 'CREATE DATABASE "' + pg["database"] + '"', "create-database")
            database = self._sql(pg["database"], "SELECT current_database()", "database-readback")
            if database != pg["database"]:
                raise InstanceError("actual PostgreSQL database differs from declaration")
            self.pg = dict(observed, database=database, peer_pid=peer)
            def create_herdr(*args, **kwargs):
                child = self._spawn("herdr", *args, **kwargs)
                path = Path(self.manifest["hosts"]["herdr"]["socket"])
                self.herdr["peer_pid"] = self._listener(path, child)
                self.herdr["client_peer_pid"] = self._listener(Path(str(path) + "-client.sock"), child)
                return child
            self._herdr = host.start_herdr(self.root.parent, self.manifest["instance_id"],
                self.spec["herdr_bin"], {}, deadline=self._startup, live_sockets=self.sockets,
                spawn=create_herdr, relative_socket=True)
            self.herdr["start_identity"] = self._herdr.record["daemon"]["start_identity"]
            return self
        except BaseException:
            self.close(time.monotonic() + self.spec["stop_grace"])
            raise

    def _check_sockets(self, remove=False):
        pg = self.manifest["postgres"]
        owned = {str(self.root / (".s.PGSQL." + str(pg["port"]))),
                 str(Path(pg["socket_dir"]) / (".s.PGSQL." + str(pg["port"]))), self.manifest["hosts"]["herdr"]["socket"],
                 self.manifest["hosts"]["herdr"]["socket"] + "-client.sock"}
        for path, identity in self.sockets.items():
            parent, leaf = _open_parent(self.root, Path(path).relative_to(self.root))
            try:
                try:
                    metadata = os.stat(leaf, dir_fd=parent, follow_symlinks=False)
                except FileNotFoundError:
                    continue
                if (not stat.S_ISSOCK(metadata.st_mode) or metadata.st_uid != os.geteuid()
                        or (metadata.st_dev, metadata.st_ino) != identity):
                    raise CleanupPending(self)
                if remove and path in owned:
                    os.unlink(leaf, dir_fd=parent)
            finally:
                os.close(parent)

    def close(self, deadline):
        if self._blocked:
            raise CleanupPending(self)
        try:
            self._check_sockets()
            if self._helper is not None:
                self._stop(self._helper, deadline)
                self._helper = None
            for kind, child in reversed(list(zip(self._kinds, self.items))):
                if kind in self._close_order:
                    continue
                if kind == "herdr" and self._herdr is not None:
                    self._herdr.close(deadline=deadline)
                else:
                    self._stop(child, deadline, signal.SIGINT if kind == "postgres" else signal.SIGTERM)
                if child.poll() is None:
                    raise CleanupPending(self)
                if kind == "postgres" and child.returncode != 0:
                    raise CleanupPending(self)
                self._close_order.append(kind)
            self._check_sockets(remove=True)
        except (InstanceError, OSError, subprocess.TimeoutExpired):
            raise CleanupPending(self) from None
        self._closed = True
        return True

    def witness(self):
        sockets = {}
        for path, identity in self.sockets.items():
            try:
                metadata = Path(path).lstat()
            except FileNotFoundError:
                continue
            if (metadata.st_dev, metadata.st_ino) == identity and stat.S_ISSOCK(metadata.st_mode):
                sockets[path] = list(identity)
        entries = [{"kind": kind, "pid": child.pid, "exitcode": child.poll(),
                    "state": "STOPPED" if child.returncode is not None else "LIVE"}
                   for kind, child in zip(self._kinds, self.items)]
        if self._helper is not None:
            entries.append({"kind": "helper", "pid": self._helper.pid, "exitcode": self._helper.poll()})
        return {"sockets": sockets, "resources": entries, "close_order": list(self._close_order),
                "known_resources_closed": self._closed and not self._blocked,
                "postgres": self.pg, "herdr": self.herdr, "commands": list(self._commands)}
