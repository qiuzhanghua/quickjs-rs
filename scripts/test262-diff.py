#!/usr/bin/env python3
"""Compare two test262 outcome files, test by test.

`docs/test262-status.md` records every fix as "net N fixed, 0 regressions", and each
of those numbers came from diffing two full runs by test name. That diff was done by
hand each time, which means it was done when someone remembered to — and the recorded
history still contains a milestone that was declared "complete" while five tests in the
sliced suite were failing, because the sample was never re-measured.

This script makes the diff a command. It reads the `--outcomes` format the runner
writes (`PASS<TAB>name`, `FAIL<TAB>name`, `SKIP<TAB>name<TAB>reason`) and reports the
two numbers that matter, as sets rather than as totals:

    +N   tests that now pass and did not before
    -M   tests that passed before and do not now

A total that stays the same while one test regresses and another is fixed is exactly
what an aggregate hides, and it is the failure mode this tool exists to catch.
"""

import argparse
import gzip
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_BASELINE = REPO / "docs" / "test262-baseline.txt.gz"


def read_outcomes(path):
    """Map test name -> outcome word, Gzipped or plain."""
    opener = gzip.open if path.suffix == ".gz" else open
    outcomes = {}
    with opener(path, "rt", encoding="utf-8") as handle:
        for line in handle:
            parts = line.rstrip("\n").split("\t")
            if len(parts) < 2:
                continue
            outcomes[parts[1]] = parts[0]
    return outcomes


def by_outcome(outcomes, word):
    return {name for name, outcome in outcomes.items() if outcome == word}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("current", help="outcomes file from the run just made (.gz ok)")
    parser.add_argument(
        "--baseline",
        default=str(DEFAULT_BASELINE),
        help="baseline outcomes file (default: docs/test262-baseline.txt.gz)",
    )
    parser.add_argument(
        "--list",
        type=int,
        default=10,
        metavar="N",
        help="how many names to show per category (0 for none)",
    )
    args = parser.parse_args()

    current_path = Path(args.current)
    baseline_path = Path(args.baseline)
    for path in (current_path, baseline_path):
        if not path.is_file():
            print(f"missing {path}", file=sys.stderr)
            return 2

    current = read_outcomes(current_path)
    baseline = read_outcomes(baseline_path)

    # Only tests present in both runs are comparable. A test that appeared or vanished
    # means the checkout changed, which is a different question from "did we regress",
    # so it is reported separately rather than folded into the two headline numbers.
    shared = set(current) & set(baseline)
    only_current = sorted(set(current) - set(baseline))
    only_baseline = sorted(set(baseline) - set(current))

    baseline_pass = by_outcome(baseline, "PASS") & shared
    current_pass = by_outcome(current, "PASS") & shared

    fixed = sorted(current_pass - baseline_pass)
    regressed = sorted(baseline_pass - current_pass)

    def counts(outcomes):
        pass_n = sum(1 for value in outcomes.values() if value == "PASS")
        fail_n = sum(1 for value in outcomes.values() if value == "FAIL")
        skip_n = sum(1 for value in outcomes.values() if value == "SKIP")
        runnable = pass_n + fail_n
        rate = 100.0 * pass_n / runnable if runnable else 100.0
        return pass_n, fail_n, skip_n, runnable, rate

    b_pass, _, _, b_runnable, b_rate = counts(baseline)
    c_pass, _, _, c_runnable, c_rate = counts(current)

    print(f"baseline  {b_pass}/{b_runnable} runnable pass ({b_rate:.1f}%)")
    print(f"current   {c_pass}/{c_runnable} runnable pass ({c_rate:.1f}%)")
    print()
    print(f"+{len(fixed)}   fixed (now pass, did not before)")
    print(f"-{len(regressed)}   regressed (passed before, do not now)")

    if only_current or only_baseline:
        print()
        print(
            f"note: {len(only_current)} test(s) only in the current run, "
            f"{len(only_baseline)} only in the baseline — excluded from the diff"
        )

    for title, names in (("regressions", regressed), ("fixes", fixed)):
        if not names or args.list <= 0:
            continue
        print(f"\n-- {min(args.list, len(names))} of {len(names)} {title}:")
        for name in names[: args.list]:
            print(f"  {name}")

    # A regression is the one outcome that must never pass silently, so it decides the
    # exit status on its own; fixes alone are good news, not a failure.
    return 1 if regressed else 0


if __name__ == "__main__":
    sys.exit(main())
