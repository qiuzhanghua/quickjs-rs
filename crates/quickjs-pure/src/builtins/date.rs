//! `Date`.
//!
//! The spec fixes the *order* of the operations in `MakeTime`/`MakeDay` more
//! tightly than it fixes the arithmetic, and the reference implementation carries
//! `volatile` temporaries to stop the C compiler from fusing them. Two assertions in
//! `test_builtin.js` (`Date.UTC(1970, 0, 1, 80063993375, 29, 1, -288230376151711740)`)
//! only hold when that exact order is used, so the arithmetic below is a line-by-line
//! port rather than a rewrite from the spec text.
//!
//! A Date's `[[DateValue]]` is kept in a hidden `_date_value` property and its class
//! name in `_class`, the same way the error classes carry theirs; `Object.prototype
//! .toString` reads `_class` to answer `[object Date]`.

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult, Signal};
use crate::object::{JsObject, Property};
use crate::value::Value;
use js_gc::Gc;

/// The name of the hidden property holding `[[DateValue]]`.
const VALUE: &str = "_date_value";
/// The class tag, so a Date is distinguishable from a plain object.
const CLASS: &str = "_class";

const MS_PER_DAY: f64 = 86_400_000.0;
/// The largest magnitude `TimeClip` accepts, in milliseconds (±100,000,000 days).
const MAX_TIME: f64 = 8.64e15;

const MONTH_DAYS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

// ---------------------------------------------------------------------------
// Calendar arithmetic
// ---------------------------------------------------------------------------

/// Floor division: rounds towards negative infinity, unlike Rust's `/`.
fn floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    if (a % b != 0) && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

/// Days from 1970-01-01 to January 1st of `y`.
fn days_from_year(y: i64) -> i64 {
    365 * (y - 1970) + floor_div(y - 1969, 4) - floor_div(y - 1901, 100) + floor_div(y - 1601, 400)
}

fn days_in_year(y: i64) -> i64 {
    365 + i64::from(y % 4 == 0) - i64::from(y % 100 == 0) + i64::from(y % 400 == 0)
}

/// The year containing the day index `*days`, which is rewritten to the day of that
/// year (0-based).
fn year_from_days(days: &mut i64) -> i64 {
    let d = *days;
    let mut y = floor_div(d * 10000, 3652425) + 1970;
    // The estimate is at most a year off, so this settles in a handful of passes.
    loop {
        let d1 = d - days_from_year(y);
        if d1 < 0 {
            y -= 1;
            continue;
        }
        let nd = days_in_year(y);
        if d1 < nd {
            *days = d1;
            return y;
        }
        y += 1;
    }
}

/// The time-zone offset at `ms` in minutes east of UTC.
///
/// The C version asks `localtime_r` for `tm_gmtoff` and divides by 60 with C's
/// truncation; whole minutes are what `Date` reports, even though a few historical
/// zones had offsets with a seconds component.
fn tz_minutes_east(ms: i64) -> i64 {
    match crate::tzif::local_offset(ms.div_euclid(1000)) {
        Some(seconds) => (seconds / 60) as i64,
        // No database installed: behave as if the machine were on UTC rather than
        // failing every local-time method.
        None => 0,
    }
}

/// `TimeClip`.
fn time_clip(t: f64) -> f64 {
    if (-MAX_TIME..=MAX_TIME).contains(&t) {
        // `+ 0.0` turns -0 into +0, which the spec requires.
        t.trunc() + 0.0
    } else {
        f64::NAN
    }
}

/// The broken-down fields of a Date.
///
/// Index order matches the reference implementation: year, month, day, hour,
/// minute, second, millisecond, weekday, and the offset in minutes east of UTC.
struct Fields {
    values: [f64; 9],
}

impl Fields {
    fn new() -> Fields {
        Fields { values: [0.0; 9] }
    }

    fn zeroed() -> Fields {
        Fields::new()
    }
}

/// Break `value` down into calendar fields.
///
/// `Ok(None)` means the Date holds `NaN` and `force` was false — the caller decides
/// whether that is "Invalid Date" or a `RangeError`. `Err` means `this` is not a
/// Date at all.
fn get_date_fields(
    interp: &Interp<'_>,
    this: &Value,
    is_local: bool,
    force: bool,
) -> Result<Option<Fields>, Signal> {
    let dval = this_date_value(interp, this)?;
    Ok(fields_of(dval, is_local, force))
}

/// The calendar arithmetic of [`get_date_fields`], split out so the print formatter
/// can render a Date without an `Interp`.
fn fields_of(dval: f64, is_local: bool, force: bool) -> Option<Fields> {
    let mut d: i64;
    let mut tz = 0i64;
    if dval.is_nan() {
        if !force {
            return None;
        }
        d = 0;
    } else {
        // Truncation towards zero, as the C cast does.
        d = dval as i64;
        if is_local {
            tz = tz_minutes_east(d);
            d += tz * 60_000;
        }
    }

    let mut h = math_mod(d, MS_PER_DAY as i64);
    let mut days = (d - h) / MS_PER_DAY as i64;
    let ms = h % 1000;
    h = (h - ms) / 1000;
    let s = h % 60;
    h = (h - s) / 60;
    let m = h % 60;
    h = (h - m) / 60;
    let wd = math_mod(days + 4, 7);
    let y = year_from_days(&mut days);

    let mut i = 0;
    while i < 11 {
        let mut md = MONTH_DAYS[i];
        if i == 1 {
            md += days_in_year(y) - 365;
        }
        if days < md {
            break;
        }
        days -= md;
        i += 1;
    }

    Some(Fields {
        values: [
            y as f64,
            i as f64,
            (days + 1) as f64,
            h as f64,
            m as f64,
            s as f64,
            ms as f64,
            wd as f64,
            tz as f64,
        ],
    })
}

/// The C `%` on a possibly negative value, i.e. the non-negative remainder.
fn math_mod(a: i64, b: i64) -> i64 {
    let r = a % b;
    if r < 0 { r + b } else { r }
}

/// `MakeDay` + `MakeTime` + `MakeDate`, then the local-time adjustment and clip.
///
/// The three `let .. = { let value = ..; value }` blocks exist to pin the evaluation
/// order: the reference uses `volatile` for the same reason, and test262 has a case
/// that only passes when the multiplications are not reordered or fused.
fn set_date_fields(fields: &[f64; 7], is_local: bool) -> f64 {
    let y = fields[0];
    let m = fields[1];
    let dt = fields[2];

    // MakeDay(year, month, date)
    let ym = y + (m / 12.0).floor();
    let mut mn = m % 12.0;
    if mn < 0.0 {
        mn += 12.0;
    }
    if ym < -271821.0 || ym > 275760.0 {
        return f64::NAN;
    }
    let yi = ym as i64;
    let mi = mn as i64;
    let mut days = days_from_year(yi);
    for i in 0..mi {
        days += MONTH_DAYS[i as usize];
        if i == 1 {
            days += days_in_year(yi) - 365;
        }
    }
    let day = days as f64 + dt - 1.0;

    // MakeTime(hour, minute, second, millisecond). Each product is stored into a
    // variable before the addition so the compiler cannot fold them together.
    let h = fields[3];
    let min = fields[4];
    let s = fields[5];
    let milli = fields[6];
    let mut time = h * 3_600_000.0;
    let t1 = min * 60_000.0;
    time += t1;
    let t2 = s * 1000.0;
    time += t2;
    time += milli;
    let _ = t2;

    // MakeDate(day, time), again with the product kept apart from the sum.
    let day_ms = day * MS_PER_DAY;
    let mut tv = day_ms + time;
    if !tv.is_finite() {
        return f64::NAN;
    }

    if is_local {
        let ti = if tv < i64::MIN as f64 {
            i64::MIN
        } else if tv >= 9.223372036854776e18 {
            i64::MAX
        } else {
            tv as i64
        };
        // The reference adds `getTimezoneOffset`, which counts minutes *west* of
        // UTC, so the east-positive offset is subtracted here.
        tv -= (tz_minutes_east(ti) * 60_000) as f64;
    }
    time_clip(tv)
}

/// `set_date_fields` with the argument coercion every caller needs: truncate each
/// field, reject non-finite ones, and map a year in `[0, 100)` into the 1900s.
fn set_date_fields_checked(fields: &mut [f64; 7], is_local: bool) -> f64 {
    for i in 0..7 {
        let a = fields[i];
        if !a.is_finite() {
            return f64::NAN;
        }
        fields[i] = a.trunc();
        if i == 0 && (0.0..100.0).contains(&fields[0]) {
            fields[0] += 1900.0;
        }
    }
    set_date_fields(fields, is_local)
}

// ---------------------------------------------------------------------------
// The object itself
// ---------------------------------------------------------------------------

/// Whether `object` is a Date instance.
fn is_date(object: &Gc<JsObject>) -> bool {
    object
        .own_property(CLASS)
        .and_then(|property| property.value)
        .is_some_and(|value| value.to_js_string() == "Date")
}

/// `thisTimeValue`: the `[[DateValue]]` of a Date, or a `TypeError`.
fn this_date_value(interp: &Interp<'_>, this: &Value) -> Result<f64, Signal> {
    if let Value::Object(object) = this
        && is_date(object)
    {
        let value = object
            .own_property(VALUE)
            .and_then(|property| property.value)
            .map(|value| value.to_number());
        if let Some(value) = value {
            return Ok(value);
        }
    }
    Err(interp.type_error("not a Date object"))
}

/// `SetThisTimeValue`.
fn set_this_date_value(interp: &Interp<'_>, this: &Value, value: f64) -> Result<f64, Signal> {
    if let Value::Object(object) = this
        && is_date(object)
    {
        object.define(
            VALUE,
            Property::with_attributes(Value::Float(value), false, false),
        );
        return Ok(value);
    }
    Err(interp.type_error("not a Date object"))
}

/// A fresh object with the Date class tag, used by `new Date` and by the
/// function-call form.
fn new_date(interp: &mut Interp<'_>, proto: Gc<JsObject>, value: f64) -> JsResult<Value> {
    let object = interp.new_object(Some(proto));
    object.define(
        CLASS,
        Property::with_attributes(interp.string("Date"), false, false),
    );
    object.define(
        VALUE,
        Property::with_attributes(Value::Float(value), false, false),
    );
    Ok(Value::Object(object))
}

/// Whether `this` is an object `new Date` built: one whose prototype chain runs
/// through `Date.prototype` (or is a Date already). A subclass instance qualifies,
/// which is what `class D extends Date` will need.
fn is_linked_to_date_proto(interp: &Interp<'_>, this: &Value) -> bool {
    let Value::Object(object) = this else {
        return false;
    };
    if is_date(object) {
        return true;
    }
    let date_proto = interp.realm.date_proto.clone();
    let mut current = Some(object.clone());
    // A bounded walk: a prototype cycle would otherwise loop forever.
    for _ in 0..100 {
        let Some(node) = current else {
            return false;
        };
        if node.ptr_eq(&date_proto) {
            return true;
        }
        current = node.with(|data| data.proto.clone());
    }
    false
}

/// The prototype of the Date being constructed, for `new Date`.
///
/// `construct` already made a `this` with `Date.prototype` as its prototype, so the
/// constructor only has to fill in the value. Keeping the receiver is what makes
/// `class MyDate extends Date` work later.
fn install_value(interp: &Interp<'_>, this: &Value, value: f64) -> JsResult<Value> {
    if let Value::Object(object) = this {
        object.define(
            CLASS,
            Property::with_attributes(interp.string("Date"), false, false),
        );
        object.define(
            VALUE,
            Property::with_attributes(Value::Float(value), false, false),
        );
        return Ok(this.clone());
    }
    Err(interp.type_error("not a Date object"))
}

/// Milliseconds since the epoch, truncated, like the reference's `date_now`.
fn date_now() -> f64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as f64,
        // A clock before 1970 is not worth supporting precisely, but must not panic.
        Err(error) => -(error.duration().as_millis() as f64),
    }
}

// ---------------------------------------------------------------------------
// String parsing
// ---------------------------------------------------------------------------

/// The bytes the parsers see: the reference converts the string to a byte array,
/// folds U+2212 MINUS into `-`, and replaces any other non-byte character with `x`.
fn parse_bytes(text: &str) -> Vec<u8> {
    text.chars()
        .take(127)
        .map(|c| {
            let code = c as u32;
            if code > 255 {
                if code == 0x2212 { b'-' } else { b'x' }
            } else {
                code as u8
            }
        })
        .collect()
}

/// A cursor over the byte string, standing in for the C `int p` plus sentinel.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn at(&self, pos: usize) -> u8 {
        self.bytes.get(pos).copied().unwrap_or(0)
    }

    fn peek(&self) -> u8 {
        self.at(self.pos)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    /// `string_get_digits`: up to `max` digits (0 = no limit), at least `min`.
    fn digits(&mut self, min: usize, max: usize) -> Option<i32> {
        let start = self.pos;
        let mut v: i32 = 0;
        loop {
            let c = self.at(self.pos);
            if !c.is_ascii_digit() {
                break;
            }
            if v >= 100_000_000 {
                return None;
            }
            v = v * 10 + i32::from(c - b'0');
            self.pos += 1;
            if max != 0 && self.pos - start == max {
                break;
            }
        }
        if self.pos - start < min {
            return None;
        }
        Some(v)
    }

    /// `string_skip_char`.
    fn skip_char(&mut self, c: u8) -> bool {
        if self.peek() == c {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// `string_skip_spaces`.
    fn skip_spaces(&mut self) -> u8 {
        while self.peek() == b' ' {
            self.pos += 1;
        }
        self.peek()
    }

    /// `string_skip_separators`: dashes, slashes, dots and commas.
    fn skip_separators(&mut self) -> u8 {
        while matches!(self.peek(), b'-' | b'/' | b'.' | b',') {
            self.pos += 1;
        }
        self.peek()
    }

    /// `string_skip_until`: stop on any byte in `stops`.
    fn skip_until(&mut self, stops: &[u8]) -> u8 {
        loop {
            let c = self.peek();
            if stops.contains(&c) || self.at_end() {
                return c;
            }
            self.pos += 1;
        }
    }

    /// `string_get_milliseconds`: an optional fractional part, truncated to ms.
    fn milliseconds(&mut self) -> Option<i32> {
        let mut mul = 100i32;
        let mut ms = 0i32;
        let mut p = self.pos;
        let c = self.at(p);
        if c == b'.' || c == b',' {
            p += 1;
            let start = p;
            loop {
                let c = self.at(p);
                if !c.is_ascii_digit() {
                    break;
                }
                ms += i32::from(c - b'0') * mul;
                mul /= 10;
                p += 1;
                if p - start == 9 {
                    break;
                }
            }
            // Only consume the separator when at least one digit followed it.
            if p > start {
                self.pos = p;
                return Some(ms);
            }
        }
        None
    }

    /// `string_match`, case-insensitively.
    fn match_str(&mut self, text: &str) -> bool {
        let start = self.pos;
        let mut p = start;
        for expected in text.bytes() {
            if upper(self.at(p)) != upper(expected) {
                self.pos = start;
                return false;
            }
            p += 1;
        }
        self.pos = p;
        true
    }

    /// `string_get_tzabbr`.
    fn tz_abbreviation(&mut self) -> Option<i32> {
        for &(name, offset) in TZ_ABBREVIATIONS {
            if self.match_str(name) {
                return Some(offset);
            }
        }
        None
    }

    /// `string_get_tzoffset`: `[+-]HH:mm`, `[+-]HHmm`, `[+-]HH` (lenient only), or `Z`.
    fn tz_offset(&mut self, strict: bool) -> Option<i32> {
        let start = self.pos;
        let sign = self.at(self.pos);
        self.pos += 1;
        let mut tz = 0i32;
        if sign == b'+' || sign == b'-' {
            let digits_start = self.pos;
            let mut hh = self.digits(1, 0)?;
            let mut n = self.pos - digits_start;
            if strict && n != 2 && n != 4 {
                self.pos = start;
                return None;
            }
            while n > 4 {
                n -= 2;
                hh /= 100;
            }
            let mm;
            if n > 2 {
                mm = hh % 100;
                hh /= 100;
            } else if self.skip_char(b':') {
                mm = self.digits(2, 2)?;
            } else if strict {
                self.pos = start;
                return None;
            } else {
                mm = 0;
            }
            if hh > 23 || mm > 59 {
                self.pos = start;
                return None;
            }
            tz = hh * 60 + mm;
            if sign != b'+' {
                tz = -tz;
            }
        } else if sign != b'Z' {
            self.pos = start;
            return None;
        }
        Some(tz)
    }

    /// `string_get_month`: a three-letter month abbreviation.
    fn month(&mut self) -> Option<i32> {
        for (index, name) in MONTH_NAMES.iter().enumerate() {
            let matched = name
                .bytes()
                .enumerate()
                .all(|(offset, expected)| upper(self.at(self.pos + offset)) == upper(expected));
            if matched {
                self.pos += 3;
                return Some(index as i32 + 1);
            }
        }
        None
    }
}

fn upper(c: u8) -> u8 {
    c.to_ascii_uppercase()
}

/// `js_tzabbr`, used by the lenient parser only.
const TZ_ABBREVIATIONS: &[(&str, i32)] = &[
    ("GMT", 0),
    ("UTC", 0),
    ("UT", 0),
    ("Z", 0),
    ("EDT", -4 * 60),
    ("EST", -5 * 60),
    ("CDT", -5 * 60),
    ("CST", -6 * 60),
    ("MDT", -6 * 60),
    ("MST", -7 * 60),
    ("PDT", -7 * 60),
    ("PST", -8 * 60),
    ("WET", 0),
    ("WEST", 60),
    ("CET", 60),
    ("CEST", 120),
    ("EET", 120),
    ("EEST", 180),
];

/// The ES5 Date Time String format, plus the extensions the reference accepts.
fn parse_isostring(bytes: &[u8]) -> Option<([i32; 9], bool)> {
    let mut fields = [0i32; 9];
    fields[2] = 1;
    let mut is_local = false;
    let mut cursor = Cursor { bytes, pos: 0 };

    // The year is four digits, or six with an explicit sign.
    let sign = cursor.peek();
    if sign == b'-' || sign == b'+' {
        cursor.pos += 1;
        let year = cursor.digits(6, 6)?;
        if sign == b'-' {
            if year == 0 {
                return None; // -000000 is rejected
            }
            fields[0] = -year;
        } else {
            fields[0] = year;
        }
    } else {
        fields[0] = cursor.digits(4, 4)?;
    }

    if cursor.skip_char(b'-') {
        let month = cursor.digits(2, 2)?;
        if month < 1 {
            return None;
        }
        fields[1] = month - 1;
        if cursor.skip_char(b'-') {
            let day = cursor.digits(2, 2)?;
            if day < 1 {
                return None;
            }
            fields[2] = day;
        }
    }

    if cursor.skip_char(b'T') {
        is_local = true;
        match (
            cursor.digits(2, 2),
            cursor.skip_char(b':'),
            cursor.digits(2, 2),
        ) {
            (Some(hour), true, Some(minute)) => {
                fields[3] = hour;
                fields[4] = minute;
            }
            _ => {
                // A well-formed date followed by a broken `T…` is rejected.
                fields[3] = 100;
                return Some((fields, is_local));
            }
        }
        if cursor.skip_char(b':') {
            fields[5] = cursor.digits(2, 2)?;
            if let Some(ms) = cursor.milliseconds() {
                fields[6] = ms;
            }
        }
    }

    // An offset may follow: [+-]HH:mm or [+-]HHmm.
    if !cursor.at_end() {
        is_local = false;
        fields[8] = cursor.tz_offset(true)?;
    }

    if !cursor.at_end() {
        return None;
    }
    Some((fields, is_local))
}

/// The lenient parser: what `toString`, `toUTCString` and friends produce, and what
/// browsers accept.
fn parse_otherstring(bytes: &[u8]) -> Option<([i32; 9], bool)> {
    // Starts at 2001-01-01, not the epoch.
    let mut fields = [0i32; 9];
    fields[0] = 2001;
    fields[1] = 1;
    fields[2] = 1;
    let mut is_local = true;
    let mut cursor = Cursor { bytes, pos: 0 };

    let mut has_year = false;
    let mut has_mon = false;
    let mut has_time = false;
    let mut num = [0i32; 3];
    let mut num_index = 0;

    while cursor.skip_spaces() != 0 {
        let p_start = cursor.pos;
        let c = cursor.peek();
        if c == b'+' || c == b'-' {
            let attempt = cursor.tz_offset(false);
            if has_time && attempt.is_some() {
                fields[8] = attempt?;
                is_local = false;
            } else {
                cursor.pos += 1;
                if let Some(mut val) = cursor.digits(1, 0) {
                    if c == b'-' {
                        if val == 0 {
                            return None;
                        }
                        val = -val;
                    }
                    fields[0] = val;
                    has_year = true;
                }
            }
        } else if let Some(val) = cursor.digits(1, 0) {
            if cursor.skip_char(b':') {
                // A time-of-day.
                fields[3] = val;
                fields[4] = cursor.digits(1, 2)?;
                if cursor.skip_char(b':') {
                    fields[5] = cursor.digits(1, 2)?;
                    if let Some(ms) = cursor.milliseconds() {
                        fields[6] = ms;
                    }
                }
                has_time = true;
                if matches!(cursor.peek(), b'+' | b'-')
                    && let Some(offset) = cursor.tz_offset(false)
                {
                    fields[8] = offset;
                    is_local = false;
                }
            } else if cursor.pos - p_start > 2 && !has_year {
                fields[0] = val;
                has_year = true;
            } else if !(1..=31).contains(&val) && !has_year {
                fields[0] = val + i32::from(val < 100) * 1900 + i32::from(val < 50) * 100;
                has_year = true;
            } else {
                if num_index == 3 {
                    return None;
                }
                num[num_index] = val;
                num_index += 1;
            }
        } else if let Some(month) = cursor.month() {
            has_mon = true;
            fields[1] = month;
            cursor.skip_until(b"0123456789 -/(");
        } else if has_time && cursor.match_str("PM") {
            if fields[3] < 12 {
                fields[3] += 12;
            }
            continue;
        } else if has_time && cursor.match_str("AM") {
            if fields[3] == 12 {
                fields[3] -= 12;
            }
            continue;
        } else if let Some(offset) = cursor.tz_abbreviation() {
            fields[8] = offset;
            is_local = false;
            continue;
        } else if c == b'(' {
            // A parenthesised phrase, e.g. the zone name in `toString` output.
            let mut level = 0;
            loop {
                let c = cursor.peek();
                if c == 0 {
                    break;
                }
                cursor.pos += 1;
                if c == b'(' {
                    level += 1;
                }
                if c == b')' {
                    level -= 1;
                }
                if level == 0 {
                    break;
                }
            }
            if level > 0 {
                return None;
            }
        } else if c == b')' {
            return None;
        } else {
            if i32::from(has_year)
                + i32::from(has_mon)
                + i32::from(has_time)
                + i32::try_from(num_index).unwrap_or(0)
                != 0
            {
                return None;
            }
            // Skip an unrecognised word.
            cursor.skip_until(b" -/(");
        }
        cursor.skip_separators();
    }

    if num_index as i32 + i32::from(has_year) + i32::from(has_mon) > 3 {
        return None;
    }

    match num_index {
        0 => {
            if !has_year {
                return None;
            }
        }
        1 => {
            if has_mon {
                fields[2] = num[0];
            } else {
                fields[1] = num[0];
            }
        }
        2 => {
            if has_year {
                fields[1] = num[0];
                fields[2] = num[1];
            } else if has_mon {
                fields[0] = num[1] + i32::from(num[1] < 100) * 1900 + i32::from(num[1] < 50) * 100;
                fields[2] = num[0];
            } else {
                fields[1] = num[0];
                fields[2] = num[1];
            }
        }
        3 => {
            fields[0] = num[2] + i32::from(num[2] < 100) * 1900 + i32::from(num[2] < 50) * 100;
            fields[1] = num[0];
            fields[2] = num[1];
        }
        _ => return None,
    }

    if fields[1] < 1 || fields[2] < 1 {
        return None;
    }
    fields[1] -= 1;
    Some((fields, is_local))
}

/// `Date.parse`, over the two parsers and the range checks between them.
fn parse_date(text: &str) -> f64 {
    let bytes = parse_bytes(text);
    let parsed = parse_isostring(&bytes).or_else(|| parse_otherstring(&bytes));
    let Some((fields, is_local)) = parsed else {
        return f64::NAN;
    };

    const FIELD_MAX: [i32; 6] = [0, 11, 31, 24, 59, 59];
    let mut valid = true;
    for i in 1..6 {
        if fields[i] > FIELD_MAX[i] {
            valid = false;
        }
    }
    // 24:00:00.000 is the one legal midnight-at-the-end-of-the-day spelling.
    if fields[3] == 24 && (fields[4] | fields[5] | fields[6]) != 0 {
        valid = false;
    }
    if !valid {
        return f64::NAN;
    }

    let mut seven = [0f64; 7];
    for i in 0..7 {
        seven[i] = fields[i] as f64;
    }
    // `set_date_fields` already folds in the local offset, and `fields[8]` is the
    // explicit offset in minutes east of UTC, which is subtracted.
    set_date_fields(&seven, is_local) - f64::from(fields[8]) * 60_000.0
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// The `toISOString` form of a time value, or `None` for a `NaN` date.
///
/// Used by the print formatter, which shows a Date as its ISO string rather than as
/// an object with hidden properties.
pub(crate) fn iso_string(dval: f64) -> Option<String> {
    let fields = fields_of(dval, false, false)?;
    let y = fields.values[0] as i64;
    let mon = fields.values[1] as i64;
    let d = fields.values[2] as i64;
    let h = fields.values[3] as i64;
    let m = fields.values[4] as i64;
    let s = fields.values[5] as i64;
    let ms = fields.values[6] as i64;
    let mut out = String::new();
    if (0..=9999).contains(&y) {
        out.push_str(&format!("{y:04}"));
    } else {
        out.push_str(&format!("{y:+07}"));
    }
    out.push_str(&format!(
        "-{:02}-{:02}T{h:02}:{m:02}:{s:02}.{ms:03}Z",
        mon + 1,
        d
    ));
    Some(out)
}

/// The `fmt`/`part` pair the reference passes as `magic`, spelled out.
struct Fmt {
    /// 0 = toUTCString, 1 = toString, 2 = toISOString, 3 = toLocaleString.
    format: u8,
    /// 1 = date part, 2 = time part, 3 = both.
    part: u8,
}

fn format_date(interp: &mut Interp<'_>, this: &Value, spec: &Fmt) -> JsResult<Value> {
    // `fmt & 1` is 1 for toString, which is the local-time variant.
    let is_local = spec.format & 1 != 0;
    let fields = match get_date_fields(interp, this, is_local, false)? {
        Some(fields) => fields,
        None => {
            if spec.format == 2 {
                return Err(interp.range_error("Date value is NaN"));
            }
            return Ok(interp.string("Invalid Date"));
        }
    };

    let y = fields.values[0] as i64;
    let mon = fields.values[1] as i64;
    let d = fields.values[2] as i64;
    let h = fields.values[3] as i64;
    let m = fields.values[4] as i64;
    let s = fields.values[5] as i64;
    let ms = fields.values[6] as i64;
    let wd = fields.values[7] as i64;
    let mut tz = fields.values[8] as i64;

    let mut out = String::new();
    if spec.part & 1 != 0 {
        match spec.format {
            0 => {
                out.push_str(&format!(
                    "{}, {:02} {} {:04} ",
                    DAY_NAMES[wd as usize], d, MONTH_NAMES[mon as usize], y
                ));
            }
            1 => {
                out.push_str(&format!(
                    "{} {} {:02} {:04}",
                    DAY_NAMES[wd as usize], MONTH_NAMES[mon as usize], d, y
                ));
                if spec.part == 3 {
                    out.push(' ');
                }
            }
            2 => {
                if (0..=9999).contains(&y) {
                    out.push_str(&format!("{y:04}"));
                } else {
                    out.push_str(&format!("{y:+07}"));
                }
                out.push_str(&format!("-{:02}-{:02}T", mon + 1, d));
            }
            _ => {
                out.push_str(&format!("{:02}/{:02}/{:04}", mon + 1, d, y));
                if spec.part == 3 {
                    out.push_str(", ");
                }
            }
        }
    }
    if spec.part & 2 != 0 {
        match spec.format {
            0 => out.push_str(&format!("{h:02}:{m:02}:{s:02} GMT")),
            1 => {
                out.push_str(&format!("{h:02}:{m:02}:{s:02} GMT"));
                if tz < 0 {
                    out.push('-');
                    tz = -tz;
                } else {
                    out.push('+');
                }
                out.push_str(&format!("{:02}{:02}", tz / 60, tz % 60));
            }
            2 => out.push_str(&format!("{h:02}:{m:02}:{s:02}.{ms:03}Z")),
            _ => out.push_str(&format!(
                "{:02}:{m:02}:{s:02} {}M",
                (h + 11) % 12 + 1,
                if h < 12 { 'A' } else { 'P' }
            )),
        }
    }
    Ok(interp.string(out))
}

// ---------------------------------------------------------------------------
// Native entry points
// ---------------------------------------------------------------------------

fn constructor(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    // `Date()` called as a function ignores its arguments and returns a string; only
    // the `new` form builds an object. `construct` supplies a fresh `this` whose
    // prototype is `Date.prototype` (or a subclass's), while a plain call gets
    // `undefined` — or, for `Date.call(obj)`, an object that is not linked to
    // `Date.prototype`. Testing the link is what separates the two forms.
    let called_as_function = !is_linked_to_date_proto(interp, &this);

    let argc = if called_as_function { 0 } else { args.len() };
    let value = if argc == 0 {
        date_now()
    } else if argc == 1 {
        let arg = &args[0];
        // A Date argument is copied by value, without going through ToPrimitive.
        let copied = match arg {
            Value::Object(object) if is_date(object) => object
                .own_property(VALUE)
                .and_then(|property| property.value)
                .map(|value| value.to_number()),
            _ => None,
        };
        match copied {
            Some(value) => time_clip(value),
            None => {
                let primitive = interp.to_primitive(arg.clone(), crate::interp::Hint::Default)?;
                if matches!(primitive, Value::String(_)) {
                    time_clip(parse_date(&primitive.to_js_string()))
                } else {
                    time_clip(interp.to_js_number(&primitive)?)
                }
            }
        }
    } else {
        let mut fields = [0f64, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        let n = argc.min(7);
        for i in 0..n {
            fields[i] = interp.to_js_number(&args[i])?;
        }
        set_date_fields_checked(&mut fields, true)
    };

    if called_as_function {
        // `Date()` returns `(new Date()).toString()`.
        let proto = interp.realm.date_proto.clone();
        let object = new_date(interp, proto, value)?;
        return format_date(interp, &object, &Fmt { format: 1, part: 3 });
    }
    install_value(interp, &this, value)
}

fn date_parse(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = interp.to_js_string(&arg(args, 0))?;
    Ok(Value::Float(parse_date(&text)))
}

fn date_utc(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    if args.is_empty() {
        return Ok(Value::Float(f64::NAN));
    }
    let mut fields = [0f64, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
    let n = args.len().min(7);
    for i in 0..n {
        fields[i] = interp.to_js_number(&args[i])?;
    }
    Ok(Value::Float(set_date_fields_checked(&mut fields, false)))
}

fn now(_interp: &mut Interp<'_>, _this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(Value::Float(date_now()))
}

fn get_time(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(Value::Float(this_date_value(interp, &this)?))
}

fn set_time(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    this_date_value(interp, &this)?;
    let value = interp.to_js_number(&arg(args, 0))?;
    Ok(Value::Float(set_this_date_value(
        interp,
        &this,
        time_clip(value),
    )?))
}

fn get_timezone_offset(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let value = this_date_value(interp, &this)?;
    if value.is_nan() {
        return Ok(Value::Float(f64::NAN));
    }
    // Reported the other way round from `tz_minutes_east`, as the spec says. The
    // reference returns an *integer* here, which matters for UTC: a float -0 would
    // print as `-0` instead of `0`.
    Ok(Value::Int(-(tz_minutes_east(value.trunc() as i64) as i32)))
}

/// Emit one `getX`/`getUTCX` wrapper. `field` is the index into `fields`, `local`
/// selects the local or UTC breakdown, and `minus_1900` is `getYear`'s quirk.
macro_rules! date_getter {
    ($fname:ident, $field:expr, $local:expr, $minus_1900:expr) => {
        fn $fname(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
            let fields = match get_date_fields(interp, &this, $local, false)? {
                Some(fields) => fields,
                None => return Ok(Value::Float(f64::NAN)),
            };
            let mut value = fields.values[$field];
            if $minus_1900 {
                value -= 1900.0;
            }
            Ok(Value::Float(value))
        }
    };
}

date_getter!(get_year, 0, true, true);
date_getter!(get_full_year, 0, true, false);
date_getter!(get_utc_full_year, 0, false, false);
date_getter!(get_month, 1, true, false);
date_getter!(get_utc_month, 1, false, false);
date_getter!(get_date, 2, true, false);
date_getter!(get_utc_date, 2, false, false);
date_getter!(get_hours, 3, true, false);
date_getter!(get_utc_hours, 3, false, false);
date_getter!(get_minutes, 4, true, false);
date_getter!(get_utc_minutes, 4, false, false);
date_getter!(get_seconds, 5, true, false);
date_getter!(get_utc_seconds, 5, false, false);
date_getter!(get_milliseconds, 6, true, false);
date_getter!(get_utc_milliseconds, 6, false, false);
date_getter!(get_day, 7, true, false);
date_getter!(get_utc_day, 7, false, false);

/// `set_date_field`: read the current fields, overwrite `first..end`, write back.
fn set_date_field(
    interp: &mut Interp<'_>,
    this: &Value,
    first_field: usize,
    end_field: usize,
    is_local: bool,
    args: &[Value],
) -> JsResult<Value> {
    // `setFullYear` on an invalid Date starts from all-zero fields instead of
    // failing, which is why `force` is set exactly when the year is being replaced.
    let read = get_date_fields(interp, this, is_local, first_field == 0)?;
    let had_value = read.is_some();
    let mut fields = read.unwrap_or_else(Fields::zeroed);

    // Argument coercion is observable, so it happens even if the receiver is NaN.
    let n = args.len().min(end_field - first_field);
    let mut ok = had_value;
    for i in 0..n {
        let a = interp.to_js_number(&args[i])?;
        if !a.is_finite() {
            ok = false;
        }
        fields.values[first_field + i] = a.trunc();
    }

    if !had_value {
        return Ok(Value::Float(f64::NAN));
    }

    let mut value = f64::NAN;
    if ok && !args.is_empty() {
        let mut seven = [0f64; 7];
        for (index, slot) in seven.iter_mut().enumerate() {
            *slot = fields.values[index];
        }
        value = set_date_fields(&seven, is_local);
    }
    Ok(Value::Float(set_this_date_value(interp, this, value)?))
}

macro_rules! date_setter {
    ($fname:ident, $first:expr, $end:expr, $local:expr) => {
        fn $fname(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
            set_date_field(interp, &this, $first, $end, $local, args)
        }
    };
}

date_setter!(set_milliseconds, 6, 7, true);
date_setter!(set_utc_milliseconds, 6, 7, false);
date_setter!(set_seconds, 5, 7, true);
date_setter!(set_utc_seconds, 5, 7, false);
date_setter!(set_minutes, 4, 7, true);
date_setter!(set_utc_minutes, 4, 7, false);
date_setter!(set_hours, 3, 7, true);
date_setter!(set_utc_hours, 3, 7, false);
date_setter!(set_date_field_date, 2, 3, true);
date_setter!(set_utc_date, 2, 3, false);
date_setter!(set_month, 1, 3, true);
date_setter!(set_utc_month, 1, 3, false);
date_setter!(set_full_year, 0, 3, true);
date_setter!(set_utc_full_year, 0, 3, false);

/// `setYear`, which maps 0..100 into the 1900s and keeps `getYear`'s offset.
fn set_year(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    // Reading the current value first is what rejects a non-Date receiver, and the
    // reference does it before coercing the argument.
    this_date_value(interp, &this)?;
    let mut y = interp.to_js_number(&arg(args, 0))?;
    if y.is_finite() {
        y = y.trunc();
        if (0.0..100.0).contains(&y) {
            y += 1900.0;
        }
    }
    set_date_field(interp, &this, 0, 1, true, &[Value::Float(y)])
}

macro_rules! date_formatter {
    ($fname:ident, $format:expr, $part:expr) => {
        fn $fname(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
            format_date(
                interp,
                &this,
                &Fmt {
                    format: $format,
                    part: $part,
                },
            )
        }
    };
}

date_formatter!(to_string, 1, 3);
date_formatter!(to_utc_string, 0, 3);
date_formatter!(to_iso_string, 2, 3);
date_formatter!(to_date_string, 1, 1);
date_formatter!(to_time_string, 1, 2);
date_formatter!(to_locale_string, 3, 3);
date_formatter!(to_locale_date_string, 3, 1);
date_formatter!(to_locale_time_string, 3, 2);

/// `toJSON`: `null` for a non-finite time value, otherwise `toISOString()`.
fn to_json(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let value = this_date_value(interp, &this)?;
    if !value.is_finite() {
        return Ok(Value::Null);
    }
    let iso = method_of(interp, &this, "toISOString")?;
    let Some(iso) = iso else {
        return Err(interp.type_error("object needs toISOString method"));
    };
    interp.call_value(iso, this, Vec::new())
}

/// Read a method off a receiver that may be a primitive, boxing it first the way
/// `GetProperty(ToObject(x))` does.
fn method_of(interp: &mut Interp<'_>, this: &Value, name: &str) -> JsResult<Option<Value>> {
    match this {
        Value::Object(object) => interp.get_property(object, name),
        _ => Ok(None),
    }
}

/// `Symbol.toPrimitive`, spelled as an ordinary method for now: a Date with the
/// default hint converts to a *string*, not a number.
fn to_primitive(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let hint = arg(args, 0).to_js_string();
    let hint = match hint.as_str() {
        "number" => crate::interp::Hint::Number,
        "string" | "default" => crate::interp::Hint::String,
        _ => return Err(interp.type_error("invalid hint")),
    };
    let order: [&str; 2] = match hint {
        crate::interp::Hint::Number => ["valueOf", "toString"],
        _ => ["toString", "valueOf"],
    };
    for method in order {
        if let Some(function) = method_of(interp, &this, method)?
            && matches!(&function, Value::Object(object) if object.is_callable())
        {
            let result = interp.call_value(function, this.clone(), Vec::new())?;
            if !matches!(result, Value::Object(_)) {
                return Ok(result);
            }
        }
    }
    Err(interp.type_error("cannot convert object to primitive value"))
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// Install `Date` and `Date.prototype`.
pub fn install(interp: &mut Interp<'_>) {
    let object_proto = interp.realm.object_proto.clone();
    let date_proto = interp.new_object(Some(object_proto));
    interp.realm.date_proto = date_proto.clone();

    for (name, length, func) in [
        ("getTime", 0, get_time as crate::object::NativeFn),
        ("valueOf", 0, get_time),
        ("getTimezoneOffset", 0, get_timezone_offset),
        ("getYear", 0, get_year),
        ("getFullYear", 0, get_full_year),
        ("getUTCFullYear", 0, get_utc_full_year),
        ("getMonth", 0, get_month),
        ("getUTCMonth", 0, get_utc_month),
        ("getDate", 0, get_date),
        ("getUTCDate", 0, get_utc_date),
        ("getHours", 0, get_hours),
        ("getUTCHours", 0, get_utc_hours),
        ("getMinutes", 0, get_minutes),
        ("getUTCMinutes", 0, get_utc_minutes),
        ("getSeconds", 0, get_seconds),
        ("getUTCSeconds", 0, get_utc_seconds),
        ("getMilliseconds", 0, get_milliseconds),
        ("getUTCMilliseconds", 0, get_utc_milliseconds),
        ("getDay", 0, get_day),
        ("getUTCDay", 0, get_utc_day),
        ("setTime", 1, set_time),
        ("setMilliseconds", 1, set_milliseconds),
        ("setUTCMilliseconds", 1, set_utc_milliseconds),
        ("setSeconds", 2, set_seconds),
        ("setUTCSeconds", 2, set_utc_seconds),
        ("setMinutes", 3, set_minutes),
        ("setUTCMinutes", 3, set_utc_minutes),
        ("setHours", 4, set_hours),
        ("setUTCHours", 4, set_utc_hours),
        ("setDate", 1, set_date_field_date),
        ("setUTCDate", 1, set_utc_date),
        ("setMonth", 2, set_month),
        ("setUTCMonth", 2, set_utc_month),
        ("setFullYear", 3, set_full_year),
        ("setUTCFullYear", 3, set_utc_full_year),
        ("setYear", 1, set_year),
        ("toString", 0, to_string),
        ("toUTCString", 0, to_utc_string),
        ("toISOString", 0, to_iso_string),
        ("toDateString", 0, to_date_string),
        ("toTimeString", 0, to_time_string),
        ("toLocaleString", 0, to_locale_string),
        ("toLocaleDateString", 0, to_locale_date_string),
        ("toLocaleTimeString", 0, to_locale_time_string),
        ("toJSON", 1, to_json),
        ("[Symbol.toPrimitive]", 1, to_primitive),
    ] {
        define_method(&date_proto, name, length, func, interp);
    }
    // `toGMTString` is the legacy alias, the same function object.
    if let Some(utc) = date_proto.own_property("toUTCString") {
        if let Some(value) = utc.value {
            date_proto.define("toGMTString", Property::with_attributes(value, true, false));
        }
    }

    let constructor = interp.new_native_constructor("Date", 7, constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(date_proto.clone()), false, false),
    );
    date_proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    for (name, length, func) in [
        ("now", 0, now as crate::object::NativeFn),
        ("parse", 1, date_parse),
        ("UTC", 7, date_utc),
    ] {
        define_method(&constructor, name, length, func, interp);
    }

    let global = interp.realm.global.clone();
    global.define(
        "Date",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_division_rounds_down() {
        assert_eq!(floor_div(7, 2), 3);
        assert_eq!(floor_div(-7, 2), -4);
        assert_eq!(floor_div(-1, 4), -1);
        assert_eq!(floor_div(0, 4), 0);
    }

    #[test]
    fn day_counts_match_known_dates() {
        assert_eq!(days_from_year(1970), 0);
        assert_eq!(days_from_year(1971), 365);
        // 1972 is a leap year, so it is the *next* year that gains a day.
        assert_eq!(days_from_year(1973) - days_from_year(1972), 366);
        assert_eq!(days_in_year(1972), 366);
        assert_eq!(days_in_year(2000), 366);
        assert_eq!(days_in_year(1900), 365);
        assert_eq!(days_in_year(2100), 365);

        // Every day index must round-trip through year_from_days.
        for start in [-1_000_000i64, -1, 0, 1, 10_000, 100_000_000] {
            let mut days = start;
            let year = year_from_days(&mut days);
            assert_eq!(
                days_from_year(year) + days,
                start,
                "round trip failed at {start}"
            );
            assert!((0..days_in_year(year)).contains(&days));
        }
    }

    #[test]
    fn time_clip_turns_negative_zero_positive() {
        assert!(time_clip(-0.5).is_sign_positive());
        assert_eq!(time_clip(-0.5), 0.0);
        assert_eq!(time_clip(1.9), 1.0);
        assert!(time_clip(8.64e15 + 1.0).is_nan());
        assert!(time_clip(-8.64e15 - 1.0).is_nan());
    }

    #[test]
    fn iso_strings_parse() {
        assert_eq!(parse_date("2000"), 946_684_800_000.0);
        assert_eq!(parse_date("2000-01"), 946_684_800_000.0);
        assert_eq!(parse_date("2000-01-01"), 946_684_800_000.0);
        assert_eq!(parse_date("2000-01-01T00:00Z"), 946_684_800_000.0);
        assert_eq!(parse_date("2000-01-01T00:00:00.1Z"), 946_684_800_100.0);
        assert_eq!(parse_date("2000-01-01T00:00:00+00:00"), 946_684_800_000.0);
        // The fractional part is truncated, not rounded. Checked against the
        // reference: `Date.parse("2020-01-01T01:01:01.9999Z")`.
        assert_eq!(parse_date("2020-01-01T01:01:01.9999Z"), 1_577_840_461_999.0);
        assert!(parse_date("").is_nan());
        assert!(parse_date("-000000").is_nan());
    }

    #[test]
    fn other_strings_parse() {
        assert!(!parse_date("Jan 1 2000").is_nan());
        // An explicit zone after the date is honoured, and the bare form is local.
        assert_eq!(parse_date("Jan 1 2000 UTC"), 946_684_800_000.0);
        assert_eq!(
            parse_date("Jan 1 2000 00:00:00 GMT+0100"),
            946_681_200_000.0
        );
        assert_eq!(
            parse_date("Jan 1 2000 00:00:00 GMT+0100"),
            parse_date("2000-01-01T00:00:00+01:00")
        );
        // Two-digit years are expanded, 00-49 into the 2000s and 50-99 into the 1900s.
        assert_eq!(parse_date("1/1/00"), parse_date("Jan 1 2000"));
        assert_eq!(parse_date("1/1/99"), parse_date("Jan 1 1999"));
    }

    #[test]
    fn make_time_keeps_the_specified_order() {
        // Straight from test262's fp-evaluation-order case, which is why the
        // multiplications are not merged.
        assert_eq!(
            set_date_fields_checked(
                &mut [
                    1970.0,
                    0.0,
                    1.0,
                    80063993375.0,
                    29.0,
                    1.0,
                    -288230376151711740.0
                ],
                false
            ),
            29312.0
        );
        assert_eq!(
            set_date_fields_checked(
                &mut [
                    1970.0,
                    0.0,
                    213503982336.0,
                    0.0,
                    0.0,
                    0.0,
                    -18446744073709552000.0
                ],
                false
            ),
            34447360.0
        );
    }

    #[test]
    fn large_offsets_wrap_around() {
        // Years and days far outside the calendar still land on the right instant.
        assert_eq!(
            set_date_fields_checked(
                &mut [2017.0, 9.0, 22.0 - 1e10, 18.0 + 24e10, 0.0, 0.0, 0.0],
                false
            ),
            1_508_695_200_000.0
        );
    }
}
