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

body = [preamble]
for helper in ("throw_error", "assert", "assert_throws", "my_func", "get_string_pos", "check_error_pos",
               "assert_json_error", "eval_error"):
    if helper in functions and helper not in kept:
        body.append(functions[helper])
for name in kept:
    body.append(functions[name])

drivers = [
    'try { %s(); print("PASS %s") } catch (e) { print("FAIL %s: " + e.message) }'
    % (name, name, name)
    for name in kept
]
print("\n".join(body) + "\n" + "\n".join(drivers) + "\n", end="")
