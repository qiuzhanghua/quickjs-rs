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
}

/// The view types, in the order their prototypes are installed.
pub const ELEMENT_TYPES: [(ElementType, &str); 10] = [
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
];

impl ElementType {
    /// Bytes per element.
    pub fn size(self) -> usize {
        match self {
            ElementType::Int8 | ElementType::Uint8 | ElementType::Uint8Clamped => 1,
            ElementType::Int16 | ElementType::Uint16 | ElementType::Float16 => 2,
            ElementType::Int32 | ElementType::Uint32 | ElementType::Float32 => 4,
            ElementType::Float64 => 8,
        }
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
    pub fn read(self, bytes: &[u8], offset: usize) -> Value {
        let slice = bytes.get(offset..offset + self.size());
        let Some(slice) = slice else {
            return Value::Int(0);
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
        }
    }

    /// Store `value` into the element at `offset`.
    pub fn write(self, bytes: &mut [u8], offset: usize, value: Value) {
        let size = self.size();
        if offset + size > bytes.len() {
            return;
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
        }
    }
}

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
}

fn install_array_buffer(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    let proto = interp.new_object(Some(object_proto.clone()));
    let constructor = interp.new_native("ArrayBuffer", 1, buffer_constructor);
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
    define_method(&shared, "slice", 2, slice, interp);
    define_method(&shared, "subarray", 2, subarray, interp);
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
        let constructor = interp.new_native(name, 3, view_constructor);
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
        global.define(
            name,
            Property::with_attributes(Value::Object(constructor), true, false),
        );
    }
}

fn install_tag(proto: &Gc<JsObject>, _tag: &str, interp: &Interp<'_>) {
    let function = interp.new_native("[Symbol.toStringTag]", 0, buffer_tag);
    proto.define_key(
        &interp
            .realm
            .well_known_key(crate::interp::WellKnown::ToStringTag),
        Property {
            getter: Some(Value::Object(function)),
            enumerable: false,
            configurable: true,
            ..Property::default()
        },
    );
}

/// `ArrayBuffer.prototype[Symbol.toStringTag]`.
fn buffer_tag(interp: &mut Interp<'_>, _this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(interp.string("ArrayBuffer"))
}

/// `new ArrayBuffer(length)`.
fn buffer_constructor(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let length = arg(args, 0).to_number();
    if length < 0.0 || length.fract() != 0.0 || length > 4294967295.0 {
        return Err(interp.range_error("invalid array buffer length"));
    }
    let storage = Rc::new(RefCell::new(BufferStorage {
        bytes: vec![0; length as usize],
        detached: false,
    }));
    let object = match this.as_object() {
        Some(object) => object.clone(),
        None => {
            let proto = interp
                .realm
                .array_buffer_proto
                .clone()
                .unwrap_or_else(|| interp.realm.object_proto.clone());
            interp.new_object(Some(proto))
        }
    };
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
fn to_index(interp: &Interp<'_>, value: &Value, what: &str) -> JsResult<usize> {
    let number = value.to_number();
    if number.is_nan() {
        return Ok(0);
    }
    if number < 0.0 || number.is_infinite() || number.fract() != 0.0 {
        return Err(interp.range_error(format!("invalid {what}")));
    }
    Ok(number as usize)
}

/// The element type this call belongs to, read off the receiver's prototype.
///
/// Not off the callee: a native call does not push a frame, so `current_callee` reports
/// whatever the *enclosing* bytecode frame was called with. The prototype is reliable —
/// `new` builds the object from `callee.prototype` — and it is what decides the type anyway.
fn element_of_receiver(interp: &Interp<'_>, this: &Value) -> Option<ElementType> {
    let object = this.as_object()?;
    let mut proto = object.proto();
    // Walk the chain so a subclass of a typed array still finds the type.
    while let Some(current) = proto {
        if let Some(raw) = current
            .own_property("_element")
            .and_then(|property| property.value)
        {
            let raw = raw.to_number() as i32;
            return ELEMENT_TYPES
                .into_iter()
                .map(|(element, _)| element)
                .find(|element| *element as i32 == raw);
        }
        proto = current.proto();
    }
    let _ = interp;
    None
}

/// `new Uint8Array(...)`: from a length, from a buffer, or from an array-like.
fn view_constructor(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    // A typed-array constructor cannot be called as a function: there is no receiver to
    // take an element type from, and the spec makes that a `TypeError`.
    let element = element_of_receiver(interp, &this)
        .ok_or_else(|| interp.type_error("constructor requires 'new'"))?;
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
        // `new X(length)`
        Value::Int(_) | Value::Float(_) => {
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
            let values = interp.array_like_to_args(&first)?;
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
                write_element(&buffer, element, 0, index, value);
            }
            (buffer, 0, count)
        }
    };
    let object = match this.as_object() {
        Some(object) => object.clone(),
        None => {
            let proto = interp
                .realm
                .typed_array_proto
                .clone()
                .unwrap_or_else(|| interp.realm.object_proto.clone());
            interp.new_object(Some(proto))
        }
    };
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
) {
    let Some(storage) = buffer.array_buffer_storage() else {
        return;
    };
    let mut storage = storage.borrow_mut();
    if storage.detached {
        return;
    }
    element.write(&mut storage.bytes, offset + index * element.size(), value);
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
            write_element(&buffer, element, base, offset + index, value);
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
        write_element(&buffer, element, base, index, value.clone());
    }
    Ok(Value::Object(object))
}

/// `a.slice(begin, end)`: a new view with a buffer of its own.
fn slice(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .ok_or_else(|| interp.type_error("not a typed array"))?;
    let Some(length) = object.typed_array_length() else {
        return Err(interp.type_error("not a typed array"));
    };
    let Some(element) = object.typed_array_element_type() else {
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
    for index in 0..count {
        if let Some(value) = object.typed_array_element(begin as usize + index) {
            write_element(&buffer, element, 0, index, value);
        }
    }
    let fresh = interp.new_object(Some(
        interp
            .realm
            .typed_array_proto
            .clone()
            .unwrap_or_else(|| interp.realm.object_proto.clone()),
    ));
    fresh.set_typed_array(buffer, element, 0, count);
    Ok(Value::Object(fresh))
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
