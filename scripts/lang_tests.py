#!/usr/bin/env python3
"""Slice an upstream QuickJS test file into runnable pieces, one per test.

The upstream files are single scripts that call every test at the bottom, so an
unsupported construct anywhere fails the whole *compile* and one failing test hides
all the rest. Slicing each test out on its own turns "the file does not run" into a
count of which tests pass.

Extraction relies on the upstream layout: a top-level function starts at column zero
with `function name(` and ends at the first line that is exactly `}`. Brace matching
would have to understand strings, comments and regexp literals; the column-zero
convention does not.

Helpers are found, not listed
-----------------------------
A test calls ordinary top-level functions that are not themselves tests — `assert`,
`assert_throws`, and also a few whose names happen to start with `test_` (upstream's
`test_array` calls `test_array_extension`, for instance). A hand-written helper list
missed those repeatedly, and a missing helper looks exactly like an engine bug: the
test fails with `x is not defined`. So the helper set is computed instead: starting
from the kept tests, take the transitive closure of the top-level functions they
mention. Anything a kept test can reach is included; nothing else is.
"""

import re

# A bare identifier that names a top-level function. It has to be a *reference*, not
# only a call: upstream's `test_function` reaches `my_func` through
# `my_func.call(null, 1, 2)`, so a call-only scan would miss it and the test would fail
# with `my_func is not defined` — which reads like an engine bug rather than a gap in
# this tooling. A property name after a dot is excluded (`o.f`), which is the one case
# where a member can share a function's name.
REFERENCE = re.compile(r"(?<![.\w$])([A-Za-z_$][\w$]*)")


class Sliced:
    """One upstream test file, taken apart."""

    def __init__(self, path):
        source = open(path).read()
        lines = source.split("\n")

        # Where each top-level function starts and ends, by the column-zero convention.
        starts = [
            (i, match.group(1))
            for i, line in enumerate(lines)
            for match in [re.match(r"^function (\w+)\(", line)]
            if match
        ]
        if not starts:
            raise ValueError(f"{path}: no top-level functions found")
        self.functions = {}
        for begin, name in starts:
            stop = begin + 1
            while stop < len(lines) and lines[stop] != "}":
                stop += 1
            self.functions[name] = "\n".join(lines[begin : stop + 1])
        self.order = [name for _, name in starts]

        # Everything before the first function: the `"use strict"`/globals header.
        # The assert helpers are pulled in as helpers instead.
        self.preamble = "\n".join(lines[: starts[0][0]])
        for match in re.finditer(r"^var poisoned_number = \{.*?^\};", source, re.M | re.S):
            self.preamble += "\n" + match.group(0) + "\n"

        # A test takes no arguments. Upstream names a few *helpers* `test_`-something —
        # `test_less(a, b)` and `test_map1(key_type, n)` — and running one with no
        # arguments fails on both sides for reasons that say nothing about the engine,
        # so the parameter list is what separates them from a real test.
        self.tests = [
            name
            for name in self.order
            if (name.startswith("test_") or name == "test")
            and re.match(rf"^function {re.escape(name)}\(\)", self.functions[name])
        ]

    def _referenced(self, name):
        return {match.group(1) for match in REFERENCE.finditer(self.functions[name])}

    def helpers_for(self, tests):
        """Every function the given tests can reach, in source order."""
        needed, pending = set(), list(tests)
        while pending:
            for callee in sorted(self._referenced(pending.pop())):
                if callee in self.functions and callee not in needed:
                    needed.add(callee)
                    pending.append(callee)
        return [name for name in self.order if name in needed]

    def script(self, tests):
        """A script that runs `tests`, each one isolated from the others' failures."""
        body = [self.preamble]
        for name in self.helpers_for(tests):
            body.append(self.functions[name])
        drivers = [
            'try { %s(); print("PASS %s") } catch (e) { print("FAIL %s: " + e.message) }'
            % (name, name, name)
            for name in tests
        ]
        return "\n".join(body) + "\n" + "\n".join(drivers) + "\n"

    def one(self, test):
        """A script for exactly one test, so a hang in it cannot take the rest down."""
        return self.script([test])
