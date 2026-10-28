#!/usr/bin/env python3
"""Sample Wisp's frame counters, process CPU time, and RSS over an interval.

Use a nested session with a representative client, such as Noctalia. No root,
profiling tools, or busy-loop are needed; this is a low-overhead idle/load
baseline, not a GPU timing or input-to-photon latency measurement.
"""

import argparse
import json
import os
import subprocess
import time
from pathlib import Path


# Repaint an ANSI-colored rectangle in a terminal at a controlled rate. The
# foot client runs inside Wisp; this process remains on the host and only
# reads counters. The child exits by itself even if sampling fails.
ANIMATE_FOOT = r"""
import sys, time
fps, duration = float(sys.argv[1]), float(sys.argv[2])
start = time.monotonic()
frame = 0
sys.stdout.write('\x1b[?25l\x1b[2J')
try:
    while time.monotonic() - start < duration:
        rgb = ((frame * 7) % 256, (frame * 13) % 256, (frame * 19) % 256)
        fill = ' ' * 76
        sys.stdout.write(''.join(
            f'\x1b[{row};1H\x1b[48;2;{rgb[0]};{rgb[1]};{rgb[2]}m{fill}'
            for row in range(1, 21)
        ))
        sys.stdout.flush()
        frame += 1
        time.sleep(max(0, start + frame / fps - time.monotonic()))
finally:
    sys.stdout.write('\x1b[0m\x1b[?25h')
    sys.stdout.flush()
"""


def sample(socket: str, ctl: Path) -> tuple[dict, float, int]:
    reply = json.loads(
        subprocess.check_output((str(ctl), "--socket", socket, "status"))
    )
    if not reply["ok"]:
        raise RuntimeError(reply["error"])
    status = reply["data"]
    pid = status["pid"]
    # The comm field is parenthesized and can contain spaces; split after its
    # closing ')' before indexing utime (field 14) and stime (field 15).
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
    cpu_seconds = (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
    rss_kib = next(
        int(line.split()[1])
        for line in Path(f"/proc/{pid}/status").read_text().splitlines()
        if line.startswith("VmRSS:")
    )
    return status, cpu_seconds, rss_kib


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", default=os.environ.get("WISP_SOCKET"), required=False)
    parser.add_argument("--settle", type=float, default=0, help="seconds before measuring")
    parser.add_argument("--seconds", type=float, default=10, help="sample duration")
    parser.add_argument("--animate-foot", action="store_true", help="spawn a bounded, animated foot client")
    parser.add_argument("--hidden-foot", action="store_true", help="switch away from the animated foot during the sample")
    parser.add_argument("--fps", type=int, default=30, help="foot repaint rate with --animate-foot")
    args = parser.parse_args()
    if not args.socket or args.seconds <= 0 or args.settle < 0 or args.fps <= 0:
        parser.error("provide --socket (or WISP_SOCKET), positive --seconds/--fps, and nonnegative --settle")
    if args.hidden_foot and not args.animate_foot:
        parser.error("--hidden-foot requires --animate-foot")

    ctl = Path(__file__).resolve().parents[1] / "target/release/wispctl"
    if args.animate_foot:
        duration = max(args.settle, 2) + args.seconds + 5
        subprocess.check_call(
            (
                str(ctl), "--socket", args.socket, "spawn", "foot", "--app-id",
                "wisp-benchmark", "-e", "python3", "-u", "-c", ANIMATE_FOOT,
                str(args.fps), str(duration),
            ),
            stdout=subprocess.DEVNULL,
        )
        if args.hidden_foot:
            time.sleep(2)  # Let foot map on workspace 1 before hiding it.
            subprocess.check_call(
                (str(ctl), "--socket", args.socket, "workspace", "2"),
                stdout=subprocess.DEVNULL,
            )
    time.sleep(max(args.settle, 2) if args.animate_foot else args.settle)
    try:
        before, cpu_before, _ = sample(args.socket, ctl)
        start = time.monotonic()
        time.sleep(args.seconds)
        after, cpu_after, rss = sample(args.socket, ctl)
    finally:
        if args.hidden_foot:
            subprocess.check_call(
                (str(ctl), "--socket", args.socket, "workspace", "1"),
                stdout=subprocess.DEVNULL,
            )
    elapsed = time.monotonic() - start
    if before["pid"] != after["pid"]:
        raise RuntimeError("Wisp was replaced during the sample")

    frames_before, frames_after = before["frames"], after["frames"]
    phases = ("scene_wall_us", "bind_wall_us", "compose_wall_us", "present_wall_us")
    phase_deltas = {
        phase: frames_after[phase] - frames_before[phase]
        for phase in phases
        if phase in frames_before and phase in frames_after
    }
    print(
        json.dumps(
            {
                "pid": after["pid"],
                "backend": after["backend"],
                "viewport": after["viewport"],
                "clients": after["clients"],
                "layers": after["layers"],
                "workload": (
                    f"hidden foot {args.fps} fps" if args.hidden_foot
                    else f"foot {args.fps} fps" if args.animate_foot
                    else "existing clients"
                ),
                "seconds": round(elapsed, 3),
                "cpu_seconds": round(cpu_after - cpu_before, 3),
                "rss_kib": rss,
                "rendered": frames_after["rendered"] - frames_before["rendered"],
                "no_damage": frames_after["no_damage"] - frames_before["no_damage"],
                "client_commits": frames_after["client_commits"] - frames_before["client_commits"],
                "skipped_invisible_commits": (
                    frames_after["skipped_invisible_commits"] - frames_before["skipped_invisible_commits"]
                    if "skipped_invisible_commits" in frames_after else None
                ),
                "host_redraws": frames_after["host_redraws"] - frames_before["host_redraws"],
                "last_cpu_render_us": frames_after["last_cpu_render_us"],
                "phase_wall_us": phase_deltas,
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
