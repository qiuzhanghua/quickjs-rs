//! C `printf`, because `std.sprintf` *is* C `printf`.
//!
//! The reference hands the format string to the C library through `dbuf_printf`, so its
//! behaviour is the C library's: `%.0f` of `2.5` is `"2"` rather than `"3"`, and `%x` of
//! `-2` is `"fffffffe"` because the argument is cast to `int` and reinterpreted as
//! unsigned. Getting close is not good enough — `tests/test_std.js` compares byte for byte.
//!
//! # What Rust gives for free, and what it does not
//!
//! `format!("{:.0}", 2.5)` is `"2"` and `format!("{:010}", 123)` is `"0000000123"`, so the
//! rounding and the zero padding already agree with C and are reused. What does *not* agree
//! is scientific notation — Rust writes `"1.2345e3"` where C writes `"1.234500e+03"` — so
//! `%e`, `%g` and `%a` are implemented here. Width, flags and precision are assembled by
//! hand because a Rust format string cannot be built at run time.
//!
//! # The grammar, which is where the surprises are
//!
//! ```text
//! %[flags][width][.precision][l]conversion
//! ```
//!
//! * flags are `0 # + - space '`, and `'` (thousands grouping) is accepted and ignored —
//!   that is what the C locale the reference runs in does with it.
//! * width and precision may be `*`, taking the value from the next argument. A negative
//!   `*` width means left justification with that magnitude; a negative `*` precision means
//!   "no precision".
//! * `l` is the only modifier, and only widens the integer conversions from `int` to
//!   `long long`. That difference is observable: `%x` of `-2` is eight digits, `%lx` is
//!   sixteen.
//! * anything else, including a trailing `%`, is a `TypeError`; running out of arguments is
//!   a `ReferenceError`. Both messages are the reference's, because scripts match on them.

use quickjs::{Interp, JsResult, Signal, Value};

/// One parsed conversion, ready to consume an argument.
struct Spec {
    minus: bool,
    plus: bool,
    space: bool,
    hash: bool,
    zero: bool,
    width: Option<usize>,
    precision: Option<usize>,
    long: bool,
    conversion: char,
    /// How many arguments the `*`s took: 0, 1 or 2.
    from_stars: usize,
}

/// Format `args` into a string: `args[0]` is the format, the rest are the arguments.
///
/// `std.sprintf()` with no arguments is `""` rather than an error, and a non-string format
/// is converted — `std.sprintf(123)` is `"123"`.
pub fn sprintf(interp: &mut Interp<'_>, args: &[Value]) -> JsResult<String> {
    let Some(first) = args.first() else {
        return Ok(String::new());
    };
    let format_string = interp.to_js_string(first)?;
    let bytes = format_string.as_bytes();
    let mut out = String::new();
    let mut cursor = 0usize;
    // The next argument to consume; 0 is the format string itself.
    let mut next = 1usize;

    while cursor < bytes.len() {
        let Some(percent) = bytes[cursor..].iter().position(|byte| *byte == b'%') else {
            out.push_str(&format_string[cursor..]);
            break;
        };
        // Everything between the last conversion and this `%` is literal.
        out.push_str(&format_string[cursor..cursor + percent]);
        cursor += percent + 1;

        let (spec, after) = match parse(bytes, cursor, args, next)? {
            Some(parsed) => parsed,
            None => return Err(invalid_specifier(interp)),
        };
        cursor = after;

        if spec.conversion == '%' {
            // `%5%` is `"%"`: C ignores width and flags for a literal percent, and it
            // consumes no argument.
            out.push('%');
            continue;
        }
        // The `*`s consumed `next..next + from_stars`; the conversion's own argument is the
        // one right after them.
        let own = next + spec.from_stars;
        let Some(argument) = args.get(own).cloned() else {
            return Err(missing_argument(interp));
        };
        next = own + 1;
        out.push_str(&render(interp, &spec, &argument)?);
    }
    Ok(out)
}

fn invalid_specifier(interp: &Interp<'_>) -> Signal {
    interp.type_error("invalid conversion specifier in format string")
}

fn missing_argument(interp: &Interp<'_>) -> Signal {
    Signal::Throw(interp.make_error(
        "ReferenceError",
        "missing argument for conversion specifier",
    ))
}

/// Parse one conversion starting just after its `%`.
///
/// Returns `Ok(None)` for a sequence that is not a conversion, which the caller reports as a
/// `TypeError`. The `*` lookups need `args` to read the width, so a missing argument there
/// also surfaces as `None` — the reference treats it as "missing", not "invalid", but both
/// arrive here as "there was no argument to read", and the caller distinguishes them by the
/// error it raises next.
fn parse(
    bytes: &[u8],
    mut cursor: usize,
    args: &[Value],
    next_argument: usize,
) -> JsResult<Option<(Spec, usize)>> {
    let mut minus = false;
    let mut plus = false;
    let mut space = false;
    let mut hash = false;
    let mut zero = false;
    let mut from_stars = 0usize;

    loop {
        match bytes.get(cursor) {
            Some(b'0') => zero = true,
            Some(b'#') => hash = true,
            Some(b'+') => plus = true,
            Some(b'-') => minus = true,
            Some(b' ') => space = true,
            // Thousands grouping: accepted, and a no-op in the C locale.
            Some(b'\'') => {}
            _ => break,
        }
        cursor += 1;
    }

    let mut width = None;
    if bytes.get(cursor) == Some(&b'*') {
        let Some(argument) = args.get(next_argument + from_stars) else {
            return Ok(None);
        };
        let value = to_int32(argument);
        from_stars += 1;
        // A negative `*` width is left justification of the same magnitude.
        if value < 0 {
            minus = true;
            width = Some(value.unsigned_abs() as usize);
        } else {
            width = Some(value as usize);
        }
        cursor += 1;
    } else {
        let start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor > start {
            width = digits(&bytes[start..cursor]);
        }
    }

    let mut precision = None;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        if bytes.get(cursor) == Some(&b'*') {
            let Some(argument) = args.get(next_argument + from_stars) else {
                return Ok(None);
            };
            let value = to_int32(argument);
            from_stars += 1;
            // A negative `*` precision is as if the precision were omitted.
            precision = (value >= 0).then_some(value as usize);
            cursor += 1;
        } else {
            let start = cursor;
            while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
                cursor += 1;
            }
            // `.` with no digits is precision 0: `%.f` is `%.0f`.
            precision = Some(digits(&bytes[start..cursor]).unwrap_or(0));
        }
    }

    let long = if bytes.get(cursor) == Some(&b'l') {
        cursor += 1;
        true
    } else {
        false
    };

    let Some(&conversion) = bytes.get(cursor) else {
        // A trailing `%`, or one followed only by flags and width.
        return Ok(None);
    };
    let conversion = conversion as char;
    if !matches!(
        conversion,
        'c' | 'd'
            | 'i'
            | 'o'
            | 'u'
            | 'x'
            | 'X'
            | 's'
            | 'e'
            | 'f'
            | 'g'
            | 'a'
            | 'E'
            | 'F'
            | 'G'
            | 'A'
            | '%'
    ) {
        return Ok(None);
    }

    Ok(Some((
        Spec {
            minus,
            plus,
            space,
            hash,
            zero,
            width,
            precision,
            long,
            conversion,
            from_stars,
        },
        cursor + 1,
    )))
}

fn digits(bytes: &[u8]) -> Option<usize> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// A rendered value split into the part that precedes zero padding and the digits.
///
/// The split is what makes `%+05d` of 42 `"+0042"` rather than `"00+42"`: a sign or an `0x`
/// prefix goes outside the zero fill.
struct Rendered {
    prefix: String,
    digits: String,
}

fn render(interp: &mut Interp<'_>, spec: &Spec, argument: &Value) -> JsResult<String> {
    Ok(match spec.conversion {
        // `%c` ignores flags, width and precision entirely — the reference encodes the code
        // point and emits it.
        'c' => char_string(interp, argument),
        'd' | 'i' => pad(spec, integer(spec, argument, true)),
        'o' | 'u' | 'x' | 'X' => pad(spec, integer(spec, argument, false)),
        's' => pad(spec, string(interp, spec, argument)?),
        'e' | 'E' | 'f' | 'F' | 'g' | 'G' | 'a' | 'A' => {
            pad(spec, float(spec, argument.to_number()))
        }
        // Unreachable: `parse` only accepts the conversions above, and `%` is handled by
        // the caller.
        _ => String::new(),
    })
}

/// Signed (`%d`/`%i`) or unsigned (`%o`/`%u`/`%x`/`%X`) integers.
fn integer(spec: &Spec, argument: &Value, signed: bool) -> Rendered {
    let value = to_int64(argument);

    if signed {
        // Without `l` the argument is cast to `int` first, and that truncation is visible.
        let (negative, magnitude) = if spec.long {
            (value < 0, value.unsigned_abs())
        } else {
            let narrowed = value as i32;
            (narrowed < 0, narrowed.unsigned_abs() as u64)
        };
        let mut digits = magnitude.to_string();
        apply_precision(spec, &mut digits);
        return Rendered {
            prefix: sign(spec, negative),
            digits,
        };
    }

    // Unsigned: the same truncation, reinterpreted. `%x` of -2 → `u32::MAX - 1`.
    let magnitude = if spec.long {
        value as u64
    } else {
        (value as i32) as u32 as u64
    };
    let (mut digits, prefix) = match spec.conversion {
        'o' => (format!("{magnitude:o}"), String::new()),
        'x' => (
            format!("{magnitude:x}"),
            alternate_hex(spec, magnitude, "0x"),
        ),
        'X' => (
            format!("{magnitude:X}"),
            alternate_hex(spec, magnitude, "0X"),
        ),
        _ => (magnitude.to_string(), String::new()),
    };
    apply_precision(spec, &mut digits);
    // `%#o` guarantees a leading zero, and precision padding may already have supplied one.
    if spec.hash && spec.conversion == 'o' && !digits.starts_with('0') {
        digits.insert(0, '0');
    }
    Rendered { prefix, digits }
}

/// Numeric precision is a *minimum* digit count, not a maximum.
fn apply_precision(spec: &Spec, digits: &mut String) {
    if let Some(precision) = spec.precision {
        if digits.len() < precision {
            *digits = format!("{}{digits}", "0".repeat(precision - digits.len()));
        }
    }
}

fn sign(spec: &Spec, negative: bool) -> String {
    if negative {
        "-".to_string()
    } else if spec.plus {
        "+".to_string()
    } else if spec.space {
        " ".to_string()
    } else {
        String::new()
    }
}

/// `%#x` prefixes `0x`, but only to a nonzero value.
fn alternate_hex(spec: &Spec, magnitude: u64, prefix: &str) -> String {
    if spec.hash && magnitude != 0 {
        prefix.to_string()
    } else {
        String::new()
    }
}

/// `%s`, with precision as a maximum length.
fn string(interp: &mut Interp<'_>, spec: &Spec, argument: &Value) -> JsResult<Rendered> {
    let text = interp.to_js_string(argument)?;
    let text = match spec.precision {
        Some(precision) if text.len() > precision => {
            // Bytes, because C's `%.5s` counts bytes — and it really does split a character:
            // `%.5s` of `"日本語"` cuts the second character in half and the result is the
            // `日` followed by one replacement character. Backing off to a character
            // boundary would be tidier and would not match, so the lossy decode reproduces
            // what `JS_NewStringLen` does with those bytes.
            String::from_utf8_lossy(&text.as_bytes()[..precision]).into_owned()
        }
        _ => text,
    };
    Ok(Rendered {
        prefix: String::new(),
        digits: text,
    })
}

/// `%f`/`%e`/`%g`/`%a` and their uppercase forms.
fn float(spec: &Spec, value: f64) -> Rendered {
    let magnitude = value.abs();
    // C's default for every float conversion.
    let precision = spec.precision.unwrap_or(6);
    let digits = match spec.conversion {
        'f' | 'F' => fixed(magnitude, precision, spec.hash),
        'e' | 'E' => scientific(magnitude, precision, spec.conversion == 'E', spec.hash),
        'g' | 'G' => general(magnitude, precision, spec.conversion == 'G', spec.hash),
        _ => hexadecimal_float(magnitude),
    };
    Rendered {
        prefix: sign(spec, value.is_sign_negative()),
        digits,
    }
}

/// `%f`: decimal notation with `precision` digits after the point.
fn fixed(magnitude: f64, precision: usize, hash: bool) -> String {
    if !magnitude.is_finite() {
        return non_finite(magnitude, false);
    }
    let mut text = format!("{magnitude:.precision$}");
    if hash && precision == 0 {
        text.push('.');
    }
    text
}

/// `%e`: one digit before the point, at least two in the exponent, sign always present.
fn scientific(magnitude: f64, precision: usize, upper: bool, hash: bool) -> String {
    if !magnitude.is_finite() {
        return non_finite(magnitude, upper);
    }
    // Rust writes `1.234500e3` and C writes `1.234500e+03`. The mantissa is identical, so
    // only the exponent is rebuilt.
    let rendered = format!("{magnitude:.precision$e}");
    let (mantissa, exponent) = rendered
        .split_once('e')
        .expect("Rust's LowerExp always contains an `e`");
    let exponent: i32 = exponent.parse().expect("the exponent is an integer");
    let mut text = format!(
        "{mantissa}{}{}{:02}",
        if upper { 'E' } else { 'e' },
        if exponent < 0 { '-' } else { '+' },
        exponent.unsigned_abs()
    );
    if hash && precision == 0 {
        // `%#.0e` keeps the point that precision 0 removed.
        let at = text.find(['e', 'E']).unwrap_or(text.len());
        text.insert(at, '.');
    }
    text
}

/// `%g`: whichever of `%e` and `%f` is shorter, with trailing zeros removed unless `#`.
fn general(magnitude: f64, precision: usize, upper: bool, hash: bool) -> String {
    if !magnitude.is_finite() {
        return non_finite(magnitude, upper);
    }
    // C takes precision 0 as 1 for `%g`.
    let significant = precision.max(1);
    if magnitude == 0.0 {
        // The reference prints a bare `0` for `%g` of zero, not `0.00000`.
        return if hash {
            format!("0.{}", "0".repeat(significant - 1))
        } else {
            "0".to_string()
        };
    }
    // The style is chosen from the exponent of the value *rounded* to `significant` digits,
    // which is why `%.3g` of 1234.5 is `1.23e+03` — rounding carries it to exponent 3.
    let exponent = decimal_exponent(magnitude, significant);
    let mut text = if significant as i32 > exponent && exponent >= -4 {
        // `%f` with just enough decimals to reach `significant` digits in total.
        let decimals = (significant as i32 - 1 - exponent).max(0) as usize;
        format!("{magnitude:.decimals$}")
    } else {
        scientific(magnitude, significant - 1, upper, false)
    };
    if !hash {
        text = strip_trailing_zeros(text, upper);
    }
    text
}

/// The base-10 exponent of `magnitude` rounded to `significant` digits.
fn decimal_exponent(magnitude: f64, significant: usize) -> i32 {
    format!("{magnitude:.prec$e}", prec = significant.saturating_sub(1))
        .split_once('e')
        .and_then(|(_, exponent)| exponent.parse().ok())
        .unwrap_or(0)
}

/// Drop the fraction's trailing zeros, and the fraction itself if it empties.
///
/// `upper` is threaded through because the exponent marker has to keep the conversion's
/// case: `%G` of `1e20` is `1E+20`, and rebuilding the exponent with a lowercase `e` here
/// would quietly undo the caller's choice.
fn strip_trailing_zeros(text: String, upper: bool) -> String {
    let trim = |mantissa: &str| -> String {
        if mantissa.contains('.') {
            mantissa
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_string()
        } else {
            mantissa.to_string()
        }
    };
    let marker = if upper { 'E' } else { 'e' };
    match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => format!("{}{marker}{exponent}", trim(mantissa)),
        None => trim(&text),
    }
}

/// `%a`: hexadecimal, `0x1.fp+3`.
///
/// C's default precision is "as many digits as needed to be exact, with trailing zeros
/// removed", which is what this produces.
fn hexadecimal_float(magnitude: f64) -> String {
    if !magnitude.is_finite() {
        return non_finite(magnitude, false);
    }
    if magnitude == 0.0 {
        return "0x0p+0".to_string();
    }
    let bits = magnitude.to_bits();
    let raw_exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & 0xf_ffff_ffff_ffff;
    // Normal: 1.f × 2^e. Subnormal: 0.f × 2^-1022.
    let (lead, exponent) = if raw_exponent == 0 {
        (0u64, -1022)
    } else {
        (1u64, raw_exponent - 1023)
    };
    if fraction == 0 {
        return format!("0x{lead:x}p{exponent:+}");
    }
    // 52 fraction bits are exactly 13 hex digits; C trims the trailing zeros.
    let digits = format!("{fraction:013x}");
    format!("0x{lead:x}.{}p{exponent:+}", digits.trim_end_matches('0'))
}

/// `inf`/`nan`, spelled the way C does and capitalised for the uppercase conversions.
fn non_finite(magnitude: f64, upper: bool) -> String {
    let text = if magnitude.is_nan() { "nan" } else { "inf" };
    if upper {
        text.to_uppercase()
    } else {
        text.to_string()
    }
}

/// `%c`: a code point, or the first code point of a string.
fn char_string(interp: &mut Interp<'_>, argument: &Value) -> String {
    let code = if matches!(argument, Value::String(_)) {
        let text = interp.to_js_string(argument).unwrap_or_default();
        text.chars().next().map_or(0xFFFD, |c| c as u32)
    } else {
        to_int32(argument) as u32
    };
    // `(unsigned)int32_arg > 0x10FFFF` becomes U+FFFD. A surrogate has no `char` either, so
    // it takes the same path rather than producing invalid UTF-8.
    char::from_u32(code).unwrap_or('\u{FFFD}').to_string()
}

/// Width and the remaining flags, applied last.
fn pad(spec: &Spec, rendered: Rendered) -> String {
    let Rendered { prefix, digits } = rendered;
    let Some(width) = spec.width else {
        return format!("{prefix}{digits}");
    };
    let body = prefix.len() + digits.len();
    if body >= width {
        return format!("{prefix}{digits}");
    }
    let fill = width - body;
    if spec.minus {
        // Left justification wins over `0`: `%-05d` pads on the right.
        format!("{prefix}{digits}{}", " ".repeat(fill))
    } else if spec.zero && (spec.precision.is_none() || is_float(spec.conversion)) {
        // `0` is ignored when an *integer* precision was given (`%08.3d` of 42 is
        // `"     042"`) but kept alongside a float precision (`%010.2f` zero-pads).
        format!("{prefix}{}{digits}", "0".repeat(fill))
    } else {
        format!("{}{prefix}{digits}", " ".repeat(fill))
    }
}

/// Whether a conversion formats a float, which decides whether precision suppresses `0`.
fn is_float(conversion: char) -> bool {
    matches!(conversion, 'e' | 'E' | 'f' | 'F' | 'g' | 'G' | 'a' | 'A')
}

/// `JS_ToInt32`: `ToNumber`, then a truncating cast, with non-finite values becoming 0.
fn to_int32(value: &Value) -> i32 {
    let number = value.to_number();
    if number.is_nan() || number.is_infinite() {
        return 0;
    }
    // `as i64` saturates, and the wrap to `i32` is the same two's-complement truncation
    // C's `(int)` cast performs.
    number as i64 as i32
}

/// `JS_ToInt64Ext`: like [`to_int32`] but 64-bit, and it accepts a `BigInt`.
fn to_int64(value: &Value) -> i64 {
    if let Value::BigInt(big) = value {
        // `JS_ToInt64Ext` truncates a big value to 64 bits rather than failing.
        return big.to_i64_wrapping();
    }
    let number = value.to_number();
    if number.is_nan() || number.is_infinite() {
        return 0;
    }
    number as i64
}
