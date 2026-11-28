"""Compare one compute program's host creation and first bind in a play log.

    python scripts/compute_pipeline_timing.py HASH control.log candidate.log

Give one run per file. A positive delta means host creation was logged after
first bind. A dash means an event is missing or the hash matched multiple
pipelines. The host event has no title pipeline ID to pair repeated matches.
The two observations alone do not tell whether a dispatch waited or ran.
"""

import argparse
import csv
import re
import sys
from datetime import datetime
from pathlib import Path


def compute_hash(value):
    if not re.fullmatch(r"[0-9a-fA-F]{16}", value):
        raise argparse.ArgumentTypeError("expected a 16-digit hexadecimal shader hash")
    return value.lower()


def timing(lines, shader):
    bind_message = re.compile(
        r"\bvictory_gpu::pipeline: first bind\s.*?\bstage=\"compute\"\s.*?\bhash="
        + shader + r"(?=\s|$)"
    )
    host_message = "victory_gpu::pipeline::host: host pipeline shaders=[compute " + shader + "]"
    binds = creates = 0
    bound_at = made_at = None
    for line in lines:
        if shader not in line:
            continue
        if bind_message.search(line):
            binds += 1
            bound_at = line.split(maxsplit=1)[0]
        elif host_message in line:
            creates += 1
            made_at = line.split(maxsplit=1)[0]
    delta = "-"
    if binds == creates == 1:
        bound = datetime.fromisoformat(bound_at.replace("Z", "+00:00"))
        made = datetime.fromisoformat(made_at.replace("Z", "+00:00"))
        delta = f"{(made - bound).total_seconds() * 1000:+.3f}"
    return binds, creates, delta


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("hash", type=compute_hash)
    parser.add_argument("logs", type=Path, nargs="+")
    args = parser.parse_args()
    out = csv.writer(sys.stdout, delimiter="\t", lineterminator="\n")
    out.writerow(("log", "first_binds", "host_creations", "creation_minus_bind_ms"))
    for path in args.logs:
        try:
            with path.open(encoding="utf-8-sig") as log:
                out.writerow((path, *timing(log, args.hash)))
        except (OSError, ValueError) as error:
            parser.exit(1, f"{path}: {error}\n")


if __name__ == "__main__":
    main()
