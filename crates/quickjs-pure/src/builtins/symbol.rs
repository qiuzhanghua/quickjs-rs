//! `Symbol`, and the well-known symbols the rest of the language keys off.
//!
//! A symbol is the one primitive that is a *property key* without being a string. Three
//! rules shape everything here:
//!
//! * Identity, not content. `Symbol("a") !== Symbol("a")`, so a property keyed by one of
//!   them is unreachable from the other. Only `Symbol.for` hands the same symbol out
//!   twice, and it does so by looking the string up in a registry.
//! * Invisible by default. A symbol-keyed property is skipped by `for…in`,
//!   `Object.keys` and `JSON.stringify`, and only `Object.getOwnPropertySymbols` and
//!   `Reflect.ownKeys` report it.
//! * Not a string. `ToString` of a symbol throws, which is why `${sym}` and `"" + sym`
//!   are `TypeError`s while `String(sym)` is not — `String` is the one conversion that
//!   special-cases symbols.
//!
//! The well-known symbols are created once per realm and stored on [`Realm`], so a
//! lookup like "does this object have `Symbol.iterator`" compares pointers rather than
//! passing a key around.
//!
//! [`Realm`]: crate::interp::Realm

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult, Signal};
use crate::object::Property;
use crate::value::{JsSymbol, Value};
use js_gc::Gc;

/// The well-known symbols.
///
/// The name is both the `Symbol` property name and the symbol's description: the
/// reference registers them as `Symbol.iterator` and so on, which is why
/// `Symbol.iterator.description` reads `"Symbol.iterator"` and `String` of it matches.
const WELL_KNOWN: [&str; 15] = [
    "toPrimitive",
    "iterator",
    "match",
    "matchAll",
    "replace",
    "search",
    "split",
    "toStringTag",
    "isConcatSpreadable",
    "hasInstance",
    "species",
    "unscopables",
    "asyncIterator",
    "dispose",
    "asyncDispose",
];

/// Install `Symbol` and create the realm's well-known symbols.
pub fn install(interp: &mut Interp<'_>) {
    let object_proto = interp.realm.object_proto.clone();
    let function_proto = interp.realm.function_proto.clone();

    let constructor = interp.new_native_constructor("Symbol", 0, constructor);
    // `Symbol` has no `[[Construct]]`, and `construct` reads this flag to say so.
    constructor.define("_not_a_constructor", Property::data(Value::Bool(true)));
    constructor.set_proto(Some(function_proto));

    let proto = interp.new_object(Some(object_proto));
    // The boxing path looks a primitive's prototype up on the realm, which is what makes
    // `sym.description` work on an unboxed symbol.
    interp.realm.symbol_proto = Some(proto.clone());
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(proto.clone()), false, false),
    );
    proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );

    // `Symbol.prototype` methods. `description` is an accessor so that
    // `Symbol().description` is `undefined` rather than the empty string.
    let description = interp.new_native("get description", 0, description_of);
    proto.define(
        "description",
        Property::accessor_with(Some(Value::Object(description)), None, false),
    );
    define_method(&proto, "toString", 0, to_string, interp);
    define_method(&proto, "valueOf", 0, value_of, interp);
    // `Symbol.prototype[Symbol.toPrimitive]` is what makes `String(sym)` work and
    // `${sym}` throw: it receives the hint and lets `toString` do the conversion.
    // Without it, an implicit `ToString` would reach `Object.prototype.toString` and
    // print `[object Symbol]` instead of throwing.
    let to_primitive = interp.new_native("[Symbol.toPrimitive]", 1, to_primitive);
    proto.define(
        "[Symbol.toPrimitive]",
        Property::with_attributes(Value::Object(to_primitive), false, false),
    );

    for (name, length, func) in [
        ("for", 1, symbol_for as crate::object::NativeFn),
        ("keyFor", 1, key_for),
    ] {
        define_method(&constructor, name, length, func, interp);
    }

    // The well-known symbols come after the prototype exists, because storing them on
    // the constructor needs no prototype but because the realm lookup below is shorter
    // when all thirteen exist by the time anything else installs.
    let mut well_known = Vec::new();
    for name in WELL_KNOWN {
        let symbol = symbol_value(interp, JsSymbol::new(Some(format!("Symbol.{name}"))));
        constructor.define(
            name,
            Property::with_attributes(symbol.clone(), false, false),
        );
        well_known.push(symbol);
    }
    interp.realm.set_well_known_symbols(well_known);

    let global = interp.realm.global.clone();
    global.define(
        "Symbol",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

/// Allocate a symbol value on the interpreter's heap.
pub fn symbol_value(interp: &Interp<'_>, symbol: JsSymbol) -> Value {
    Value::Symbol(Gc::new(interp.heap, symbol))
}

/// `Symbol(description)`.
///
/// Never constucts: `new Symbol()` is a `TypeError`. The description is optional and
/// converted with `ToString`, so `Symbol(1)` is `Symbol(1)`.
fn constructor(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let description = match args.first() {
        None | Some(Value::Undefined) => None,
        Some(value) => Some(interp.checked_to_string(value)?),
    };
    Ok(symbol_value(interp, JsSymbol::new(description)))
}

/// `Symbol.for(key)`: the registry-backed symbol for `key`.
///
/// The same key always answers the same symbol within a realm, and `Symbol.keyFor`
/// recovers the key. This is the only way to get two equal symbols.
fn symbol_for(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let key = match args.first() {
        None | Some(Value::Undefined) => String::new(),
        Some(value) => interp.checked_to_string(value)?,
    };
    if let Some(existing) = interp.realm.symbol_registry.get(&key) {
        return Ok(existing.clone());
    }
    let symbol = symbol_value(interp, JsSymbol::registered(key.clone()));
    interp.realm.symbol_registry.insert(key, symbol.clone());
    Ok(symbol)
}

/// `Symbol.keyFor(sym)`: the registry key, or `undefined` for a non-registered symbol.
fn key_for(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let first = arg(args, 0);
    let Some(symbol) = first.as_symbol() else {
        return Err(Signal::Throw(
            interp.make_error("TypeError", "not a symbol"),
        ));
    };
    Ok(match symbol.registry_key() {
        Some(key) => interp.string(key),
        None => Value::Undefined,
    })
}

/// The symbol `this`, for the prototype methods.
fn this_symbol(interp: &Interp<'_>, this: &Value) -> JsResult<Gc<JsSymbol>> {
    match this {
        Value::Symbol(symbol) => Ok(symbol.clone()),
        // `Object(sym)` boxes the symbol, and `valueOf`/`toString` have to reach through
        // the box — that is the whole point of the wrapper.
        Value::Object(object) => match object
            .own_property("_value")
            .and_then(|property| property.value)
        {
            Some(Value::Symbol(symbol)) => Ok(symbol),
            _ => Err(Signal::Throw(
                interp.make_error("TypeError", "not a symbol"),
            )),
        },
        _ => Err(Signal::Throw(
            interp.make_error("TypeError", "not a symbol"),
        )),
    }
}

/// `Symbol.prototype.description`.
fn description_of(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let symbol = this_symbol(interp, &this)?;
    Ok(match symbol.description() {
        Some(text) => interp.string(text),
        None => Value::Undefined,
    })
}

/// `Symbol.prototype.toString`.
fn to_string(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let symbol = this_symbol(interp, &this)?;
    Ok(interp.string(symbol.to_display_string()))
}

/// `Symbol.prototype.valueOf`, which answers the symbol itself.
fn value_of(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    // Unwrap the box, so `Object(sym).valueOf()` hands back the symbol itself — the
    // counterpart of `Symbol.prototype.toString`, which needs the same unwrapping.
    Ok(Value::Symbol(this_symbol(interp, &this)?))
}

/// `Symbol.prototype[Symbol.toPrimitive]`.
///
/// The hint decides: `"string"` converts, anything else keeps the symbol, and that is
/// exactly what makes `String(sym)` work while `"" + sym` (hint `"default"`) throws —
/// the arithmetic path rejects a symbol that comes back unchanged.
fn to_primitive(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    if let Some(Value::String(hint)) = args.first()
        && hint.as_str() == Some("string")
        && let Value::Symbol(symbol) = &this
    {
        return Ok(interp.string(symbol.to_display_string()));
    }
    Ok(this)
}
