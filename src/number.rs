//! JavaScript-facing wrappers for the Candid number types.
//!
//! An interpolated JavaScript number is width-undetermined, so it encodes as
//! `int` — right for a method that declares `int` or `nat`, wrong for one that
//! declares a fixed width. These classes say which type is meant:
//!
//! ```js
//! candid`(record { amount = ${new Nat64(10)} })`
//! ```
//!
//! Each takes a number, a `BigInt` or a string, so a value too wide for a
//! JavaScript number (a `nat64` past 2^53, say) can still be written exactly.
//! They are write-only wrappers for encoding: a script reads one back with
//! `toString()`.

use ::candid::types::value::IDLValue;
use rquickjs::class::{Trace, Tracer};
use rquickjs::{Class, Coerced, Ctx, Exception, JsLifetime, Result as JsResult, Value};

/// Declares one class per Candid number type, mapping it to the Rust type it
/// parses into and the [`IDLValue`] variant it encodes as.
macro_rules! candid_numbers {
    ($($class:ident($name:literal, $rust:ty) => $variant:ident,)*) => {
        $(
            #[rquickjs::class(rename = $name)]
            #[derive(Clone)]
            pub struct $class {
                value: $rust,
            }

            /// Holds no JS values, so its GC trace is empty.
            impl<'js> Trace<'js> for $class {
                fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
            }

            // No `'js`-bound state, so the lifetime brand is the identity. See
            // [`crate::principal::Principal`] for why this is written by hand.
            unsafe impl<'js> JsLifetime<'js> for $class {
                type Changed<'to> = $class;
            }

            #[rquickjs::methods]
            impl $class {
                #[qjs(constructor)]
                pub fn new(ctx: Ctx<'_>, value: Coerced<String>) -> JsResult<Self> {
                    match value.0.trim().parse() {
                        Ok(value) => Ok(Self { value }),
                        Err(_) => Err(Exception::throw_type(
                            &ctx,
                            &format!("{}: '{}' is not a valid {}", $name, value.0, $name),
                        )),
                    }
                }

                /// The value in decimal, at full precision.
                #[qjs(rename = "toString")]
                pub fn to_string_js(&self) -> String {
                    self.value.to_string()
                }
            }
        )*

        /// Register every number class as a global constructor.
        pub fn register(ctx: &Ctx<'_>) -> JsResult<()> {
            let globals = ctx.globals();
            $(Class::<$class>::define(&globals)?;)*
            Ok(())
        }

        /// The Candid value a number class instance holds, if `value` is one.
        pub fn to_candid(value: &Value<'_>) -> Option<IDLValue> {
            $(
                if let Ok(number) = Class::<$class>::from_value(value) {
                    return Some(IDLValue::$variant(number.borrow().value.clone()));
                }
            )*
            None
        }
    };
}

candid_numbers! {
    Nat("Nat", ::candid::Nat) => Nat,
    Int("Int", ::candid::Int) => Int,
    Nat8("Nat8", u8) => Nat8,
    Nat16("Nat16", u16) => Nat16,
    Nat32("Nat32", u32) => Nat32,
    Nat64("Nat64", u64) => Nat64,
    Int8("Int8", i8) => Int8,
    Int16("Int16", i16) => Int16,
    Int32("Int32", i32) => Int32,
    Int64("Int64", i64) => Int64,
    Float32("Float32", f32) => Float32,
    Float64("Float64", f64) => Float64,
}
