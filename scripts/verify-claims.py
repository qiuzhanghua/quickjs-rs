#!/usr/bin/env python3
"""Check the numbers `README.md` publishes against what the suites actually measure.

Why this exists: the "与 C 版的一致性验证" table used to be maintained by hand, and by the
time anyone looked, six of its numbers had drifted from reality — 35 literals where the
corpus held 36, 41 declaration-conflict cases where it held 59, 365 bytecode cases where
366 were compared. Every one of those was a claim nobody could check, which is the same
failure mode as a strip of green tests that never ran.

So each row gets a source of truth:

  * bytecode rows  — counted at runtime by the test itself (`QUICKJS_CASE_COUNTS`),
                     so the number is what was *compared*, not what was listed;
  * everything else — counted from the corpora and the test sources by parsing the same
                     constructs the tests use.

    ./scripts/verify-claims.py          # exit 0 when every published number is right
    ./scripts/verify-claims.py -v       # print every row, matching or not
"""

import gzip
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
README = ROOT / "README.md"
STATUS = ROOT / "docs" / "test262-status.md"

VERBOSE = "-v" in sys.argv or "--verbose" in sys.argv

problems = []
checked = 0


def check(name, published, measured):
    """Compare one published number with its measurement."""
    global checked
    checked += 1
    ok = published == measured
    if not ok:
        problems.append(f"{name}: README says {published}, measured {measured}")
    if VERBOSE or not ok:
        mark = "ok  " if ok else "FAIL"
        print(f"  {mark} {name}: README {published}, measured {measured}")
    return ok


def read(path):
    return path.read_text(encoding="utf-8")


def array_items(source, name, occurrence=1):
    """The string literals of the `let <name> = [ … ];` array in `source`.

    Handles both plain and raw string literals, and stops at the `];` that closes the
    array. Comments are dropped first so a quoted example in a comment is not counted.
    """
    pattern = rf"let {name}\s*(?::[^=]+)?=\s*\["
    matches = list(re.finditer(pattern, source))
    if len(matches) < occurrence:
        return None
    match = matches[occurrence - 1]
    start = match.end()
    depth = 1
    index = start
    while index < len(source) and depth:
        char = source[index]
        if char == "[":
            depth += 1
        elif char == "]":
            depth -= 1
        index += 1
    body = source[start:index - 1]
    items = []
    position = 0
    while position < len(body):
        if body.startswith('r#"', position):
            end = body.find('"#', position + 3)
            items.append(body[position + 3:end])
            position = end + 2
        elif body[position] == '"':
            end = position + 1
            while end < len(body) and body[end] != '"':
                end += 2 if body[end] == "\\" else 1
            items.append(body[position + 1:end])
            position = end + 1
        else:
            position += 1
    return items


def bytecode_counts():
    """Ask the disassembly tests how many cases they actually compared."""
    with tempfile.TemporaryDirectory() as scratch:
        env = dict(os.environ, QUICKJS_CASE_COUNTS=scratch)
        subprocess.run(
            ["cargo", "test", "--offline", "-q", "-p", "quickjs-compile",
             "--test", "disasm_diff"],
            cwd=ROOT, env=env, capture_output=True, check=False,
        )
        counts_file = Path(scratch) / "disasm-cases.txt"
        if not counts_file.is_file():
            return None
        counts = {}
        for line in counts_file.read_text().splitlines():
            name, given, compared = line.split("\t")
            counts[name] = (int(given), int(compared))
        return counts


def consistency_table():
    """The lines of the `## 与 C 版的一致性验证` table.

    Restricted to that section on purpose: `字节码` and several other row labels also appear
    in the milestone table, and matching the first occurrence picked up the wrong row.
    """
    lines = read(README).splitlines()
    start = next(i for i, line in enumerate(lines) if line.startswith("## 与 C 版的一致性验证"))
    end = next(i for i, line in enumerate(lines[start + 1:], start + 1)
               if line.startswith("## ") or line.startswith("### "))
    return lines[start:end]


def readme_row(label):
    """Every number in the consistency-table row whose first cell contains `label`."""
    for line in consistency_table():
        if line.startswith("|") and label in line.split("|")[1]:
            # Drop backtick spans first: the tool column names commands like
            # `-DDUMP_BYTECODE=1`, whose "1" is not a count. Everything else in the row
            # is prose that may carry a number (`同上 + **27** 条不变量测试`).
            without_code = re.sub(r"`[^`]*`", "", line)
            numbers = re.findall(r"\*\*(\d+)\*\*|(?<![\w.])(\d+)(?![\w.])", without_code)
            return [int(one or two) for one, two in numbers]
    return None


def main():
    print("verify-claims: checking every number README.md publishes")

    # -- bytecode rows, counted at runtime ---------------------------------
    counts = bytecode_counts()
    if counts is None:
        problems.append("bytecode counts: the disassembly tests produced no counts")
    else:
        total_given = sum(given for given, _ in counts.values())
        total_compared = sum(compared for _, compared in counts.values())
        rows = {
            "字节码": total_compared,
            "break/continue/标签": counts["loops_and_labels_match_the_reference"][1],
            "switch": counts["switch_matches_the_reference"][1],
            "try/catch/finally": counts["try_catch_finally_matches_the_reference"][1],
            "for-in / for-of": counts["for_in_of_matches_the_reference"][1],
            "对象与数组字面量": counts["literals_build_in_place"][1],
            "赋值": counts["assignments_match_the_reference"][1],
            "方法调用": counts["calls_pass_their_receiver"][1],
            "`??` 与可选链": counts["nullish_and_optional_chains_match_the_reference"][1],
        }
        for label, measured in rows.items():
            numbers = readme_row(label)
            if numbers is None:
                problems.append(f"{label}: no row found in README.md")
                continue
            check(f"README row {label}", numbers[0], measured)

    # -- literal corpora, counted from the test sources --------------------
    lexer = read(ROOT / "crates/quickjs-parse/tests/oracle.rs")
    check("数值字面量", readme_row("数值字面量")[0],
          len(array_items(lexer, "literals")))
    strings = re.search(r"fn string_literals_decode_to_the_same_text.*?let literals = \[(.*?)\n    \];",
                        lexer, re.S)
    check("字符串字面量", readme_row("字符串字面量")[0],
          len(re.findall(r'r#"', strings.group(1))))

    # -- syntax acceptance -------------------------------------------------
    parser = read(ROOT / "crates/quickjs-parse/tests/parse_oracle.rs")
    row = readme_row("语法接受性")
    corpora = re.findall(r"let corpus\s*(?::[^=]+)?=\s*\[", parser)
    # The file holds two `corpus` arrays: the valid scripts and the invalid ones, in that
    # order, plus a `valid`/`invalid` pair for modules.
    arrays = re.findall(r"let (?:corpus|valid|invalid)\s*(?::[^=]+)?=\s*\[", parser)
    sizes = [len(array_items(parser, name) or []) for name in ("corpus",)]
    valid = sizes[0]
    invalid = len(array_items(parser, "corpus", occurrence=2) or [])
    modules_valid = len(array_items(parser, "valid") or [])
    modules_invalid = len(array_items(parser, "invalid") or [])
    check("语法接受性 有效脚本", row[0], valid)
    check("语法接受性 无效脚本", row[1], invalid)
    check("语法接受性 模块用例", row[2], modules_valid + modules_invalid)

    scope = read(ROOT / "crates/quickjs-parse/tests/scope_oracle.rs")
    rejected = array_items(scope, "rejected_by_reference") or []
    accepted = array_items(scope, "accepted_by_reference") or []
    conflicts = rejected + accepted
    check("声明冲突", readme_row("声明冲突")[0], len(conflicts))
    # Each source may appear in only one list: the test now checks that each label matches
    # the reference, so a source in both lists would demand two different verdicts.
    distinct = len(set(conflicts))
    if distinct != len(conflicts):
        problems.append(
            f"声明冲突: {len(conflicts)} cases but only {distinct} distinct — "
            f"the same source is listed twice"
        )

    # -- the TDZ invariants are a separate file ----------------------------
    tdz = read(ROOT / "crates/quickjs-compile/tests/tdz.rs")
    invariants = len(re.findall(r"#\[test\]\s*\nfn ", tdz))
    row = readme_row("TDZ/作用域")
    check("TDZ 不变量测试数", row[0], invariants)

    # -- test262 baseline, from the committed outcome file -----------------
    baseline = ROOT / "docs/test262-baseline.txt.gz"
    if baseline.is_file():
        lines = gzip.open(baseline, "rt").read().splitlines()
        running = sum(1 for line in lines if line.startswith(("PASS", "FAIL")))
        passing = sum(1 for line in lines if line.startswith("PASS"))
        status = read(STATUS)
        match = re.search(r"\*\*可运行中通过\*\* \| \*\*(\d+)（([\d.]+)%）", status)
        if match:
            check("test262 通过数", int(match.group(1)), passing)
            check("test262 可运行数", re.search(
                r"可运行（runner 不跳过） \| (\d+)", status).group(1) and
                int(re.search(r"可运行（runner 不跳过） \| (\d+)", status).group(1)),
                running)

    print()
    if problems:
        print(f"{len(problems)} of {checked} claims are wrong:")
        for problem in problems:
            print(f"  - {problem}")
        return 1
    print(f"all {checked} published numbers match what the suites measure")
    return 0


if __name__ == "__main__":
    sys.exit(main())
