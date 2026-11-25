#!/usr/bin/env python3
"""Immutable bounded child supervisor and /workspace artifact extractor.

Runs inside the hardened read-only OCI container as PID 1 only when a sandbox
invocation requests bounded artifact extraction from ephemeral tmpfs `/workspace`.
Never executed via shell; enforces process-group teardown before reading artifacts.
"""

import argparse
import ctypes
import errno
import hashlib
import os
import selectors
import signal
import stat
import struct
import subprocess
import sys
import time

MAGIC_HEADER = b"SVXART01"
POST_EXIT_DRAIN_SECS = 0.5
READ_CHUNK_BYTES = 65536
PR_SET_DUMPABLE = 4


def set_non_dumpable() -> None:
    try:
        libc = ctypes.CDLL("libc.so.6", use_errno=True)
        res = libc.prctl(PR_SET_DUMPABLE, 0, 0, 0, 0)
        if res != 0:
            err = ctypes.get_errno()
            raise OSError(err, os.strerror(err))
    except Exception as exc:
        sys.stderr.write(f"failed to enforce PR_SET_DUMPABLE=0: {exc}\n")
        sys.exit(125)


def parse_artifact_spec(raw: str) -> tuple[str, int]:
    if ":" not in raw:
        raise ValueError(f"invalid artifact spec '{raw}'")
    rel_path, max_str = raw.rsplit(":", 1)
    max_bytes = int(max_str)
    if (
        not rel_path
        or len(rel_path) > 256
        or rel_path.startswith("/")
        or "\\" in rel_path
        or ":" in rel_path
        or any(ord(ch) < 32 for ch in rel_path)
    ):
        raise ValueError(f"unsafe artifact path '{rel_path}'")
    parts = rel_path.split("/")
    if any(seg in ("", ".", "..") for seg in parts):
        raise ValueError(f"unsafe artifact path segment in '{rel_path}'")
    if max_bytes <= 0:
        raise ValueError("artifact max_bytes must be > 0")
    return rel_path, max_bytes


def write_exact(fd: int, data: bytes) -> None:
    view = memoryview(data)
    written = 0
    while written < len(view):
        n = os.write(fd, view[written:])
        if n <= 0:
            raise OSError("short write on protocol stdout")
        written += n


def emit_stdout_frame(proto_fd: int, chunk: bytes) -> None:
    if not chunk:
        return
    write_exact(proto_fd, b"O" + struct.pack(">I", len(chunk)))
    write_exact(proto_fd, chunk)


def kill_process_group(pgid: int) -> None:
    try:
        os.killpg(pgid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except PermissionError:
        pass


def open_workspace_regular_file(rel_path: str) -> tuple[int, int]:
    dir_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    file_flags = os.O_RDONLY | os.O_NOFOLLOW
    cur_fd = os.open("/workspace", dir_flags)
    try:
        parts = rel_path.split("/")
        for seg in parts[:-1]:
            next_fd = os.open(seg, dir_flags, dir_fd=cur_fd)
            os.close(cur_fd)
            cur_fd = next_fd
        leaf_fd = os.open(parts[-1], file_flags, dir_fd=cur_fd)
    finally:
        try:
            os.close(cur_fd)
        except OSError:
            pass

    try:
        st = os.fstat(leaf_fd)
        if not stat.S_ISREG(st.st_mode):
            raise ValueError(f"artifact '/workspace/{rel_path}' is not a regular file")
        if st.st_nlink != 1:
            raise ValueError(f"artifact '/workspace/{rel_path}' has nlink={st.st_nlink} (hardlinks prohibited)")
        return leaf_fd, st.st_size
    except Exception:
        os.close(leaf_fd)
        raise


def stream_artifact(
    proto_fd: int,
    rel_path: str,
    max_bytes: int,
    remaining_total: int,
) -> int:
    leaf_fd, expected_size = open_workspace_regular_file(rel_path)
    try:
        if expected_size > max_bytes:
            raise ValueError(
                f"artifact '{rel_path}' ({expected_size} bytes) exceeds per-file limit ({max_bytes} bytes)"
            )
        if expected_size > remaining_total:
            raise ValueError(
                f"artifact '{rel_path}' exceeds remaining total artifact budget ({remaining_total} bytes)"
            )

        hasher = hashlib.sha256()
        measured = 0
        while True:
            chunk = os.read(leaf_fd, READ_CHUNK_BYTES)
            if not chunk:
                break
            measured += len(chunk)
            if measured > max_bytes or measured > remaining_total:
                raise ValueError(f"artifact '{rel_path}' grew past byte limit during read")
            hasher.update(chunk)

        if measured != expected_size:
            raise ValueError(f"artifact '{rel_path}' changed size during extraction")

        digest = hasher.digest()
        path_bytes = rel_path.encode("utf-8")
        header = (
            b"A"
            + struct.pack(">H", len(path_bytes))
            + path_bytes
            + digest
            + struct.pack(">Q", measured)
        )
        write_exact(proto_fd, header)

        os.lseek(leaf_fd, 0, os.SEEK_SET)
        sent = 0
        while sent < measured:
            to_read = min(READ_CHUNK_BYTES, measured - sent)
            chunk = os.read(leaf_fd, to_read)
            if not chunk:
                raise ValueError(f"unexpected EOF reading artifact '{rel_path}'")
            write_exact(proto_fd, chunk)
            sent += len(chunk)
        return measured
    finally:
        os.close(leaf_fd)


def run_child(
    proto_fd: int,
    max_stdout: int,
    max_stderr: int,
    command: list[str],
) -> int:
    proc = subprocess.Popen(
        command,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        close_fds=True,
        start_new_session=True,
    )
    pgid = proc.pid
    assert proc.stdout is not None and proc.stderr is not None

    sel = selectors.DefaultSelector()
    os.set_blocking(proc.stdout.fileno(), False)
    os.set_blocking(proc.stderr.fileno(), False)
    sel.register(proc.stdout, selectors.EVENT_READ, data="stdout")
    sel.register(proc.stderr, selectors.EVENT_READ, data="stderr")

    stdout_seen = 0
    stderr_seen = 0
    exit_observed_at: float | None = None
    exit_code: int | None = None

    while sel.get_map():
        if exit_code is None:
            exit_code = proc.poll()
            if exit_code is not None:
                exit_observed_at = time.monotonic()

        timeout = 0.05
        if exit_observed_at is not None:
            elapsed_drain = time.monotonic() - exit_observed_at
            if elapsed_drain >= POST_EXIT_DRAIN_SECS:
                retained = [key.data for key in sel.get_map().values()]
                kill_process_group(pgid)
                sel.close()
                proc.wait()
                sys.stderr.write(
                    f"descendant process retained {','.join(retained)} pipe after container process exited\n"
                )
                return 124
            timeout = min(timeout, max(0.01, POST_EXIT_DRAIN_SECS - elapsed_drain))

        events = sel.select(timeout=timeout)
        for key, _ in events:
            stream_name = key.data
            fd = key.fileobj.fileno()
            try:
                chunk = os.read(fd, READ_CHUNK_BYTES)
            except BlockingIOError:
                continue
            if not chunk:
                sel.unregister(key.fileobj)
                key.fileobj.close()
                continue

            if stream_name == "stdout":
                stdout_seen += len(chunk)
                if stdout_seen > max_stdout:
                    kill_process_group(pgid)
                    sel.close()
                    proc.wait()
                    sys.stderr.write(f"child stdout exceeded limit ({max_stdout} bytes)\n")
                    return 125
                emit_stdout_frame(proto_fd, chunk)
            else:
                stderr_seen += len(chunk)
                if stderr_seen > max_stderr:
                    kill_process_group(pgid)
                    sel.close()
                    proc.wait()
                    sys.stderr.write(f"child stderr exceeded limit ({max_stderr} bytes)\n")
                    return 125
                write_exact(2, chunk)

    sel.close()
    rc = proc.wait() if exit_code is None else exit_code
    kill_process_group(pgid)
    return rc


def main() -> int:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--max-stdout", type=int, required=True)
    parser.add_argument("--max-stderr", type=int, required=True)
    parser.add_argument("--max-total", type=int, required=True)
    parser.add_argument("--artifact", action="append", default=[])
    parser.add_argument("cmd", nargs=argparse.REMAINDER)
    ns = parser.parse_args()

    cmd = list(ns.cmd)
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]
    if not cmd:
        sys.stderr.write("missing command after '--'\n")
        return 125
    if not ns.artifact:
        sys.stderr.write("at least one --artifact spec is required\n")
        return 125

    try:
        artifacts = [parse_artifact_spec(item) for item in ns.artifact]
    except Exception as exc:
        sys.stderr.write(f"invalid artifact specification: {exc}\n")
        return 125

    set_non_dumpable()

    # Move protocol stdout off FD 1 before spawning untrusted child.
    proto_fd = os.dup(1)
    devnull_fd = os.open("/dev/null", os.O_WRONLY)
    os.dup2(devnull_fd, 1)
    os.close(devnull_fd)

    try:
        write_exact(proto_fd, MAGIC_HEADER)
        rc = run_child(proto_fd, ns.max_stdout, ns.max_stderr, cmd)
        if rc != 0:
            return rc

        remaining_total = ns.max_total
        for rel_path, max_bytes in artifacts:
            transferred = stream_artifact(proto_fd, rel_path, max_bytes, remaining_total)
            remaining_total -= transferred

        write_exact(proto_fd, b"D" + struct.pack(">I", 0))
        return 0
    except Exception as exc:
        sys.stderr.write(f"artifact transfer failed: {exc}\n")
        return 125
    finally:
        try:
            os.close(proto_fd)
        except OSError:
            pass


if __name__ == "__main__":
    sys.exit(main())
