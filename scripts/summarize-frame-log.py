"""Summarize VIctory diagnostic frame samples without printing raw log lines."""

import argparse
from bisect import bisect_left
from datetime import datetime
import json
import re
from statistics import median

FRAME = re.compile(
    r"\b(\d+(?:\.\d+)?) fps, (\d+(?:\.\d+)?) ms/frame"
    r" \((\d+(?:\.\d+)?) preparing:.*?\),\s*(\d+) draws/frame,"
    r" dropped \d+ without a host pipeline \((\d+) being made\)"
)
ROOM = re.compile(
    r"the texture queue's room:.*?\+ (\d+) MB kept for render targets"
    r" = \d+ MB against a line of \d+ MB.*?: (.+)"
)
UPLOADS = re.compile(r"texture uploads: (\d+) recorded, (\d+) left out")
TIMESTAMP = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z")
MAX_OFFSET_SECONDS = 0.5


def read_samples(lines):
    frames, budgets, uploads = [], [], []
    for line in lines:
        match = FRAME.search(line)
        budget = ROOM.search(line)
        upload = UPLOADS.search(line) if "frame account per frame:" in line else None
        if not (match or budget or upload):
            continue
        stamp = TIMESTAMP.match(line)
        if not stamp:
            continue
        time = datetime.fromisoformat(stamp[0]).timestamp()
        if match:
            fps, milliseconds, preparing, draws, building = match.groups()
            frames.append((time, float(fps), float(milliseconds), float(preparing),
                           int(draws), int(building)))
        if budget:
            budgets.append((time, (int(budget[1]), budget[2].startswith("no room"))))
        if upload:
            uploads.append((time, tuple(map(int, upload.groups()))))
    return frames, budgets, uploads


def correlate(frames, records):
    """Return at most one diagnostic record per frame window."""
    records = sorted(records)
    stamps = [record[0] for record in records]
    matched = []
    for frame in frames:
        time = frame[0]
        index = bisect_left(stamps, time)
        nearest = min(records[max(0, index - 1):index + 1],
                      key=lambda record: abs(record[0] - time), default=None)
        if nearest is not None and abs(nearest[0] - time) <= MAX_OFFSET_SECONDS:
            matched.append(nearest[1])
    return matched


def summarize(lines, min_draws=1000, slow_ms=200, start=0, end=None):
    frames, budgets, uploads = read_samples(lines)
    if not frames:
        return {"samples": 0}
    origin = frames[0][0]
    world = [frame for frame in frames
             if frame[4] >= min_draws and frame[0] - origin >= start
             and (end is None or frame[0] - origin < end)]
    if not world:
        return {"samples": 0}
    slow = [frame for frame in world if frame[2] >= slow_ms]
    room = correlate(world, budgets)
    slow_room = correlate(slow, budgets)
    slow_uploads = correlate(slow, uploads)

    return {
        "samples": len(world),
        "median_fps": round(median(frame[1] for frame in world), 2),
        "median_frame_ms": round(median(frame[2] for frame in world), 2),
        "median_prepare_ms": round(median(frame[3] for frame in world), 2),
        "slow_threshold_ms": slow_ms,
        "slow_samples": len(slow),
        "slow_samples_with_zero_pipelines_building": sum(frame[5] == 0 for frame in slow),
        "budget_samples": len(room),
        "median_target_reserve_mb": median(reserved for reserved, _ in room) if room else None,
        "slow_samples_with_budget": len(slow_room),
        "slow_samples_with_texture_queue_blocked": sum(blocked for _, blocked in slow_room),
        "slow_samples_with_upload_counts": len(slow_uploads),
        "median_slow_uploads_left_out": median(skipped for _, skipped in slow_uploads)
        if slow_uploads else None,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log")
    parser.add_argument("--min-draws", type=int, default=1000)
    parser.add_argument("--slow-ms", type=float, default=200)
    parser.add_argument("--start", type=float, default=0, help="seconds after the first FPS sample")
    parser.add_argument("--end", type=float, help="exclusive seconds after the first FPS sample")
    args = parser.parse_args()
    if args.min_draws < 0 or args.slow_ms <= 0 or args.start < 0:
        parser.error("min-draws and start must be nonnegative, and slow-ms positive")
    if args.end is not None and args.end <= args.start:
        parser.error("end must be greater than start")
    try:
        with open(args.log, encoding="utf-8-sig", errors="replace") as log:
            report = summarize(log, args.min_draws, args.slow_ms, args.start, args.end)
    except OSError:
        parser.exit(1, "Could not read the log.\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
