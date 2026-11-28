//! `std.__sprintObject` / `std.__printObject`: the reference's `JS_PrintValue`.
//!
//! # Not `JSON.stringify`, and not `console.log`
//!
//! `JS_PrintValue` is quickjs's own dumper, with its own rules, and both of quickjs's
//! front-ends print through it. It is not JSON: keys are bare when they are identifiers,
//! values are truncated, cycles are marked, and a `Map` prints as a `Map` rather than as
//! `{}`. Matching it byte for byte is the whole job, so every rule below is taken from
//! `js_print_value`/`js_print_object` in `quickjs.c` rather than invented:
//!
//! ```text
//! [ 1, 2, 3 ]              arrays get spaces inside the brackets
//! [  ]                     and so does an empty one
//! {  }                     an empty object has two spaces
//! { a: 1, "a b": 2 }       bare key when it is an identifier, quoted otherwise
//! { a: { b: [Array] } }    past `max_depth` (2) an object prints as its class name
//! { self: [circular 0] }   a cycle names the depth it was first seen at
//! [ 0: 1, 2: 3 ]           a sparse array prints index: value pairs
//! Map(1) { 1 => 2 }
//! Set(2) { 1, 2 }
//! [Function f]
//! ```
//!
//! The defaults are the reference's: `max_depth = 2`, `max_string_length = 1000`,
//! `max_item_count = 100`. Hitting a limit is visible in the output (`... 3 more items`,
//! `... 5 more characters`), which is why the numbers have to be exact.

use std::fmt::Write as _;

use crate::object::PropertyKey;
use crate::{Gc, Interp, JsObject, JsResult, Value};

/// How deep objects are expanded before they print as `[ClassName]`.
const MAX_DEPTH: usize = 2;
/// How many characters of a string are printed.
const MAX_STRING_LENGTH: usize = 1000;
/// How many items of a collection are printed.
const MAX_ITEM_COUNT: usize = 100;

/// `std.__sprintObject(value)` — the same text `std.__printObject` would write.
pub fn sprint(interp: &mut Interp<'_>, value: &Value) -> JsResult<String> {
    let mut printer = Printer {
        interp,
        out: String::new(),
        stack: Vec::new(),
    };
    printer.print_value(value)?;
    Ok(printer.out)
}

struct Printer<'i, 'a> {
    interp: &'i mut Interp<'a>,
    out: String,
    /// The objects being printed, outermost first. A hit is a cycle, reported by index.
    stack: Vec<Gc<JsObject>>,
}

/// How the next separator should be written, the part of the reference's `comma_state` that
/// still applies here.
///
/// `comma_state` in `quickjs.c` has a third value, `2`, which suppresses the closing ` }`
/// after a `Function`/`RegExp`/`Date`/`Error`. That case does not arise here because those
/// classes return before a brace body is opened at all, so there is nothing to suppress.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Comma {
    First,
    Next,
}

impl Comma {
    fn before(&mut self, out: &mut String) {
        if *self == Comma::Next {
            out.push_str(", ");
        }
        *self = Comma::Next;
    }
}

impl Printer<'_, '_> {
    fn print_value(&mut self, value: &Value) -> JsResult<()> {
        match value {
            Value::Undefined => self.out.push_str("undefined"),
            Value::Null => self.out.push_str("null"),
            Value::Bool(flag) => self.out.push_str(if *flag { "true" } else { "false" }),
            Value::Int(number) => {
                let _ = write!(self.out, "{number}");
            }
            // `js_print_float64` uses `JS_DTOA_MINUS_ZERO`, so `-0` keeps its sign where
            // `String(-0)` would print `0`. Everything else is the ordinary JS form,
            // including `1e+21` and `NaN`.
            Value::Float(number) => {
                if *number == 0.0 && number.is_sign_negative() {
                    // `JS_DTOA_MINUS_ZERO`: the one place the printer differs from
                    // `String()`, which prints `0`.
                    self.out.push_str("-0");
                } else {
                    // Everything else is the ordinary JS form — `1e+21`, `1e-7`,
                    // `Infinity` — because that is what `js_dtoa(JS_DTOA_FORMAT_FREE)` is.
                    self.out.push_str(&value.to_js_string());
                }
            }
            Value::BigInt(big) => {
                self.out.push_str(&big.to_decimal_string());
                self.out.push('n');
            }
            Value::Symbol(symbol) => {
                let _ = write!(
                    self.out,
                    "Symbol({:?})",
                    symbol.description().unwrap_or_default()
                );
            }
            Value::String(_) => {
                let text = value.to_js_string();
                self.print_string(&text);
            }
            Value::Object(object) => self.print_object(object)?,
            // `Value` is non-exhaustive in spirit: anything else has no reference name.
            other => {
                let _ = write!(self.out, "[unknown value {}]", other.type_of());
            }
        }
        Ok(())
    }
}

impl Printer<'_, '_> {
    /// A string in double quotes, with the reference's escaping.
    fn print_string(&mut self, text: &str) {
        self.out.push('"');
        let chars: Vec<char> = text.chars().collect();
        let shown = chars.len().min(MAX_STRING_LENGTH);
        for &c in &chars[..shown] {
            match c {
                '\t' => self.out.push_str("\\t"),
                '\r' => self.out.push_str("\\r"),
                '\n' => self.out.push_str("\\n"),
                '\u{8}' => self.out.push_str("\\b"),
                '\u{c}' => self.out.push_str("\\f"),
                '\\' | '"' => {
                    self.out.push('\\');
                    self.out.push(c);
                }
                // Printable ASCII is literal; the C1 range is escaped as well, because it
                // is invisible in a terminal.
                c if (' '..='~').contains(&c) => self.out.push(c),
                c if (c as u32) < 0x20 || (0x7f..=0x9f).contains(&(c as u32)) => {
                    let _ = write!(self.out, "\\u{:04x}", c as u32);
                }
                c => self.out.push(c),
            }
        }
        self.out.push('"');
        if chars.len() > MAX_STRING_LENGTH {
            let more = chars.len() - MAX_STRING_LENGTH;
            let _ = write!(
                self.out,
                "... {more} more character{}",
                if more > 1 { "s" } else { "" }
            );
        }
    }

    fn print_object(&mut self, object: &Gc<JsObject>) -> JsResult<()> {
        // A cycle is reported by the depth it was first seen at, which is what makes two
        // cycles in one value distinguishable.
        if let Some(index) = self.stack.iter().position(|seen| seen.ptr_eq(object)) {
            let _ = write!(self.out, "[circular {index}]");
            return Ok(());
        }
        if self.stack.len() >= MAX_DEPTH {
            let class = self.class_name(object);
            let _ = write!(self.out, "[{class}]");
            return Ok(());
        }
        self.stack.push(object.clone());
        let result = self.print_object_body(object);
        self.stack.pop();
        result
    }

    /// The class tag, matching `Object.prototype.toString`'s `[object X]` name so the
    /// printer and the builtin cannot disagree.
    fn class_name(&mut self, object: &Gc<JsObject>) -> String {
        if object.is_array() {
            return "Array".to_string();
        }
        if object.is_callable() {
            return "Function".to_string();
        }
        if object.typed_array_parts().is_some() {
            return "TypedArray".to_string();
        }
        if object.array_buffer_storage().is_some() {
            return "ArrayBuffer".to_string();
        }
        if let Some(is_set) = object.collection_is_set() {
            return if is_set { "Set" } else { "Map" }.to_string();
        }
        // A boxed primitive names itself after what it wraps: `Object(true)` is
        // `Boolean {  }`. `_value` is the engine's marker for exactly that.
        if let Some(boxed) = self.interp.internal_get(object, "_value") {
            let name = match boxed {
                Value::Bool(_) => "Boolean",
                Value::String(_) => "String",
                Value::Int(_) | Value::Float(_) => "Number",
                Value::Symbol(_) => "Symbol",
                _ => "Object",
            };
            return name.to_string();
        }
        match self.interp.internal_get(object, "_class") {
            Some(class) => class.to_js_string(),
            None => "Object".to_string(),
        }
    }

    fn print_object_body(&mut self, object: &Gc<JsObject>) -> JsResult<()> {
        let class = self.class_name(object);

        // A function prints as one token: `[Function name]`, `(anonymous)` when unnamed.
        if object.is_callable() {
            let name = object
                .own_property("name")
                .and_then(|property| property.value)
                .map(|value| value.to_js_string())
                .unwrap_or_default();
            self.out.push_str("[Function ");
            self.out.push_str(if name.is_empty() {
                "(anonymous)"
            } else {
                &name
            });
            self.out.push(']');
            return Ok(());
        }

        // `Map(n) { k => v }` and `Set(n) { v, ... }`.
        if let Some(is_set) = object.collection_is_set() {
            let entries = object.collection_entries().unwrap_or_default();
            let _ = write!(self.out, "{class}({}) {{ ", entries.len());
            let mut comma = Comma::First;
            let mut shown = 0usize;
            for (key, value) in &entries {
                if shown >= MAX_ITEM_COUNT {
                    break;
                }
                comma.before(&mut self.out);
                if is_set {
                    // A `Set`'s record keeps the member in the key slot and `undefined` in
                    // the value slot.
                    self.print_value(key)?;
                } else {
                    self.print_value(key)?;
                    self.out.push_str(" => ");
                    self.print_value(value)?;
                }
                shown += 1;
            }
            if shown < entries.len() {
                Self::print_more(&mut self.out, &mut comma, entries.len() - shown);
            }
            self.out.push_str(" }");
            return Ok(());
        }

        // A `RegExp` prints as its own literal, `Date` as ISO and `Error` as its name, its
        // message and its stack — the reference's `js_print_regexp`, `get_date_string` and
        // `js_print_error`. Each prints as one token, so the `{ }` wrapper is skipped.
        if let Some(data) = object.regexp() {
            let _ = write!(self.out, "/{}/{}", regexp_source(&data.source), data.flags);
            return Ok(());
        }
        if class == "Date" {
            if let Some(value) = self.interp.internal_get(object, "_date_value") {
                // An invalid Date makes `toISOString` throw in the reference, which drops it
                // to the default object form: `Date {  }`.
                if value.to_number().is_finite() {
                    let _ = write!(self.out, "{}", iso_date(value.to_number()));
                    return Ok(());
                }
            }
        }
        if class.ends_with("Error") || class == "Error" {
            self.print_error(object)?;
            return Ok(());
        }

        // `Uint8Array(3) [ 1, 2, 255 ]`: the element type names itself and the body is a
        // plain list of the values in the buffer.
        if let Some((_, element, offset, length)) = object.typed_array_parts() {
            let _ = write!(self.out, "{}({length}) [ ", element.name());
            let mut comma = Comma::First;
            let shown = length.min(MAX_ITEM_COUNT);
            // `BigInt64Array`/`BigUint64Array` print their elements as plain integers: the
            // reference formats them with `%lld`/`%llu`, not through the BigInt path, so
            // there is no `n` suffix.
            let big = matches!(
                element,
                crate::builtins::typedarray::ElementType::BigInt64
                    | crate::builtins::typedarray::ElementType::BigUint64
            );
            for index in 0..shown {
                comma.before(&mut self.out);
                match object.typed_array_element(offset + index) {
                    Some(Value::BigInt(integer)) if big => {
                        self.out.push_str(&integer.to_decimal_string());
                    }
                    Some(value) if big => self.print_value(&value)?,
                    Some(value) => self.print_value(&value)?,
                    None => self.out.push_str("undefined"),
                }
            }
            if shown < length {
                Self::print_more(&mut self.out, &mut comma, length - shown);
            }
            self.out.push_str(" ]");
            return Ok(());
        }

        if object.is_array() {
            return self.print_array(object);
        }
        self.print_plain_object(object, &class)
    }

    /// `name: message` and then the stack, without the trailing newline the reference strips.
    fn print_error(&mut self, object: &Gc<JsObject>) -> JsResult<()> {
        // `name` and `message` may be inherited (`TypeError.prototype.name`), so these are
        // full property reads rather than own-property lookups — the reference's
        // `get_prop_string` runs the same lookup, getters included.
        let read = |interp: &mut Interp<'_>, name: &str| -> String {
            let key = PropertyKey::String(crate::value::Wtf8::new(name));
            match interp.get_property_key(object, &key) {
                Ok(Some(value)) => value.to_js_string(),
                _ => String::new(),
            }
        };
        let name = read(self.interp, "name");
        self.out
            .push_str(if name.is_empty() { "Error" } else { &name });
        let message = read(self.interp, "message");
        if !message.is_empty() {
            self.out.push_str(": ");
            self.out.push_str(&message);
        }
        // The reference tests `JS_IsString(stack)`, not emptiness: an error built with no
        // JavaScript frames has an empty trace and still gets the newline, which is why a
        // module link error prints a blank line where testing emptiness printed none.
        // A script that sets `stack` to a non-string gets no newline, and `None` here is
        // that case.
        let stack = {
            let key = PropertyKey::String(crate::value::Wtf8::new("stack"));
            match self.interp.get_property_key(object, &key) {
                Ok(Some(Value::String(text))) => Some(text.to_string_lossy().to_string()),
                _ => None,
            }
        };
        if let Some(stack) = stack {
            self.out.push('\n');
            self.out.push_str(stack.trim_end_matches('\n'));
        }
        Ok(())
    }

    /// `[ v, v ]`, `<N empty items>` for a hole at the end, or `i: v` pairs when the array
    /// is sparse (a hole *before* a value), which is how the reference's fast-array and
    /// shape-property paths differ.
    fn print_array(&mut self, object: &Gc<JsObject>) -> JsResult<()> {
        let length = object.array_length().unwrap_or(0);
        let mut present: Vec<usize> = Vec::new();
        for index in 0..length {
            let key = PropertyKey::String(crate::value::Wtf8::new(index.to_string()));
            // A dense array keeps its elements in a vector, not in the property table, so
            // the property lookup alone finds nothing.
            if crate::object::own_property_including_array(object, &key).is_some() {
                present.push(index);
            }
        }
        // Dense iff the values are the first `present.len()` indices.
        let sparse = present
            .iter()
            .enumerate()
            .any(|(expected, index)| *index != expected);
        self.out.push_str("[ ");
        let mut comma = Comma::First;
        for (order, index) in present.iter().enumerate() {
            if order >= MAX_ITEM_COUNT {
                Self::print_more(&mut self.out, &mut comma, present.len() - order);
                self.out.push_str(" ]");
                return Ok(());
            }
            comma.before(&mut self.out);
            let key = PropertyKey::String(crate::value::Wtf8::new(index.to_string()));
            if sparse {
                let _ = write!(self.out, "{index}: ");
            }
            match self.interp.get_property_key(object, &key)? {
                Some(value) => self.print_value(&value)?,
                None => self.out.push_str("undefined"),
            }
        }
        if !sparse && present.len() < length {
            let holes = length - present.len();
            // `[ <3 empty items> ]`: the count takes the place of the values, and the
            // space after `[ ` is already there.
            if !present.is_empty() {
                comma.before(&mut self.out);
            }
            let _ = write!(
                self.out,
                "<{holes} empty item{}>",
                if holes > 1 { "s" } else { "" }
            );
        }
        // Non-index properties on an array still print, as `x: 4`. The reference reaches
        // the same place: its array branch runs first and the shape loop after it.
        for key in object.own_property_keys_in_definition_order() {
            let PropertyKey::String(name) = &key else {
                continue;
            };
            let text = name.to_string_lossy();
            if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            if text == "length" {
                continue;
            }
            let Some(property) = object.own_property_key(&key) else {
                continue;
            };
            if !property.enumerable {
                continue;
            }
            comma.before(&mut self.out);
            self.print_key(&key);
            self.out.push_str(": ");
            if let Some(value) = property.value.clone() {
                self.print_value(&value)?;
            }
        }
        self.out.push_str(" ]");
        Ok(())
    }

    /// `{ key: value, ... }`, with the class name in front when it is not a plain object.
    fn print_plain_object(&mut self, object: &Gc<JsObject>, class: &str) -> JsResult<()> {
        // A `Proxy` has the class *name* `Object` but is not `JS_CLASS_OBJECT`, so the
        // reference prefixes it like any other non-plain class.
        if class != "Object" || object.proxy_state().is_some() {
            let _ = write!(self.out, "{class} ");
        }
        self.out.push_str("{ ");
        // A boxed primitive prints as an empty body: its wrapped value and (for `String`)
        // its index properties are internal in the reference, so `String {  }` is right.
        let boxed = matches!(class, "Boolean" | "Number" | "String" | "Symbol")
            && self.interp.internal_get(object, "_value").is_some();
        let keys = if boxed {
            Vec::new()
        } else {
            object.own_property_keys_in_definition_order()
        };
        let mut comma = Comma::First;
        let mut shown = 0usize;
        for key in &keys {
            let Some(property) = object.own_property_key(key) else {
                continue;
            };
            if !property.enumerable {
                continue;
            }
            if shown >= MAX_ITEM_COUNT {
                Self::print_more(&mut self.out, &mut comma, keys.len() - shown);
                break;
            }
            shown += 1;
            comma.before(&mut self.out);
            self.print_key(key);
            self.out.push_str(": ");
            // An accessor prints as its kind rather than being invoked: printing must not
            // run a script's getter.
            if property.getter.is_some() || property.setter.is_some() {
                self.out.push_str(
                    match (property.getter.is_some(), property.setter.is_some()) {
                        (true, true) => "[Getter/Setter]",
                        (true, false) => "[Getter]",
                        _ => "[Setter]",
                    },
                );
            } else if let Some(value) = property.value.clone() {
                self.print_value(&value)?;
            }
        }
        self.out.push_str(" }");
        Ok(())
    }

    fn print_more(out: &mut String, comma: &mut Comma, rest: usize) {
        comma.before(out);
        let _ = write!(
            out,
            "... {rest} more item{}",
            if rest > 1 { "s" } else { "" }
        );
    }

    /// A property name: bare when it is an identifier, quoted otherwise, and
    /// `[Symbol("x")]` for a symbol key.
    fn print_key(&mut self, key: &PropertyKey) {
        match key {
            PropertyKey::Symbol(symbol) => {
                let _ = write!(
                    self.out,
                    "[Symbol(\"{}\")]",
                    symbol.description().unwrap_or_default()
                );
            }
            PropertyKey::String(text) => {
                let name = text.to_string_lossy();
                // Two ways a key goes in bare: it is an array index (the reference stores
                // those as tagged integers and writes them with `%u`), or it is already an
                // identifier. Everything else is quoted.
                let bare_index = !name.is_empty()
                    && name.bytes().all(|b| b.is_ascii_digit())
                    && (name == "0" || !name.starts_with('0'));
                if bare_index || is_identifier(&name) {
                    self.out.push_str(&name);
                } else {
                    self.print_string(&name);
                }
            }
        }
    }
}

/// Whether `name` can be written without quotes, matching `is_ascii_ident` in the reference:
/// ASCII letters, digits, `_` and `$`, not starting with a digit.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// A `RegExp` source with `/` escaped, which is what `js_print_regexp` does so the literal
/// stays readable. An empty pattern prints as `(?:)` rather than `//`, which would be a
/// comment.
fn regexp_source(source: &str) -> String {
    if source.is_empty() {
        return "(?:)".to_string();
    }
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' => out.push_str("\\/"),
            // A `\` in the pattern keeps its escape and gets one of its own.
            '\\' => {
                out.push_str("\\\\");
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// A time value as `toISOString()` writes it, including the `Invalid Date` case.
fn iso_date(milliseconds: f64) -> String {
    if !milliseconds.is_finite() {
        return "Invalid Date".to_string();
    }
    // `toISOString` covers years 0000–9999 with four digits; outside that range it throws,
    // and the reference falls back to the plain-object form. Printing the raw range here
    // keeps the value readable instead of inventing an error.
    let ms = milliseconds;
    let seconds_total = (ms / 1000.0).floor();
    let millis = (ms - seconds_total * 1000.0).round() as i64;
    let seconds = seconds_total as i64;
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
    );
    let (year, month, day) = civil_from_days(days);
    if !(0..=9999).contains(&year) {
        return format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
        );
    }
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Days since 1970-01-01 to a civil date, by Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}
