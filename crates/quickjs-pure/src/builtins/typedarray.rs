//! `ArrayBuffer` and the typed-array views.
//!
//! # A view, not a copy
//!
//! A typed array is a *window* onto an `ArrayBuffer`'s bytes: `new Float32Array(buffer, 8, 1)`
//! shares storage with `new Uint8Array(buffer)`, so a write through one is visible through
//! the other. That is the whole point of the type — it is how a script reinterprets a byte
//! range as numbers — and it is why the bytes live in the buffer object rather than in each
//! view. The views hold a `Gc<JsObject>` of their buffer rather than an `Rc` of its storage,
//! so the collector can see the reference and the buffer cannot be freed while a view lives.
//!
//! # Coercion on write
//!
//! Every store runs `ToNumber` and then the type's own conversion. The interesting cases are
//! two:
//!
//! - an integer type *wraps*, so `new Int8Array(1)[0] = 255` reads back `-1`;
//! - `Uint8ClampedArray` *clamps* instead, so `-100` becomes `0` and `1233.5` becomes `255`,
//!   and it rounds to nearest-even rather than truncating (`0.5` becomes `0`, `1.5` becomes
//!   `2`).
//!
//! # `Float16`
//!
//! Rust has no half-precision type, so the encoding is done by hand. It only has to be exact
//! enough to round-trip the values a script stores; the code below is the standard
//! round-to-nearest-even conversion with subnormals, infinities and `NaN` handled.
//!
//! # Detached buffers
//!
//! `ArrayBuffer.prototype.transfer()` hands the bytes to a new buffer and leaves this one
//! detached. A view of a detached buffer reads `undefined` from every index and reports a
//! length of `0`, which is what the reference does; the storage stays behind the `Rc` so all
//! the views see the transition at once.

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult};
use crate::object::{BufferStorage, JsObject, Property};
use crate::value::Value;
use js_gc::Gc;
use std::cell::RefCell;
use std::rc::Rc;

/// Which element type a view reads and writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementType {
    /// `Int8Array`
    Int8,
    /// `Uint8Array`
    Uint8,
    /// `Uint8ClampedArray`
    Uint8Clamped,
    /// `Int16Array`
    Int16,
    /// `Uint16Array`
    Uint16,
    /// `Int32Array`
    Int32,
    /// `Uint32Array`
    Uint32,
    /// `Float16Array`
    Float16,
    /// `Float32Array`
    Float32,
    /// `Float64Array`
    Float64,
    /// `BigInt64Array`
    BigInt64,
    /// `BigUint64Array`
    BigUint64,
}

/// The view types, in the order their prototypes are installed.
pub const ELEMENT_TYPES: [(ElementType, &str); 12] = [
    (ElementType::Int8, "Int8Array"),
    (ElementType::Uint8, "Uint8Array"),
    (ElementType::Uint8Clamped, "Uint8ClampedArray"),
    (ElementType::Int16, "Int16Array"),
    (ElementType::Uint16, "Uint16Array"),
    (ElementType::Int32, "Int32Array"),
    (ElementType::Uint32, "Uint32Array"),
    (ElementType::Float16, "Float16Array"),
    (ElementType::Float32, "Float32Array"),
    (ElementType::Float64, "Float64Array"),
    (ElementType::BigInt64, "BigInt64Array"),
    (ElementType::BigUint64, "BigUint64Array"),
];

impl ElementType {
    /// Bytes per element.
    pub fn size(self) -> usize {
        match self {
            ElementType::Int8 | ElementType::Uint8 | ElementType::Uint8Clamped => 1,
            ElementType::Int16 | ElementType::Uint16 | ElementType::Float16 => 2,
            ElementType::Int32 | ElementType::Uint32 | ElementType::Float32 => 4,
            ElementType::Float64 | ElementType::BigInt64 | ElementType::BigUint64 => 8,
        }
    }

    /// Whether elements are `BigInt`s rather than numbers.
    ///
    /// The two behave differently in ways that reach most methods: the sort order, what a
    /// literal element converts to, and whether a Number is a `TypeError`.
    pub fn is_bigint(self) -> bool {
        matches!(self, ElementType::BigInt64 | ElementType::BigUint64)
    }

    /// The name, as `Symbol.toStringTag` and the class tag report it.
    pub fn name(self) -> &'static str {
        ELEMENT_TYPES
            .into_iter()
            .find(|(kind, _)| *kind == self)
            .map(|(_, name)| name)
            .unwrap_or("TypedArray")
    }

    /// Read the element at `offset`.
    ///
    /// A short buffer — only reachable if a view was built over a truncated one — reads as
    /// `0` rather than panicking, because a script must not be able to make the engine abort.
    pub fn read(self, heap: &js_gc::Heap, bytes: &[u8], offset: usize) -> Value {
        let slice = bytes.get(offset..offset + self.size());
        let Some(slice) = slice else {
            // A short buffer reads as the type's zero, which for a BigInt type is `0n`.
            return match self {
                ElementType::BigInt64 | ElementType::BigUint64 => {
                    Value::BigInt(js_gc::Gc::new(heap, crate::bigint::BigInt::zero()))
                }
                _ => Value::Int(0),
            };
        };
        match self {
            ElementType::Int8 => Value::Int(slice[0] as i8 as i32),
            ElementType::Uint8 | ElementType::Uint8Clamped => Value::Int(slice[0] as i32),
            ElementType::Int16 => Value::Int(i16::from_le_bytes([slice[0], slice[1]]) as i32),
            ElementType::Uint16 => Value::Int(u16::from_le_bytes([slice[0], slice[1]]) as i32),
            ElementType::Int32 => {
                Value::Int(i32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
            }
            // An unsigned 32-bit value does not fit `i32`, so it becomes a float. The
            // reference keeps it a number too, and the value is exact in an `f64`.
            ElementType::Uint32 => {
                Value::Float(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]) as f64)
            }
            ElementType::Float16 => {
                Value::Float(f16_to_f64(u16::from_le_bytes([slice[0], slice[1]])))
            }
            ElementType::Float32 => {
                Value::Float(f32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]) as f64)
            }
            ElementType::Float64 => Value::Float(f64::from_le_bytes([
                slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
            ])),
            // A BigInt element reads back as a `BigInt`, never as a number: `typeof
            // new BigInt64Array(1)[0]` is `"bigint"` and `0n`, not `0`.
            ElementType::BigInt64 => Value::BigInt(js_gc::Gc::new(
                heap,
                crate::bigint::BigInt::from_i64(i64::from_le_bytes([
                    slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
                ])),
            )),
            ElementType::BigUint64 => Value::BigInt(js_gc::Gc::new(
                heap,
                crate::bigint::BigInt::from_u64(u64::from_le_bytes([
                    slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
                ])),
            )),
        }
    }

    /// Store `value` into the element at `offset`.
    /// The `Err` carries the reason, which the caller turns into a `TypeError`.
    pub fn write(self, bytes: &mut [u8], offset: usize, value: Value) -> Result<(), &'static str> {
        let size = self.size();
        if offset + size > bytes.len() {
            return Ok(());
        }
        // `ToNumber` refuses a Symbol and a BigInt; `Value::to_number` answers `NaN` so
        // that it stays total, which here would store a wrong element instead of throwing.
        if !self.is_bigint() && matches!(value, Value::Symbol(_) | Value::BigInt(_)) {
            return Err(NOT_A_NUMBER);
        }
        // A BigInt element type runs `ToBigInt`, not `ToNumber`: a Number is a TypeError,
        // and a BigInt is stored modulo `2^64`. That is why this can fail at all.
        if matches!(self, ElementType::BigInt64 | ElementType::BigUint64) {
            let Value::BigInt(big) = &value else {
                return Err(NOT_A_BIGINT);
            };
            let slice = &mut bytes[offset..offset + size];
            let bits = match self {
                ElementType::BigInt64 => big.to_i64_wrapping() as u64,
                _ => big.to_u64_wrapping(),
            };
            slice.copy_from_slice(&bits.to_le_bytes());
            return Ok(());
        }
        let number = value.to_number();
        let slice = &mut bytes[offset..offset + size];
        match self {
            ElementType::Int8 => slice[0] = to_integer(number) as i8 as u8,
            ElementType::Uint8 => slice[0] = to_integer(number) as u8,
            ElementType::Uint8Clamped => slice[0] = clamp_to_uint8(number),
            ElementType::Int16 => slice.copy_from_slice(&(to_integer(number) as i16).to_le_bytes()),
            ElementType::Uint16 => {
                slice.copy_from_slice(&(to_integer(number) as u16).to_le_bytes())
            }
            ElementType::Int32 => slice.copy_from_slice(&(to_integer(number) as i32).to_le_bytes()),
            ElementType::Uint32 => {
                slice.copy_from_slice(&(to_integer(number) as u32).to_le_bytes())
            }
            ElementType::Float16 => slice.copy_from_slice(&f64_to_f16(number).to_le_bytes()),
            ElementType::Float32 => slice.copy_from_slice(&(number as f32).to_le_bytes()),
            ElementType::Float64 => slice.copy_from_slice(&number.to_le_bytes()),
            // Handled above; unreachable, but the match has to be exhaustive.
            ElementType::BigInt64 | ElementType::BigUint64 => {}
        }
        Ok(())
    }
}

/// Why an element write was refused: the target is a BigInt type and the value is not one.
pub const NOT_A_BIGINT: &str = "cannot convert a Number to a BigInt";

/// Why an element write was refused: the target is numeric and the value has no number form.
pub const NOT_A_NUMBER: &str = "cannot convert a Symbol or BigInt to a number";

/// `ToIntegerOrInfinity`, truncated toward zero, as the integer element types need it.
///
/// A cast through `i64` would be undefined for out-of-range floats in Rust (`as` saturates
/// since 1.45, which is the behaviour wanted here anyway), so the wrap happens at the target
/// width — `as i8` and friends keep the low bits, which is exactly the spec's modulo.
fn to_integer(number: f64) -> i64 {
    if number.is_nan() {
        return 0;
    }
    if number.is_infinite() {
        // The modulo of an infinity is 0 in every engine; the alternative is to wrap the
        // saturated value, which no script expects.
        return 0;
    }
    number.trunc() as i64
}

/// `ToUint8Clamp`: round half to even, then clamp into `0..=255`.
fn clamp_to_uint8(number: f64) -> u8 {
    if number.is_nan() || number <= 0.0 {
        return 0;
    }
    if number >= 255.0 {
        return 255;
    }
    // `round_ties_even` is what the spec asks for; `0.5` must land on `0`, not `1`.
    let rounded = number.round_ties_even();
    rounded as u8
}

/// Decode an IEEE-754 binary16 value.
fn f16_to_f64(bits: u16) -> f64 {
    let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exponent = ((bits >> 10) & 0x1f) as i32;
    let mantissa = (bits & 0x3ff) as f64;
    if exponent == 0 {
        // Zero or subnormal: no implicit leading bit, and the scale is the smallest one.
        sign * mantissa * 2f64.powi(-24)
    } else if exponent == 0x1f {
        if mantissa == 0.0 {
            sign * f64::INFINITY
        } else {
            f64::NAN
        }
    } else {
        sign * (1.0 + mantissa / 1024.0) * 2f64.powi(exponent - 15)
    }
}

/// Encode a value as IEEE-754 binary16, rounding to nearest even.
fn f64_to_f16(value: f64) -> u16 {
    if value.is_nan() {
        return 0x7e00;
    }
    let sign = if value.is_sign_negative() {
        0x8000u16
    } else {
        0
    };
    let magnitude = value.abs();
    if magnitude == 0.0 {
        return sign;
    }
    if magnitude >= 65504.0 {
        return sign | 0x7c00; // infinity
    }
    if magnitude < 6.103_515_625e-5 {
        // Subnormal: the mantissa counts in units of 2^-24, rounded to nearest even.
        let scaled = magnitude / 2f64.powi(-24);
        let mantissa = scaled.round_ties_even();
        return sign | (mantissa as u16);
    }
    // Normal: 1.m × 2^(e-15), so the exponent field is e + 15 and the mantissa keeps 10 bits.
    let exponent = magnitude.log2().floor() as i32;
    let biased = exponent + 15;
    let mantissa = (magnitude / 2f64.powi(exponent) - 1.0) * 1024.0;
    let rounded = mantissa.round_ties_even();
    if rounded >= 1024.0 {
        // Rounding carried into the exponent.
        let biased = biased + 1;
        if biased >= 0x1f {
            return sign | 0x7c00;
        }
        return sign | ((biased as u16) << 10);
    }
    sign | ((biased as u16) << 10) | (rounded as u16)
}

/// Install `ArrayBuffer` and every typed-array view.
pub fn install(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    install_array_buffer(interp, object_proto);
    install_views(interp, object_proto);
    install_data_view(interp, object_proto);
}

fn install_array_buffer(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    let proto = interp.new_object(Some(object_proto.clone()));
    let constructor = interp.new_native_constructor("ArrayBuffer", 1, buffer_constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(proto.clone()), false, false),
    );
    proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    define_method(&proto, "transfer", 0, buffer_transfer, interp);
    define_method(&proto, "slice", 2, buffer_slice, interp);
    // `byteLength` is an accessor so a detached buffer reports `0` instead of a stale size.
    let getter = interp.new_native("get byteLength", 0, buffer_byte_length);
    proto.define(
        "byteLength",
        Property {
            getter: Some(Value::Object(getter)),
            enumerable: false,
            configurable: true,
            ..Property::default()
        },
    );
    install_tag(&proto, "ArrayBuffer", interp);
    interp.realm.array_buffer_proto = Some(proto);
    let global = interp.realm.global.clone();
    global.define(
        "ArrayBuffer",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

fn install_views(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    // One shared prototype carries every method and every accessor; each element type gets a
    // prototype of its own on top of it, which is what lets `BYTES_PER_ELEMENT` and the
    // string tag differ per type while the behaviour is written once.
    let shared = interp.new_object(Some(object_proto.clone()));
    define_method(&shared, "set", 1, set_from, interp);
    define_method(&shared, "fill", 1, fill, interp);
    define_method(&shared, "join", 1, join, interp);
    define_method(&shared, "toString", 0, join, interp);
    define_method(&shared, "toLocaleString", 0, to_locale_string, interp);
    define_method(&shared, "slice", 2, slice, interp);
    define_method(&shared, "subarray", 2, subarray, interp);
    // The whole `%TypedArray%.prototype` iteration family, which was absent: every
    // `[].map.call(typedArray, f)` reached `undefined is not a function`.
    define_method(&shared, "at", 1, at as crate::object::NativeFn, interp);
    define_method(&shared, "every", 1, every, interp);
    define_method(&shared, "some", 1, some, interp);
    define_method(&shared, "forEach", 1, for_each, interp);
    define_method(&shared, "map", 1, map, interp);
    define_method(&shared, "filter", 1, filter, interp);
    define_method(&shared, "reduce", 1, reduce, interp);
    define_method(&shared, "reduceRight", 1, reduce_right, interp);
    define_method(&shared, "find", 1, find, interp);
    define_method(&shared, "findIndex", 1, find_index, interp);
    define_method(&shared, "findLast", 1, find_last, interp);
    define_method(&shared, "findLastIndex", 1, find_last_index, interp);
    define_method(&shared, "indexOf", 1, index_of, interp);
    define_method(&shared, "lastIndexOf", 1, last_index_of, interp);
    define_method(&shared, "includes", 1, includes, interp);
    define_method(&shared, "reverse", 0, reverse, interp);
    define_method(&shared, "sort", 1, sort, interp);
    define_method(&shared, "copyWithin", 2, copy_within, interp);
    define_method(&shared, "with", 2, with, interp);
    define_method(&shared, "toReversed", 0, to_reversed, interp);
    define_method(&shared, "toSorted", 1, to_sorted, interp);
    define_method(&shared, "values", 0, values, interp);
    define_method(&shared, "keys", 0, keys, interp);
    define_method(&shared, "entries", 0, entries, interp);
    // `%TypedArray%.prototype[Symbol.iterator]` *is* `values`, not a wrapper around it.
    if let Some(values_fn) = shared.own_property("values").and_then(|p| p.value) {
        let key = interp
            .realm
            .well_known_key(crate::interp::WellKnown::Iterator);
        shared.define_key(&key, Property::with_attributes(values_fn, true, false));
    }
    for (name, getter) in [
        ("buffer", buffer_of as crate::object::NativeFn),
        ("byteOffset", byte_offset_of),
        ("byteLength", byte_length_of),
        ("length", length_of),
        ("[Symbol.toStringTag]", tag_of),
    ] {
        let function = interp.new_native(name, 0, getter);
        let key = if name == "[Symbol.toStringTag]" {
            interp
                .realm
                .well_known_key(crate::interp::WellKnown::ToStringTag)
        } else {
            crate::object::PropertyKey::from(name)
        };
        shared.define_key(
            &key,
            Property {
                getter: Some(Value::Object(function)),
                enumerable: false,
                configurable: true,
                ..Property::default()
            },
        );
    }
    interp.realm.typed_array_proto = Some(shared.clone());
    // Built after the shared prototype exists, because `%TypedArray%.prototype` *is* that
    // object and `constructor` has to point back at the intrinsic.
    let intrinsic = install_typed_array_intrinsic(interp, &shared);

    let global = interp.realm.global.clone();
    for (element, name) in ELEMENT_TYPES {
        let proto = interp.new_object(Some(shared.clone()));
        // The constructor reads the element type off this, so `new` is the only way in.
        proto.define(
            "_element",
            Property::with_attributes(Value::Int(element as i32), false, false),
        );
        proto.define(
            "BYTES_PER_ELEMENT",
            Property::with_attributes(Value::Int(element.size() as i32), false, false),
        );
        let constructor = interp.new_native_ctor(name, 3, view_constructor);
        // `Object.getPrototypeOf(Int8Array)` is `%TypedArray%`, which is where `from` and
        // `of` live; without it the statics were unreachable from every constructor.
        constructor.set_proto(Some(intrinsic.clone()));
        constructor.define(
            "_element",
            Property::with_attributes(Value::Int(element as i32), false, false),
        );
        constructor.define(
            "BYTES_PER_ELEMENT",
            Property::with_attributes(Value::Int(element.size() as i32), false, false),
        );
        constructor.define(
            "prototype",
            Property::with_attributes(Value::Object(proto.clone()), false, false),
        );
        proto.define(
            "constructor",
            Property::with_attributes(Value::Object(constructor.clone()), true, false),
        );
        // Remembered so a method that answers with a fresh view can give it the right
        // prototype; `new_view` looks the type up in this vector.
        interp.realm.typed_array_protos.push(proto.clone());
        global.define(
            name,
            Property::with_attributes(Value::Object(constructor), true, false),
        );
    }
}

/// Give `proto` a `Symbol.toStringTag` answering `tag`.
///
/// The tag has to be baked into the function: a native one is a bare pointer and cannot
/// capture. This used to ignore its `tag` argument and always answer `"ArrayBuffer"`, so
/// `Object.prototype.toString.call(new DataView(...))` reported the wrong type.
fn install_tag(proto: &Gc<JsObject>, tag: &str, interp: &Interp<'_>) {
    // A *data* property: `{value, writable: false, enumerable: false, configurable: true}`.
    // The tag is a constant, and an accessor here is observable — the conformance check
    // reads `writable` off the descriptor and a getter has none.
    proto.define_key(
        &interp
            .realm
            .well_known_key(crate::interp::WellKnown::ToStringTag),
        Property {
            value: Some(interp.string(tag)),
            writable: false,
            enumerable: false,
            configurable: true,
            ..Property::default()
        },
    );
}

/// `ArrayBuffer.prototype[Symbol.toStringTag]`.
/// `new ArrayBuffer(length)`.
fn buffer_constructor(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let length = arg(args, 0).to_number();
    if length < 0.0 || length.fract() != 0.0 || length > 4294967295.0 {
        return Err(interp.range_error("invalid array buffer length"));
    }
    let storage = Rc::new(RefCell::new(BufferStorage {
        bytes: vec![0; length as usize],
        detached: false,
    }));
    let default_proto = interp
        .realm
        .array_buffer_proto
        .clone()
        .unwrap_or_else(|| interp.realm.object_proto.clone());
    let object = interp.new_ctor_instance(default_proto)?;
    object.set_array_buffer(storage);
    Ok(Value::Object(object))
}

/// The storage of `this`, which must be an `ArrayBuffer`.
fn buffer_storage_of(interp: &Interp<'_>, this: &Value) -> JsResult<Rc<RefCell<BufferStorage>>> {
    this.as_object()
        .and_then(|object| object.array_buffer_storage())
        .ok_or_else(|| interp.type_error("not an ArrayBuffer"))
}

/// `ArrayBuffer.prototype.byteLength`.
fn buffer_byte_length(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let storage = buffer_storage_of(interp, &this)?;
    let storage = storage.borrow();
    Ok(Value::Int(if storage.detached {
        0
    } else {
        storage.bytes.len() as i32
    }))
}

/// `Buffer.prototype.transfer()`: hand the bytes to a new buffer and detach this one.
fn buffer_transfer(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let storage = buffer_storage_of(interp, &this)?;
    let bytes = {
        let mut storage = storage.borrow_mut();
        if storage.detached {
            return Err(interp.type_error("ArrayBuffer has already been detached"));
        }
        storage.detached = true;
        std::mem::take(&mut storage.bytes)
    };
    let fresh_storage = Rc::new(RefCell::new(BufferStorage {
        bytes,
        detached: false,
    }));
    let proto = interp
        .realm
        .array_buffer_proto
        .clone()
        .unwrap_or_else(|| interp.realm.object_proto.clone());
    let object = interp.new_object(Some(proto));
    object.set_array_buffer(fresh_storage);
    Ok(Value::Object(object))
}

/// `buffer.slice(begin, end)`: a copy, which is what keeps the two independent.
fn buffer_slice(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let storage = buffer_storage_of(interp, &this)?;
    let storage = storage.borrow();
    if storage.detached {
        return Err(interp.type_error("ArrayBuffer has already been detached"));
    }
    let length = storage.bytes.len() as i64;
    let begin = clamp_index(arg(args, 0).to_number(), length, 0);
    let end = clamp_index(arg(args, 1).to_number(), length, length);
    let bytes = storage.bytes[begin as usize..end.max(begin) as usize].to_vec();
    drop(storage);
    let proto = interp
        .realm
        .array_buffer_proto
        .clone()
        .unwrap_or_else(|| interp.realm.object_proto.clone());
    let object = interp.new_object(Some(proto));
    object.set_array_buffer(Rc::new(RefCell::new(BufferStorage {
        bytes,
        detached: false,
    })));
    Ok(Value::Object(object))
}

/// Resolve a `begin`/`end` argument the way every slice does: relative to the end when
/// negative, then clamped into range.
fn clamp_index(value: f64, length: i64, default: i64) -> i64 {
    if value.is_nan() {
        return default.min(length).max(0);
    }
    let value = if value.is_infinite() {
        if value > 0.0 { length } else { 0 }
    } else {
        value.trunc() as i64
    };
    (if value < 0 { length + value } else { value }).clamp(0, length)
}

/// `ToIndex`: `undefined` and `NaN` are `0`, a negative or infinite value is a
/// `RangeError`, and so is a fraction rather than being truncated.
///
/// This is what makes `new Uint8Array(buffer)` mean "from byte 0, to the end": the missing
/// second argument is `undefined`, which is `0`, and a `NaN` offset must not be a failure.
/// `ToIntegerOrInfinity(value)`, which refuses a Symbol and a BigInt.
///
/// `Value::to_number` answers `NaN` for both so that it stays total; that made
/// `copyWithin(0, Symbol())` a silent no-op instead of the `TypeError` the spec requires.
fn to_integer_or_infinity(interp: &mut Interp<'_>, value: &Value) -> JsResult<f64> {
    // `to_js_number` already refuses a Symbol and a BigInt, boxed ones included.
    let number = interp.to_js_number(value)?;
    Ok(if number.is_nan() { 0.0 } else { number.trunc() })
}

/// `ToIndex(value)`: `ToIntegerOrInfinity` and then a range check.
///
/// `ToIntegerOrInfinity` *truncates*, so `new Int32Array(2.7)` is two elements rather than
/// a `RangeError`; only a negative or infinite result is refused. `ToNumber` on a BigInt is
/// a `TypeError`, which is why the conversion goes through the interpreter.
fn to_index(interp: &mut Interp<'_>, value: &Value, what: &str) -> JsResult<usize> {
    // Through `ToIntegerOrInfinity`, so a Symbol or BigInt is a `TypeError` rather than the
    // `NaN` that `Value::to_number` answers to stay total.
    let number = to_integer_or_infinity(interp, value)?;
    // `2^53 - 1` is the largest index the spec allows.
    if number < 0.0 || number > 9_007_199_254_740_991.0 {
        return Err(interp.range_error(format!("invalid {what}")));
    }
    Ok(number as usize)
}

/// The element type a typed-array *constructor* stands for, read off its `_element`.
fn element_of_constructor(interp: &Interp<'_>, callee: &Gc<JsObject>) -> Option<ElementType> {
    let raw = callee
        .own_property("_element")
        .and_then(|property| property.value)?;
    let raw = raw.to_number() as i32;
    let found = ELEMENT_TYPES
        .into_iter()
        .map(|(element, _)| element)
        .find(|element| *element as i32 == raw);
    let _ = interp;
    found
}

/// `new Uint8Array(...)`: from a length, from a buffer, or from an array-like.
fn view_constructor(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    // A typed-array constructor cannot be called as a function: there is no receiver to
    // take an element type from, and the spec makes that a `TypeError`.
    // The element type comes from the *callee*, not from a receiver: a native constructor is
    // handed `this = undefined` and builds its own instance once its arguments are valid.
    let _ = &this;
    let callee = interp
        .current_native()
        .ok_or_else(|| interp.type_error("constructor requires 'new'"))?;
    let element = element_of_constructor(interp, &callee)
        .ok_or_else(|| interp.type_error("constructor requires 'new'"))?;
    if !interp.is_constructing() {
        return Err(interp.type_error("constructor requires 'new'"));
    }
    let first = arg(args, 0);
    let (buffer, offset, length) = match &first {
        // `new X(buffer, byteOffset, length)`: a window onto existing bytes.
        Value::Object(object) if object.array_buffer_storage().is_some() => {
            let storage = object
                .array_buffer_storage()
                .expect("checked by the guard above");
            let total = storage.borrow().bytes.len();
            let byte_offset = to_index(interp, &arg(args, 1), "byte offset")?;
            if byte_offset % element.size() != 0 {
                return Err(interp.range_error("byte offset is not aligned"));
            }
            if byte_offset > total {
                return Err(interp.range_error("byte offset is out of range"));
            }
            let available = (total - byte_offset) / element.size();
            // No third argument means "the rest of the buffer", which is different from
            // `undefined` meaning 0 — hence a length check rather than a nullish one.
            let length = if args.len() < 3 || matches!(arg(args, 2), Value::Undefined) {
                available
            } else {
                let requested = to_index(interp, &arg(args, 2), "length")?;
                if requested > available {
                    return Err(interp.range_error("invalid length"));
                }
                requested
            };
            (object.clone(), byte_offset, length)
        }
        // `new X(length)` for any *primitive* argument: the spec goes to `ToIndex` before
        // it ever looks for `Symbol.iterator`, so `new Int32Array("3")` is three zeroes
        // rather than the three characters. Only an object is treated as a source.
        Value::Int(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Bool(_)
        | Value::BigInt(_)
        | Value::Null
        | Value::Undefined => {
            let count = to_index(interp, &first, "typed array length")?;
            let proto = interp
                .realm
                .array_buffer_proto
                .clone()
                .unwrap_or_else(|| interp.realm.object_proto.clone());
            let buffer = interp.new_object(Some(proto));
            buffer.set_array_buffer(Rc::new(RefCell::new(BufferStorage {
                bytes: vec![0; count * element.size()],
                detached: false,
            })));
            (buffer, 0, count)
        }
        // `new X(iterable)`: a buffer sized by the source, filled from it.
        _ => {
            // `Symbol.iterator` wins over `length`: an object that is both iterable and
            // array-like is read through the iterator. That is what the harness's
            // `makeIterable` — a plain object carrying only `Symbol.iterator` — needs, and
            // without it every such source constructed an *empty* view.
            let values = if interp.has_iterator(&first)? {
                interp.collect_iterable(&first)?
            } else {
                interp.array_like_to_args(&first)?
            };
            let proto = interp
                .realm
                .array_buffer_proto
                .clone()
                .unwrap_or_else(|| interp.realm.object_proto.clone());
            let buffer = interp.new_object(Some(proto));
            let storage = Rc::new(RefCell::new(BufferStorage {
                bytes: vec![0; values.len() * element.size()],
                detached: false,
            }));
            buffer.set_array_buffer(storage);
            let count = values.len();
            for (index, value) in values.into_iter().enumerate() {
                // `new BigInt64Array([1])` is a TypeError: the initialiser runs
                // `ToBigInt` on each value, and a Number is not convertible.
                if write_element(&buffer, element, 0, index, value).is_err() {
                    return Err(interp.type_error("cannot convert a Number to a BigInt"));
                }
            }
            (buffer, 0, count)
        }
    };
    let default_proto = interp
        .realm
        .typed_array_protos
        .get(element as usize)
        .cloned()
        .or_else(|| interp.realm.typed_array_proto.clone())
        .unwrap_or_else(|| interp.realm.object_proto.clone());
    let object = interp.new_ctor_instance(default_proto)?;
    object.set_typed_array(buffer, element, offset, length);
    Ok(Value::Object(object))
}

/// Write one element through a buffer, for the paths that do not have a view in hand.
fn write_element(
    buffer: &Gc<JsObject>,
    element: ElementType,
    offset: usize,
    index: usize,
    value: Value,
) -> Result<(), &'static str> {
    let Some(storage) = buffer.array_buffer_storage() else {
        return Ok(());
    };
    let mut storage = storage.borrow_mut();
    if storage.detached {
        return Ok(());
    }
    element.write(&mut storage.bytes, offset + index * element.size(), value)
}

/// Read the elements of a typed array, which `join`/`set`/`slice` all need.
fn elements_of(interp: &Interp<'_>, this: &Value) -> JsResult<Vec<Value>> {
    let object = this
        .as_object()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let Some(length) = object.typed_array_length() else {
        return Err(interp.type_error("not a typed array"));
    };
    Ok((0..length)
        .map(|index| {
            object
                .typed_array_element(index)
                .unwrap_or(Value::Undefined)
        })
        .collect())
}

/// `a.join(separator)`; `toString` shares it, which is what `Array.prototype.toString` does.
fn join(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let separator = match arg(args, 0) {
        Value::Undefined => ",".to_string(),
        other => interp.to_js_string(&other)?,
    };
    let elements = elements_of(interp, &this)?;
    let mut parts = Vec::with_capacity(elements.len());
    for element in elements {
        parts.push(match element {
            Value::Undefined | Value::Null => String::new(),
            other => interp.to_js_string(&other)?,
        });
    }
    Ok(interp.string(parts.join(&separator)))
}

/// `a.set(source, offset)`.
fn set_from(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let Some(length) = object.typed_array_length() else {
        return Err(interp.type_error("not a typed array"));
    };
    let offset = to_index(interp, &arg(args, 1), "offset")?;
    let values = interp.array_like_to_args(&arg(args, 0))?;
    if offset + values.len() > length {
        return Err(interp.range_error("source is too large"));
    }
    // Read every value first, so `a.set(a)` — a self-copy — sees the original contents
    // rather than a half-overwritten array.
    for (index, value) in values.into_iter().enumerate() {
        if let Some(element) = object.typed_array_element_type() {
            let (buffer, base) = object
                .typed_array_parts()
                .map(|(buffer, _, base, _)| (buffer, base))
                .expect("a typed array has parts");
            if write_element(&buffer, element, base, offset + index, value).is_err() {
                return Err(interp.type_error("cannot convert a Number to a BigInt"));
            }
        }
    }
    Ok(Value::Undefined)
}

/// `a.fill(value, start, end)`.
fn fill(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .ok_or_else(|| interp.type_error("not a typed array"))?
        .clone();
    let Some(length) = object.typed_array_length() else {
        return Err(interp.type_error("not a typed array"));
    };
    let value = arg(args, 0);
    let length_i = length as i64;
    let start = clamp_index(
        match args.get(1) {
            None => 0.0,
            Some(value) => value.to_number(),
        },
        length_i,
        0,
    );
    let end = clamp_index(
        match args.get(2) {
            None => length as f64,
            Some(value) => value.to_number(),
        },
        length_i,
        length_i,
    );
    let Some(element) = object.typed_array_element_type() else {
        return Err(interp.type_error("not a typed array"));
    };
    let (buffer, base) = object
        .typed_array_parts()
        .map(|(buffer, _, base, _)| (buffer, base))
        .expect("a typed array has parts");
    for index in start as usize..end.max(start) as usize {
        if write_element(&buffer, element, base, index, value.clone()).is_err() {
            return Err(interp.type_error("cannot convert a Number to a BigInt"));
        }
    }
    Ok(Value::Object(object))
}

/// `%TypedArray%`: the intrinsic every view constructor inherits from.
///
/// Absent until now, which is why `Object.getPrototypeOf(Int8Array)` answered
/// `Function.prototype` and `Object.getPrototypeOf(Int8Array).from` was `undefined`. It is
/// not a constructor — calling or `new`-ing it is a `TypeError` — it exists to hold the
/// shared statics and `Symbol.species`.
fn install_typed_array_intrinsic(interp: &mut Interp<'_>, shared: &Gc<JsObject>) -> Gc<JsObject> {
    let intrinsic = interp.new_native("TypedArray", 0, typed_array_intrinsic_call);
    // `from` and `of` go on before `prototype`, because `Object.getOwnPropertyNames` reports
    // insertion order and the reference lists them in this order.
    define_method(
        &intrinsic,
        "from",
        1,
        from as crate::object::NativeFn,
        interp,
    );
    define_method(&intrinsic, "of", 0, of, interp);
    // `%TypedArray%.prototype` is the shared prototype, and it is read-only.
    intrinsic.define(
        "prototype",
        Property::prototype_of(Value::Object(shared.clone()), false),
    );
    // The constructor property points back, which is what `TA.prototype.constructor === TA`
    // and a species lookup both rely on.
    shared.define(
        "constructor",
        Property {
            value: Some(Value::Object(intrinsic.clone())),
            writable: true,
            enumerable: false,
            configurable: true,
            ..Property::default()
        },
    );
    // `get %TypedArray%[Symbol.species]` answers `this`, so a subclass that does not
    // override it still produces instances of itself.
    let species = interp.new_native("[Symbol.species]", 0, species_getter);
    let key = interp
        .realm
        .well_known_key(crate::interp::WellKnown::Species);
    intrinsic.define_key(
        &key,
        Property {
            getter: Some(Value::Object(species)),
            enumerable: false,
            configurable: true,
            ..Property::default()
        },
    );
    intrinsic
}

/// `%TypedArray%` is abstract: both forms of construction are refused.
fn typed_array_intrinsic_call(
    interp: &mut Interp<'_>,
    _this: Value,
    _args: &[Value],
) -> JsResult<Value> {
    Err(interp.type_error("Abstract class TypedArray not directly constructable"))
}

fn species_getter(_interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(this)
}

/// `%TypedArray%.from(source, mapfn, thisArg)`.
///
/// The result is built by calling `this` as a constructor, which is what makes
/// `MyArray.from(...)` answer a `MyArray` rather than an `Int8Array`.
fn from(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let source = arg(args, 0);
    let mapper = match args.get(1) {
        None | Some(Value::Undefined) => None,
        Some(candidate) if matches!(candidate, Value::Object(o) if o.is_callable()) => {
            Some(candidate.clone())
        }
        Some(_) => return Err(interp.type_error("mapfn is not a function")),
    };
    let this_arg = arg(args, 2);
    // The source is read either through `Symbol.iterator` or as an array-like, the same
    // choice the constructor makes.
    let values = if interp.has_iterator(&source)? {
        interp.collect_iterable(&source)?
    } else {
        interp.array_like_to_args(&source)?
    };
    let mut mapped = Vec::with_capacity(values.len());
    for (index, value) in values.into_iter().enumerate() {
        match &mapper {
            Some(mapper) => mapped.push(interp.call_value(
                mapper.clone(),
                this_arg.clone(),
                vec![value, Value::Int(index as i32)],
            )?),
            None => mapped.push(value),
        }
    }
    typed_array_from_values(interp, &this, &mapped)
}

/// `%TypedArray%.of(...items)`.
fn of(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    typed_array_from_values(interp, &this, args)
}

/// Build a view by calling `constructor` with a length, then filling it.
fn typed_array_from_values(
    interp: &mut Interp<'_>,
    constructor: &Value,
    values: &[Value],
) -> JsResult<Value> {
    if !matches!(constructor, Value::Object(o) if o.is_callable()) {
        return Err(interp.type_error("not a constructor"));
    }
    let created = interp.construct(
        constructor.clone(),
        constructor.clone(),
        vec![Value::Int(values.len() as i32)],
    )?;
    let Some(object) = created.as_object().cloned() else {
        return Err(interp.type_error("constructor did not answer an object"));
    };
    let Some(element) = object.typed_array_element_type() else {
        return Err(interp.type_error("constructor did not answer a typed array"));
    };
    // The constructor is arbitrary script: it may answer a shorter view than asked for,
    // which the spec refuses rather than writing past the end.
    let available = object.typed_array_length().unwrap_or(0);
    if values.len() > available {
        return Err(interp.type_error("derived typed array has an insufficient length"));
    }
    write_back(interp, &object, element, values)?;
    Ok(created)
}

/// A new view of `element` type holding `values`, which is what `map`/`filter`/`slice`
/// and friends answer with.
///
/// `%TypedArray%.prototype.map` is *not* `Array.prototype.map`: the result is a typed array
/// of the same element type, not an ordinary array, which is why the spec goes through
/// `TypedArraySpeciesCreate` rather than `ArraySpeciesCreate`.
fn new_view(
    interp: &mut Interp<'_>,
    element: ElementType,
    values: &[Value],
) -> JsResult<Gc<JsObject>> {
    let proto = interp
        .realm
        .array_buffer_proto
        .clone()
        .unwrap_or_else(|| interp.realm.object_proto.clone());
    let buffer = interp.new_object(Some(proto));
    buffer.set_array_buffer(Rc::new(RefCell::new(BufferStorage {
        bytes: vec![0; values.len() * element.size()],
        detached: false,
    })));
    for (index, value) in values.iter().enumerate() {
        if write_element(&buffer, element, 0, index, value.clone()).is_err() {
            return Err(interp.type_error("cannot convert a Number to a BigInt"));
        }
    }
    let proto = interp
        .realm
        .typed_array_protos
        .get(element as usize)
        .cloned()
        .or_else(|| interp.realm.typed_array_proto.clone())
        .unwrap_or_else(|| interp.realm.object_proto.clone());
    let fresh = interp.new_object(Some(proto));
    fresh.set_typed_array(buffer, element, 0, values.len());
    Ok(fresh)
}

/// `TypedArraySpeciesCreate(receiver, len)`: the view a method answers with.
///
/// The spec does not simply build a view of the same type. It reads `receiver.constructor`,
/// then that function's `Symbol.species`, and calls it with the length — so a subclass can
/// redirect the result and a test can assert the override was honoured, or that a throwing
/// species propagates. Falling straight through to `new_view` made every `speciesctor` case
/// fail even though the elements were right.
fn species_create(
    interp: &mut Interp<'_>,
    receiver: &Gc<JsObject>,
    element: ElementType,
    values: &[Value],
) -> JsResult<Gc<JsObject>> {
    let constructor = interp.get_property(receiver, "constructor")?;
    let species = match &constructor {
        // `undefined` — or no `constructor` at all — means "use the default".
        None | Some(Value::Undefined) => None,
        Some(Value::Object(constructor)) => {
            let key = interp
                .realm
                .well_known_key(crate::interp::WellKnown::Species);
            interp.get_property_key(constructor, &key)?
        }
        // Anything else is neither absent nor an object, and `SpeciesConstructor` refuses
        // it outright: `sample.constructor = 42` makes `sample.map(f)` a `TypeError`.
        Some(_) => return Err(interp.type_error("constructor is not an object")),
    };
    let species = match species {
        Some(Value::Null) | None | Some(Value::Undefined) => {
            return new_view(interp, element, values);
        }
        Some(other) => other,
    };
    if !matches!(&species, Value::Object(o) if o.is_callable()) {
        return Err(interp.type_error("species is not a constructor"));
    }
    let created = interp.construct(
        species.clone(),
        species,
        vec![Value::Int(values.len() as i32)],
    )?;
    let Some(object) = created.as_object().cloned() else {
        return Err(interp.type_error("species constructor did not answer an object"));
    };
    let Some(actual_element) = object.typed_array_element_type() else {
        return Err(interp.type_error("species constructor did not answer a typed array"));
    };
    // A species is allowed to answer a *longer* view but not a shorter one: the spec
    // checks `srcLength <= targetLength`.
    let available = object.typed_array_length().unwrap_or(0);
    if values.len() > available {
        return Err(interp.type_error("derived typed array has an insufficient length"));
    }
    write_back(interp, &object, actual_element, values)?;
    Ok(object)
}

/// The receiver as `(elements, element type)`, the shape every method here starts from.
///
/// A typed array is never a plain object: unlike `Array.prototype`'s methods, these do not
/// run `ToObject` on anything, they refuse a receiver that is not a view.
fn view_parts(interp: &mut Interp<'_>, this: &Value) -> JsResult<(Vec<Value>, ElementType)> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("not a typed array"));
    };
    let Some(element) = object.typed_array_element_type() else {
        return Err(interp.type_error("not a typed array"));
    };
    // `ValidateTypedArray` runs `IsDetachedBuffer` and refuses. Without this a detached
    // view silently behaved like an empty one, so every `detached-buffer` test — which
    // expects a `TypeError` — saw a clean return instead.
    if is_detached(object) {
        return Err(interp.type_error("the buffer is detached"));
    }
    Ok((elements_of(interp, this)?, element))
}

/// Whether this view's buffer has been detached.
fn is_detached(object: &Gc<JsObject>) -> bool {
    let Some((buffer, _, _, _)) = object.typed_array_parts() else {
        return false;
    };
    buffer
        .array_buffer_storage()
        .is_some_and(|storage| storage.borrow().detached)
}

/// The callback a typed-array method requires, or a `TypeError`.
fn typed_callback(interp: &mut Interp<'_>, args: &[Value]) -> JsResult<Value> {
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    Ok(callback)
}

/// Call `callback(element, index, receiver)` for every element.
fn call_typed(
    interp: &mut Interp<'_>,
    callback: &Value,
    value: Value,
    index: usize,
    receiver: &Gc<JsObject>,
) -> JsResult<Value> {
    interp.call_value(
        callback.clone(),
        Value::Undefined,
        vec![
            value,
            Value::Int(index as i32),
            Value::Object(receiver.clone()),
        ],
    )
}

/// `a.at(index)`: negative counts from the end, out of range is `undefined`.
fn at(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (elements, _) = view_parts(interp, &this)?;
    let length = elements.len() as f64;
    let raw = arg(args, 0).to_number().trunc();
    let index = if raw < 0.0 { length + raw } else { raw };
    if index < 0.0 || index >= length {
        return Ok(Value::Undefined);
    }
    Ok(elements[index as usize].clone())
}

/// `a.every(f)` / `a.some(f)`.
fn every(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    quantify(interp, this, args, false)
}

fn some(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    quantify(interp, this, args, true)
}

fn quantify(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    want_any: bool,
) -> JsResult<Value> {
    let receiver = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let (elements, _) = view_parts(interp, &this)?;
    let callback = typed_callback(interp, args)?;
    for (index, value) in elements.into_iter().enumerate() {
        let answer = call_typed(interp, &callback, value, index, &receiver)?;
        // `every` stops on the first falsy answer, `some` on the first truthy one.
        if answer.to_boolean() == want_any {
            return Ok(Value::Bool(want_any));
        }
    }
    Ok(Value::Bool(!want_any))
}

fn for_each(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let receiver = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let (elements, _) = view_parts(interp, &this)?;
    let callback = typed_callback(interp, args)?;
    for (index, value) in elements.into_iter().enumerate() {
        call_typed(interp, &callback, value, index, &receiver)?;
    }
    Ok(Value::Undefined)
}

fn map(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let receiver = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let (elements, element_type) = view_parts(interp, &this)?;
    let callback = typed_callback(interp, args)?;
    let mut out = Vec::with_capacity(elements.len());
    for (index, value) in elements.into_iter().enumerate() {
        out.push(call_typed(interp, &callback, value, index, &receiver)?);
    }
    Ok(Value::Object(species_create(
        interp,
        &receiver,
        element_type,
        &out,
    )?))
}

fn filter(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let receiver = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let (elements, element_type) = view_parts(interp, &this)?;
    let callback = typed_callback(interp, args)?;
    let mut out = Vec::new();
    for (index, value) in elements.into_iter().enumerate() {
        let keep = call_typed(interp, &callback, value.clone(), index, &receiver)?;
        if keep.to_boolean() {
            out.push(value);
        }
    }
    Ok(Value::Object(species_create(
        interp,
        &receiver,
        element_type,
        &out,
    )?))
}

fn reduce(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    reduce_with(interp, this, args, false)
}

fn reduce_right(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    reduce_with(interp, this, args, true)
}

fn reduce_with(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    right_to_left: bool,
) -> JsResult<Value> {
    let receiver = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let (elements, _) = view_parts(interp, &this)?;
    let callback = typed_callback(interp, args)?;
    let mut indices: Vec<usize> = (0..elements.len()).collect();
    if right_to_left {
        indices.reverse();
    }
    let mut ordered = indices.into_iter();
    let mut accumulator = match args.get(1) {
        Some(value) if !matches!(value, Value::Undefined) => value.clone(),
        _ => match ordered.next() {
            Some(index) => elements[index].clone(),
            None => {
                return Err(interp.type_error("Reduce of empty array with no initial value"));
            }
        },
    };
    for index in ordered {
        accumulator = interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![
                accumulator,
                elements[index].clone(),
                Value::Int(index as i32),
                Value::Object(receiver.clone()),
            ],
        )?;
    }
    Ok(accumulator)
}

fn find(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    find_with(interp, this, args, FindKind::Value, false)
}

fn find_index(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    find_with(interp, this, args, FindKind::Index, false)
}

fn find_last(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    find_with(interp, this, args, FindKind::Value, true)
}

fn find_last_index(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    find_with(interp, this, args, FindKind::Index, true)
}

#[derive(Clone, Copy)]
enum FindKind {
    Value,
    Index,
}

fn find_with(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    kind: FindKind,
    from_end: bool,
) -> JsResult<Value> {
    let receiver = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let (elements, _) = view_parts(interp, &this)?;
    let callback = typed_callback(interp, args)?;
    let mut indices: Vec<usize> = (0..elements.len()).collect();
    if from_end {
        indices.reverse();
    }
    for index in indices {
        let value = elements[index].clone();
        let answer = call_typed(interp, &callback, value.clone(), index, &receiver)?;
        if answer.to_boolean() {
            return Ok(match kind {
                FindKind::Value => value,
                FindKind::Index => Value::Int(index as i32),
            });
        }
    }
    Ok(match kind {
        FindKind::Value => Value::Undefined,
        FindKind::Index => Value::Int(-1),
    })
}

fn index_of(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    search(interp, this, args, SearchKind::IndexOf, false)
}

fn last_index_of(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    search(interp, this, args, SearchKind::IndexOf, true)
}

fn includes(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    search(interp, this, args, SearchKind::Includes, false)
}

#[derive(Clone, Copy)]
enum SearchKind {
    /// `===`-style, so `NaN` is never found.
    IndexOf,
    /// `SameValueZero`, so `NaN` is found, and the answer is a boolean.
    Includes,
}

fn search(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    kind: SearchKind,
    from_end: bool,
) -> JsResult<Value> {
    let (elements, _) = view_parts(interp, &this)?;
    let needle = arg(args, 0);
    let length = elements.len();
    // `fromIndex` is `ToIntegerOrInfinity`, so a Symbol here is a `TypeError` — which is
    // why the conversion has to happen even when the answer is already decided. Ignoring
    // the argument made `includes(7, Symbol())` return cleanly instead of throwing.
    let missing = match args.get(1) {
        None | Some(Value::Undefined) => true,
        Some(_) => false,
    };
    let start = if from_end {
        // `lastIndexOf`: the default is the last index, and a negative offset counts back
        // from the end; anything below zero means "not found".
        let raw = if missing {
            length as f64
        } else {
            to_integer_or_infinity(interp, &arg(args, 1))?
        };
        if raw < 0.0 {
            match (length as f64 + raw) as i64 {
                n if n < 0 => None,
                n => Some(n as usize),
            }
        } else {
            Some((raw.min(length as f64) as usize).min(length.saturating_sub(1)))
        }
    } else {
        // `indexOf`/`includes`: the default is the first index, and a negative offset
        // counts back from the end but clamps to zero.
        let raw = if missing {
            0.0
        } else {
            to_integer_or_infinity(interp, &arg(args, 1))?
        };
        if raw >= 0.0 {
            Some(raw.min(length as f64) as usize)
        } else {
            Some(((length as f64 + raw).max(0.0)) as usize)
        }
    };
    let mut order: Vec<usize> = match start {
        None => Vec::new(),
        Some(start) if from_end => (0..=start.min(length.saturating_sub(1))).rev().collect(),
        Some(start) => (start..length).collect(),
    };
    if length == 0 {
        order.clear();
    }
    for index in order {
        let value = &elements[index];
        let found = match kind {
            SearchKind::IndexOf => value.strict_equals(&needle),
            // `includes` uses SameValueZero, which finds a `NaN` that `indexOf` cannot.
            SearchKind::Includes => value.same_value_zero(&needle),
        };
        if found {
            return Ok(match kind {
                SearchKind::IndexOf => Value::Int(index as i32),
                SearchKind::Includes => Value::Bool(true),
            });
        }
    }
    Ok(match kind {
        SearchKind::IndexOf => Value::Int(-1),
        SearchKind::Includes => Value::Bool(false),
    })
}

/// `a.reverse()`: in place, and it answers the receiver.
fn reverse(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let (mut elements, element_type) = view_parts(interp, &this)?;
    elements.reverse();
    write_back(interp, &object, element_type, &elements)?;
    Ok(Value::Object(object))
}

fn sort(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let (mut elements, element_type) = view_parts(interp, &this)?;
    match args.first() {
        // The default order is numeric for a typed array, unlike `Array.prototype.sort`'s
        // string order — `new Int32Array([10, 9]).sort()` is `[9, 10]`.
        None | Some(Value::Undefined) => {
            if element_type.is_bigint() {
                elements.sort_by(compare_bigints);
            } else {
                elements.sort_by(|a, b| {
                    a.to_number()
                        .partial_cmp(&b.to_number())
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
        }
        Some(comparator) if matches!(comparator, Value::Object(o) if o.is_callable()) => {
            // A comparator is arbitrary script, so Rust's sort cannot be told it is a
            // total order; this merge sort keeps the comparison count deterministic.
            merge_sort_typed(interp, &mut elements, comparator)?;
        }
        Some(_) => return Err(interp.type_error("comparator is not a function")),
    }
    write_back(interp, &object, element_type, &elements)?;
    Ok(Value::Object(object))
}

fn compare_bigints(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::BigInt(a), Value::BigInt(b)) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    }
}

fn merge_sort_typed(
    interp: &mut Interp<'_>,
    items: &mut Vec<Value>,
    comparator: &Value,
) -> JsResult<()> {
    if items.len() <= 1 {
        return Ok(());
    }
    let mid = items.len() / 2;
    let mut right = items.split_off(mid);
    merge_sort_typed(interp, items, comparator)?;
    merge_sort_typed(interp, &mut right, comparator)?;
    let mut merged = Vec::with_capacity(items.len() + right.len());
    let mut left_iter = std::mem::take(items).into_iter().peekable();
    let mut right_iter = right.into_iter().peekable();
    loop {
        match (left_iter.peek(), right_iter.peek()) {
            (Some(left), Some(right_value)) => {
                let answer = interp.call_value(
                    comparator.clone(),
                    Value::Undefined,
                    vec![left.clone(), right_value.clone()],
                )?;
                if answer.to_number() > 0.0 {
                    merged.push(right_iter.next().expect("peeked"));
                } else {
                    merged.push(left_iter.next().expect("peeked"));
                }
            }
            (Some(_), None) => merged.push(left_iter.next().expect("peeked")),
            (None, Some(_)) => merged.push(right_iter.next().expect("peeked")),
            (None, None) => break,
        }
    }
    *items = merged;
    Ok(())
}

/// Store `values` back into the receiver's view, which is how the in-place methods answer.
fn write_back(
    interp: &mut Interp<'_>,
    object: &Gc<JsObject>,
    element_type: ElementType,
    values: &[Value],
) -> JsResult<()> {
    let Some((buffer, _, base, _)) = object.typed_array_parts() else {
        return Err(interp.type_error("not a typed array"));
    };
    for (index, value) in values.iter().enumerate() {
        if write_element(&buffer, element_type, base, index, value.clone()).is_err() {
            return Err(interp.type_error("cannot convert a Number to a BigInt"));
        }
    }
    Ok(())
}

/// `a.copyWithin(target, start, end)`: copies a run of elements over an earlier one.
fn copy_within(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let (mut elements, element_type) = view_parts(interp, &this)?;
    let length_i = elements.len() as i64;
    let target = clamp_index(to_integer_or_infinity(interp, &arg(args, 0))?, length_i, 0);
    let start = clamp_index(to_integer_or_infinity(interp, &arg(args, 1))?, length_i, 0);
    let end = match args.get(2) {
        None | Some(Value::Undefined) => length_i,
        Some(value) => clamp_index(to_integer_or_infinity(interp, value)?, length_i, length_i),
    };
    // The index arguments are arbitrary script through `valueOf`, and one of them may
    // detach the buffer: `copyWithin(0, {valueOf(){ detach() }})` must throw, not copy from
    // storage that is gone. `ValidateTypedArray` therefore runs again here.
    if is_detached(&object) {
        return Err(interp.type_error("the buffer is detached"));
    }
    let count = (end - start).min(length_i - target).max(0) as usize;
    // A temporary, because the ranges may overlap: `copyWithin(0, 1)` would otherwise read
    // values it had already overwritten.
    let source: Vec<Value> = (0..count)
        .map(|offset| elements[start as usize + offset].clone())
        .collect();
    for (offset, value) in source.into_iter().enumerate() {
        elements[target as usize + offset] = value;
    }
    write_back(interp, &object, element_type, &elements)?;
    Ok(Value::Object(object))
}

/// `a.toLocaleString()`: each element through its own `toLocaleString`, joined by `,`.
fn to_locale_string(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let elements = elements_of(interp, &this)?;
    let mut parts = Vec::with_capacity(elements.len());
    for element in elements {
        // `undefined` and `null` contribute an empty field, exactly as `join` does.
        if matches!(element, Value::Undefined | Value::Null) {
            parts.push(String::new());
            continue;
        }
        // Read `toLocaleString` off the element's own chain, so a Number reaches
        // `Number.prototype.toLocaleString` and a BigInt reaches `BigInt.prototype`'s.
        let method = match &element {
            Value::Object(object) => interp.get_property(object, "toLocaleString")?,
            primitive => {
                let boxed = interp.object_from(primitive)?;
                interp.get_property(&boxed, "toLocaleString")?
            }
        };
        let text = match method {
            Some(method) => {
                let result = interp.call_value(method, element.clone(), Vec::new())?;
                interp.to_js_string(&result)?
            }
            None => interp.to_js_string(&element)?,
        };
        parts.push(text);
    }
    Ok(interp.string(parts.join(",")))
}

/// `a.with(index, value)`: a copy with one element replaced.
fn with(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (mut elements, element_type) = view_parts(interp, &this)?;
    let length = elements.len() as f64;
    let raw = arg(args, 0).to_number().trunc();
    let index = if raw < 0.0 { length + raw } else { raw };
    if index < 0.0 || index >= length {
        return Err(interp.range_error("index is out of range"));
    }
    elements[index as usize] = arg(args, 1);
    let receiver = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    Ok(Value::Object(species_create(
        interp,
        &receiver,
        element_type,
        &elements,
    )?))
}

fn to_reversed(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let receiver = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let (mut elements, element_type) = view_parts(interp, &this)?;
    elements.reverse();
    Ok(Value::Object(species_create(
        interp,
        &receiver,
        element_type,
        &elements,
    )?))
}

fn to_sorted(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (elements, element_type) = view_parts(interp, &this)?;
    // Reuse `sort` by handing it a fresh view of the same contents.
    let copy = new_view(interp, element_type, &elements)?;
    sort(interp, Value::Object(copy.clone()), args)?;
    Ok(Value::Object(copy))
}

/// `a.values()` / `a.keys()` / `a.entries()`.
///
/// The iterator machinery is shared with `Array.prototype`: `length_of` and `element_at`
/// reach a typed array through its exotic `length` and index properties, so the same
/// `next` walks it correctly and the laziness carries over.
fn values(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    // `%TypedArray%.prototype.values` runs `ValidateTypedArray` before building the
    // iterator, so a detached view throws here rather than on the first `next`.
    view_parts(interp, &this)?;
    crate::builtins::array::iterator(interp, this, crate::builtins::array::IteratorKind::Value)
}

fn keys(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    view_parts(interp, &this)?;
    crate::builtins::array::iterator(interp, this, crate::builtins::array::IteratorKind::Key)
}

fn entries(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    view_parts(interp, &this)?;
    crate::builtins::array::iterator(interp, this, crate::builtins::array::IteratorKind::Entry)
}

/// `a.slice(begin, end)`: a new view with a buffer of its own.
fn slice(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (elements, element_type) = view_parts(interp, &this)?;
    let receiver = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let length_i = elements.len() as i64;
    let begin = clamp_index(arg(args, 0).to_number(), length_i, 0);
    let end = clamp_index(
        match args.get(1) {
            None | Some(Value::Undefined) => length_i as f64,
            Some(value) => value.to_number(),
        },
        length_i,
        length_i,
    );
    let count = (end - begin).max(0) as usize;
    // Copied into a temporary first: `slice` reads the source through `[[Get]]` and builds
    // the result through `TypedArraySpeciesCreate`, and a species override may resize the
    // source, so the two must not be interleaved.
    let taken: Vec<Value> = (0..count)
        .map(|index| elements[begin as usize + index].clone())
        .collect();
    Ok(Value::Object(species_create(
        interp,
        &receiver,
        element_type,
        &taken,
    )?))
}

/// `a.subarray(begin, end)`: a view over the *same* buffer.
fn subarray(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let Some((buffer, element, base, length)) = object.typed_array_parts() else {
        return Err(interp.type_error("not a typed array"));
    };
    let length_i = length as i64;
    let begin = clamp_index(arg(args, 0).to_number(), length_i, 0);
    let end = clamp_index(
        match args.get(1) {
            None => length as f64,
            Some(value) => value.to_number(),
        },
        length_i,
        length_i,
    );
    let count = (end - begin).max(0) as usize;
    let fresh = interp.new_object(Some(
        interp
            .realm
            .typed_array_proto
            .clone()
            .unwrap_or_else(|| interp.realm.object_proto.clone()),
    ));
    fresh.set_typed_array(
        buffer,
        element,
        base + begin as usize * element.size(),
        count,
    );
    Ok(Value::Object(fresh))
}

/// The `buffer` accessor.
fn buffer_of(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    object
        .typed_array_parts()
        .map(|(buffer, _, _, _)| Value::Object(buffer))
        .ok_or_else(|| interp.type_error("not a typed array"))
}

/// The `byteOffset` accessor.
fn byte_offset_of(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    object
        .typed_array_parts()
        .map(|(_, _, base, _)| Value::Int(base as i32))
        .ok_or_else(|| interp.type_error("not a typed array"))
}

/// The `byteLength` accessor.
fn byte_length_of(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    object
        .typed_array_parts()
        .map(|(_, element, _, length)| Value::Int((length * element.size()) as i32))
        .ok_or_else(|| interp.type_error("not a typed array"))
}

/// The `length` accessor.
fn length_of(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    object
        .typed_array_length()
        .map(|length| Value::Int(length as i32))
        .ok_or_else(|| interp.type_error("not a typed array"))
}

/// The `Symbol.toStringTag` accessor, which names the element type.
fn tag_of(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let name = object
        .typed_array_element_type()
        .map(|element| element.name());
    Ok(match name {
        Some(name) => interp.string(name),
        None => Value::Undefined,
    })
}

// ---------------------------------------------------------------------------
// DataView
// ---------------------------------------------------------------------------

/// One `get`/`set` pair per width. `NativeFn` is a bare function pointer, so the width has
/// to be baked into the function rather than captured: the macro below writes one pair of
/// thin wrappers per row.
macro_rules! data_view_accessors {
    ($($get:ident, $set:ident => $element:expr, $width:literal);* $(;)?) => {
        $(
            fn $get(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
                data_view_get(interp, &this, args, $element)
            }
            fn $set(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
                data_view_set(interp, &this, args, $element)
            }
        )*

        /// Every accessor, as `(name, length, body)`.
        const DATA_VIEW_METHODS: &[(&str, u32, crate::object::NativeFn)] = &[
            $(
                (concat!("get", $width), 1, $get as crate::object::NativeFn),
                (concat!("set", $width), 2, $set as crate::object::NativeFn),
            )*
        ];
    };
}

data_view_accessors! {
    get_int8, set_int8 => ElementType::Int8, "Int8";
    get_uint8, set_uint8 => ElementType::Uint8, "Uint8";
    get_int16, set_int16 => ElementType::Int16, "Int16";
    get_uint16, set_uint16 => ElementType::Uint16, "Uint16";
    get_int32, set_int32 => ElementType::Int32, "Int32";
    get_uint32, set_uint32 => ElementType::Uint32, "Uint32";
    get_float16, set_float16 => ElementType::Float16, "Float16";
    get_float32, set_float32 => ElementType::Float32, "Float32";
    get_float64, set_float64 => ElementType::Float64, "Float64";
    get_bigint64, set_bigint64 => ElementType::BigInt64, "BigInt64";
    get_biguint64, set_biguint64 => ElementType::BigUint64, "BigUint64";
}

/// Install `DataView` and its prototype.
pub fn install_data_view(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    let proto = interp.new_object(Some(object_proto.clone()));
    let constructor = interp.new_native_ctor("DataView", 1, data_view_constructor);
    constructor.define(
        "prototype",
        Property::prototype_of(Value::Object(proto.clone()), false),
    );
    proto.define(
        "constructor",
        Property {
            value: Some(Value::Object(constructor.clone())),
            writable: true,
            enumerable: false,
            configurable: true,
            ..Property::default()
        },
    );
    for (name, getter) in [
        ("buffer", data_view_buffer as crate::object::NativeFn),
        ("byteOffset", data_view_byte_offset),
        ("byteLength", data_view_byte_length),
    ] {
        let function = interp.new_native(format!("get {name}"), 0, getter);
        proto.define(
            name,
            Property {
                getter: Some(Value::Object(function)),
                enumerable: false,
                configurable: true,
                ..Property::default()
            },
        );
    }
    for (name, length, body) in DATA_VIEW_METHODS {
        define_method(&proto, name, *length, *body, interp);
    }
    interp.realm.data_view_proto = Some(proto.clone());
    let global = interp.realm.global.clone();
    global.define(
        "DataView",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
    install_tag(&proto, "DataView", interp);
}

/// The receiver as `(buffer, offset, length)`, refusing anything that is not a live view.
///
/// A detached buffer is a `TypeError` from every accessor and every property read, which is
/// what `MakeDataViewWithBufferWitnessRecord`'s `IsDetachedBuffer` check amounts to.
fn data_view_parts(
    interp: &mut Interp<'_>,
    this: &Value,
) -> JsResult<(Gc<JsObject>, usize, usize)> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("not a DataView"));
    };
    let Some(parts) = object.data_view_parts() else {
        return Err(interp.type_error("not a DataView"));
    };
    Ok(parts)
}

/// Whether this view's buffer has been detached.
fn data_view_is_detached(object: &Gc<JsObject>) -> bool {
    object
        .data_view_parts()
        .and_then(|(buffer, _, _)| buffer.array_buffer_storage())
        .is_some_and(|storage| storage.borrow().detached)
}

fn data_view_buffer(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let (buffer, _, _) = data_view_parts(interp, &this)?;
    Ok(Value::Object(buffer))
}

fn data_view_byte_offset(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let (_, offset, _) = data_view_parts(interp, &this)?;
    // A detached view is zero-length, so its offset is 0 as well.
    let detached = this.as_object().is_some_and(data_view_is_detached);
    Ok(Value::Int(if detached { 0 } else { offset as i32 }))
}

fn data_view_byte_length(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let (_, _, length) = data_view_parts(interp, &this)?;
    // A detached buffer has no bytes left, so the view reports 0 rather than throwing.
    let detached = this.as_object().is_some_and(data_view_is_detached);
    Ok(Value::Int(if detached { 0 } else { length as i32 }))
}

/// `dv.getInt32(byteOffset, littleEndian)` and siblings.
fn data_view_get(
    interp: &mut Interp<'_>,
    this: &Value,
    args: &[Value],
    element: ElementType,
) -> JsResult<Value> {
    let (buffer, offset, length) = data_view_parts(interp, this)?;
    // `ToIndex(byteOffset)` runs before the detached check, so a `valueOf` that detaches
    // still runs and an `Infinity` offset is a `RangeError` either way.
    // `GetViewByteLength` then bounds the access.
    let byte_offset = to_index(interp, &arg(args, 0), "byte offset")?;
    if data_view_is_detached(
        this.as_object()
            .ok_or_else(|| interp.type_error("not a DataView"))?,
    ) {
        return Err(interp.type_error("the buffer is detached"));
    }
    let size = element.size();
    if byte_offset + size > length {
        return Err(interp.range_error("byte offset is out of range"));
    }
    let little_endian = arg(args, 1).to_boolean();
    let storage = buffer
        .array_buffer_storage()
        .ok_or_else(|| interp.type_error("not a DataView"))?;
    let storage = storage.borrow();
    let start = offset + byte_offset;
    // A `DataView` defaults to *big*-endian while the element types read little-endian, so
    // the bytes are reversed into a scratch buffer first. One-byte widths are unaffected.
    let mut scratch = [0u8; 8];
    let raw = &storage.bytes[start..start + size];
    if little_endian {
        scratch[..size].copy_from_slice(raw);
    } else {
        for (slot, byte) in scratch[..size].iter_mut().zip(raw.iter().rev()) {
            *slot = *byte;
        }
    }
    Ok(element.read(buffer.heap(), &scratch, 0))
}

/// `dv.setInt32(byteOffset, value, littleEndian)` and siblings.
fn data_view_set(
    interp: &mut Interp<'_>,
    this: &Value,
    args: &[Value],
    element: ElementType,
) -> JsResult<Value> {
    let (buffer, offset, length) = data_view_parts(interp, this)?;
    // Same order as `data_view_get`: `ToIndex` first, then the detached check.
    let byte_offset = to_index(interp, &arg(args, 0), "byte offset")?;
    if data_view_is_detached(
        this.as_object()
            .ok_or_else(|| interp.type_error("not a DataView"))?,
    ) {
        return Err(interp.type_error("the buffer is detached"));
    }
    let size = element.size();
    if byte_offset + size > length {
        return Err(interp.range_error("byte offset is out of range"));
    }
    let little_endian = arg(args, 2).to_boolean();
    // Little-endian first, into scratch, so the conversion (wrap, clamp, or BigInt modulo)
    // runs before any byte is reversed.
    let mut scratch = [0u8; 8];
    element
        .write(&mut scratch, 0, arg(args, 1))
        .map_err(|reason| interp.type_error(reason))?;
    let storage = buffer
        .array_buffer_storage()
        .ok_or_else(|| interp.type_error("not a DataView"))?;
    let mut storage = storage.borrow_mut();
    let start = offset + byte_offset;
    let target = &mut storage.bytes[start..start + size];
    if little_endian {
        target.copy_from_slice(&scratch[..size]);
    } else {
        for (slot, byte) in target.iter_mut().zip(scratch[..size].iter().rev()) {
            *slot = *byte;
        }
    }
    Ok(Value::Undefined)
}

/// `new DataView(buffer, byteOffset, byteLength)`.
///
/// The step order is observable and follows 25.3.2.1 exactly: `ToIndex(byteOffset)` runs
/// *before* the detached-buffer check, so a `valueOf` that detaches is still called, and the
/// detached check is repeated after the prototype lookup because
/// `OrdinaryCreateFromConstructor` can run a getter that detaches.
fn data_view_constructor(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    // Called as a function rather than with `new`: there is no instance to build.
    let _ = &this;
    if !interp.is_constructing() {
        return Err(interp.type_error("Constructor DataView requires 'new'"));
    }
    let Some(buffer) = arg(args, 0).as_object().cloned() else {
        return Err(interp.type_error("first argument to DataView must be an ArrayBuffer"));
    };
    if buffer.array_buffer_storage().is_none() {
        return Err(interp.type_error("first argument to DataView must be an ArrayBuffer"));
    }
    // Step 5 before step 6.
    let offset = to_index(interp, &arg(args, 1), "byte offset")?;
    let detached = |buffer: &Gc<JsObject>| {
        buffer
            .array_buffer_storage()
            .is_some_and(|storage| storage.borrow().detached)
    };
    if detached(&buffer) {
        return Err(interp.type_error("the buffer is detached"));
    }
    let total = buffer
        .array_buffer_storage()
        .map(|storage| storage.borrow().bytes.len())
        .unwrap_or(0);
    if offset > total {
        return Err(interp.range_error("byte offset is out of range"));
    }
    let length = match args.get(2) {
        // An absent or explicit `undefined` length means "the rest of the buffer".
        None | Some(Value::Undefined) => total - offset,
        Some(_) => {
            let requested = to_index(interp, &arg(args, 2), "byte length")?;
            if requested > total - offset {
                return Err(interp.range_error("byte length is out of range"));
            }
            requested
        }
    };
    // The prototype lookup already happened — the engine builds `this` from
    // `new.target.prototype` — but a getter there may have detached the buffer, so the
    // check is repeated.
    if detached(&buffer) {
        return Err(interp.type_error("the buffer is detached"));
    }
    // `OrdinaryCreateFromConstructor` last, so a throwing `new.target.prototype` getter is
    // never reached for arguments that are already refused.
    let proto = interp
        .realm
        .data_view_proto
        .clone()
        .unwrap_or_else(|| interp.realm.object_proto.clone());
    let instance = interp.new_ctor_instance(proto)?;
    instance.set_data_view(buffer, offset, length);
    Ok(Value::Object(instance))
}
