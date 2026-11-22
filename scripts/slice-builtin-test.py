#!/usr/bin/env python3
"""Build a runnable slice of an upstream test file.

Kept as the single-file entry point it has always been; the slicing itself lives in
`lang_tests.py`, which `lang-diff.py` uses to run each test on its own.

    ./scripts/slice-builtin-test.py <upstream-test.js> [skip,comma,separated]
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from lang_tests import Sliced  # noqa: E402


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    sliced = Sliced(sys.argv[1])
    skip = set(sys.argv[2].split(",")) if len(sys.argv) > 2 and sys.argv[2] else set()
    print(sliced.script([name for name in sliced.tests if name not in skip]), end="")


if __name__ == "__main__":
    main()
