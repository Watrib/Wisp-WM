#!/usr/bin/env python3
"""Real nested-session smoke test. Never sends input to the host desktop."""
import json
import argparse
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
RUNTIME = Path(os.environ["XDG_RUNTIME_DIR"])


def request(path, command, **fields):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(3)
        connection.connect(str(path))
        connection.sendall(json.dumps({"command": command, **fields}).encode() + b"\n")
        with connection.makefile("rb") as response:
            result = json.loads(response.readline())
    return result


def wait_until(description, callback, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = callback()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError(f"timed out: {description}")


def find_socket(process, before):
    assert process.poll() is None, "Wisp exited during startup"
    for path in set(RUNTIME.glob("wisp-wayland-*.sock")) - before:
        try:
            result = request(path, "status")
            if (result.get("data") or {}).get("pid") == process.pid:
                return path
        except (OSError, ValueError):
            pass
    return None


def stop_session(process, ipc):
    if process.poll() is not None:
        return
    if ipc is not None:
        try:
            request(ipc, "dispatch", action={"name": "quit"})
        except (OSError, ValueError):
            pass
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


def check_shutdown_log(path, remaining_clients=0, expired=False):
    text = re.sub(r"\x1b\[[0-9;]*m", "", path.read_text())
    assert "Wisp shutting down" in text
    assert 'reason="IPC quit requested"' in text
    assert "Wisp exited with an error" not in text
    stopped = next(line for line in text.splitlines() if "Wisp stopped" in line)
    assert f"remaining_clients={remaining_clients}" in stopped, stopped
    assert "remaining_children=0" in stopped, stopped
    assert ("shutdown grace period expired" in text) == expired, text
    for error in ["Broken pipe", "failed to roundtrip Wayland display", "failed to flush wayland socket"]:
        assert error not in text, text
    for warning in [
        "compositor does not implement XDG activation",
        "compositor does not implement fractional scaling",
        "compositor does not implement server-side cursors",
        "compositor does not implement the xdg-toplevel-icon protocol",
        "text input interface not implemented by compositor",
    ]:
        assert warning not in text, text


def shutdown_regressions(binary):
    scratch = Path("/tmp/opencode") if Path("/tmp/opencode").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="wisp-shutdown-", dir=scratch) as temporary:
        temporary = Path(temporary)
        config = temporary / "config.toml"
        shutil.copy(ROOT / "config/wisp.toml", config)
        # An empty session checks acknowledgement-before-exit ordering. A raw
        # connected peer with no windows checks both socket tracking and the bound.
        for stalled in [False, True]:
            before = set(RUNTIME.glob("wisp-wayland-*.sock"))
            log_path = temporary / f"shutdown-{stalled}.log"
            ipc = None
            peer = None
            with log_path.open("w+") as log:
                process = subprocess.Popen([str(binary), "--config", str(config)], cwd=ROOT, stdout=log, stderr=log,
                                           env=os.environ | {"RUST_LOG": "wisp=info,smithay=warn"})
                try:
                    ipc = wait_until("shutdown-test IPC socket", lambda: find_socket(process, before), timeout=30)
                    if stalled:
                        display = request(ipc, "status")["data"]["wayland_display"]
                        peer = socket.socket(socket.AF_UNIX)
                        peer.settimeout(3)
                        peer.connect(str(RUNTIME / display))
                        wait_until("connected peer without a window", lambda: request(ipc, "status")["data"]["clients"] == 1)
                    start = time.monotonic()
                    reply = subprocess.run([str(binary.with_name("wispctl")), "--socket", str(ipc), "quit"],
                                           check=True, capture_output=True, text=True, timeout=5)
                    assert json.loads(reply.stdout)["ok"]
                    assert process.wait(timeout=5) == 0
                    if stalled:
                        assert time.monotonic() - start >= 1.5, "connected client was dropped without a grace period"
                        assert peer.recv(1) == b"", "Wayland connection was not closed"
                    assert not ipc.exists(), "IPC socket was not cleaned up"
                    check_shutdown_log(log_path, remaining_clients=int(stalled), expired=stalled)
                except BaseException:
                    log.flush()
                    print(log_path.read_text())
                    raise
                finally:
                    if peer is not None:
                        peer.close()
                    stop_session(process, ipc)
    print("PASS: empty-session quit acknowledgement, bounded shutdown with a connected windowless client")


def main(release=False):
    assert shutil.which("foot"), "install foot for the smoke test"
    binary = ROOT / ("target/release/wisp" if release else "target/debug/wisp")
    assert binary.exists(), "run cargo build first"
    before = set(RUNTIME.glob("wisp-wayland-*.sock"))
    # Keep all scratch files in the harness-approved temporary root.
    scratch = Path("/tmp/opencode") if Path("/tmp/opencode").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="wisp-smoke-", dir=scratch) as temporary:
        temporary = Path(temporary)
        config = temporary / "config.toml"
        shutil.copy(ROOT / "config/wisp.toml", config)
        original_config = config.read_text()
        config.write_text(original_config.replace("smart_gaps = false", "smart_gaps = true"))
        original_config = config.read_text()
        log_path = temporary / "wisp.log"
        ipc = None
        with log_path.open("w+") as log:
            process = subprocess.Popen([str(binary), "--config", str(config)], cwd=ROOT, stdout=log, stderr=log,
                                       env=os.environ | {"RUST_LOG": "wisp=info,smithay=warn"})
            try:
                ipc = wait_until("IPC socket", lambda: find_socket(process, before), timeout=30)
                assert ipc.stat().st_mode & 0o777 == 0o600
                status = request(ipc, "status")
                assert status["ok"] and status["data"]["workspace"] == 1
                assert status["data"]["layout"] == "dwindle"
                assert status["data"]["max_fps"] == 60
                assert status["data"]["output_refresh_mhz"] == 60000
                assert not status["data"]["shutting_down"]
                assert status["data"]["wayland_display"] != os.environ.get("WAYLAND_DISPLAY")
                ctl = binary.with_name("wispctl")
                result = subprocess.run([str(ctl), "--socket", str(ipc), "status"], check=True, capture_output=True, text=True)
                assert json.loads(result.stdout)["data"]["pid"] == process.pid
                invalid = subprocess.run([str(ctl), "--socket", str(ipc), "workspace", "0"], capture_output=True, text=True)
                assert invalid.returncode != 0 and not json.loads(invalid.stdout)["ok"]

                def action(name, **fields):
                    result = request(ipc, "dispatch", action={"name": name, **fields})
                    assert result["ok"], result

                action("spawn", argv=["foot", "--config=/dev/null", "--override=cursor.blink=no", "--title", "Wisp smoke one", "--app-id", "wisp-smoke", "sh", "-c", "printf 'Wisp nested client\\n'; exec sleep 60"])
                one = wait_until("one mapped tiled client", lambda: next((w for w in request(ipc, "windows")["data"] if w["mapped"]), None))
                viewport = request(ipc, "status")["data"]["viewport"]
                assert one["geometry"] == {"x": viewport["x"] + 2, "y": viewport["y"] + 2,
                                           "width": viewport["width"] - 4, "height": viewport["height"] - 4}
                action("spawn", argv=["foot", "--config=/dev/null", "--override=cursor.blink=no", "--title", "Wisp smoke two", "--app-id", "wisp-smoke", "sh", "-c", "printf 'Wisp nested client\\n'; exec sleep 60"])

                def two_windows():
                    result = request(ipc, "windows")["data"]
                    return result if len(result) == 2 and all(w["mapped"] for w in result) else None

                windows = wait_until("two mapped native clients", two_windows)
                before_overview = request(ipc, "status")["data"]["frames"]["rendered"]
                toggle = subprocess.run([str(ctl), "--socket", str(ipc), "toggle-overview"],
                                        check=True, capture_output=True, text=True)
                assert json.loads(toggle.stdout)["ok"]
                assert request(ipc, "status")["data"]["overview"] is True
                wait_until("overview enter frame", lambda: request(ipc, "status")["data"]["frames"]["rendered"] > before_overview)
                time.sleep(.12)
                action("toggle-overview")
                assert request(ipc, "status")["data"]["overview"] is False
                wait_until("overview exit animation", lambda: not request(ipc, "status")["data"]["overview_exiting"])
                assert all(w["geometry"]["x"] >= viewport["x"] + 14 and
                           w["geometry"]["y"] >= viewport["y"] + 14 and
                           w["geometry"]["x"] + w["geometry"]["width"] <= viewport["x"] + viewport["width"] - 14 and
                           w["geometry"]["y"] + w["geometry"]["height"] <= viewport["y"] + viewport["height"] - 14
                           for w in windows)
                assert all(w["app_id"] == "wisp-smoke" for w in windows)
                assert not any(w["floating"] for w in windows)
                assert sum(w["focused"] for w in windows) == 1
                action("layout", kind="master")
                action("master-ratio", ratio=0.6)
                action("toggle-floating")
                assert sum(w["floating"] for w in request(ipc, "windows")["data"]) == 1
                action("toggle-fullscreen")
                assert sum(w["fullscreen"] for w in request(ipc, "windows")["data"]) == 1
                action("toggle-fullscreen")
                action("focus-next")
                action("swap-next")
                action("move-to-workspace", index=2)
                action("workspace", index=2)
                assert request(ipc, "status")["data"]["workspace"] == 2
                assert request(ipc, "workspaces")["data"][1]["windows"] == 1
                action("workspace", index=1)
                config.write_text(original_config.replace("max_fps = 60", "max_fps = 90"))
                action("reload")
                reloaded = request(ipc, "status")["data"]
                assert reloaded["max_fps"] == 90
                assert reloaded["output_refresh_mhz"] == 90000
                action("reload")
                config.write_text("[render]\nmax_fps = 0\n")
                assert not request(ipc, "dispatch", action={"name": "reload"})["ok"]
                assert request(ipc, "status")["data"]["max_fps"] == 90
                assert request(ipc, "status")["data"]["output_refresh_mhz"] == 90000
                assert not request(ipc, "dispatch", action={"name": "workspace", "index": 0})["ok"]
                assert not request(ipc, "dispatch", action={"name": "spawn", "argv": ["/nonexistent/wisp-smoke"]})["ok"]

                wait_until("rendered frames", lambda: request(ipc, "status")["data"]["frames"]["rendered"] > 0)
                # The compositor should become quiescent, not increment frames forever.
                def frames():
                    stats = request(ipc, "status")["data"]["frames"]
                    return stats["rendered"] + stats["no_damage"]

                def quiescent():
                    first = frames()
                    time.sleep(1.0)
                    return frames() == first

                wait_until("one full second idle after pending client configure/commit round trips", quiescent, timeout=15)
                action("close")
                wait_until("closed client", lambda: len(request(ipc, "windows")["data"]) == 1)
                config.write_text(original_config + '\n[[rules]]\napp_id = "wisp-rule-smoke"\nfloating = true\nworkspace = 3\nsize = [360, 240]\n')
                action("reload")
                bell_trigger = temporary / "ring-bell"
                action("spawn", argv=["foot", "--config=/dev/null", "--override=bell.urgent=yes", "--override=bell.system=no", "--app-id=wisp-rule-smoke", "sh", "-c",
                                      'while [ ! -e "$1" ]; do sleep 0.05; done; printf "\\a"; exec sleep 60', "wisp-bell-test", str(bell_trigger)])
                wait_until("rule-matched client", lambda: any(w["app_id"] == "wisp-rule-smoke" and w["mapped"] for w in request(ipc, "windows")["data"]))
                bell_trigger.touch()
                wait_until("background bell marks client urgent", lambda: any(w["app_id"] == "wisp-rule-smoke" and w["urgent"] for w in request(ipc, "windows")["data"]))
                assert request(ipc, "status")["data"]["workspace"] == 1, "background bell stole focus"
                assert request(ipc, "workspaces")["data"][2]["urgent"]
                action("workspace", index=3)
                ruled = next(w for w in request(ipc, "windows")["data"] if w["app_id"] == "wisp-rule-smoke")
                assert ruled["floating"] and ruled["workspace"] == 3
                assert ruled["focused"] and not ruled["urgent"]
                assert not request(ipc, "workspaces")["data"][2]["urgent"]
                assert ruled["geometry"]["width"] == 360 and ruled["geometry"]["height"] == 240
                reply = subprocess.run([str(ctl), "--socket", str(ipc), "quit"], check=True, capture_output=True, text=True, timeout=5)
                assert json.loads(reply.stdout)["ok"]
                assert process.wait(timeout=5) == 0
                assert not ipc.exists(), "IPC socket was not cleaned up"
                check_shutdown_log(log_path)
                print("PASS: native clients without missing-protocol warnings, background bell urgency without focus stealing, layouts, floating/fullscreen, workspaces, window rules, reload/error handling, idle rendering, close, clean shutdown")
            except BaseException:
                log.flush()
                print(log_path.read_text())
                raise
            finally:
                stop_session(process, ipc)
    shutdown_regressions(binary)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", action="store_true", help="test the optimized binary")
    main(release=parser.parse_args().release)
