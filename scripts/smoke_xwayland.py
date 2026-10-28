#!/usr/bin/env python3
"""Opt-in nested XWayland client check; never drives the host session.

Checks the XWM, X11 root, and one X11 window's mapping and destruction.
It does not assert keyboard/pointer focus, popups, clipboard or lock isolation.
"""

import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time

from smoke import find_socket, request, stop_session, RUNTIME, ROOT


def main():
    if not shutil.which("xprop"):
        raise SystemExit("xprop is required for the opt-in XWayland check")
    scratch = Path("/tmp/opencode") if Path("/tmp/opencode").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="wisp-xwayland-", dir=scratch) as directory:
        config = Path(directory) / "config.toml"
        config.write_text((ROOT / "config/wisp.toml").read_text().replace(
            "xwayland = false", "xwayland = true", 1,
        ))
        log_path = Path(directory) / "wisp.log"
        before = set(RUNTIME.glob("wisp-wayland-*.sock"))
        ipc = None
        with log_path.open("w+") as log:
            process = subprocess.Popen(
                [str(ROOT / "target/debug/wisp"), "--config", str(config)],
                cwd=ROOT, stdout=log, stderr=log,
                env=os.environ | {"RUST_LOG": "wisp=info,smithay=warn"},
            )
            try:
                deadline = time.monotonic() + 25
                display = None
                while time.monotonic() < deadline and process.poll() is None:
                    ipc = ipc or find_socket(process, before)
                    log.flush()
                    text = re.sub(r"\x1b\[[0-9;]*m", "", log_path.read_text())
                    match = re.search(
                        r"experimental XWayland window manager ready x_display=(:\d+)",
                        text,
                    )
                    if match:
                        display = match.group(1)
                        break
                    time.sleep(0.05)
                assert display, f"XWayland/XWM did not become ready:\n{log_path.read_text()}"
                result = subprocess.run(
                    ["xprop", "-root"],
                    capture_output=True, text=True, timeout=5,
                    env=os.environ | {"DISPLAY": display},
                )
                assert result.returncode == 0, result.stderr
                assert process.poll() is None, log_path.read_text()
                assert request(ipc, "status")["ok"]
                from Xlib import X, display as xlib_display

                x11 = xlib_display.Display(display)
                screen = x11.screen()
                window = screen.root.create_window(
                    20, 20, 240, 160, 0, screen.root_depth,
                    X.InputOutput, X.CopyFromParent,
                    background_pixel=screen.white_pixel,
                    event_mask=X.ExposureMask,
                )
                try:
                    window.set_wm_name("wisp-x11-smoke")
                    window.set_wm_class("wisp-x11-smoke", "WispX11Smoke")
                    window.map()
                    gc = window.create_gc(foreground=screen.black_pixel)
                    window.fill_rectangle(gc, 20, 20, 120, 80)
                    x11.sync()
                    deadline = time.monotonic() + 10
                    while time.monotonic() < deadline:
                        windows = request(ipc, "windows")["data"]
                        if any(w["title"] == "wisp-x11-smoke" and w["mapped"] for w in windows):
                            break
                        time.sleep(0.05)
                    else:
                        raise AssertionError(f"X11 window did not map: {windows}; {log_path.read_text()}")
                    focus = x11.get_input_focus().focus
                    host_focused = request(ipc, "status")["data"]["host_focused"]
                    if host_focused:
                        assert getattr(focus, "id", focus) == window.id, (
                            f"X11 input focus is {getattr(focus, 'id', focus)}, not {window.id}"
                        )
                    else:
                        print("NOTE: nested host window is unfocused; X11 keyboard focus check skipped")
                    popup = screen.root.create_window(
                        80, 80, 110, 75, 0, screen.root_depth,
                        X.InputOutput, X.CopyFromParent,
                        override_redirect=1, background_pixel=screen.white_pixel,
                    )
                    popup.set_wm_name("wisp-x11-popup")
                    popup.map()
                    popup.fill_rectangle(popup.create_gc(foreground=screen.black_pixel), 5, 5, 60, 40)
                    x11.sync()
                    assert process.poll() is None, "override-redirect popup crashed Wisp"
                    assert not any(w["title"] == "wisp-x11-popup" for w in request(ipc, "windows")["data"]), (
                        "override-redirect popup must not enter the managed window list"
                    )
                    popup.destroy()
                    x11.sync()
                    assert request(ipc, "dispatch", action={"name": "close"})["ok"]
                    deadline = time.monotonic() + 5
                    while time.monotonic() < deadline:
                        if not any(w["title"] == "wisp-x11-smoke" for w in request(ipc, "windows")["data"]):
                            break
                        time.sleep(0.05)
                    else:
                        raise AssertionError("X11 window remained after Wisp IPC close")
                    print(f"PASS: opt-in XWayland/XWM on {display}, X11 mapping, popup and IPC close")
                finally:
                    x11.close()
            except BaseException:
                log.flush()
                print(log_path.read_text())
                raise
            finally:
                stop_session(process, ipc)


if __name__ == "__main__":
    main()
