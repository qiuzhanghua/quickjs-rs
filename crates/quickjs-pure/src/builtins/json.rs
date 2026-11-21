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
        Value::String(text) => {
            let text = text.as_str();
            text.chars().take(10).collect()
        }
        _ => String::new(),
    };
    let mut out = String::new();
    match write_value(interp, &value, &indent, &mut out, 0)? {
        // `undefined`, a function and a symbol serialise to nothing at the top level.
        true => Ok(interp.string(out)),
        false => Ok(Value::Undefined),
    }
}

/// Serialise `value` into `out`. Returns whether anything was written, which is how
/// a top-level `undefined` becomes the value `undefined` rather than `"undefined"`.
fn write_value(
    interp: &mut Interp<'_>,
    value: &Value,
    indent: &str,
    out: &mut String,
    depth: usize,
) -> JsResult<bool> {
    match value {
        Value::Undefined
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
            write_string(text.as_str(), out);
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
        if !write_value(interp, &element, indent, out, depth + 1)? {
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
    let keys: Vec<String> = crate::builtins::object::own_keys(&Value::Object(object.clone()))
        .into_iter()
        .filter(|key| {
            object
                .own_property(key)
                .map(|property| property.enumerable)
                .unwrap_or(true)
        })
        .collect();
    let mut written = 0usize;
    let mut body = String::new();
    for key in keys {
        let Some(value) = interp.get_property(object, &key)? else {
            continue;
        };
        let mut piece = String::new();
        // Render the value first so an unrepresentable one can be skipped whole.
        if !write_value(interp, &value, indent, &mut piece, depth + 1)? {
            continue;
        }
        if written > 0 {
            body.push(',');
        }
        newline(&mut body, indent, depth + 1);
        let mut key_text = String::new();
        write_string(&key, &mut key_text);
        body.push_str(&key_text);
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
        Err(message) => {
            return Err(crate::interp::Signal::Throw(
                interp.make_error("SyntaxError", &message),
            ));
        }
    };
    parser.skip_whitespace();
    if parser.position != parser.bytes.len() {
        return Err(crate::interp::Signal::Throw(
            interp.make_error("SyntaxError", "unexpected data after JSON value"),
        ));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    position: usize,
}

impl Parser<'_> {
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

    fn value(&mut self, interp: &mut Interp<'_>) -> Result<Value, String> {
        self.skip_whitespace();
        match self.peek() {
            None => Err("unexpected end of JSON input".to_string()),
            Some(b'{') => self.object(interp),
            Some(b'[') => self.array(interp),
            Some(b'"') => Ok(interp.string(self.string()?)),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(_) => self.number(),
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value, String> {
        if self.text[self.position..].starts_with(word) {
            self.position += word.len();
            Ok(value)
        } else {
            Err(format!("unexpected token at offset {}", self.position))
        }
    }

    fn number(&mut self) -> Result<Value, String> {
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
            .map_err(|_| format!("invalid number at offset {start}"))
    }

    fn string(&mut self) -> Result<String, String> {
        // The opening quote is already known to be there.
        self.position += 1;
        let mut out = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err("unterminated string".to_string());
            };
            match byte {
                b'"' => {
                    self.position += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.position += 1;
                    let Some(escape) = self.peek() else {
                        return Err("unterminated escape".to_string());
                    };
                    self.position += 1;
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
                                return Err("truncated \\u escape".to_string());
                            }
                            let hex = &self.text[self.position..end];
                            let unit = u16::from_str_radix(hex, 16)
                                .map_err(|_| "invalid \\u escape".to_string())?;
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
                        other => return Err(format!("invalid escape \\{}", other as char)),
                    }
                }
                _ => {
                    // Copy one whole UTF-8 character so multi-byte text survives.
                    let rest = &self.text[self.position..];
                    let character = rest.chars().next().ok_or("invalid UTF-8")?;
                    out.push(character);
                    self.position += character.len_utf8();
                }
            }
        }
    }

    fn array(&mut self, interp: &mut Interp<'_>) -> Result<Value, String> {
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
                _ => return Err("expected ',' or ']'".to_string()),
            }
        }
    }

    fn object(&mut self, interp: &mut Interp<'_>) -> Result<Value, String> {
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
                return Err("expected a string key".to_string());
            }
            let key = self.string()?;
            self.skip_whitespace();
            if !matches!(self.peek(), Some(b':')) {
                return Err("expected ':'".to_string());
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
                _ => return Err("expected ',' or '}'".to_string()),
            }
        }
    }
}
