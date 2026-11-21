//! Identifier character classes.
//!
//! The specification defines identifiers with the Unicode `ID_Start` /
//! `ID_Continue` properties plus `$`, `_`, ZWNJ and ZWJ. Upstream QuickJS ships
//! generated tables (`libunicode.c`); this module is intentionally a
//! *conservative approximation* until the `js-unicode` milestone (FR-8) replaces
//! it with tables generated from the same Unicode version.
//!
//! The approximation is `Alphabetic`-based, which covers every identifier a
//! program realistically contains: letters of all scripts, plus `Nl`/`No` digits
//! through [`char::is_alphanumeric`]. Characters that are `ID_Start` but neither
//! alphabetic nor numeric (a handful of `Other_ID_Start`) are handled by an
//! explicit list.

/// `ID_Start` approximation.
pub(crate) fn is_id_start(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphabetic() || c == '$' || c == '_';
    }
    c.is_alphabetic() || is_other_id_start(c)
}

/// `ID_Continue` approximation.
pub(crate) fn is_id_continue(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric() || c == '$' || c == '_';
    }
    if c.is_alphanumeric() {
        return true;
    }
    // Unicode 15 `Other_ID_Start` / `Other_ID_Continue`, plus ZWNJ/ZWJ which the
    // spec allows in identifiers for script joining.
    matches!(c, '\u{200C}' | '\u{200D}') || is_other_id_start(c) || is_other_id_continue(c)
}

fn is_other_id_start(c: char) -> bool {
    matches!(
        c,
        '\u{1885}' | '\u{1886}' | '\u{2118}' | '\u{212E}' | '\u{309B}' | '\u{309C}' | '\u{2E2F}'
    )
}

fn is_other_id_continue(c: char) -> bool {
    matches!(
        c,
        '\u{00B7}'
            | '\u{0387}'
            | '\u{1369}'
            | '\u{136A}'
            | '\u{136B}'
            | '\u{136C}'
            | '\u{136D}'
            | '\u{136E}'
            | '\u{136F}'
            | '\u{1370}'
            | '\u{1371}'
            | '\u{19DA}'
    ) || ('\u{0300}'..='\u{036F}').contains(&c)
}
