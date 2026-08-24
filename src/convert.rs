//! JavaScript ↔ Candid value conversion, in the three directions a script
//! needs.
//!
//! - [`to_candid`] maps a JavaScript value onto the Candid value its own
//!   literal syntax denotes, with no expected type to go on. This is what an
//!   interpolated `${…}` in the `candid` template becomes.
//! - [`coerce`] does the same against a type read from a service's interface,
//!   which is what lets a script pass `"aaaaa-aa"` where a `principal` is
//!   wanted, a bare integer where a `nat64` is, and `{ ok: 1 }` where a variant
//!   is. Every rule exists to make a JavaScript *literal* land on the type the
//!   method declares; anything else the type cannot account for is an error,
//!   never a guess.
//! - [`to_js`] maps a decoded Candid value back to the JavaScript value the
//!   same rules would have converted, so a decoded response can be passed
//!   straight back into another call.

use std::fmt::Display;
use std::str::FromStr;

use ::candid::Principal as CandidPrincipal;
use ::candid::types::value::{IDLField, IDLValue, VariantValue};
use ::candid::types::{Field, Label, Type, TypeEnv, TypeInner};
use rquickjs::{
    Array, Class, Coerced, Ctx, FromJs, Function, Object, Result as JsResult, TypedArray, Value,
};

use crate::exact;
use crate::principal::Principal;

// ---------------------------------------------------------------------------
// Untyped: JavaScript value → Candid value
// ---------------------------------------------------------------------------

/// The Candid value a JavaScript value stands for with no type to go on.
///
/// The mapping is the one Candid's own syntax gives the same literal: an
/// integer is a width-undetermined number (an `int` unless a type says
/// otherwise), a fractional number is a `float64`, a string is `text`, an array
/// is a `vec`, a `Uint8Array` is a `blob`, a `Principal` is a `principal`, and
/// any other object is a `record`. `null` and `undefined` are Candid's `null`.
/// The wrapper classes of [`crate::exact`] and [`crate::number`] say exactly
/// what no literal can.
///
/// `path` names the value in error messages, extended with the field or index
/// the offending value sits at.
pub fn to_candid<'js>(ctx: &Ctx<'js>, value: &Value<'js>, path: &str) -> Result<IDLValue, String> {
    if let Some(exact) = exact::exact_value(value, path)? {
        return Ok(exact);
    }
    if value.is_null() || value.is_undefined() {
        return Ok(IDLValue::Null);
    }
    if let Some(boolean) = value.as_bool() {
        return Ok(IDLValue::Bool(boolean));
    }
    if let Some(number) = value.as_number() {
        return to_candid_number(number, path);
    }
    if value.is_big_int() {
        // A decimal string of any width: what an integer literal parses to.
        return Ok(IDLValue::Number(js_text(ctx, value, path)?));
    }
    if let Some(text) = value.as_string() {
        return text
            .to_string()
            .map(IDLValue::Text)
            .map_err(|e| format!("{path}: {e}"));
    }
    if let Ok(principal) = Class::<Principal>::from_value(value) {
        return Ok(IDLValue::Principal(principal.borrow().inner));
    }
    if let Some(array) = value.as_array() {
        return to_candid_vec(ctx, array, path);
    }
    if let Some(object) = value.as_object() {
        if object.is_typed_array::<u8>() {
            return to_candid_blob(object, path);
        }
        if object.as_function().is_some() {
            return Err(format!("{path} is a function, which has no Candid form"));
        }
        return to_candid_record(ctx, object, path);
    }
    Err(format!(
        "{path} is {}, which has no Candid form",
        type_name(value),
    ))
}

fn to_candid_number(number: f64, path: &str) -> Result<IDLValue, String> {
    if !number.is_finite() {
        return Err(format!("{path} is {number}, which has no Candid form"));
    }
    if number.fract() == 0.0 {
        // Integral, so width-undetermined like a bare integer literal — the
        // callee's type decides how wide it encodes.
        return Ok(IDLValue::Number(format!("{number:.0}")));
    }
    Ok(IDLValue::Float64(number))
}

fn to_candid_vec<'js>(ctx: &Ctx<'js>, array: &Array<'js>, path: &str) -> Result<IDLValue, String> {
    let mut items = Vec::with_capacity(array.len());
    for (index, item) in array.iter::<Value<'js>>().enumerate() {
        let path = format!("{path}[{index}]");
        let item = item.map_err(|e| format!("{path}: {e}"))?;
        items.push(to_candid(ctx, &item, &path)?);
    }
    Ok(IDLValue::Vec(items))
}

fn to_candid_blob(object: &Object<'_>, path: &str) -> Result<IDLValue, String> {
    Ok(IDLValue::Blob(blob_bytes(object, path)?))
}

fn to_candid_record<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    path: &str,
) -> Result<IDLValue, String> {
    let mut fields = Vec::new();
    for property in object.props::<String, Value<'js>>() {
        let (name, value) = property.map_err(|e| format!("{path}: {e}"))?;
        let val = to_candid(ctx, &value, &format!("{path}.{name}"))?;
        fields.push(IDLField {
            id: parse_label(&name),
            val,
        });
    }

    if fields.is_empty() {
        check_readable(ctx, object, path)?;
    }

    // Hashed-label order, the order the parser leaves a `record { … }` in.
    fields.sort_unstable_by_key(|field| field.id.get_id());
    Ok(IDLValue::Record(fields))
}

/// Reject a `vec` whose elements do not share one Candid type. Without a
/// declared type a vector takes the type of its first element and every element
/// is then written as its own, which yields bytes no decoder can read — so a
/// mixed array is caught here rather than sent.
pub fn check_uniform_vecs(value: &IDLValue, path: &str) -> Result<(), String> {
    match value {
        IDLValue::Vec(items) => {
            let expected = items.first().map(IDLValue::value_ty);
            for (index, item) in items.iter().enumerate() {
                let found = item.value_ty();
                if Some(&found) != expected.as_ref() {
                    return Err(format!(
                        "candid: {path}[{index}] is {found} but {path}[0] is {}; a vec holds one type",
                        expected.expect("a first element exists when a later one does"),
                    ));
                }
                check_uniform_vecs(item, &format!("{path}[{index}]"))?;
            }
            Ok(())
        }
        IDLValue::Opt(inner) => check_uniform_vecs(inner, &format!("{path}?")),
        IDLValue::Record(fields) => fields
            .iter()
            .try_for_each(|field| check_uniform_vecs(&field.val, &format!("{path}.{}", field.id))),
        IDLValue::Variant(VariantValue(field, _)) => {
            check_uniform_vecs(&field.val, &format!("{path}.{}", field.id))
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Typed: JavaScript value → Candid value of a known type
// ---------------------------------------------------------------------------

/// The Candid value of type `ty` a JavaScript value stands for.
///
/// Knowing the type is what takes the annotations out of a script: an integer
/// lands on the declared width, a string becomes a `principal` where one is
/// expected, a one-entry object becomes a `variant`, and an omitted optional
/// record field is absent rather than missing. A value one of the exact-encoding
/// wrappers holds passes through as it stands — it already says what it is, and
/// Candid checks it against `ty` while serializing.
pub fn coerce<'js>(
    ctx: &Ctx<'js>,
    env: &TypeEnv,
    ty: &Type,
    value: &Value<'js>,
    path: &str,
) -> Result<IDLValue, String> {
    if let Some(exact) = exact::exact_value(value, path)? {
        return Ok(exact);
    }

    let resolved = env.trace_type(ty).map_err(|e| format!("{path}: {e}"))?;
    let mismatch = || format!("{path}: expected {ty}, got {}", type_name(value));

    Ok(match resolved.as_ref() {
        // Reserved accepts anything and encodes nothing.
        TypeInner::Reserved => IDLValue::Reserved,
        TypeInner::Null => {
            if !is_nullish(value) {
                return Err(mismatch());
            }
            IDLValue::Null
        }
        TypeInner::Bool => IDLValue::Bool(value.as_bool().ok_or_else(mismatch)?),
        TypeInner::Text => IDLValue::Text(
            value
                .as_string()
                .ok_or_else(mismatch)?
                .to_string()
                .map_err(|e| format!("{path}: {e}"))?,
        ),
        TypeInner::Nat => IDLValue::Nat(coerce_number(ctx, value, path, ty)?),
        TypeInner::Int => IDLValue::Int(coerce_number(ctx, value, path, ty)?),
        TypeInner::Nat8 => IDLValue::Nat8(coerce_number(ctx, value, path, ty)?),
        TypeInner::Nat16 => IDLValue::Nat16(coerce_number(ctx, value, path, ty)?),
        TypeInner::Nat32 => IDLValue::Nat32(coerce_number(ctx, value, path, ty)?),
        TypeInner::Nat64 => IDLValue::Nat64(coerce_number(ctx, value, path, ty)?),
        TypeInner::Int8 => IDLValue::Int8(coerce_number(ctx, value, path, ty)?),
        TypeInner::Int16 => IDLValue::Int16(coerce_number(ctx, value, path, ty)?),
        TypeInner::Int32 => IDLValue::Int32(coerce_number(ctx, value, path, ty)?),
        TypeInner::Int64 => IDLValue::Int64(coerce_number(ctx, value, path, ty)?),
        TypeInner::Float32 => IDLValue::Float32(value.as_number().ok_or_else(mismatch)? as f32),
        TypeInner::Float64 => IDLValue::Float64(value.as_number().ok_or_else(mismatch)?),
        TypeInner::Principal => IDLValue::Principal(coerce_principal(value, path, ty)?),
        TypeInner::Service(_) => IDLValue::Service(coerce_principal(value, path, ty)?),
        // A function reference is a (service, method) pair, which no JavaScript
        // literal denotes.
        TypeInner::Func(_) => {
            return Err(format!(
                "{path}: expected {ty}; build one with new Func(canister, method)",
            ));
        }
        // `null` and `undefined` mean absent; anything else is the payload. An
        // optional that must hold `null`, or another optional, needs `new Opt(…)`.
        TypeInner::Opt(inner) => {
            if is_nullish(value) {
                IDLValue::None
            } else {
                IDLValue::Opt(Box::new(coerce(
                    ctx,
                    env,
                    inner,
                    value,
                    &format!("{path}?"),
                )?))
            }
        }
        TypeInner::Vec(inner) => coerce_vec(ctx, env, inner, value, path, &mismatch)?,
        TypeInner::Record(fields) => coerce_record(ctx, env, fields, value, path, &mismatch)?,
        TypeInner::Variant(fields) => coerce_variant(ctx, env, fields, value, path, &mismatch)?,
        TypeInner::Empty => return Err(format!("{path}: no value can have type empty")),
        other => return Err(format!("{path}: unsupported Candid type {other}")),
    })
}

fn coerce_vec<'js>(
    ctx: &Ctx<'js>,
    env: &TypeEnv,
    inner: &Type,
    value: &Value<'js>,
    path: &str,
    mismatch: &impl Fn() -> String,
) -> Result<IDLValue, String> {
    // `blob` is `vec nat8`, and a Uint8Array is how a script holds bytes.
    if matches!(env.trace_type(inner).as_deref(), Ok(TypeInner::Nat8))
        && let Some(object) = value.as_object()
        && object.is_typed_array::<u8>()
    {
        return Ok(IDLValue::Blob(blob_bytes(object, path)?));
    }

    let array = value.as_array().ok_or_else(mismatch)?;
    let mut items = Vec::with_capacity(array.len());
    for (index, item) in array.iter::<Value<'js>>().enumerate() {
        let path = format!("{path}[{index}]");
        let item = item.map_err(|e| format!("{path}: {e}"))?;
        items.push(coerce(ctx, env, inner, &item, &path)?);
    }
    Ok(IDLValue::Vec(items))
}

/// Records come from an object keyed by field name, or — when the type's fields
/// are all unnamed, as a tuple's are — from an array in field order.
fn coerce_record<'js>(
    ctx: &Ctx<'js>,
    env: &TypeEnv,
    fields: &[Field],
    value: &Value<'js>,
    path: &str,
    mismatch: &impl Fn() -> String,
) -> Result<IDLValue, String> {
    if let Some(array) = value.as_array() {
        return coerce_tuple(ctx, env, fields, array, path);
    }

    let object = value.as_object().ok_or_else(mismatch)?;
    let mut given: Vec<(String, Value<'js>)> = Vec::new();
    for property in object.props::<String, Value<'js>>() {
        given.push(property.map_err(|e| format!("{path}: {e}"))?);
    }
    if given.is_empty() {
        check_readable(ctx, object, path)?;
    }

    // An unknown key is a typo, not a value to drop on the floor.
    for (name, _) in &given {
        let id = parse_label(name);
        if !fields.iter().any(|f| f.id.get_id() == id.get_id()) {
            return Err(format!(
                "{path}: unknown field '{name}'; expected {}",
                field_names(fields),
            ));
        }
    }

    let mut out = Vec::with_capacity(fields.len());
    for field in fields {
        let name = label_key(field.id.as_ref());
        let at = format!("{path}.{name}");
        let id = field.id.get_id();
        let val = match given.iter().find(|(n, _)| parse_label(n).get_id() == id) {
            Some((_, v)) => coerce(ctx, env, &field.ty, v, &at)?,
            // An omitted field is only allowed where "absent" is a value.
            None => match env.trace_type(&field.ty).as_deref() {
                Ok(TypeInner::Opt(_)) => IDLValue::None,
                Ok(TypeInner::Null) => IDLValue::Null,
                Ok(TypeInner::Reserved) => IDLValue::Reserved,
                _ => return Err(format!("{at}: missing field of type {}", field.ty)),
            },
        };
        out.push(IDLField {
            id: field.id.as_ref().clone(),
            val,
        });
    }
    Ok(IDLValue::Record(out))
}

fn coerce_tuple<'js>(
    ctx: &Ctx<'js>,
    env: &TypeEnv,
    fields: &[Field],
    array: &Array<'js>,
    path: &str,
) -> Result<IDLValue, String> {
    if fields
        .iter()
        .any(|f| matches!(f.id.as_ref(), Label::Named(_)))
    {
        return Err(format!(
            "{path}: expected an object with fields {}, got an array",
            field_names(fields),
        ));
    }
    if array.len() != fields.len() {
        return Err(format!(
            "{path}: expected {} tuple element(s), got {}",
            fields.len(),
            array.len(),
        ));
    }

    let mut out = Vec::with_capacity(fields.len());
    for (index, (field, item)) in fields.iter().zip(array.iter::<Value<'js>>()).enumerate() {
        let at = format!("{path}[{index}]");
        let item = item.map_err(|e| format!("{at}: {e}"))?;
        out.push(IDLField {
            id: field.id.as_ref().clone(),
            val: coerce(ctx, env, &field.ty, &item, &at)?,
        });
    }
    Ok(IDLValue::Record(out))
}

/// Variants come from a one-entry object (`{ ok: 1 }`), or from a bare string
/// naming a tag whose payload is `null` (`"ok"`).
fn coerce_variant<'js>(
    ctx: &Ctx<'js>,
    env: &TypeEnv,
    fields: &[Field],
    value: &Value<'js>,
    path: &str,
    mismatch: &impl Fn() -> String,
) -> Result<IDLValue, String> {
    let (tag, payload) = match value.as_string() {
        Some(text) => (text.to_string().map_err(|e| format!("{path}: {e}"))?, None),
        None => {
            let object = value.as_object().ok_or_else(mismatch)?;
            let mut entries: Vec<(String, Value<'js>)> = Vec::new();
            for property in object.props::<String, Value<'js>>() {
                entries.push(property.map_err(|e| format!("{path}: {e}"))?);
            }
            if entries.len() != 1 {
                if entries.is_empty() {
                    check_readable(ctx, object, path)?;
                }
                return Err(format!(
                    "{path}: a variant is one of {}, so it needs an object of exactly one field, got {}",
                    field_names(fields),
                    entries.len(),
                ));
            }
            let (name, payload) = entries.pop().expect("checked length");
            (name, Some(payload))
        }
    };

    let id = parse_label(&tag);
    let (index, field) = fields
        .iter()
        .enumerate()
        .find(|(_, f)| f.id.get_id() == id.get_id())
        .ok_or_else(|| {
            format!(
                "{path}: unknown tag '{tag}'; expected {}",
                field_names(fields),
            )
        })?;

    let at = format!("{path}.{tag}");
    let val = match payload {
        Some(payload) => coerce(ctx, env, &field.ty, &payload, &at)?,
        None => match env.trace_type(&field.ty).as_deref() {
            Ok(TypeInner::Null) => IDLValue::Null,
            _ => {
                return Err(format!(
                    "{at}: tag '{tag}' carries a {}; write it as {{ {tag}: value }}",
                    field.ty,
                ));
            }
        },
    };
    Ok(IDLValue::Variant(VariantValue(
        Box::new(IDLField {
            id: field.id.as_ref().clone(),
            val,
        }),
        index as u64,
    )))
}

fn field_names(fields: &[Field]) -> String {
    let names = fields
        .iter()
        .map(|f| format!("'{}'", label_key(f.id.as_ref())))
        .collect::<Vec<_>>();
    match names.split_last() {
        None => "no fields".to_string(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// Read an integer of a declared width from a JavaScript number or `BigInt`. A
/// string is not accepted: `BigInt` is how JavaScript writes an integer too wide
/// for a number, so there is no value a decimal string could add.
fn coerce_number<'js, T: FromStr>(
    ctx: &Ctx<'js>,
    value: &Value<'js>,
    path: &str,
    ty: &Type,
) -> Result<T, String>
where
    T::Err: Display,
{
    let Some(text) = integer_text(ctx, value) else {
        return Err(match value.as_number() {
            Some(number) => format!("{path}: expected {ty}, got {number}, which is not an integer"),
            None => format!("{path}: expected {ty}, got {}", type_name(value)),
        });
    };
    text.parse::<T>()
        .map_err(|e| format!("{path}: '{text}' is not a valid {ty}: {e}"))
}

/// The decimal form of an integer written as a JavaScript number or a `BigInt`.
fn integer_text<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> Option<String> {
    if let Some(number) = value.as_number() {
        if !number.is_finite() || number.fract() != 0.0 {
            return None;
        }
        return Some(format!("{number:.0}"));
    }
    if value.is_big_int() {
        return Coerced::<String>::from_js(ctx, value.clone())
            .ok()
            .map(|coerced| coerced.0);
    }
    None
}

fn coerce_principal(value: &Value<'_>, path: &str, ty: &Type) -> Result<CandidPrincipal, String> {
    principal_of(value).map_err(|e| match e {
        Some(bad) => format!("{path}: {bad}"),
        None => format!("{path}: expected {ty}, got {}", type_name(value)),
    })
}

/// A `Principal`, or the text of one. `Err(Some(..))` reports text that was
/// meant to be a principal but is not one; `Err(None)` a value that is nothing
/// like one.
pub fn principal_of(value: &Value<'_>) -> Result<CandidPrincipal, Option<String>> {
    if let Ok(principal) = Class::<Principal>::from_value(value) {
        return Ok(principal.borrow().inner);
    }
    if let Some(text) = value.as_string() {
        let text = text.to_string().map_err(|e| Some(e.to_string()))?;
        return CandidPrincipal::from_text(&text)
            .map_err(|e| Some(format!("'{text}' is not a principal: {e}")));
    }
    Err(None)
}

// ---------------------------------------------------------------------------
// Candid value → JavaScript value
// ---------------------------------------------------------------------------

/// The JavaScript value a decoded Candid value reads back as.
///
/// The inverse of the coercion rules, so a decoded response can go straight
/// back into another call: `text` is a string, a record is an object keyed by
/// field name, a variant is a one-entry object, `principal` and `service` are
/// `Principal`s, and an optional is its payload or `null`. Integers wider than
/// a JavaScript number is exact for — `nat`, `int`, `nat64`, `int64` — come
/// back as `BigInt`s.
pub fn to_js<'js>(ctx: &Ctx<'js>, value: &IDLValue) -> JsResult<Value<'js>> {
    Ok(match value {
        IDLValue::Null | IDLValue::None | IDLValue::Reserved => Value::new_null(ctx.clone()),
        IDLValue::Bool(b) => Value::new_bool(ctx.clone(), *b),
        IDLValue::Text(text) => rquickjs::String::from_str(ctx.clone(), text)?.into_value(),
        IDLValue::Number(text) => big_int(ctx, text)?,
        IDLValue::Int(i) => big_int(ctx, &i.0.to_string())?,
        IDLValue::Nat(n) => big_int(ctx, &n.0.to_string())?,
        IDLValue::Nat64(n) => big_int(ctx, &n.to_string())?,
        IDLValue::Int64(n) => big_int(ctx, &n.to_string())?,
        IDLValue::Nat8(n) => Value::new_number(ctx.clone(), f64::from(*n)),
        IDLValue::Nat16(n) => Value::new_number(ctx.clone(), f64::from(*n)),
        IDLValue::Nat32(n) => Value::new_number(ctx.clone(), f64::from(*n)),
        IDLValue::Int8(n) => Value::new_number(ctx.clone(), f64::from(*n)),
        IDLValue::Int16(n) => Value::new_number(ctx.clone(), f64::from(*n)),
        IDLValue::Int32(n) => Value::new_number(ctx.clone(), f64::from(*n)),
        IDLValue::Float32(f) => Value::new_number(ctx.clone(), f64::from(*f)),
        IDLValue::Float64(f) => Value::new_number(ctx.clone(), *f),
        // An optional reads back as its payload, the way `null` writes as an
        // absent one. `opt null` and nested optionals are the cases this loses;
        // `candidDecode` shows a response exactly.
        IDLValue::Opt(inner) => to_js(ctx, inner)?,
        IDLValue::Blob(bytes) => TypedArray::new(ctx.clone(), bytes.clone())?.into_value(),
        IDLValue::Vec(items) => {
            let array = Array::new(ctx.clone())?;
            for (index, item) in items.iter().enumerate() {
                array.set(index, to_js(ctx, item)?)?;
            }
            array.into_value()
        }
        IDLValue::Record(fields) => {
            let object = Object::new(ctx.clone())?;
            for field in fields {
                object.set(label_key(&field.id), to_js(ctx, &field.val)?)?;
            }
            object.into_value()
        }
        IDLValue::Variant(VariantValue(field, _)) => {
            let object = Object::new(ctx.clone())?;
            object.set(label_key(&field.id), to_js(ctx, &field.val)?)?;
            object.into_value()
        }
        IDLValue::Principal(p) | IDLValue::Service(p) => {
            Class::instance(ctx.clone(), Principal::from(*p))?.into_value()
        }
        IDLValue::Func(p, method) => {
            Class::instance(ctx.clone(), exact::Func::of(*p, method.clone()))?.into_value()
        }
    })
}

/// A `BigInt` of a decimal string, built through the global constructor so a
/// value of any width stays exact.
fn big_int<'js>(ctx: &Ctx<'js>, decimal: &str) -> JsResult<Value<'js>> {
    let constructor: Function<'js> = ctx.globals().get("BigInt")?;
    constructor.call((decimal,))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Read a field name the way Candid text does: `_123_` (and a bare number) is
/// the field hash itself, anything else is a name to be hashed.
pub fn parse_label(name: &str) -> Label {
    let digits = match name
        .strip_prefix('_')
        .and_then(|rest| rest.strip_suffix('_'))
    {
        Some(inner) => inner,
        None => name,
    };
    match digits.parse::<u32>() {
        Ok(id) => Label::Id(id),
        Err(_) => Label::Named(name.to_string()),
    }
}

/// The inverse of [`parse_label`]: the property name a field reads back under.
pub fn label_key(label: &Label) -> String {
    match label {
        Label::Named(name) => name.clone(),
        Label::Id(id) | Label::Unnamed(id) => format!("_{id}_"),
    }
}

/// Whether a value means "nothing there".
fn is_nullish(value: &Value<'_>) -> bool {
    value.is_null() || value.is_undefined()
}

/// A value's JavaScript string form, for a hole inside a string literal — where
/// the interpolation is textual, and JavaScript's own coercion is the rule.
pub fn js_text<'js>(ctx: &Ctx<'js>, value: &Value<'js>, path: &str) -> Result<String, String> {
    Coerced::<String>::from_js(ctx, value.clone())
        .map(|coerced| coerced.0)
        .map_err(|e| format!("{path} cannot be read as text: {e}"))
}

/// A value's type as a script would name it, phrased to follow "got": QuickJS
/// spells its own numeric types `int` and `float`, which is not what a script
/// wrote.
pub fn type_name(value: &Value<'_>) -> String {
    if value.is_null() {
        return "null".to_string();
    }
    if value.is_undefined() {
        return "undefined".to_string();
    }
    if value.is_bool() {
        return "a boolean".to_string();
    }
    if value.is_number() {
        return "a number".to_string();
    }
    if value.is_big_int() {
        return "a BigInt".to_string();
    }
    if value.is_string() {
        return "a string".to_string();
    }
    if let Some(object) = value.as_object() {
        if object.is_typed_array::<u8>() {
            return "a Uint8Array".to_string();
        }
        if object.as_function().is_some() {
            return "a function".to_string();
        }
        if value.as_array().is_some() {
            return "an array".to_string();
        }
        return match class_name(object) {
            Some(name) => format!("a {name}"),
            None => "an object".to_string(),
        };
    }
    format!("a {}", value.type_of())
}

fn blob_bytes(object: &Object<'_>, path: &str) -> Result<Vec<u8>, String> {
    let array =
        TypedArray::<u8>::from_object(object.clone()).map_err(|e| format!("{path}: {e}"))?;
    array
        .as_bytes()
        .map(<[u8]>::to_vec)
        .ok_or_else(|| format!("{path}: Uint8Array buffer is detached"))
}

/// Reject an object a record can read nothing out of. A `Map`, a `Set` or a
/// `Date` keeps its contents in internal slots, which enumerating properties
/// sees as nothing at all — a plausible thing to write and an implausible thing
/// to mean by one of those.
fn check_readable<'js>(ctx: &Ctx<'js>, object: &Object<'js>, path: &str) -> Result<(), String> {
    if is_plain_object(ctx, object) {
        return Ok(());
    }
    Err(format!(
        "{path} is a {} with no properties to read; pass a plain object, an array, a Uint8Array or a Principal",
        class_name(object).unwrap_or_else(|| "object".to_string()),
    ))
}

/// Whether the object is an object literal — its prototype `Object.prototype`,
/// or none at all — rather than an instance of some other class.
fn is_plain_object<'js>(ctx: &Ctx<'js>, object: &Object<'js>) -> bool {
    let Some(prototype) = object.get_prototype() else {
        return true;
    };
    let base = ctx
        .globals()
        .get::<_, Object<'js>>("Object")
        .and_then(|constructor| constructor.get::<_, Object<'js>>("prototype"));
    matches!(base, Ok(base) if prototype == base)
}

/// The name of the class an object was built from, for error messages.
fn class_name(object: &Object<'_>) -> Option<String> {
    let constructor = object
        .get_prototype()?
        .get::<_, Object<'_>>("constructor")
        .ok()?;
    constructor.get::<_, String>("name").ok()
}
