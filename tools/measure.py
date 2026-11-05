#!/usr/bin/env python3
"""Resources of the desktop stack: memory (PSS, RSS) and CPU over a window.

    tools/measure.py [hyprland|davide-wm] [seconds]

PSS splits shared pages among the processes that map them, so the column
adds up; RSS counts them in every process. CPU is the share of one core over
the window. Apps (browsers, terminals) and what both stacks keep
(xdg-desktop-portal, the gtk backend, PipeWire) are left out.
"""

import os
import re
import sys
import time

PROFILES = {
    "hyprland": [
        ("Hyprland", r"^(start-hyprland|Hyprland)\b"),
        ("Xwayland", r"^Xwayland\b"),
        ("topbar", r"/topbar$"),
        ("super", r"/super\b"),
        ("nwg-notifications", r"nwg-notifications"),
        ("awww + wallpaper-watch", r"awww-daemon|wallpaper-watch|^ncat -U .*hypr"),
        ("monitor-manager", r"monitor-manager"),
        ("nm-applet", r"^nm-applet"),
        ("cliphist (wl-paste)", r"^wl-paste --watch"),
        ("xdg-desktop-portal-hyprland", r"xdg-desktop-portal-hyprland"),
        ("lid inhibitors", r"^systemd-inhibit --who=lid-toggle|^sleep infinity$"),
    ],
    "davide-wm": [
        ("davide-wm", r"(^|/)davide-wm( |$)"),
        ("Xwayland", r"^Xwayland\b"),
    ],
}

TICK = os.sysconf("SC_CLK_TCK")
PAGE = os.sysconf("SC_PAGE_SIZE")


def processes():
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                cmd = f.read().replace(b"\0", b" ").decode(errors="replace").strip()
            if os.stat(f"/proc/{pid}").st_uid != os.getuid() or not cmd:
                continue
            yield int(pid), cmd
        except OSError:
            continue


def cpu_ticks(pid):
    with open(f"/proc/{pid}/stat") as f:
        fields = f.read().rsplit(")", 1)[1].split()
    return int(fields[11]) + int(fields[12])


def memory(pid):
    pss = rss = 0
    with open(f"/proc/{pid}/smaps_rollup") as f:
        for line in f:
            if line.startswith("Pss:"):
                pss = int(line.split()[1])
            elif line.startswith("Rss:"):
                rss = int(line.split()[1])
    return pss, rss


def main():
    profile = sys.argv[1] if len(sys.argv) > 1 else "hyprland"
    seconds = float(sys.argv[2]) if len(sys.argv) > 2 else 60
    groups = [(name, re.compile(rx)) for name, rx in PROFILES[profile]]
    members = {name: [] for name, _ in groups}
    for pid, cmd in processes():
        for name, rx in groups:
            if rx.search(cmd):
                members[name].append(pid)
                break
    start = {}
    for pids in members.values():
        for p in pids:
            try:
                start[p] = cpu_ticks(p)
            except OSError:
                pass
    t0 = time.monotonic()
    time.sleep(seconds)
    elapsed = time.monotonic() - t0
    rows = []
    for name, _ in groups:
        n = pss = rss = ticks = 0
        for p in members[name]:
            try:
                a, b = memory(p)
                ticks += cpu_ticks(p) - start.get(p, 0)
            except OSError:
                continue
            n += 1
            pss += a
            rss += b
        if n:
            rows.append((name, n, pss, rss, 100.0 * ticks / TICK / elapsed))
    print(f"{profile}: {elapsed:.0f} s")
    print(f"{'':30} {'proc':>5} {'PSS MiB':>9} {'RSS MiB':>9} {'CPU %':>7}")
    for name, n, pss, rss, cpu in rows:
        print(f"{name:30} {n:5} {pss / 1024:9.1f} {rss / 1024:9.1f} {cpu:7.2f}")
    tot = [sum(r[i] for r in rows) for i in (1, 2, 3, 4)]
    print(f"{'total':30} {tot[0]:5} {tot[1] / 1024:9.1f} {tot[2] / 1024:9.1f} {tot[3]:7.2f}")


if __name__ == "__main__":
    main()
