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

use crate::builtins::{arg, define_method, install_tag};
use crate::interp::{Interp, JsResult};
use crate::object::{JsObject, Property, PropertyKey};
use crate::value::{Value, number_to_string};
use js_gc::Gc;

/// Install `JSON`.
pub fn install(interp: &mut Interp<'_>) {
    let object_proto = interp.realm.object_proto.clone();
    let json = interp.new_object(Some(object_proto));
    // §21.3: `Object.prototype.toString.call(JSON)` is `[object JSON]`.
    install_tag(&json, "JSON", interp);
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
    // The second argument restricts or rewrites what is serialised: a function is called for
    // every key, an array is the list of keys that survive. Ignoring it made
    // `JSON.stringify({a:1,b:2}, ["a"])` emit both keys.
    let replacer = Replacer::new(interp, &arg(args, 1))?;
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
    let root = make_root_holder(interp, &value)?;
    match write_value(
        interp,
        &mut Writer {
            out: &mut out,
            indent: &indent,
            depth: 0,
        },
        &root,
        &value,
        "",
        &replacer,
    )? {
        // `undefined`, a function and a symbol serialise to nothing at the top level.
        true => Ok(interp.string(out)),
        false => Ok(Value::Undefined),
    }
}

/// The `{"": value}` wrapper the spec puts around the root value.
///
/// A replacer function is called with this as `this` for the root, and `this[""]` is the
/// value — which is how a replacer that recurses through `this` gets its starting point.
fn make_root_holder(interp: &mut Interp<'_>, value: &Value) -> JsResult<Value> {
    let holder = interp.new_object(None);
    interp.set_property(&holder, "", value.clone())?;
    Ok(Value::Object(holder))
}

/// `JSON.stringify`'s `replacer` argument.
///
/// A function is called with the holder and the key; an array names the properties to keep.
/// Anything else (including `undefined`) is the identity.
enum Replacer {
    None,
    /// The function to call, and the raw value to pass as `this` (the holder).
    Function(Value),
    /// The keys to keep, in the order first seen.
    Keys(Vec<String>),
}

impl Replacer {
    fn new(interp: &mut Interp<'_>, value: &Value) -> JsResult<Replacer> {
        let Value::Object(object) = value else {
            return Ok(Replacer::None);
        };
        if object.is_callable() {
            return Ok(Replacer::Function(value.clone()));
        }
        if !object.is_array() {
            return Ok(Replacer::None);
        }
        // `SerializeJSONProperty`'s step 3: every element is coerced, and only a string or
        // number contributes a key. A repeated key keeps its first position.
        let length = object.array_length().unwrap_or(0);
        let mut keys: Vec<String> = Vec::new();
        for index in 0..length {
            let element = interp.get_property(object, &format!("{index}"))?;
            let Some(element) = element else {
                continue;
            };
            let text = match &element {
                Value::String(text) => text.to_string_lossy().to_string(),
                Value::Int(_) | Value::Float(_) => number_to_string(element.to_number()),
                _ => continue,
            };
            if !keys.contains(&text) {
                keys.push(text);
            }
        }
        Ok(Replacer::Keys(keys))
    }

    /// Whether `key` may be serialised. Only the array form filters; a function is applied
    /// to the value instead, in [`Replacer::apply`].
    fn keeps(&self, key: &str) -> bool {
        match self {
            // The root is always visited.
            Replacer::Keys(keys) => key.is_empty() || keys.iter().any(|k| k == key),
            Replacer::None | Replacer::Function(_) => true,
        }
    }

    /// `SerializeJSONProperty`'s replacer step: the function replaces the value.
    fn apply(
        &self,
        interp: &mut Interp<'_>,
        holder: &Value,
        value: &Value,
        key: &str,
    ) -> JsResult<Value> {
        let Replacer::Function(function) = self else {
            return Ok(value.clone());
        };
        interp.call_value(
            function.clone(),
            holder.clone(),
            vec![interp.string(key), value.clone()],
        )
    }
}

/// Serialise `value` into `out`. Returns whether anything was written, which is how
/// a top-level `undefined` becomes the value `undefined` rather than `"undefined"`.
///
/// `key` is the property name the value was read from, which `toJSON` receives; the
/// root value is passed the empty string, as the spec's wrapper object does.
/// The primitive inside a wrapper object, or the value unchanged.
///
/// `_value` is the slot this engine's `new Number(x)` and `Object(x)` fill in, so it is
/// the same thing the spec's internal slots carry.
fn unwrap_primitive_wrapper(interp: &Interp<'_>, value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let Some(inner) = interp.internal_get(object, "_value") else {
        return value.clone();
    };
    // Only a box of a *primitive*: an ordinary object that happens to have `_value` must
    // keep serialising as an object.
    match inner {
        Value::Int(_) | Value::Float(_) | Value::String(_) | Value::Bool(_) | Value::BigInt(_) => {
            inner
        }
        _ => value.clone(),
    }
}

/// Where the text is being written: the buffer, the indent unit and how deep the current
/// value sits. Grouped so `write_value` keeps a readable argument list.
struct Writer<'a> {
    out: &'a mut String,
    indent: &'a str,
    depth: usize,
}

fn write_value(
    interp: &mut Interp<'_>,
    writer: &mut Writer<'_>,
    holder: &Value,
    value: &Value,
    key: &str,
    replacer: &Replacer,
) -> JsResult<bool> {
    let Writer { out, indent, depth } = writer;
    let (out, indent, depth) = (out, *indent, *depth);
    // The array form filters by key before anything else looks at the value.
    if !replacer.keeps(key) {
        return Ok(false);
    }
    // A value carrying a `toJSON` method is replaced by whatever that returns, and
    // the replacement is not asked for `toJSON` again. This is how a `Date`
    // serialises to its ISO string.
    let value = &call_to_json(interp, value, key)?;
    // A replacer *function* receives the value after `toJSON` and may replace it outright.
    // `this` is the holder — the object or array the value was read from — because a
    // replacer commonly reads sibling properties through `this[key]`.
    let value = &replacer.apply(interp, holder, value, key)?;
    // A wrapper object serialises as the primitive it wraps: `JSON.stringify(Object(1))`
    // is `"1"`, not `"{}"`. SerialiseJSONProperty unwraps `[[NumberData]]`,
    // `[[StringData]]`, `[[BooleanData]]` and `[[BigIntData]]` before looking at the
    // object's enumerable keys, and a wrapper has none of its own — which is why skipping
    // the step printed an empty object.
    let value = &unwrap_primitive_wrapper(interp, value);
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
                write_array(interp, &mut Writer { out, indent, depth }, object, replacer)?;
            } else {
                write_object(interp, &mut Writer { out, indent, depth }, object, replacer)?;
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
    writer: &mut Writer<'_>,
    array: &Gc<JsObject>,
    replacer: &Replacer,
) -> JsResult<()> {
    let Writer { out, indent, depth } = writer;
    let (out, indent, depth) = (out, *indent, *depth);
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
        // `[[Get]]`, not the element vector: an accessor defined on an index lives in the
        // property table and the slot it shadows is a hole, so reading the vector directly
        // printed `null` for `Object.defineProperty(a, "0", {get})`.
        let element = match array.array_get(index) {
            Some(Value::Hole) | None => interp
                .get_property_key(array, &PropertyKey::from(index.to_string()))?
                .unwrap_or(Value::Undefined),
            Some(value) => value,
        };
        // A hole or an unserialisable element becomes `null`, because an array's
        // positions are part of its meaning.
        if !write_value(
            interp,
            &mut Writer {
                out,
                indent,
                depth: depth + 1,
            },
            &Value::Object(array.clone()),
            &element,
            &index.to_string(),
            replacer,
        )? {
            out.push_str("null");
        }
    }
    newline(out, indent, depth);
    out.push(']');
    Ok(())
}

fn write_object(
    interp: &mut Interp<'_>,
    writer: &mut Writer<'_>,
    object: &Gc<JsObject>,
    replacer: &Replacer,
) -> JsResult<()> {
    let Writer { out, indent, depth } = writer;
    let (out, indent, depth) = (out, *indent, *depth);
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
        if !write_value(
            interp,
            &mut Writer {
                out: &mut piece,
                indent,
                depth: depth + 1,
            },
            &Value::Object(object.clone()),
            &value,
            &key_text,
            replacer,
        )? {
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
    // The second argument rewrites the parsed tree bottom-up. Ignoring it made
    // `JSON.parse('{"a":1}', (k, v) => v * 2).a` answer `1`.
    let reviver = arg(args, 1);
    let is_reviver = matches!(&reviver, Value::Object(function) if function.is_callable());
    if !is_reviver {
        return Ok(value);
    }
    // `InternalizeJSONProperty` starts from `{"": value}`, so the root is visited last and
    // may itself be replaced.
    let root = make_root_holder(interp, &value)?;
    let Value::Object(root) = &root else {
        return Ok(value);
    };
    internalize_json_property(interp, root, "", &reviver)
}

/// `CreateDataProperty`, which differs from `Set` in exactly the case a reviver hits: an
/// existing non-configurable property keeps its value instead of being overwritten, and the
/// refusal is not an error.
fn create_data_property(
    interp: &mut Interp<'_>,
    object: &Gc<JsObject>,
    key: &str,
    value: Value,
) -> JsResult<()> {
    let key = crate::object::PropertyKey::from(key);
    // A property that already exists and cannot be redefined keeps its old value: the
    // definition is refused rather than applied. `Object.defineProperty` would report that
    // refusal through its return value, but here a reviver that froze a property while
    // visiting an earlier key must simply see it stay frozen.
    if interp
        .object_get_own_property(object, &key)?
        .is_some_and(|existing| !existing.configurable)
    {
        return Ok(());
    }
    let property = crate::object::Property {
        value: Some(value),
        writable: true,
        enumerable: true,
        configurable: true,
        ..crate::object::Property::default()
    };
    interp.object_define_own_property(object, &key, property)?;
    Ok(())
}

/// `InternalizeJSONProperty`: call the reviver on every property, depth first, and drop the
/// ones it answers `undefined` for.
fn internalize_json_property(
    interp: &mut Interp<'_>,
    holder: &Gc<JsObject>,
    key: &str,
    reviver: &Value,
) -> JsResult<Value> {
    let Some(value) = interp.get_property(holder, key)? else {
        return Ok(Value::Undefined);
    };
    // Arrays are walked by index and objects by their enumerable own keys; a primitive has
    // nothing to walk, so it goes straight to the call.
    if let Value::Object(object) = &value {
        if object.is_array() {
            let length = object.array_length().unwrap_or(0);
            for index in 0..length {
                let element_key = index.to_string();
                let replaced = internalize_json_property(interp, object, &element_key, reviver)?;
                if matches!(replaced, Value::Undefined) {
                    interp.delete_property(object, &element_key)?;
                } else {
                    // `CreateDataProperty`, not `Set`: when the property already exists and
                    // refuses redefinition the new value is dropped. A reviver that froze a
                    // property while visiting an earlier key must not have it overwritten by
                    // a later branch.
                    create_data_property(interp, object, &element_key, replaced)?;
                }
            }
        } else {
            for name in interp.object_own_property_keys(object)? {
                let Some(name) = name.as_str() else {
                    continue;
                };
                let name = name.to_string();
                let replaced = internalize_json_property(interp, object, &name, reviver)?;
                if matches!(replaced, Value::Undefined) {
                    interp.delete_property(object, &name)?;
                } else {
                    create_data_property(interp, object, &name, replaced)?;
                }
            }
        }
    }
    interp.call_value(
        reviver.clone(),
        Value::Object(holder.clone()),
        vec![interp.string(key), value],
    )
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
                // A raw control character is not allowed in a JSON string: it has to be
                // escaped. Accepting one made `JSON.parse('"a<TAB>b"')` succeed where the
                // grammar requires a `SyntaxError`.
                0x00..=0x1F => {
                    return self.error("control character in string");
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
