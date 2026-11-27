#!/usr/bin/env python3
r"""Generate `crates/quickjs-pure/src/regexp/unicode_tables.rs` from the UCD.

The ECMAScript `\p{…}` grammar (§22.2.1) names three unrelated kinds of property, and
getting them confused is the usual bug:

* **General_Category** — one value per code point, e.g. `Lu` (`Uppercase_Letter`).
  `\p{Lu}` is a *property value* escape.
* **Script** / **Script_Extensions** — also one value per code point, but from a
  different partition. `\p{Script=Greek}` is a *property=value* escape.
* **Binary properties** — a yes/no flag, e.g. `Alphabetic`. Usually written bare
  (`\p{Alphabetic}`), and *never* with a value.

A name in `\p{…}` may be any of the Unicode aliases, so `\p{Alpha}` and
`\p{Alphabetic}` are the same property; that is what the alias tables below are for.

Data source: the Unicode Character Database, read straight from the `.txt` files. It is
deliberately *not* the test262 expected-value files — deriving the tables from the tests
would make them agree by construction and prove nothing.

Usage:
    scripts/gen-unicode-tables.py /path/to/ucd > crates/quickjs-pure/src/regexp/unicode_tables.rs
"""

import sys
import os
from collections import defaultdict

MAX_CP = 0x110000


def read_lines(path):
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            yield line


def parse_range(field):
    """`0041..005A` or `0041` -> (lo, hi)."""
    if ".." in field:
        lo, hi = field.split("..")
        return int(lo, 16), int(hi, 16)
    value = int(field, 16)
    return value, value


def ranges_to_set(ranges):
    """A sorted, merged list of closed intervals."""
    ranges = sorted(ranges)
    merged = []
    for lo, hi in ranges:
        if merged and lo <= merged[-1][1] + 1:
            merged[-1] = (merged[-1][0], max(merged[-1][1], hi))
        else:
            merged.append((lo, hi))
    return merged


def set_to_ranges(flags):
    """A `[bool]` bitmap -> the intervals where it is true."""
    ranges = []
    start = None
    for cp in range(MAX_CP):
        if flags[cp]:
            if start is None:
                start = cp
        elif start is not None:
            ranges.append((start, cp - 1))
            start = None
    if start is not None:
        ranges.append((start, MAX_CP - 1))
    return ranges


class Ucd:
    def __init__(self, root):
        self.root = root
        self.gc = [None] * MAX_CP
        self.scripts = [None] * MAX_CP
        self.script_extensions = [None] * MAX_CP
        self.binary = defaultdict(lambda: [False] * MAX_CP)
        self.aliases = defaultdict(dict)   # kind -> short name -> long name
        self.load()

    def load(self):
        self.load_unicode_data()
        self.load_property_file(os.path.join(self.root, "Scripts.txt"), self.set_script)
        self.load_script_extensions(os.path.join(self.root, "ScriptExtensions.txt"))
        for name in (
            "PropList.txt",
            "DerivedCoreProperties.txt",
            "DerivedNormalizationProps.txt",
            "emoji-data.txt",
            # `Bidi_Mirrored` lives here rather than in `PropList.txt`, which is the kind of
            # split that makes a property silently absent.
            os.path.join("extracted", "DerivedBinaryProperties.txt"),
        ):
            path = os.path.join(self.root, name)
            if os.path.exists(path):
                self.load_property_file(path, self.set_binary)
        self.load_property_aliases(os.path.join(self.root, "PropertyAliases.txt"))
        self.load_property_value_aliases(
            os.path.join(self.root, "PropertyValueAliases.txt")
        )

    def load_unicode_data(self):
        """`UnicodeData.txt` carries General_Category, one line per code point.

        Ranges are spelled as a `First>`/`Last>` pair, so the lines in between inherit the
        category of the `First` line.
        """
        pending = None
        for line in read_lines(os.path.join(self.root, "UnicodeData.txt")):
            fields = line.split(";")
            cp = int(fields[0], 16)
            name, category = fields[1], fields[2]
            if name.endswith(", First>"):
                pending = category
                start = cp
                continue
            if name.endswith(", Last>"):
                for value in range(start, cp + 1):
                    self.gc[value] = pending
                pending = None
                continue
            self.gc[cp] = category
        # Anything UnicodeData.txt omits is unassigned.
        for cp in range(MAX_CP):
            if self.gc[cp] is None:
                self.gc[cp] = "Cn"

    def load_property_file(self, path, apply):
        for line in read_lines(path):
            fields = [field.strip() for field in line.split(";")]
            lo, hi = parse_range(fields[0])
            apply(lo, hi, fields[1])

    def set_script(self, lo, hi, script):
        for cp in range(lo, hi + 1):
            self.scripts[cp] = script

    def set_binary(self, lo, hi, name):
        flags = self.binary[name]
        for cp in range(lo, hi + 1):
            flags[cp] = True

    def load_script_extensions(self, path):
        """`ScriptExtensions.txt` lists the set of scripts a code point belongs to.

        A code point with an entry belongs to *several* scripts, so `Script_Extensions`
        is not a partition and cannot reuse the single `scripts` column.
        """
        self.script_extension_sets = defaultdict(lambda: [False] * MAX_CP)
        # Which code points the file mentions at all. `scx` is the entry when there is one
        # and `Script` otherwise — *not* the entry plus `Script`, which would wrongly put a
        # code point in `Common` as well as in the scripts it actually belongs to.
        self.has_extension = [False] * MAX_CP
        if not os.path.exists(path):
            return
        for line in read_lines(path):
            fields = [field.strip() for field in line.split(";")]
            lo, hi = parse_range(fields[0])
            names = [name.strip() for name in fields[1].split()]
            for cp in range(lo, hi + 1):
                self.has_extension[cp] = True
                for name in names:
                    self.script_extension_sets[name][cp] = True

    def load_property_aliases(self, path):
        """Property names: `Alpha ; Alphabetic` plus a third field, the canonical kind."""
        if not os.path.exists(path):
            return
        for line in read_lines(path):
            fields = [field.strip() for field in line.split(";")]
            if len(fields) < 2:
                continue
            short, long = fields[0], fields[1]
            # A third field is a further alias: `WSpace ; White_Space ; space`.
            for name in fields[:3]:
                if name:
                    self.aliases["property"][name] = long

    def load_property_value_aliases(self, path):
        """Value names per property, e.g. `gc ; Lu ; Uppercase_Letter`.

        The *first* field is the canonical short form and every other field is an alias for
        it, so the mapping runs the other way from `load_property_aliases`: `Lu` is the key
        the tables use, and `Uppercase_Letter`/`uppercaseletter` are the spellings the
        grammar also has to accept.
        """
        if not os.path.exists(path):
            return
        for line in read_lines(path):
            fields = [field.strip() for field in line.split(";")]
            if len(fields) < 3:
                continue
            prop = fields[0]
            short, long = fields[1], fields[2] if len(fields) > 2 else fields[1]
            # Scripts and properties are keyed by their *long* name because that is what the
            # data files spell (`Scripts.txt` writes `Greek`, not `Grek`), so every alias has
            # to resolve to the long form. General_Category is the exception: its two-letter
            # code *is* the canonical spelling the tables use.
            canonical = short if prop == "gc" else long
            for value in fields[1:]:
                if value:
                    self.aliases[prop][value] = canonical


def rust_name(name):
    """A table identifier for a Unicode name: upper case, since it is a `static`."""
    return name.replace("-", "_").replace(" ", "_").upper()


def emit_ranges(out, ident, ranges, comment):
    r"""A `&[(u32, u32)]` of inclusive intervals, which is the whole representation.

    Ranges rather than a bitmap: the largest `\p{Any}` is one interval, and even the
    most fragmented property is a few thousand, while a bitmap per property would be
    1.1 M bits each and there are hundreds of properties.
    """
    out.append(f"/// {comment}")
    out.append(f"pub static {ident}: &[(u32, u32)] = &[")
    line = []
    for lo, hi in ranges:
        line.append(f"({lo:#x}, {hi:#x})")
        if len(line) == 6:
            out.append("    " + ", ".join(line) + ",")
            line = []
    if line:
        out.append("    " + ", ".join(line) + ",")
    out.append("];")
    out.append("")


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: gen-unicode-tables.py <ucd-dir>")
    ucd = Ucd(sys.argv[1])
    out = []
    out.append("//! Unicode property tables for `\\p{…}`, generated from the UCD.")
    out.append("//!")
    out.append("//! Produced by `scripts/gen-unicode-tables.py`; do not edit by hand.")
    out.append("//! The generator reads the Unicode Character Database itself, not any")
    out.append("//! test expectation, so agreement with test262 is evidence rather than")
    out.append("//! a tautology.")
    out.append("")
    # The table names are Unicode's (`GC_Lu`, `SCRIPT_Greek`), so the usual Rust naming
    # rules do not apply; they are referred to by the match arms in `unicode.rs`.
    out.append("#![allow(non_upper_case_globals)]")
    out.append("")
    out.append("/// The highest code point any table can describe.")
    out.append("pub const MAX_CODE_POINT: u32 = 0x10FFFF;")
    out.append("")

    # -- General_Category ---------------------------------------------------
    # One table per category, plus the "group" values (`L`, `LC`, …) that the grammar
    # accepts as well.
    groups = {
        "C": ["Cc", "Cf", "Cn", "Co", "Cs"],
        "L": ["Ll", "Lm", "Lo", "Lt", "Lu"],
        "LC": ["Ll", "Lt", "Lu"],
        "M": ["Mc", "Me", "Mn"],
        "N": ["Nd", "Nl", "No"],
        "P": ["Pc", "Pd", "Pe", "Pf", "Pi", "Po", "Ps"],
        "S": ["Sc", "Sk", "Sm", "So"],
        "Z": ["Zl", "Zp", "Zs"],
    }
    categories = sorted({c for c in ucd.gc if c})
    gc_tables = {}
    for category in categories:
        flags = [ucd.gc[cp] == category for cp in range(MAX_CP)]
        gc_tables[category] = ranges_to_set(set_to_ranges(flags))
    for group, members in groups.items():
        flags = [ucd.gc[cp] in members for cp in range(MAX_CP)]
        gc_tables[group] = ranges_to_set(set_to_ranges(flags))
    for category in sorted(gc_tables):
        emit_ranges(
            out,
            f"GC_{rust_name(category)}",
            gc_tables[category],
            f"`General_Category={category}`.",
        )

    # -- Script and Script_Extensions ---------------------------------------
    # `ScriptExtensions.txt` spells scripts with their four-letter codes (`Adlm`) while
    # `Scripts.txt` spells them out (`Adlam`). Both tables are keyed by the long name — that
    # is what `SCRIPT_ALIASES` resolves to — so every extension name has to be translated
    # back through the alias table or the two lookups disagree.
    short_to_long = {short: long for short, long in ucd.aliases.get("sc", {}).items()}
    # `Unknown` (`Zzzz`) is a real `Script` value covering every code point with no
    # assignment; no line in `Scripts.txt` says so, which is why it has to be derived.
    ucd.scripts = [script or "Unknown" for script in ucd.scripts]
    scripts = sorted({s for s in ucd.scripts if s})
    for script in scripts:
        flags = [ucd.scripts[cp] == script for cp in range(MAX_CP)]
        emit_ranges(
            out,
            f"SCRIPT_{rust_name(script)}",
            ranges_to_set(set_to_ranges(flags)),
            f"`Script={script}`.",
        )
    extension_names = sorted(
        {name for name, _ in ucd.script_extension_sets.items() if name}
    )
    # One table per script that has any extension entries, keyed by the long name so it
    # lines up with the `Script` table and with `SCRIPT_ALIASES`.
    extension_by_long = defaultdict(lambda: [False] * MAX_CP)
    for short, flags in ucd.script_extension_sets.items():
        long = short_to_long.get(short, short)
        merged = extension_by_long[long]
        for cp in range(MAX_CP):
            if flags[cp]:
                merged[cp] = True
    # A script with *no* `ScriptExtensions.txt` entry at all still needs an `scx` table:
    # the standard says the property falls back to `Script` per code point, so for such a
    # script the two tables are identical. Emitting only the scripts that appear in
    # `ScriptExtensions.txt` is the bug that makes `\p{scx=Balinese}` match nothing.
    extension_scripts = sorted(set(extension_by_long) | {s for s in scripts if s})
    for script in extension_scripts:
        # `Script_Extensions` falls back to `Script` for a code point with no entry, which
        # is what the standard says and what `\p{scx=…}` means.
        entry = extension_by_long.get(script)
        flags = [
            # An explicit entry wins outright; the `Script` fallback only applies where
            # `ScriptExtensions.txt` says nothing.
            (entry[cp] if ucd.has_extension[cp] and entry is not None else False)
            or (not ucd.has_extension[cp] and ucd.scripts[cp] == script)
            for cp in range(MAX_CP)
        ]
        emit_ranges(
            out,
            f"SCX_{rust_name(script)}",
            ranges_to_set(set_to_ranges(flags)),
            f"`Script_Extensions={script}`.",
        )

    # -- The three properties the grammar defines itself --------------------
    # §22.2.1 names `Any`, `ASCII` and `Assigned` directly; no UCD file lists them, so they
    # have to be added here rather than parsed.
    ucd.binary["Any"] = [True] * MAX_CP
    ucd.binary["ASCII"] = [cp <= 0x7F for cp in range(MAX_CP)]
    ucd.binary["Assigned"] = [ucd.gc[cp] != "Cn" for cp in range(MAX_CP)]

    # -- Binary properties --------------------------------------------------
    # The grammar's list is closed: §22.2.1's table names exactly these, so a property that
    # exists in the UCD but is not in the table (`Expands_On_NFC`, `Hyphen`,
    # `Prepended_Concatenation_Mark`) is a `SyntaxError` rather than a match. Taking the
    # union of whatever the data files contain would silently accept all of them.
    #
    # The `v`-mode string properties (`Basic_Emoji`, `RGI_Emoji`) are deliberately absent:
    # they denote sequences of code points rather than a set of single ones, so they need a
    # different representation and a different matcher path.
    GRAMMAR_BINARY_PROPERTIES = [
        "ASCII", "ASCII_Hex_Digit", "Alphabetic", "Any", "Assigned", "Bidi_Control",
        "Bidi_Mirrored", "Case_Ignorable", "Cased", "Changes_When_Casefolded",
        "Changes_When_Casemapped", "Changes_When_Lowercased",
        "Changes_When_NFKC_Casefolded", "Changes_When_Titlecased",
        "Changes_When_Uppercased", "Dash", "Default_Ignorable_Code_Point", "Deprecated",
        "Diacritic", "Emoji", "Emoji_Component", "Emoji_Modifier", "Emoji_Modifier_Base",
        "Emoji_Presentation", "Extended_Pictographic", "Extender", "Grapheme_Base",
        "Grapheme_Extend", "Hex_Digit", "IDS_Binary_Operator", "IDS_Trinary_Operator",
        "ID_Continue", "ID_Start", "Ideographic", "Join_Control",
        "Logical_Order_Exception", "Lowercase", "Math", "Noncharacter_Code_Point",
        "Pattern_Syntax", "Pattern_White_Space", "Quotation_Mark", "Radical",
        "Regional_Indicator", "Sentence_Terminal", "Soft_Dotted", "Terminal_Punctuation",
        "Unified_Ideograph", "Uppercase", "Variation_Selector", "White_Space",
        "XID_Continue", "XID_Start",
    ]
    missing = [name for name in GRAMMAR_BINARY_PROPERTIES if name not in ucd.binary]
    if missing:
        sys.exit(f"the grammar lists properties the UCD data does not define: {missing}")
    # Drop everything outside that list, so an accidental extra cannot leak through.
    ucd.binary = {name: ucd.binary[name] for name in GRAMMAR_BINARY_PROPERTIES}

    binary = sorted(ucd.binary)
    supported = []
    for name in binary:
        ranges = ranges_to_set(set_to_ranges(ucd.binary[name]))
        if not ranges:
            continue
        supported.append(name)
        emit_ranges(
            out,
            f"BIN_{rust_name(name)}",
            ranges,
            f"Binary property `{name}`.",
        )

    # -- Name tables --------------------------------------------------------
    # Every alias the grammar accepts, mapped to the canonical spelling the tables use.
    out.append("/// `General_Category` values and their aliases, canonical name last.")
    out.append("pub static GC_ALIASES: &[(&str, &str)] = &[")
    rows = sorted(ucd.aliases.get("gc", {}).items())
    for alias, canonical in rows:
        out.append(f'    ("{alias}", "{canonical}"),')
    out.append("];")
    out.append("")

    out.append("/// `Script` values and their aliases.")
    out.append("pub static SCRIPT_ALIASES: &[(&str, &str)] = &[")
    rows = sorted(ucd.aliases.get("sc", {}).items())
    for alias, canonical in rows:
        out.append(f'    ("{alias}", "{canonical}"),')
    out.append("];")
    out.append("")

    out.append("/// Binary property names and their aliases, canonical name last.")
    out.append("pub static BINARY_ALIASES: &[(&str, &str)] = &[")
    rows = sorted(ucd.aliases.get("property", {}).items())
    seen_in_alias_file = {canonical for _, canonical in rows}
    for alias, canonical in rows:
        if canonical in binary:
            out.append(f'    ("{alias}", "{canonical}"),')
    # `Any`/`ASCII`/`Assigned` are absent from `PropertyAliases.txt` because the grammar
    # defines them, so they get no alias of their own — the name is the alias.
    for name in binary:
        if name not in seen_in_alias_file:
            out.append(f'    ("{name}", "{name}"),')
    out.append("];")
    out.append("")

    # The match tables the query layer reaches: canonical name -> table identifier.
    # Emitted as separate files because 174 + 99 + 77 match arms is too much to keep in
    # sync by hand, and a missing arm would silently match nothing.
    outdir = os.path.join(
        os.path.dirname(os.path.abspath(__file__)),
        "..", "crates", "quickjs-regexp", "src",
    )
    # Every script gets an `scx` table, not just the ones listed in `ScriptExtensions.txt`,
    # because `scx` falls back to `sc` per code point.
    extensions_long = scripts
    for filename, macro, prefix, names in (
        ("unicode_script_tables.inc", "scripts", "SCRIPT_", scripts),
        ("unicode_extension_tables.inc", "extensions", "SCX_", extensions_long),
        ("unicode_binary_tables.inc", "binaries", "BIN_", supported),
    ):
        with open(os.path.join(outdir, filename), "w", encoding="utf-8") as handle:
            handle.write("// Generated by scripts/gen-unicode-tables.py; do not edit by hand.\n")
            handle.write("// Canonical property name -> table, for the names this engine exposes.\n")
            handle.write(f"{macro}! {{\n")
            for name in sorted(names):
                handle.write(f'    "{name}" => {prefix}{rust_name(name)},\n')
            handle.write("}\n")

    sys.stdout.write("\n".join(out))
    sys.stderr.write(
        f"gc={len(gc_tables)} script={len(scripts)} scx={len(extension_names)} "
        f"binary={len(supported)}\n"
    )


if __name__ == "__main__":
    main()
