//! Unicode property escapes: name resolution and membership.
//!
//! [`crate::unicode_tables`] holds the code point ranges, generated from the UCD.
//! This module is the grammar on top of them: which spellings `\p{…}` accepts, and how a
//! name maps to a table.
//!
//! # The three shapes a name can have
//!
//! §22.2.1's `UnicodePropertyValueExpression` is either
//!
//! ```text
//! UnicodePropertyName = UnicodePropertyValue      // \p{General_Category=Lu}
//! LoneUnicodePropertyNameOrValue                  // \p{Lu}, \p{Alphabetic}
//! ```
//!
//! and the two are *not* interchangeable. A bare name is looked up as a binary property
//! first and as a `General_Category` value second; it is never a `Script`. That is why
//! `\p{Greek}` is a `SyntaxError` even though `Greek` is a script: the grammar wants
//! `\p{Script=Greek}`. Writing `\p{Alphabetic=Yes}` is refused for the mirror-image reason:
//! a binary property has no value.
//!
//! # Strictness matters as much as coverage
//!
//! A name the table does not know must be a `SyntaxError`, not a property that matches
//! nothing. test262 checks this in both directions — 163 of its cases are patterns that
//! must be *rejected* — so [`lookup`] answers `None` for anything unrecognised rather than
//! falling back to an empty set.

use crate::unicode_tables as tables;

/// A resolved property, ready to answer membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Property {
    /// A `General_Category` value, including the group values `L`, `LC`, `C`, …
    GeneralCategory(GeneralCategory),
    /// `Script=…`.
    Script(Script),
    /// `Script_Extensions=…`.
    ScriptExtensions(Script),
    /// A binary property.
    Binary(Binary),
}

/// The `General_Category` tables the generated module exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneralCategory {
    /// `Cc`, `Cf`, `Cn`, `Co`, `Cs` and the `C` group.
    C,
    /// `Ll`, `Lm`, `Lo`, `Lt`, `Lu` and the `L` group.
    L,
    /// The `LC` group only (`Ll`, `Lt`, `Lu`).
    LC,
    /// `Mc`, `Me`, `Mn` and the `M` group.
    M,
    /// `Nd`, `Nl`, `No` and the `N` group.
    N,
    /// `Pc`, `Pd`, `Pe`, `Pf`, `Pi`, `Po`, `Ps` and the `P` group.
    P,
    /// `Sc`, `Sk`, `Sm`, `So` and the `S` group.
    S,
    /// `Zl`, `Zp`, `Zs` and the `Z` group.
    Z,
    /// A concrete two-letter category, indexed into the per-value tables.
    Value(&'static str),
}

impl GeneralCategory {
    /// The table for this category.
    fn ranges(self) -> &'static [(u32, u32)] {
        match self {
            GeneralCategory::C => tables::GC_C,
            GeneralCategory::L => tables::GC_L,
            GeneralCategory::LC => tables::GC_LC,
            GeneralCategory::M => tables::GC_M,
            GeneralCategory::N => tables::GC_N,
            GeneralCategory::P => tables::GC_P,
            GeneralCategory::S => tables::GC_S,
            GeneralCategory::Z => tables::GC_Z,
            GeneralCategory::Value(name) => gc_value_ranges(name),
        }
    }
}

/// The table for a concrete two-letter `General_Category`, if it is one.
fn gc_value_ranges(name: &str) -> &'static [(u32, u32)] {
    match name {
        "Cc" => tables::GC_CC,
        "Cf" => tables::GC_CF,
        "Cn" => tables::GC_CN,
        "Co" => tables::GC_CO,
        "Cs" => tables::GC_CS,
        "Ll" => tables::GC_LL,
        "Lm" => tables::GC_LM,
        "Lo" => tables::GC_LO,
        "Lt" => tables::GC_LT,
        "Lu" => tables::GC_LU,
        "Mc" => tables::GC_MC,
        "Me" => tables::GC_ME,
        "Mn" => tables::GC_MN,
        "Nd" => tables::GC_ND,
        "Nl" => tables::GC_NL,
        "No" => tables::GC_NO,
        "Pc" => tables::GC_PC,
        "Pd" => tables::GC_PD,
        "Pe" => tables::GC_PE,
        "Pf" => tables::GC_PF,
        "Pi" => tables::GC_PI,
        "Po" => tables::GC_PO,
        "Ps" => tables::GC_PS,
        "Sc" => tables::GC_SC,
        "Sk" => tables::GC_SK,
        "Sm" => tables::GC_SM,
        "So" => tables::GC_SO,
        "Zl" => tables::GC_ZL,
        "Zp" => tables::GC_ZP,
        "Zs" => tables::GC_ZS,
        _ => &[],
    }
}

/// A script, named by its canonical (long) spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Script(&'static str);

impl Script {
    /// The `Script` table.
    fn ranges(self) -> &'static [(u32, u32)] {
        script_ranges(self.0)
    }

    /// The `Script_Extensions` table.
    fn extension_ranges(self) -> &'static [(u32, u32)] {
        extension_ranges(self.0)
    }
}

/// A binary property, named by its canonical (long) spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binary(&'static str);

impl Binary {
    /// The table.
    fn ranges(self) -> &'static [(u32, u32)] {
        binary_ranges(self.0)
    }
}

/// The table for a canonical `Script` name.
fn script_ranges(name: &str) -> &'static [(u32, u32)] {
    macro_rules! scripts {
        ($($canonical:literal => $table:ident),* $(,)?) => {
            match name {
                $($canonical => tables::$table,)*
                _ => &[],
            }
        };
    }
    include!("unicode_script_tables.inc")
}

/// The table for a canonical `Script_Extensions` name.
fn extension_ranges(name: &str) -> &'static [(u32, u32)] {
    macro_rules! extensions {
        ($($canonical:literal => $table:ident),* $(,)?) => {
            match name {
                $($canonical => tables::$table,)*
                _ => &[],
            }
        };
    }
    include!("unicode_extension_tables.inc")
}

/// The table for a canonical binary property name.
fn binary_ranges(name: &str) -> &'static [(u32, u32)] {
    macro_rules! binaries {
        ($($canonical:literal => $table:ident),* $(,)?) => {
            match name {
                $($canonical => tables::$table,)*
                _ => &[],
            }
        };
    }
    include!("unicode_binary_tables.inc")
}

impl Property {
    /// Whether `cp` has this property.
    ///
    /// A value beyond the last code point has no property at all, which keeps the answer
    /// total without every caller having to range-check first.
    pub fn holds(self, cp: u32) -> bool {
        if cp > tables::MAX_CODE_POINT {
            return false;
        }
        let ranges = match self {
            Property::GeneralCategory(category) => category.ranges(),
            Property::Script(script) => script.ranges(),
            Property::ScriptExtensions(script) => script.extension_ranges(),
            Property::Binary(binary) => binary.ranges(),
        };
        in_ranges(ranges, cp)
    }

    /// Whether this property is the special `Any` binary property, which every code point
    /// has. Kept separate because a negated `\P{Any}` matches nothing at all, and that is
    /// worth being able to special-case at the call site.
    pub fn is_any(self) -> bool {
        matches!(self, Property::Binary(Binary("Any")))
    }
}

/// Binary search over a sorted, merged interval list.
fn in_ranges(ranges: &[(u32, u32)], cp: u32) -> bool {
    // Binary search by the interval's upper bound: the first interval whose `hi` is at least
    // `cp` is the only candidate, and it contains `cp` exactly when its `lo` is at most.
    let mut low = 0usize;
    let mut high = ranges.len();
    while low < high {
        let mid = (low + high) / 2;
        let (lo, hi) = ranges[mid];
        if cp < lo {
            high = mid;
        } else if cp > hi {
            low = mid + 1;
        } else {
            return true;
        }
    }
    false
}

/// Resolve the inside of `\p{…}`.
///
/// `None` means the name is not one the grammar accepts, which the parser turns into a
/// `SyntaxError`. Both shapes are handled here so the two spellings cannot drift apart.
pub fn lookup(name: &str) -> Option<Property> {
    // The grammar allows exactly one `=`; two is a `SyntaxError` rather than a name.
    let mut parts = name.split('=');
    let first = parts.next()?;
    match parts.next() {
        None => lone_name(first),
        Some(value) => {
            if parts.next().is_some() {
                return None;
            }
            property_value(first, value)
        }
    }
}

/// `UnicodePropertyName = UnicodePropertyValue`.
fn property_value(property: &str, value: &str) -> Option<Property> {
    match canonical_property_name(property)? {
        "General_Category" => Some(Property::GeneralCategory(canonical_gc(value)?)),
        "Script" => Some(Property::Script(canonical_script(value)?)),
        "Script_Extensions" => Some(Property::ScriptExtensions(canonical_script(value)?)),
        _ => None,
    }
}

/// `LoneUnicodePropertyNameOrValue`: a binary property, or a `General_Category` value.
///
/// The order is the spec's: binary properties are tried first, and a `General_Category`
/// value is the fallback. A `Script` is deliberately not reachable here.
fn lone_name(name: &str) -> Option<Property> {
    if let Some(binary) = canonical_binary(name) {
        // `Any`/`ASCII`/`Assigned` live in the binary tables, so they come back here.
        return Some(Property::Binary(binary));
    }
    canonical_gc(name).map(Property::GeneralCategory)
}

/// The canonical spelling of a property *name* (`gc`, `Script`, `scx`, …).
fn canonical_property_name(name: &str) -> Option<&'static str> {
    // The grammar's own three names, plus their aliases from `PropertyAliases.txt`.
    for (alias, canonical) in PROPERTY_NAME_ALIASES {
        if *alias == name {
            return Some(canonical);
        }
    }
    None
}

/// `PropertyAliases.txt` entries that name one of the three properties the grammar allows.
const PROPERTY_NAME_ALIASES: &[(&str, &str)] = &[
    ("General_Category", "General_Category"),
    ("gc", "General_Category"),
    ("Script", "Script"),
    ("sc", "Script"),
    ("Script_Extensions", "Script_Extensions"),
    ("scx", "Script_Extensions"),
];

/// The canonical `General_Category` value for `name`, if it names one.
fn canonical_gc(name: &str) -> Option<GeneralCategory> {
    let canonical = lookup_alias(tables::GC_ALIASES, name)?;
    Some(match canonical {
        "C" => GeneralCategory::C,
        "L" => GeneralCategory::L,
        "LC" => GeneralCategory::LC,
        "M" => GeneralCategory::M,
        "N" => GeneralCategory::N,
        "P" => GeneralCategory::P,
        "S" => GeneralCategory::S,
        "Z" => GeneralCategory::Z,
        other => {
            // A concrete two-letter category. The alias table lists them all, so this arm
            // only has to confirm the table exists.
            let name = match other {
                "Cc" => "Cc",
                "Cf" => "Cf",
                "Cn" => "Cn",
                "Co" => "Co",
                "Cs" => "Cs",
                "Ll" => "Ll",
                "Lm" => "Lm",
                "Lo" => "Lo",
                "Lt" => "Lt",
                "Lu" => "Lu",
                "Mc" => "Mc",
                "Me" => "Me",
                "Mn" => "Mn",
                "Nd" => "Nd",
                "Nl" => "Nl",
                "No" => "No",
                "Pc" => "Pc",
                "Pd" => "Pd",
                "Pe" => "Pe",
                "Pf" => "Pf",
                "Pi" => "Pi",
                "Po" => "Po",
                "Ps" => "Ps",
                "Sc" => "Sc",
                "Sk" => "Sk",
                "Sm" => "Sm",
                "So" => "So",
                "Zl" => "Zl",
                "Zp" => "Zp",
                "Zs" => "Zs",
                _ => return None,
            };
            GeneralCategory::Value(name)
        }
    })
}

/// The canonical `Script` name for `name`, if it names one.
fn canonical_script(name: &str) -> Option<Script> {
    let canonical = lookup_alias(tables::SCRIPT_ALIASES, name)?;
    // A script the tables know about only. `Unknown` is a real `Script` value but has no
    // `\p{Script=Unknown}` table, and the grammar allows it only through `scx`.
    if script_ranges(canonical).is_empty() && extension_ranges(canonical).is_empty() {
        return None;
    }
    Some(Script(canonical))
}

/// The canonical binary property name for `name`, if it names one.
fn canonical_binary(name: &str) -> Option<Binary> {
    let canonical = lookup_alias(tables::BINARY_ALIASES, name)?;
    if binary_ranges(canonical).is_empty() {
        return None;
    }
    Some(Binary(canonical))
}

/// Look `name` up in an alias table, answering the canonical spelling.
fn lookup_alias(table: &[(&'static str, &'static str)], name: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(alias, _)| *alias == name)
        .map(|(_, canonical)| *canonical)
}
