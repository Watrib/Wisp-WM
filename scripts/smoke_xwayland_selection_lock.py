#!/usr/bin/env python3
"""Nested X11->Wayland held-offer and in-flight transfer lock checks.

Requires GTK3 Python GI, swaylock and `cargo build --example selection_probe`.
No host clipboard/locker is accessed; Wisp runs in a separate nested window.

`old-offer` is the strict check: an X11-origin offer that was granted before
locking and only received after lock confirmation must deliver zero bytes.

`in-flight` is diagnostic. Smithay 0.7's XWM pumps an already-started
X11->Wayland receive without a public cancel API, so data may continue after
lock confirmation. That count is reported as KNOWN LEAK; once an
upstream/vendor cancel path exists, a nonzero count must be treated as a
failing check. Do not use this check to claim lock security.
"""
import os
from pathlib import Path
import re
import select
import socket
import subprocess
import tempfile

from smoke import ROOT, RUNTIME, find_socket, request, stop_session, wait_until


def line(process, description, timeout=12):
    assert select.select([process.stdout], [], [], timeout)[0], f"{description}: timed out (exit={process.poll()})"
    value = process.stdout.readline().strip()
    assert value, f"{description}: closed (exit={process.poll()})"
    return value


def cleanup_socket(ipc, process):
    # A disconnected locker deliberately leaves Wisp black; forced nested
    # shutdown may leave behind only this test's IPC socket. Never unlink a
    # socket with a live listener, nor one belonging to another process.
    if ipc is None or process.poll() is None or not ipc.is_socket():
        return
    try:
        with socket.socket(socket.AF_UNIX) as peer:
            peer.settimeout(1)
            peer.connect(str(ipc))
    except ConnectionRefusedError:
        ipc.unlink()


def check(mode):
    binary = ROOT / "target/release/wisp"
    probe = ROOT / "target/debug/examples/selection_probe"
    assert binary.exists() and probe.exists(), "build Wisp release and the selection_probe example first"
    with tempfile.TemporaryDirectory(prefix="wisp-selection-lock-", dir="/tmp/opencode") as directory:
        directory = Path(directory)
        config = directory / "config.toml"
        config.write_text((ROOT / "config/wisp.toml").read_text()
                          .replace("xwayland = false", "xwayland = true", 1)
                          .replace("session_lock_protocol = false", "session_lock_protocol = true", 1))
        logpath = directory / "wisp.log"
        before = set(RUNTIME.glob("wisp-wayland-*.sock"))
        ipc = None
        children = []
        with logpath.open("w+") as log:
            server = subprocess.Popen([str(binary), "--config", str(config)], cwd=ROOT,
                                      stdout=log, stderr=log,
                                      env=os.environ | {"RUST_LOG": "wisp=info,smithay=warn"})
            try:
                ipc = wait_until("nested Wisp IPC", lambda: find_socket(server, before), timeout=30)

                def xdisplay():
                    log.flush()
                    text = re.sub(r"\x1b\[[0-9;]*m", "", logpath.read_text())
                    found = re.search(r"XWayland window manager ready x_display=(:\d+)", text)
                    return found.group(1) if found else None

                display = wait_until("nested XWM", xdisplay, timeout=30)
                wayland = request(ipc, "status")["data"]["wayland_display"]
                env = os.environ | {"DISPLAY": display, "WAYLAND_DISPLAY": wayland}
                owner = subprocess.Popen(["python3", str(ROOT / "scripts/selection_owner.py"), mode],
                                         env=env | {"GDK_BACKEND": "x11"},
                                         stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                         text=True)
                children.append(owner)
                assert line(owner, "X11 selection owner") == "OWNED"
                held = subprocess.Popen([str(probe), mode], env=env,
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                        text=True, bufsize=1)
                children.append(held)
                assert line(held, "Wayland held offer").startswith("READY ")
                if mode == "in-flight":
                    held.stdin.write("receive\n")
                    held.stdin.flush()
                    assert line(held, "transfer start").startswith("STARTED ")

                read_fd, write_fd = os.pipe()
                locker = subprocess.Popen(["swaylock", "-C", "/dev/null", "-c", "000000", "-R", str(write_fd)],
                                          env=env, pass_fds=[write_fd], stdin=subprocess.DEVNULL,
                                          stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                children.append(locker)
                os.close(write_fd)
                try:
                    assert select.select([read_fd], [], [], 12)[0], "nested locker did not confirm"
                    assert os.read(read_fd, 1), "nested locker exited before confirmation"
                finally:
                    os.close(read_fd)
                held.stdin.write("receive\n" if mode == "old-offer" else "check\n")
                held.stdin.flush()
                result = line(held, "post-lock receive", timeout=12)
                assert result.startswith("AFTER_LOCK "), result
                count = int(result.split()[-1])
                if count == 0:
                    print(f"PASS: {mode} X11->Wayland offer delivered zero bytes after nested lock confirmation")
                elif mode == "old-offer":
                    raise AssertionError(f"{mode}: {count} desktop selection bytes read after lock confirmation")
                else:
                    # Smithay 0.7 has no public API to cancel an in-flight
                    # X11->Wayland receive. Keep the harness green but loud
                    # until a cancel path exists to enforce it.
                    print(f"KNOWN LEAK: in-flight X11->Wayland transfer continued after lock confirmation; {count} desktop selection bytes delivered", flush=True)
            except BaseException:
                log.flush()
                print(logpath.read_text())
                if ipc is not None and server.poll() is None:
                    print("status:", request(ipc, "status"))
                    print("windows:", request(ipc, "windows"))
                for child in children:
                    if child.poll() is not None and child.stderr:
                        print(child.stderr.read())
                raise
            finally:
                for child in reversed(children):
                    if child.poll() is None:
                        child.terminate()
                    try:
                        child.communicate(timeout=3)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.communicate(timeout=3)
                stop_session(server, ipc)
                cleanup_socket(ipc, server)


if __name__ == "__main__":
    for scenario in ("old-offer", "in-flight"):
        check(scenario)
