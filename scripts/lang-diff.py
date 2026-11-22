#!/usr/bin/env python3
"""Differential runner for the upstream QuickJS language tests.

The reference suite in `tests/` is the ground truth this engine is measured against.
Running it ad hoc kept losing tests: `test_loop.js` and `test_bigint.js` went
unmeasured for whole sessions, and `test_bigint.js` was green except for two silent
wrong answers that only showed up once someone ran it.

So the coverage is written down instead. `FILES` below lists every upstream file this
runner knows about, and for each one the tests that are *not* expected to pass, with
the reason. The runner then enforces three things:

  * a test the reference passes must pass here too, unless it is listed as a gap;
  * a listed gap that starts passing is an error, so the list cannot rot;
  * a test the reference itself fails (or hangs on) is reported and ignored — those
    are not our bugs.

    ./scripts/lang-diff.py              # compare against the oracle
    ./scripts/lang-diff.py --ours-only  # no oracle needed, just count passes
    ./scripts/lang-diff.py -v           # show every test, not only the exceptions
"""

import argparse
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from lang_tests import Sliced  # noqa: E402

# Where the upstream tests come from. `setup-oracle.sh` puts them here.
ORACLE_DIR = os.environ.get("QJS_ORACLE_DIR", "/tmp/quickjs-master")
TESTS_DIR = os.path.join(ORACLE_DIR, "tests")
OURS = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "target/release/qjs",
)

# How long one test may take. Generous: `test_pi` computes 2000 digits of pi through
# repeated bigint multiplication, which is seconds of work in a debug-ish build.
TIMEOUT = 60

# The files this runner measures, and the tests inside them that are known gaps.
#
# A file is only listed here when its tests can run with no host injection: `print` and
# `assert` are enough. Files that need `std`/`os`, modules, workers or an `Intl` build
# are left out entirely — see EXCLUDED_FILES for why each one is missing, so "not
# listed" is never mistaken for "nothing to do".
FILES = {
    "test_language.js": {
        "skip": {},
    },
    "test_closure.js": {
        "skip": {},
    },
    "test_loop.js": {
        # Every test the reference passes, passes here. `test_cyclic_labels` shows up in
        # the runner's "the reference itself fails" bucket: it loops forever on both
        # sides, because it only checks that `l: break l` inside `for(;;)` *compiles*.
        "skip": {},
    },
    "test_bigint.js": {
        "skip": {},
    },
    "test_builtin.js": {
        # Every test in this file that the reference passes, passes here too. The ones
        # it does not pass need `std`/`os` for GC assertions (`test_weak_map`,
        # `test_weak_ref`, `test_finalization_registry`), which is a host gap rather
        # than an engine one, and the runner ignores those.
        "skip": {},
    },
}

# Upstream files this runner does not attempt, with the reason. Listed so the gap is
# visible rather than forgotten.
EXCLUDED_FILES = {
    "assert.js": "a helper library, not a test file",
    "microbench.js": "a benchmark, not a conformance test",
    "test_bjson.js": "needs the `std` module for bytecode I/O",
    "test_intl.js": "needs an Intl-enabled build of the reference",
    "test_rw_handler.js": "needs Proxy (no Proxy in the realm yet)",
    "test_std.js": "needs the `std`/`os` host modules",
    "test_worker.js": "needs the worker host API",
    "test_worker_module.js": "needs the worker host API and module loading",
    # Covered instead by the whole-file harness: it is one module with a cycle in it,
    # not a set of `test_*` functions, so slicing it would break the imports that are
    # the whole point. See `quickjs-conformance::script_cases`.
    "test_cyclic_import.js": "a module, run whole by the conformance harness",
    "fixture_cyclic_import.js": "a module fixture, not a test file",
}


def run(script_path, test, binary):
    """Run one sliced test; returns (passed, detail).

    The driver catches its own exception so that one failing test cannot hide the rest,
    which means the exit status says nothing — the `PASS <name>` line it prints is the
    only signal. Reading the status instead reported every throwing test as a pass.
    """
    try:
        result = subprocess.run(
            [binary, script_path],
            capture_output=True,
            text=True,
            timeout=TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        return False, "TIMEOUT"
    if f"PASS {test}" in result.stdout:
        return True, ""
    detail = ""
    for line in result.stdout.split("\n") + result.stderr.split("\n"):
        if line.startswith(f"FAIL {test}:"):
            detail = line
            break
    if not detail:
        detail = (result.stderr or result.stdout).strip().split("\n")[0]
    return False, detail[:100]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--ours-only",
        action="store_true",
        help="do not run the reference; just report which tests pass here",
    )
    parser.add_argument(
        "-v", "--verbose", action="store_true", help="list every test, not only exceptions"
    )
    parser.add_argument(
        "--require-oracle",
        action="store_true",
        help="fail instead of skipping when the reference is not available (for CI)",
    )
    args = parser.parse_args()

    if not os.path.exists(OURS):
        sys.exit(f"missing {OURS}: run `cargo build --release` first")
    if args.ours_only:
        oracle = None
    else:
        oracle = os.path.join(ORACLE_DIR, "qjs")
        missing = [
            path
            for path in (oracle, TESTS_DIR)
            if not os.path.exists(path)
        ]
        if missing:
            message = (
                f"the reference interpreter or its tests are missing ({missing[0]});\n"
                "         run `./scripts/setup-oracle.sh` to build them"
            )
            if args.require_oracle:
                sys.exit(f"ERROR: {message}")
            print(f"SKIPPED: {message}")
            print("         every other gate step still ran; this one measured nothing")
            return 0

    scratch = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target")
    scratch = os.path.abspath(scratch)
    os.makedirs(scratch, exist_ok=True)
    slice_path = os.path.join(scratch, "lang-diff-slice.js")

    gaps, stale, reference_failed = [], [], []
    measured = passed = 0
    for filename, spec in FILES.items():
        path = os.path.join(TESTS_DIR, filename)
        if not os.path.exists(path):
            sys.exit(f"missing upstream test file {path}")
        sliced = Sliced(path)
        skip = spec["skip"]
        for test in sliced.tests:
            with open(slice_path, "w") as handle:
                handle.write(sliced.one(test))
            ours_ok, ours_detail = run(slice_path, test, OURS)
            measured += 1
            if args.ours_only:
                if ours_ok:
                    passed += 1
                    if args.verbose:
                        print(f"ok    {filename} {test}")
                else:
                    print(f"FAIL  {filename} {test}: {ours_detail}")
                continue

            oracle_ok, oracle_detail = run(slice_path, test, oracle)
            if not oracle_ok:
                reference_failed.append((filename, test, oracle_detail))
                continue
            if ours_ok:
                passed += 1
                if test in skip:
                    stale.append((filename, test, skip[test]))
                elif args.verbose:
                    print(f"ok    {filename} {test}")
            elif test in skip:
                if args.verbose:
                    print(f"known {filename} {test}: {skip[test]}")
            else:
                gaps.append((filename, test, ours_detail))

    print()
    print(f"{passed}/{measured} tests pass" + (" (ours only)" if args.ours_only else ""))
    for filename, test, reason in stale:
        print(f"STALE  {filename} {test} now passes — drop it from the gap list")
    for filename, test, detail in gaps:
        print(f"GAP    {filename} {test}: {detail}")
    if reference_failed and not args.ours_only:
        print(f"({len(reference_failed)} test(s) the reference itself fails, ignored)")
        for filename, test, detail in reference_failed:
            if args.verbose:
                print(f"  ref-fail {filename} {test}: {detail}")
    if not args.ours_only:
        known = sum(1 for spec in FILES.values() for _ in spec["skip"])
        print(f"({known} known gaps declared in the list)")
    return 1 if gaps or stale else 0


if __name__ == "__main__":
    sys.exit(main())
