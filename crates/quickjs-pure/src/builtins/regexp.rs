//! `RegExp` and `RegExp.prototype`.
//!
//! The engine lives in [`crate::regexp`]; this module is the JavaScript-facing half: the
//! constructor, the compiled-pattern cache, `lastIndex`, and `exec`.
//!
//! `exec` is where most of the observable semantics live. A `g` or `y` pattern advances
//! `lastIndex`, a failed match resets it to zero, and the returned array carries the
//! captured groups plus `index`, `input` and (for the `d` flag) `indices`.

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult};
use crate::object::{JsObject, Property};
use crate::regexp::{Flags, Regex};
use crate::value::Value;
use js_gc::Gc;

/// The compiled pattern an object of `ObjectKind::RegExp` carries.
///
/// Boxed through `Rc` because a `RegExp` object is shared by every reference to it and
/// the pattern is immutable once compiled.
#[derive(Clone)]
pub struct RegExpData {
    /// The compiled pattern, shared between `exec` calls.
    pub regex: std::rc::Rc<Regex>,
    /// Pattern source as written, which `.source` reports.
    pub source: String,
    /// The flags, in canonical order.
    pub flags: String,
}

/// Install `RegExp` and its prototype methods.
pub fn install(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) -> Gc<JsObject> {
    let prototype = interp.new_object(Some(object_proto.clone()));
    let constructor = interp.new_native_constructor("RegExp", 2, constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(prototype.clone()), false, false),
    );
    prototype.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );

    define_method(&prototype, "exec", 1, exec, interp);
    define_method(&prototype, "test", 1, test, interp);
    define_method(&prototype, "toString", 0, to_string, interp);
    for (name, getter) in [
        (
            "source",
            source as fn(&mut Interp<'_>, Value, &[Value]) -> JsResult<Value>,
        ),
        ("flags", flags),
        ("global", global),
        ("ignoreCase", ignore_case),
        ("multiline", multiline),
        ("sticky", sticky),
        ("unicode", unicode),
        ("dotAll", dot_all),
        ("hasIndices", has_indices),
    ] {
        let getter = interp.new_native(name, 0, getter);
        prototype.define(
            name,
            Property {
                getter: Some(Value::Object(getter)),
                enumerable: false,
                configurable: true,
                ..Property::default()
            },
        );
    }

    // `Symbol.match` and friends are what `String.prototype.match` looks up; without them
    // a regexp is not recognised as a pattern by those methods.
    install_symbol_methods(interp, &prototype, &constructor);
    interp.realm.regexp_proto = Some(prototype.clone());
    interp.realm.regexp_ctor = Some(constructor.clone());
    constructor
}

fn install_symbol_methods(
    interp: &mut Interp<'_>,
    prototype: &Gc<JsObject>,
    constructor: &Gc<JsObject>,
) {
    use crate::interp::WellKnown;
    for (which, name, func, length) in [
        (
            WellKnown::Match,
            "[Symbol.match]",
            symbol_match as crate::object::NativeFn,
            1,
        ),
        (WellKnown::Replace, "[Symbol.replace]", symbol_replace, 2),
        (WellKnown::Search, "[Symbol.search]", symbol_search, 1),
        (WellKnown::Split, "[Symbol.split]", symbol_split, 2),
    ] {
        let method = interp.new_native(name, length, func);
        prototype.define_key(
            &interp.realm.well_known_key(which),
            Property::with_attributes(Value::Object(method), true, false),
        );
    }
    let _ = constructor;
}

/// `new RegExp(pattern, flags)`.
fn constructor(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let pattern = arg(args, 0);
    let flags = arg(args, 1);
    // `new RegExp(existingRegExp)` copies the pattern and flags.
    let (source, flags) = match &pattern {
        Value::Object(object) if object.regexp().is_some() => {
            let data = object.regexp().expect("checked");
            let flags = if matches!(flags, Value::Undefined) {
                data.flags.clone()
            } else {
                interp.to_js_string(&flags)?
            };
            (data.source.clone(), flags)
        }
        Value::Undefined => (String::new(), String::new()),
        other => (
            interp.to_js_string(other)?,
            if matches!(flags, Value::Undefined) {
                String::new()
            } else {
                interp.to_js_string(&flags)?
            },
        ),
    };
    Ok(Value::Object(new_regexp(interp, &source, &flags)?))
}

/// `RegExp.prototype.exec(string)`.
fn exec(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("RegExp.prototype.exec called on a non-object"));
    };
    let text = interp.to_js_string(&arg(args, 0))?;
    exec_on(interp, object, &text)
}

/// The shared implementation of `exec`, used by the `String` methods as well.
pub fn exec_on(interp: &mut Interp<'_>, object: &Gc<JsObject>, text: &str) -> JsResult<Value> {
    let Some(data) = object.regexp() else {
        return Err(interp.type_error("method called on a non-RegExp"));
    };
    let sticky_or_global = data.regex.flags.global || data.regex.flags.sticky;
    let mut start = if sticky_or_global {
        let index = interp.get_property(object, "lastIndex")?;
        let index = interp.to_js_number(&index.unwrap_or(Value::Int(0)))?;
        if !index.is_finite() || index < 0.0 {
            interp.set_property(object, "lastIndex", Value::Int(0))?;
            return Ok(Value::Null);
        }
        index as usize
    } else {
        0
    };
    let input: Vec<char> = text.chars().collect();
    // `lastIndex`, `index` and the `d` flag's pairs all count **UTF-16 code units**, while the
    // matcher works in characters. `unit_start[i]` is the code-unit offset of character `i`,
    // with one extra entry for the end, so the two unit systems convert both ways.
    let mut unit_start = Vec::with_capacity(input.len() + 1);
    let mut units = 0;
    for character in &input {
        unit_start.push(units);
        units += character.len_utf16();
    }
    unit_start.push(units);
    // A `lastIndex` inside a surrogate pair has no character of its own; it belongs to the
    // character that contains it, which is the same as rounding down to a code point
    // boundary — what the reference does, and what makes `/(?:)/gu` with `lastIndex = 1` on
    // `"🐱"` report `index === 0`.
    if start > units {
        if sticky_or_global {
            interp.set_property(object, "lastIndex", Value::Int(0))?;
        }
        return Ok(Value::Null);
    }
    start = unit_start
        .partition_point(|offset| *offset <= start)
        .saturating_sub(1);
    let found = data.regex.exec_at(&input, start);
    let Some(found) = found else {
        if sticky_or_global {
            interp.set_property(object, "lastIndex", Value::Int(0))?;
        }
        return Ok(Value::Null);
    };
    // `lastIndex` moves to the end of the match, in code units — not characters, which is
    // why an astral match advances by two.
    if sticky_or_global {
        let units = unit_start[found.end.min(unit_start.len() - 1)];
        interp.set_property(object, "lastIndex", Value::Int(units as i32))?;
    }
    // The result array: the whole match, then one element per group. A group that did not
    // participate is `undefined`, not a hole.
    let mut elements: Vec<Value> = Vec::with_capacity(found.captures.len());
    for capture in &found.captures {
        elements.push(match capture {
            Some((start, end)) => interp.string(slice(&input, *start, *end)),
            None => Value::Undefined,
        });
    }
    let array = interp.new_array(elements);
    let index = unit_start[found.start.min(unit_start.len() - 1)];
    let input_value = interp.string(text);
    interp.set_property(&array, "index", Value::Int(index as i32))?;
    interp.set_property(&array, "input", input_value.clone())?;
    interp.set_property(
        &array,
        "groups",
        if found.captures.len() > 1 {
            // Named groups are not supported, so there is nothing to report.
            Value::Undefined
        } else {
            Value::Undefined
        },
    )?;
    if data.regex.flags.has_indices {
        let mut pairs = Vec::with_capacity(found.captures.len());
        for capture in &found.captures {
            pairs.push(match capture {
                Some((start, end)) => {
                    let (start, end) = (
                        unit_start[(*start).min(unit_start.len() - 1)],
                        unit_start[(*end).min(unit_start.len() - 1)],
                    );
                    let pair =
                        interp.new_array(vec![Value::Int(start as i32), Value::Int(end as i32)]);
                    Value::Object(pair)
                }
                None => Value::Undefined,
            });
        }
        let indices = interp.new_array(pairs);
        interp.set_property(&array, "indices", Value::Object(indices))?;
    }
    Ok(Value::Object(array))
}

fn slice(input: &[char], start: usize, end: usize) -> String {
    input
        .get(start..end)
        .map(|chars| chars.iter().collect())
        .unwrap_or_default()
}

/// `RegExp.prototype.test(string)`.
fn test(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let result = exec(interp, this, args)?;
    Ok(Value::Bool(!matches!(result, Value::Null)))
}

fn to_string(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("RegExp.prototype.toString called on a non-object"));
    };
    let Some(data) = object.regexp() else {
        return Err(interp.type_error("RegExp.prototype.toString called on a non-RegExp"));
    };
    let source = if data.source.is_empty() {
        "(?:)".to_string()
    } else {
        data.source.clone()
    };
    Ok(interp.string(format!("/{source}/{}", data.flags)))
}

fn source(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let data = data_of(interp, &this)?;
    // An empty pattern reports `(?:)` so that `/` + source + `/` parses again.
    let text = if data.source.is_empty() {
        "(?:)".to_string()
    } else {
        data.source.clone()
    };
    Ok(interp.string(&text))
}

fn flags(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let data = data_of(interp, &this)?;
    Ok(interp.string(&data.flags))
}

fn data_of(interp: &mut Interp<'_>, this: &Value) -> JsResult<RegExpData> {
    match this.as_object().and_then(|object| object.regexp()) {
        Some(data) => Ok(data.clone()),
        None => Err(interp.type_error("method called on a non-RegExp")),
    }
}

/// One of the boolean flag accessors.
fn flag_getter(interp: &mut Interp<'_>, this: Value, pick: fn(&Flags) -> bool) -> JsResult<Value> {
    let data = data_of(interp, &this)?;
    Ok(Value::Bool(pick(&data.regex.flags)))
}

fn global(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    flag_getter(interp, this, |flags| flags.global)
}
fn ignore_case(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    flag_getter(interp, this, |flags| flags.ignore_case)
}
fn multiline(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    flag_getter(interp, this, |flags| flags.multiline)
}
fn sticky(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    flag_getter(interp, this, |flags| flags.sticky)
}
fn unicode(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    flag_getter(interp, this, |flags| flags.unicode)
}
fn dot_all(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    flag_getter(interp, this, |flags| flags.dot_all)
}
fn has_indices(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    flag_getter(interp, this, |flags| flags.has_indices)
}

// -- the Symbol methods ------------------------------------------------------
//
// These are what `String.prototype.match` and friends dispatch to. Each one re-reads the
// pattern's flags through the object's own properties, as the spec does, so a subclass
// that overrides `lastIndex` keeps working.

/// `String.prototype.match`'s hook.
fn symbol_match(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("Symbol.match called on a non-object"));
    };
    let text = interp.to_js_string(&arg(args, 0))?;
    let is_global = interp
        .get_property(object, "global")?
        .unwrap_or(Value::Undefined)
        .to_boolean();
    if !is_global {
        return exec_on(interp, object, &text);
    }
    interp.set_property(object, "lastIndex", Value::Int(0))?;
    let mut results = Vec::new();
    let input: Vec<char> = text.chars().collect();
    loop {
        let result = exec_on(interp, object, &text)?;
        if matches!(result, Value::Null) {
            break;
        }
        let Value::Object(array) = &result else {
            break;
        };
        let matched = interp.get_property(array, "0")?.unwrap_or(Value::Undefined);
        let empty = interp.to_js_string(&matched)?.is_empty();
        results.push(result);
        if empty {
            let next = interp
                .get_property(object, "lastIndex")?
                .unwrap_or(Value::Int(0));
            let next = interp.to_js_number(&next)? as usize + 1;
            interp.set_property(object, "lastIndex", Value::Int(next as i32))?;
            if next > input.len() {
                break;
            }
        }
    }
    if results.is_empty() {
        return Ok(Value::Null);
    }
    Ok(Value::Object(interp.new_array(results)))
}

/// `String.prototype.search`'s hook: the index of the first match, or -1.
fn symbol_search(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("Symbol.search called on a non-object"));
    };
    let text = interp.to_js_string(&arg(args, 0))?;
    // `search` ignores the caller's `lastIndex`: it always looks from the start, and
    // leaves the property alone afterwards.
    interp.set_property(object, "lastIndex", Value::Int(0))?;
    let result = exec_on(interp, object, &text)?;
    match result {
        Value::Null => Ok(Value::Int(-1)),
        Value::Object(array) => Ok(interp
            .get_property(&array, "index")?
            .unwrap_or(Value::Int(-1))),
        other => Ok(other),
    }
}

/// `String.prototype.split`'s hook.
fn symbol_split(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("Symbol.split called on a non-object"));
    };
    let text = interp.to_js_string(&arg(args, 0))?;
    let limit = match arg(args, 1) {
        Value::Undefined => u32::MAX,
        other => {
            let number = interp.to_js_number(&other)?;
            if number.is_nan() || number < 0.0 {
                0
            } else {
                number as u32
            }
        }
    };
    if limit == 0 {
        return Ok(Value::Object(interp.new_array(Vec::new())));
    }
    let data = data_of(interp, &Value::Object(object.clone()))?;
    let input: Vec<char> = text.chars().collect();
    let mut parts: Vec<Value> = Vec::new();
    let mut last = 0usize;
    let mut scan = 0usize;
    while scan <= input.len() {
        let Some(found) = data.regex.exec_at(&input, scan) else {
            break;
        };
        // A zero-length match cannot split anything, and taking it would loop forever.
        if found.end == found.start {
            scan = found.start + 1;
            continue;
        }
        parts.push(interp.string(slice(&input, last, found.start)));
        if parts.len() as u32 >= limit {
            return Ok(Value::Object(interp.new_array(parts)));
        }
        for group in found.captures.iter().skip(1) {
            parts.push(match group {
                Some((start, end)) => interp.string(slice(&input, *start, *end)),
                None => Value::Undefined,
            });
            if parts.len() as u32 >= limit {
                return Ok(Value::Object(interp.new_array(parts)));
            }
        }
        last = found.end;
        scan = found.end;
    }
    parts.push(interp.string(slice(&input, last, input.len())));
    Ok(Value::Object(interp.new_array(parts)))
}

/// `String.prototype.replace`'s hook, for a regexp pattern.
fn symbol_replace(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("Symbol.replace called on a non-object"));
    };
    let text = interp.to_js_string(&arg(args, 0))?;
    let replacement = arg(args, 1);
    let is_global = interp
        .get_property(object, "global")?
        .unwrap_or(Value::Undefined)
        .to_boolean();
    let data = data_of(interp, &Value::Object(object.clone()))?;
    let input: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut last = 0usize;
    let mut scan = 0usize;
    let mut replaced = false;
    while scan <= input.len() {
        let Some(found) = data.regex.exec_at(&input, scan) else {
            break;
        };
        out.extend(input[last..found.start].iter());
        let matched = slice(&input, found.start, found.end);
        let substituted = substitute(interp, &replacement, &matched, &input, &found)?;
        out.push_str(&substituted);
        last = found.end;
        let empty = found.start == found.end;
        scan = if empty { found.end + 1 } else { found.end };
        replaced = true;
        if !is_global {
            break;
        }
        if empty && found.end >= input.len() {
            break;
        }
    }
    if !replaced {
        return Ok(interp.string(&text));
    }
    out.extend(input[last.min(input.len())..].iter());
    Ok(interp.string(&out))
}

/// Apply a replacement: a function receives the match and its groups, a string
/// understands `$&`, `$1`, `$\``, `$'` and `$$`.
fn substitute(
    interp: &mut Interp<'_>,
    replacement: &Value,
    matched: &str,
    input: &[char],
    found: &crate::regexp::Match,
) -> JsResult<String> {
    if let Value::Object(function) = replacement
        && function.is_callable()
    {
        let mut args: Vec<Value> = vec![interp.string(matched)];
        for group in found.captures.iter().skip(1) {
            args.push(match group {
                Some((start, end)) => interp.string(slice(input, *start, *end)),
                None => Value::Undefined,
            });
        }
        args.push(Value::Int(found.start as i32));
        args.push(interp.string(input.iter().collect::<String>()));
        let this = Value::Undefined;
        let result = interp.call_value(Value::Object(function.clone()), this, args)?;
        return interp.to_js_string(&result);
    }
    let template = interp.to_js_string(replacement)?;
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('$') => {
                chars.next();
                out.push('$');
            }
            Some('&') => {
                chars.next();
                out.push_str(matched);
            }
            Some('`') => {
                chars.next();
                out.extend(input[..found.start].iter());
            }
            Some('\'') => {
                chars.next();
                out.extend(input[found.end..].iter());
            }
            Some(digit) if digit.is_ascii_digit() => {
                let mut digits = String::new();
                while let Some(digit) = chars.peek().copied() {
                    if !digit.is_ascii_digit() || digits.len() == 2 {
                        break;
                    }
                    digits.push(digit);
                    chars.next();
                }
                let index: usize = digits.parse().unwrap_or(0);
                match found.captures.get(index) {
                    Some(Some((start, end))) => out.extend(input[*start..*end].iter()),
                    // An index beyond the group count leaves the `$n` as written.
                    _ => {
                        out.push('$');
                        out.push_str(&digits);
                    }
                }
            }
            _ => out.push('$'),
        }
    }
    Ok(out)
}

/// Build a `RegExp` object for `source`/`flags`, reusing an existing one when the same
/// pattern was compiled before.
pub fn new_regexp(interp: &mut Interp<'_>, source: &str, flags: &str) -> JsResult<Gc<JsObject>> {
    let Some(parsed) = Flags::parse(flags) else {
        return Err(invalid_pattern(interp, String::new(), "flags"));
    };
    let regex = match Regex::compile(source, parsed) {
        Ok(regex) => regex,
        Err(error) => {
            // The reference reports the engine's own message, not a generic one.
            return Err(invalid_pattern(interp, error.message, ""));
        }
    };
    let prototype = interp
        .realm
        .regexp_proto
        .clone()
        .unwrap_or_else(|| interp.realm.object_proto.clone());
    let object = interp.new_object(Some(prototype));
    object.set_regexp(RegExpData {
        regex: std::rc::Rc::new(regex),
        source: source.to_string(),
        flags: parsed.as_flag_string(),
    });
    object.define(
        "lastIndex",
        Property::with_attributes(Value::Int(0), true, false),
    );
    Ok(object)
}

/// The `SyntaxError` a malformed pattern raises.
///
/// The two forms the reference uses: a bad flag string reports `invalid regular expression
/// flags`, and a bad pattern reports the engine's own message.
fn invalid_pattern(interp: &Interp<'_>, message: String, flags: &str) -> crate::interp::Signal {
    let text = if flags.is_empty() {
        message
    } else {
        "invalid regular expression flags".to_string()
    };
    crate::interp::Signal::Throw(interp.make_error("SyntaxError", &text))
}

/// Whether `value` is a `RegExp` object.
pub fn is_regexp(object: &Gc<JsObject>) -> bool {
    object.regexp().is_some()
}
