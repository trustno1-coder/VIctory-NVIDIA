"""Exercise the command with synthetic, release-format play logs."""
import csv
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "compute_pipeline_timing.py"
SHADER = "0123456789abcdef"
BIND = f'INFO victory_gpu::pipeline: first bind pipeline=0x1 frame=7 stage="compute" size=4 hash={SHADER}'
HOST = f"INFO victory_gpu::pipeline::host: host pipeline shaders=[compute {SHADER}]"


def line(timestamp, event):
    return f"{timestamp}  {event}\n"


class PipelineTiming(unittest.TestCase):
    def test_logs_are_separate_and_timestamps_cross_midnight(self):
        first = "2026-01-01T23:59:59.999000Z"
        second = "2026-01-02T00:00:00.001250Z"
        samples = {
            "early.log": line(second, BIND) + line(first, HOST),
            "late.log": line(first, BIND) + line(second, HOST),
            "bind-only.log": line(first, BIND) + line(first, HOST.replace("host pipeline ", "host pipeline refused ")),
            "host-only.log": line(first, HOST) + line(first, BIND.replace('"compute"', '"pixel"')),
            "two-binds.log": line(first, HOST) + line(first, BIND) + line(second, BIND),
            "two-creations.log": line(first, BIND) + line(first, HOST) + line(second, HOST),
            "unrelated.log": line(first, BIND + "0") + line(first, HOST.replace(SHADER, "f" * 16)),
        }
        with tempfile.TemporaryDirectory() as tmp:
            paths = [Path(tmp, name) for name in samples]
            for path in paths:
                path.write_text(samples[path.name], encoding="utf-8-sig")
            run = subprocess.run(
                [sys.executable, "-I", str(SCRIPT), SHADER.upper(), *samples], cwd=tmp,
                text=True, capture_output=True, check=True,
            )
        rows = list(csv.reader(run.stdout.splitlines(), delimiter="\t"))
        self.assertEqual(rows[1:], [
            ["early.log", "1", "1", "-2.250"], ["late.log", "1", "1", "+2.250"],
            ["bind-only.log", "1", "0", "-"], ["host-only.log", "0", "1", "-"],
            ["two-binds.log", "2", "1", "-"], ["two-creations.log", "1", "2", "-"],
            ["unrelated.log", "0", "0", "-"],
        ])


if __name__ == "__main__":
    unittest.main()
