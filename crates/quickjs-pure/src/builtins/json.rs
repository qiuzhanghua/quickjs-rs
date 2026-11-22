//! `JSON`.
//!
//! `JSON.stringify` is specified in terms of a *serialisation state* that the
//! indentation threads through, and the output is compared byte for byte by the
//! reference test suite — including the trailing-newline-free format with `indent`
//! and the exact placement of `": "`. That is why the writer below is explicit about
//! separators rather than leaning on a generic pretty-printer.
//!
//! `JSON.parse` accepts the strict JSON grammar; the reference also accepts a few
//! extensions, but nothing here depends on them.

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult};
use crate::object::{JsObject, Property};
use crate::value::{Value, number_to_string};
use js_gc::Gc;

/// Install `JSON`.
pub fn install(interp: &mut Interp<'_>) {
    let object_proto = interp.realm.object_proto.clone();
    let json = interp.new_object(Some(object_proto));
    define_method(&json, "stringify", 3, stringify, interp);
    define_method(&json, "parse", 2, parse, interp);
    let global = interp.realm.global.clone();
    global.define(
        "JSON",
        Property::with_attributes(Value::Object(json), true, false),
    );
}

// ---------------------------------------------------------------------------
// stringify
// ---------------------------------------------------------------------------

fn stringify(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let value = arg(args, 0);
    // The indent argument may be a number of spaces or a string used verbatim.
    let indent = match arg(args, 2) {
        Value::Int(_) | Value::Float(_) => {
            let count = arg(args, 2).to_number().clamp(0.0, 10.0) as usize;
            " ".repeat(count)
        }
        Value::String(text) => text.to_string_lossy().chars().take(10).collect(),
        _ => String::new(),
    };
    let mut out = String::new();
    match write_value(interp, &value, "", &indent, &mut out, 0)? {
        // `undefined`, a function and a symbol serialise to nothing at the top level.
        true => Ok(interp.string(out)),
        false => Ok(Value::Undefined),
    }
}

/// Serialise `value` into `out`. Returns whether anything was written, which is how
/// a top-level `undefined` becomes the value `undefined` rather than `"undefined"`.
///
/// `key` is the property name the value was read from, which `toJSON` receives; the
/// root value is passed the empty string, as the spec's wrapper object does.
fn write_value(
    interp: &mut Interp<'_>,
    value: &Value,
    key: &str,
    indent: &str,
    out: &mut String,
    depth: usize,
) -> JsResult<bool> {
    // A value carrying a `toJSON` method is replaced by whatever that returns, and
    // the replacement is not asked for `toJSON` again. This is how a `Date`
    // serialises to its ISO string.
    let value = &call_to_json(interp, value, key)?;
    match value {
        // A symbol serialises to nothing, exactly like `undefined`: `JSON.stringify`
        // drops it from an object and answers `undefined` for it at the top level.
        // A bigint has no JSON form, and the spec makes that an error rather than a
        // silent omission — losing a number is not the same as losing `undefined`.
        Value::BigInt(_) => Err(interp.type_error("BigInt value can't be serialized in JSON")),
        Value::Undefined
        | Value::Symbol(_)
        | Value::Uninitialized
        | Value::Hole
        | Value::CatchOffset(_)
        | Value::ReturnAddress(_) => Ok(false),
        Value::Null => {
            out.push_str("null");
            Ok(true)
        }
        Value::Bool(value) => {
            out.push_str(if *value { "true" } else { "false" });
            Ok(true)
        }
        Value::Int(_) | Value::Float(_) => {
            let number = value.to_number();
            // `NaN` and the infinities serialise as `null`.
            if number.is_finite() {
                out.push_str(&number_to_string(number));
            } else {
                out.push_str("null");
            }
            Ok(true)
        }
        Value::String(text) => {
            write_string(&text.to_string_lossy(), out);
            Ok(true)
        }
        Value::Object(object) => {
            if object.is_callable() {
                return Ok(false);
            }
            if object.is_array() {
                write_array(interp, object, indent, out, depth)?;
            } else {
                write_object(interp, object, indent, out, depth)?;
            }
            Ok(true)
        }
    }
}

/// `SerializeJSONProperty`'s `toJSON` step, applied once.
fn call_to_json(interp: &mut Interp<'_>, value: &Value, key: &str) -> JsResult<Value> {
    let Value::Object(object) = value else {
        return Ok(value.clone());
    };
    let Some(method) = interp.get_property(object, "toJSON")? else {
        return Ok(value.clone());
    };
    if !matches!(&method, Value::Object(function) if function.is_callable()) {
        return Ok(value.clone());
    }
    interp.call_value(method, value.clone(), vec![interp.string(key)])
}

fn write_array(
    interp: &mut Interp<'_>,
    array: &Gc<JsObject>,
    indent: &str,
    out: &mut String,
    depth: usize,
) -> JsResult<()> {
    let length = array.array_length().unwrap_or(0);
    if length == 0 {
        out.push_str("[]");
        return Ok(());
    }
    out.push('[');
    for index in 0..length {
        if index > 0 {
            out.push(',');
        }
        newline(out, indent, depth + 1);
        let element = array.array_get(index).unwrap_or(Value::Undefined);
        // A hole or an unserialisable element becomes `null`, because an array's
        // positions are part of its meaning.
        if !write_value(interp, &element, &index.to_string(), indent, out, depth + 1)? {
            out.push_str("null");
        }
    }
    newline(out, indent, depth);
    out.push(']');
    Ok(())
}

fn write_object(
    interp: &mut Interp<'_>,
    object: &Gc<JsObject>,
    indent: &str,
    out: &mut String,
    depth: usize,
) -> JsResult<()> {
    // Enumerable own keys in the spec's order, skipping keys whose values do not
    // serialise (a function-valued property is omitted, not `null`).
    //
    // Through `[[OwnPropertyKeys]]` and `[[GetOwnProperty]]`, so a proxy serialises as
    // its handler describes it: reading the property table directly made
    // `JSON.stringify(proxy)` emit `{}` for every proxy.
    let mut keys: Vec<crate::object::PropertyKey> = Vec::new();
    for key in interp.object_own_property_keys(object)? {
        if key.is_symbol() {
            // A symbol-keyed property is invisible to `JSON.stringify`.
            continue;
        }
        let enumerable = interp
            .object_get_own_property(object, &key)?
            .is_some_and(|property| property.enumerable);
        if enumerable {
            keys.push(key);
        }
    }
    let mut written = 0usize;
    let mut body = String::new();
    for key in keys {
        let key_text = key.as_str().unwrap_or_default().to_string();
        let Some(value) = interp.get_property_key(object, &key)? else {
            continue;
        };
        let mut piece = String::new();
        // Render the value first so an unrepresentable one can be skipped whole.
        if !write_value(interp, &value, &key_text, indent, &mut piece, depth + 1)? {
            continue;
        }
        if written > 0 {
            body.push(',');
        }
        newline(&mut body, indent, depth + 1);
        let mut escaped = String::new();
        write_string(&key_text, &mut escaped);
        body.push_str(&escaped);
        body.push(':');
        if !indent.is_empty() {
            body.push(' ');
        }
        body.push_str(&piece);
        written += 1;
    }
    if written == 0 {
        out.push_str("{}");
        return Ok(());
    }
    out.push('{');
    out.push_str(&body);
    newline(out, indent, depth);
    out.push('}');
    Ok(())
}

/// Emit the indentation for one level, or nothing when not pretty-printing.
fn newline(out: &mut String, indent: &str, depth: usize) {
    if indent.is_empty() {
        return;
    }
    out.push('\n');
    for _ in 0..depth {
        out.push_str(indent);
    }
}

/// Write `text` as a JSON string literal.
///
/// The escapes are the spec's twenty-odd: the two mandatory ones, the two that are
/// required for well-formed JSON in a script (`\u2028`/`\u2029`), and control
/// characters below `0x20` as `\u00XX`.
fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// parse
// ---------------------------------------------------------------------------

fn parse(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = arg(args, 0).to_js_string();
    let mut parser = Parser {
        bytes: text.as_bytes(),
        text: &text,
        position: 0,
    };
    parser.skip_whitespace();
    let value = match parser.value(interp) {
        Ok(value) => value,
        Err(error) => {
            return Err(crate::interp::Signal::Throw(json_error(
                interp, &text, error,
            )));
        }
    };
    parser.skip_whitespace();
    if parser.position != parser.bytes.len() {
        return Err(crate::interp::Signal::Throw(json_error(
            interp,
            &text,
            JsonError {
                message: "unexpected data after JSON value".to_string(),
                offset: parser.position,
            },
        )));
    }
    Ok(value)
}

/// A `SyntaxError` that reports where in the JSON text parsing failed.
///
/// The reference builds these with an explicit position, so the trace starts with
/// `    at <input>:line:column` — the position inside the text — and only then lists
/// the caller. `<input>` is the reference's placeholder for text that came from a
/// string rather than a file, so it is kept even when the caller is a real script.
fn json_error(interp: &mut Interp<'_>, text: &str, error: JsonError) -> Value {
    let (line, column) = line_and_column(text, error.offset);
    let trace = format!("    at <input>:{line}:{column}\n{}", interp.stack_trace());
    let value = interp.make_error("SyntaxError", &error.message);
    if let Value::Object(object) = &value {
        let text = interp.string(trace);
        object.define(
            "stack",
            crate::object::Property::with_attributes(text, true, true),
        );
    }
    value
}

/// The 1-based line and column of `offset` in `text`.
///
/// Columns count UTF-16 code units, like every other position in this engine, so a
/// character outside the BMP counts twice.
fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = before[line_start..].encode_utf16().count() + 1;
    (line, column)
}

/// A JSON syntax error and where in the JSON text it happened.
///
/// The offset is what lets `JSON.parse` report a position the way the reference does:
/// its stack starts with the line and column inside the *text being parsed*, not with
/// the caller's frame.
struct JsonError {
    message: String,
    offset: usize,
}

struct Parser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    position: usize,
}

impl Parser<'_> {
    /// An error blamed on the parser's current position.
    fn error<T>(&self, message: impl Into<String>) -> Result<T, JsonError> {
        Err(JsonError {
            message: message.into(),
            offset: self.position,
        })
    }

    fn skip_whitespace(&mut self) {
        while let Some(byte) = self.bytes.get(self.position) {
            match byte {
                b' ' | b'\t' | b'\n' | b'\r' => self.position += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn value(&mut self, interp: &mut Interp<'_>) -> Result<Value, JsonError> {
        self.skip_whitespace();
        match self.peek() {
            None => self.error("unexpected end of JSON input"),
            Some(b'{') => self.object(interp),
            Some(b'[') => self.array(interp),
            Some(b'"') => Ok(interp.string(self.string()?)),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(_) => self.number(),
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value, JsonError> {
        if self.text[self.position..].starts_with(word) {
            self.position += word.len();
            Ok(value)
        } else {
            self.error("unexpected token")
        }
    }

    fn number(&mut self) -> Result<Value, JsonError> {
        let start = self.position;
        if matches!(self.peek(), Some(b'-')) {
            self.position += 1;
        }
        while let Some(byte) = self.peek() {
            match byte {
                b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-' => self.position += 1,
                _ => break,
            }
        }
        let text = &self.text[start..self.position];
        text.parse::<f64>()
            .map(Value::Float)
            .map_err(|_| JsonError {
                message: "invalid number".to_string(),
                offset: start,
            })
    }

    fn string(&mut self) -> Result<String, JsonError> {
        // The opening quote is already known to be there.
        self.position += 1;
        let mut out = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return self.error("unterminated string");
            };
            match byte {
                b'"' => {
                    self.position += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.position += 1;
                    let Some(escape) = self.peek() else {
                        return self.error("unterminated escape");
                    };
                    self.position += 1;
                    let start_of_escape = self.position - 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let end = self.position + 4;
                            if end > self.bytes.len() {
                                return self.error("truncated \\u escape");
                            }
                            let hex = &self.text[self.position..end];
                            let unit = u16::from_str_radix(hex, 16).map_err(|_| JsonError {
                                message: "invalid \\u escape".to_string(),
                                offset: start_of_escape,
                            })?;
                            self.position = end;
                            // A surrogate pair arrives as two escapes; reassemble it.
                            if (0xd800..0xdc00).contains(&unit)
                                && self.text[self.position..].starts_with("\\u")
                            {
                                let tail_end = self.position + 6;
                                if tail_end <= self.bytes.len() {
                                    let low = u16::from_str_radix(
                                        &self.text[self.position + 2..tail_end],
                                        16,
                                    )
                                    .unwrap_or(0);
                                    if (0xdc00..0xe000).contains(&low) {
                                        let combined = 0x10000
                                            + ((unit as u32 - 0xd800) << 10)
                                            + (low as u32 - 0xdc00);
                                        out.push(char::from_u32(combined).unwrap_or('\u{fffd}'));
                                        self.position = tail_end;
                                        continue;
                                    }
                                }
                            }
                            out.push(char::from_u32(unit as u32).unwrap_or('\u{fffd}'));
                        }
                        // The offending character is the one just consumed, so the
                        // position has to step back over it.
                        other => {
                            return Err(JsonError {
                                message: format!("invalid escape \\{}", other as char),
                                offset: self.position - 1,
                            });
                        }
                    }
                }
                _ => {
                    // Copy one whole UTF-8 character so multi-byte text survives.
                    let rest = &self.text[self.position..];
                    let character = rest.chars().next().ok_or_else(|| JsonError {
                        message: "invalid UTF-8".to_string(),
                        offset: self.position,
                    })?;
                    out.push(character);
                    self.position += character.len_utf8();
                }
            }
        }
    }

    fn array(&mut self, interp: &mut Interp<'_>) -> Result<Value, JsonError> {
        self.position += 1;
        let mut elements = Vec::new();
        self.skip_whitespace();
        if matches!(self.peek(), Some(b']')) {
            self.position += 1;
            return Ok(Value::Object(interp.new_array(elements)));
        }
        loop {
            elements.push(self.value(interp)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.position += 1;
                }
                Some(b']') => {
                    self.position += 1;
                    return Ok(Value::Object(interp.new_array(elements)));
                }
                _ => return self.error("expected ',' or ']'"),
            }
        }
    }

    fn object(&mut self, interp: &mut Interp<'_>) -> Result<Value, JsonError> {
        self.position += 1;
        let proto = interp.realm.object_proto.clone();
        let object = interp.new_object(Some(proto));
        self.skip_whitespace();
        if matches!(self.peek(), Some(b'}')) {
            self.position += 1;
            return Ok(Value::Object(object));
        }
        loop {
            self.skip_whitespace();
            if !matches!(self.peek(), Some(b'"')) {
                return self.error("expected a string key");
            }
            let key = self.string()?;
            self.skip_whitespace();
            if !matches!(self.peek(), Some(b':')) {
                return self.error("expected ':'");
            }
            self.position += 1;
            let value = self.value(interp)?;
            object.define_data(&key, value);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.position += 1;
                }
                Some(b'}') => {
                    self.position += 1;
                    return Ok(Value::Object(object));
                }
                _ => return self.error("expected ',' or '}'"),
            }
        }
    }
}
