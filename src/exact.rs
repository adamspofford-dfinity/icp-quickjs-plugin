//! JavaScript-facing classes for the Candid values no JavaScript literal
//! denotes.
//!
//! The `candid` template maps each interpolated literal onto the Candid value
//! its own syntax gives it — a string is `text`, an object is a `record`, an
//! array is a `vec`. What is left over is a variant, a canister or function
//! reference, an optional distinct from `null`, and a tuple of mixed types, so
//! each of those has a class that says exactly what is meant:
//!
//! ```js
//! candid`(record { role = ${new Variant("member", { since: 1 })} })`
//! candid`(${new Opt(null)}, ${new Service(canister)}, ${new Tuple("x", 7)})`
//! ```
//!
//! Like the number classes in [`crate::number`] these are wrappers for
//! encoding: a script reads one back with `toString()`, which renders the value
//! as Candid text. They are accepted by a coerced call too, where they say the
//! script means this value and not whatever the declared type would have
//! coerced.

use ::candid::Principal as CandidPrincipal;
use ::candid::types::Label;
use ::candid::types::value::{IDLField, IDLValue, VariantValue};
use rquickjs::class::{Trace, Tracer};
use rquickjs::function::{Opt as OptArg, Rest};
use rquickjs::{Class, Ctx, JsLifetime, Result as JsResult, Value};

use crate::candid::CandidArgs;
use crate::convert;
use crate::engine::throw;
use crate::number;

/// A `variant` value: a tag, and the value it carries.
#[rquickjs::class(rename = "Variant")]
#[derive(Clone)]
pub struct Variant {
    value: IDLValue,
}

#[rquickjs::methods]
impl Variant {
    /// `new Variant(tag)` for a tag that carries nothing, `new Variant(tag,
    /// value)` for one that carries a value. The tag is read the way a Candid
    /// field name is: `_123_` is the hash itself, anything else is hashed.
    #[qjs(constructor)]
    pub fn new<'js>(ctx: Ctx<'js>, tag: String, payload: OptArg<Value<'js>>) -> JsResult<Self> {
        let val = match payload.0 {
            Some(payload) if !payload.is_undefined() => {
                convert::to_candid(&ctx, &payload, &format!("Variant('{tag}')"))
                    .map_err(|e| throw(&ctx, &e))?
            }
            _ => IDLValue::Null,
        };
        let field = IDLField {
            id: convert::parse_label(&tag),
            val,
        };
        // The index is fixed up against the variant's declared type when the
        // value is serialized; on its own a variant has only the one field.
        Ok(Self {
            value: IDLValue::Variant(VariantValue(Box::new(field), 0)),
        })
    }

    /// The tag, as it was written.
    #[qjs(get)]
    pub fn tag(&self) -> String {
        match &self.value {
            IDLValue::Variant(VariantValue(field, _)) => convert::label_key(&field.id),
            _ => unreachable!("a Variant holds a variant value"),
        }
    }

    #[qjs(rename = "toString")]
    pub fn to_string_js(&self) -> String {
        self.value.to_string()
    }
}

/// An `opt` value, present or absent.
#[rquickjs::class(rename = "Opt")]
#[derive(Clone)]
pub struct Opt {
    value: IDLValue,
}

#[rquickjs::methods]
impl Opt {
    /// `new Opt(value)` for a present optional, `new Opt()` for an absent one.
    ///
    /// A coerced call already reads `null` as an absent optional and anything
    /// else as a present one, so this is for the two cases that rule cannot
    /// spell: an optional holding `null` (`new Opt(null)`) and an optional of an
    /// optional (`new Opt(new Opt(1))`).
    #[qjs(constructor)]
    pub fn new<'js>(ctx: Ctx<'js>, value: OptArg<Value<'js>>) -> JsResult<Self> {
        let value = match value.0 {
            Some(value) if !value.is_undefined() => IDLValue::Opt(Box::new(
                convert::to_candid(&ctx, &value, "Opt(…)").map_err(|e| throw(&ctx, &e))?,
            )),
            _ => IDLValue::None,
        };
        Ok(Self { value })
    }

    /// Whether the optional is present.
    #[qjs(get, rename = "hasValue")]
    pub fn has_value(&self) -> bool {
        matches!(self.value, IDLValue::Opt(_))
    }

    #[qjs(rename = "toString")]
    pub fn to_string_js(&self) -> String {
        self.value.to_string()
    }
}

/// A `service` value: a reference to a canister.
#[rquickjs::class(rename = "Service")]
#[derive(Clone)]
pub struct Service {
    value: IDLValue,
}

#[rquickjs::methods]
impl Service {
    /// `new Service(canister)`, from a `Principal` or a textual principal.
    #[qjs(constructor)]
    pub fn new<'js>(ctx: Ctx<'js>, canister: Value<'js>) -> JsResult<Self> {
        Ok(Self {
            value: IDLValue::Service(as_principal(&ctx, &canister, "Service")?),
        })
    }

    /// The canister referred to.
    #[qjs(get)]
    pub fn canister(&self) -> crate::principal::Principal {
        match &self.value {
            IDLValue::Service(p) => crate::principal::Principal::from(*p),
            _ => unreachable!("a Service holds a service value"),
        }
    }

    #[qjs(rename = "toString")]
    pub fn to_string_js(&self) -> String {
        self.value.to_string()
    }
}

/// A `func` value: a reference to one method of one canister.
#[rquickjs::class(rename = "Func")]
#[derive(Clone)]
pub struct Func {
    value: IDLValue,
}

impl Func {
    /// The value a decoded `func` reads back as.
    pub fn of(canister: CandidPrincipal, method: String) -> Self {
        Self {
            value: IDLValue::Func(canister, method),
        }
    }

    fn parts(&self) -> (&CandidPrincipal, &String) {
        match &self.value {
            IDLValue::Func(canister, method) => (canister, method),
            _ => unreachable!("a Func holds a func value"),
        }
    }
}

#[rquickjs::methods]
impl Func {
    /// `new Func(canister, method)`, taking a `Principal` or a textual
    /// principal and the method's name.
    #[qjs(constructor)]
    pub fn new<'js>(ctx: Ctx<'js>, canister: Value<'js>, method: String) -> JsResult<Self> {
        Ok(Self::of(as_principal(&ctx, &canister, "Func")?, method))
    }

    /// The canister the method lives on.
    #[qjs(get)]
    pub fn canister(&self) -> crate::principal::Principal {
        crate::principal::Principal::from(*self.parts().0)
    }

    /// The method's name.
    #[qjs(get)]
    pub fn method(&self) -> String {
        self.parts().1.clone()
    }

    #[qjs(rename = "toString")]
    pub fn to_string_js(&self) -> String {
        self.value.to_string()
    }
}

/// A tuple: a `record` whose fields are numbered rather than named.
#[rquickjs::class(rename = "Tuple")]
#[derive(Clone)]
pub struct Tuple {
    value: IDLValue,
}

#[rquickjs::methods]
impl Tuple {
    /// `new Tuple(a, b, …)`. An array is a `vec`, which holds one type, so a
    /// tuple of mixed types is written this way instead.
    #[qjs(constructor)]
    pub fn new<'js>(ctx: Ctx<'js>, items: Rest<Value<'js>>) -> JsResult<Self> {
        let mut fields = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let val = convert::to_candid(&ctx, item, &format!("Tuple[{index}]"))
                .map_err(|e| throw(&ctx, &e))?;
            fields.push(IDLField {
                id: Label::Id(index as u32),
                val,
            });
        }
        Ok(Self {
            value: IDLValue::Record(fields),
        })
    }

    /// How many elements the tuple has.
    #[qjs(get)]
    pub fn length(&self) -> usize {
        match &self.value {
            IDLValue::Record(fields) => fields.len(),
            _ => unreachable!("a Tuple holds a record value"),
        }
    }

    #[qjs(rename = "toString")]
    pub fn to_string_js(&self) -> String {
        self.value.to_string()
    }
}

/// Boilerplate shared by every wrapper: an empty GC trace (each holds a Candid
/// value, never a JavaScript one), the identity lifetime brand that goes with
/// it, and the Rust-side accessor conversion reads them through.
macro_rules! exact_classes {
    ($($class:ident,)*) => {
        $(
            impl<'js> Trace<'js> for $class {
                fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
            }

            // No `'js`-bound state, so the lifetime brand is the identity. See
            // [`crate::principal::Principal`] for why this is written by hand.
            unsafe impl<'js> JsLifetime<'js> for $class {
                type Changed<'to> = $class;
            }
        )*

        /// Register every exact-encoding class as a global constructor.
        pub fn register(ctx: &Ctx<'_>) -> JsResult<()> {
            let globals = ctx.globals();
            $(Class::<$class>::define(&globals)?;)*
            Ok(())
        }

        /// The Candid value one of the wrappers holds, if `value` is one.
        fn wrapped(value: &Value<'_>) -> Option<IDLValue> {
            $(
                if let Ok(wrapper) = Class::<$class>::from_value(value) {
                    return Some(wrapper.borrow().value.clone());
                }
            )*
            None
        }
    };
}

exact_classes! {
    Variant,
    Opt,
    Service,
    Func,
    Tuple,
}

/// The Candid value a script has already pinned down exactly, if `value` is one
/// of the things that does so: an exact-encoding wrapper, a number class, or a
/// one-value `` candid`…` `` template.
///
/// `path` names the value in error messages.
pub fn exact_value(value: &Value<'_>, path: &str) -> Result<Option<IDLValue>, String> {
    if let Some(value) = wrapped(value) {
        return Ok(Some(value));
    }
    // A `Nat32` and friends, which say what an integer's width is.
    if let Some(value) = number::to_candid(value) {
        return Ok(Some(value));
    }
    if let Ok(args) = Class::<CandidArgs>::from_value(value) {
        return args.borrow().single(path).map(Some);
    }
    Ok(None)
}

/// A `Principal` or the text of one, for the reference-type constructors.
fn as_principal<'js>(ctx: &Ctx<'js>, value: &Value<'js>, what: &str) -> JsResult<CandidPrincipal> {
    convert::principal_of(value).map_err(|e| {
        let message = match e {
            Some(bad) => format!("{what}: {bad}"),
            None => format!(
                "{what}: expected a Principal or its text, got {}",
                convert::type_name(value),
            ),
        };
        throw(ctx, &message)
    })
}
