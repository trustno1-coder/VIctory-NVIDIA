r"""Compares fixed-spot runs by the layer's frame-rate line ("12.3 fps, 81.2 ms/frame (...) ... 8034 draws/frame").

Usage: python compare-logs.py <log 1> <log 2> [...]
  log: %LOCALAPPDATA%\victory\logs\victory-nvidia-*.log (or foxhound-*.log).
Stable lines: from 90 s after the first line of the game's world (1,000 draws or more), draws at least
85 % of the run's median, and no hitch (fps above half the median). The first log is the reference.
Run-to-run noise on a Ryzen 5 5600X was about +-9 % for 2-minute runs: compare 6-7 minute runs, and
read the "real" / "within the noise" verdict (two standard errors) with that in mind.
"""
import os
import re
import statistics
import sys
from datetime import datetime

LINE = re.compile(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)\S*\s.*?\s(\d+(?:\.\d+)?) fps, (\d+(?:\.\d+)?) ms/frame.*?(\d+) draws/frame")
COLOR = re.compile(r"\x1b\[[0-9;]*m")


def read(path):
    rows = []
    with open(path, encoding="utf-8", errors="replace") as f:
        for raw in f:
            m = LINE.search(COLOR.sub("", raw))
            if m:
                rows.append((datetime.fromisoformat(m.group(1)), float(m.group(2)), float(m.group(4))))
    return rows


def stable(rows):
    game = [r for r in rows if r[2] >= 1000]
    if not game:
        return []
    start = game[0][0]
    game = [r for r in game if (r[0] - start).total_seconds() >= 90]
    if not game:
        return []
    med = statistics.median(r[2] for r in game)
    game = [r for r in game if r[2] >= 0.85 * med]
    fps_med = statistics.median(r[1] for r in game)
    return [r for r in game if r[1] >= 0.5 * fps_med]


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    ref = None
    for path in sys.argv[1:]:
        rows = read(path)
        s = stable(rows)
        if len(s) < 3:
            print(f"{os.path.basename(path)}: too few stable lines ({len(s)} of {len(rows)})")
            continue
        fps = [r[1] for r in s]
        mean = statistics.mean(fps)
        err = statistics.stdev(fps) / len(fps) ** 0.5
        span = (s[-1][0] - s[0][0]).total_seconds()
        line = (f"{os.path.basename(path)}: {mean:.2f} +-{err:.2f} rendered fps ({len(fps)} lines, {span:.0f} s), "
                f"{statistics.mean(r[2] for r in s):.0f} draws")
        if ref is None:
            ref = (mean, err)
        else:
            diff = mean - ref[0]
            e = (err ** 2 + ref[1] ** 2) ** 0.5
            line += f"  | {diff:+.2f} fps ({100 * diff / ref[0]:+.0f}%) against the first, {'real' if abs(diff) > 2 * e else 'within the noise'}"
        print(line)


main()
