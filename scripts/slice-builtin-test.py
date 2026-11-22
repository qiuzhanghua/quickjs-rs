#!/usr/bin/env python3
"""Build a runnable slice of test_builtin.js.

The reference test file exercises features this engine does not have yet (regexp
literals, Map, TypedArray), and one unsupported construct anywhere fails the whole
*compile*. So each test function is extracted on its own and the drivers are emitted
so that one test failing does not hide the rest.

Extraction relies on the upstream file's layout: a top-level function starts at column
zero with `function name(` and ends at the first line that is exactly `}`. Brace
matching would have to understand strings, comments and regexp literals; the column-zero
convention does not.

# Helpers are found, not listed

A test calls ordinary top-level functions that are not themselves tests — `assert`,
`assert_throws`, and also a few whose names happen to start with `test_` (upstream's
`test_array` calls `test_array_extension`, for instance). A hand-written helper list
missed those repeatedly, and a missing helper looks exactly like an engine bug: the
test fails with `x is not defined`. So the helper set is computed instead: starting
from the kept tests, take the transitive closure of the top-level functions they
mention. Anything a kept test can reach is included; nothing else is.
"""
import re
import sys

src = open(sys.argv[1]).read()
skip = set(sys.argv[2].split(",")) if len(sys.argv) > 2 and sys.argv[2] else set()
lines = src.split("\n")

# Where each top-level function starts and ends, by the column-zero convention.
starts = [(i, m.group(1)) for i, line in enumerate(lines)
          for m in [re.match(r"^function (\w+)\(", line)] if m]
functions = {}
for position, (begin, name) in enumerate(starts):
    stop = begin + 1
    while stop < len(lines) and lines[stop] != "}":
        stop += 1
    functions[name] = "\n".join(lines[begin:stop + 1])

# The preamble is everything before the first function, so it carries only the
# `"use strict"`/globals header; the assert helpers are pulled in as helpers below.
preamble = "\n".join(lines[:starts[0][0]])
for match in re.finditer(r"^var poisoned_number = \{.*?^\};", src, re.M | re.S):
    preamble += "\n" + match.group(0) + "\n"

tests = [name for _, name in starts if name.startswith("test_") or name == "test"]
kept = [name for name in tests if name not in skip]

# A bare identifier that names a top-level function. It has to be a *reference*, not only
# a call: upstream's `test_function` reaches `my_func` through `my_func.call(null, 1, 2)`,
# so a call-only scan would miss it and the test would fail with `my_func is not defined` —
# which reads like an engine bug rather than a gap in this script. A property name after a
# dot is excluded (`o.f`), which is the one case where a member can share a function's name.
REFERENCE = re.compile(r"(?<![.\w$])([A-Za-z_$][\w$]*)")


def calls_in(name):
    return {match.group(1) for match in REFERENCE.finditer(functions[name])}


# Every function the kept tests can reach, in source order so the emitted file reads
# like the upstream one. `kept` is a subset, so the drivers still only cover the tests
# that were actually asked for.
needed, pending = set(), list(kept)
while pending:
    for callee in sorted(calls_in(pending.pop())):
        if callee in functions and callee not in needed:
            needed.add(callee)
            pending.append(callee)

body = [preamble]
for _, name in starts:
    if name in needed:
        body.append(functions[name])

drivers = [
    'try { %s(); print("PASS %s") } catch (e) { print("FAIL %s: " + e.message) }'
    % (name, name, name)
    for name in kept
]
print("\n".join(body) + "\n" + "\n".join(drivers) + "\n", end="")
