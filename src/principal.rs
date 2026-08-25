//! A JavaScript-facing wrapper around [`candid::Principal`].
//!
//! The class mirrors the `Principal` of
//! [icp-js-core](https://github.com/dfinity/icp-js-core), minus
//! `selfAuthenticating`: the same statics (`from`, `fromText`,
//! `fromUint8Array`, `fromHex`, `anonymous`, `managementCanister`,
//! `isPrincipal`) and the same instance methods (`isAnonymous`, `toText`,
//! `toUint8Array`, `toHex`, `toString`, `toJSON`, `compareTo`, `ltEq`, `gtEq`),
//! so a script can be written the way one would be against that package.
//!
//! Scripts also receive principals ready-made in the injected `canister`,
//! `identity` and `proxy` globals.

use std::cmp::Ordering;

use rquickjs::class::{Trace, Tracer};
use rquickjs::{
    Class, Coerced, Ctx, Exception, FromJs, JsLifetime, Object, Result as JsResult, TypedArray,
    Value,
};

/// The key `toJSON` puts the textual principal under, as in icp-js-core.
const JSON_KEY: &str = "__principal__";

/// Newtype so we can implement JS class bindings on a foreign type. Holds no JS
/// values, so its GC trace is empty.
#[rquickjs::class(rename = "Principal")]
#[derive(Clone)]
pub struct Principal {
    pub inner: candid::Principal,
}

impl<'js> Trace<'js> for Principal {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

// `candid::Principal` carries no `'js`-bound state, so the lifetime brand is the
// identity. This is the documented pattern for an all-`'static` class type (the
// `JsLifetime` derive can't be used: it requires every field to itself be
// `JsLifetime`, which the foreign `candid::Principal` is not).
unsafe impl<'js> JsLifetime<'js> for Principal {
    type Changed<'to> = Principal;
}

impl From<candid::Principal> for Principal {
    fn from(inner: candid::Principal) -> Self {
        Self { inner }
    }
}

#[rquickjs::methods]
impl Principal {
    /// `new Principal(…)` takes what `Principal.from` does. icp-js-core keeps
    /// its constructor protected, and a script has no use for one that takes
    /// unvalidated bytes.
    #[qjs(constructor)]
    pub fn new<'js>(ctx: Ctx<'js>, value: Value<'js>) -> JsResult<Self> {
        Self::coerce(ctx, value)
    }

    /// A textual principal, raw principal bytes, or another `Principal`.
    #[qjs(static, rename = "from")]
    pub fn coerce<'js>(ctx: Ctx<'js>, value: Value<'js>) -> JsResult<Self> {
        if let Some(text) = value.as_string() {
            return Self::from_text(ctx, text.to_string()?);
        }
        if let Ok(bytes) = TypedArray::<u8>::from_value(value.clone()) {
            return Self::from_uint8_array(ctx, bytes);
        }
        if let Ok(principal) = Class::<Principal>::from_value(&value) {
            return Ok(principal.borrow().clone());
        }
        let shown = Coerced::<String>::from_js(&ctx, value.clone())
            .map(|coerced| coerced.0)
            .unwrap_or_else(|_| value.type_of().to_string());
        Err(Exception::throw_type(
            &ctx,
            &format!(
                "cannot convert '{shown}' to a Principal: expected a textual principal, principal bytes, or another Principal",
            ),
        ))
    }

    /// The textual form, e.g. `"ryjl3-tyaaa-aaaaa-aaaba-cai"`. Throws when the
    /// text is not a principal, checksum included.
    #[qjs(static, rename = "fromText")]
    pub fn from_text(ctx: Ctx<'_>, text: String) -> JsResult<Self> {
        candid::Principal::from_text(&text)
            .map(Self::from)
            .map_err(|e| Exception::throw_type(&ctx, &format!("invalid principal '{text}': {e}")))
    }

    /// Raw principal bytes.
    #[qjs(static, rename = "fromUint8Array")]
    pub fn from_uint8_array(ctx: Ctx<'_>, bytes: TypedArray<'_, u8>) -> JsResult<Self> {
        let bytes = bytes
            .as_bytes()
            .ok_or_else(|| Exception::throw_type(&ctx, "Uint8Array buffer is detached"))?;
        candid::Principal::try_from_slice(bytes)
            .map(Self::from)
            .map_err(|e| Exception::throw_type(&ctx, &format!("invalid principal bytes: {e}")))
    }

    /// Raw principal bytes, hex-encoded.
    #[qjs(static, rename = "fromHex")]
    pub fn from_hex(ctx: Ctx<'_>, hex: String) -> JsResult<Self> {
        let bytes = ::hex::decode(&hex)
            .map_err(|e| Exception::throw_type(&ctx, &format!("invalid principal hex: {e}")))?;
        candid::Principal::try_from_slice(&bytes)
            .map(Self::from)
            .map_err(|e| Exception::throw_type(&ctx, &format!("invalid principal bytes: {e}")))
    }

    /// The anonymous principal, `"2vxsx-fae"`.
    #[qjs(static)]
    pub fn anonymous() -> Self {
        Self::from(candid::Principal::anonymous())
    }

    /// The management canister, `"aaaaa-aa"`.
    #[qjs(static, rename = "managementCanister")]
    pub fn management_canister() -> Self {
        Self::from(candid::Principal::management_canister())
    }

    /// Whether a value is a `Principal`.
    #[qjs(static, rename = "isPrincipal")]
    pub fn is_principal(value: Value<'_>) -> bool {
        Class::<Principal>::from_value(&value).is_ok()
    }

    /// The marker icp-js-core recognizes its own principals by.
    #[qjs(get, rename = "_isPrincipal")]
    pub fn is_principal_marker(&self) -> bool {
        true
    }

    #[qjs(rename = "isAnonymous")]
    pub fn is_anonymous(&self) -> bool {
        self.inner == candid::Principal::anonymous()
    }

    #[qjs(rename = "toText")]
    pub fn to_text(&self) -> String {
        self.inner.to_text()
    }

    /// Raw principal bytes as a `Uint8Array`.
    #[qjs(rename = "toUint8Array")]
    pub fn to_uint8_array<'js>(&self, ctx: Ctx<'js>) -> JsResult<TypedArray<'js, u8>> {
        TypedArray::new(ctx, self.inner.as_slice())
    }

    #[qjs(rename = "toHex")]
    pub fn to_hex(&self) -> String {
        ::hex::encode(self.inner.as_slice())
    }

    /// String coercion (`` `${p}` ``, `String(p)`) yields the textual principal.
    #[qjs(rename = "toString")]
    pub fn to_string_js(&self) -> String {
        self.inner.to_text()
    }

    /// `{ "__principal__": "<text>" }`, the shape icp-js-core serializes to.
    #[qjs(rename = "toJSON")]
    pub fn to_json<'js>(&self, ctx: Ctx<'js>) -> JsResult<Object<'js>> {
        let json = Object::new(ctx)?;
        json.set(JSON_KEY, self.inner.to_text())?;
        Ok(json)
    }

    /// `"lt"`, `"eq"` or `"gt"`, comparing the principals byte by byte.
    #[qjs(rename = "compareTo")]
    pub fn compare_to<'js>(&self, other: Class<'js, Principal>) -> &'static str {
        match self.inner.as_slice().cmp(other.borrow().inner.as_slice()) {
            Ordering::Less => "lt",
            Ordering::Equal => "eq",
            Ordering::Greater => "gt",
        }
    }

    #[qjs(rename = "ltEq")]
    pub fn lt_eq<'js>(&self, other: Class<'js, Principal>) -> bool {
        self.inner.as_slice() <= other.borrow().inner.as_slice()
    }

    #[qjs(rename = "gtEq")]
    pub fn gt_eq<'js>(&self, other: Class<'js, Principal>) -> bool {
        self.inner.as_slice() >= other.borrow().inner.as_slice()
    }
}

/// Register the `Principal` class as a global.
pub fn register(ctx: &Ctx<'_>) -> JsResult<()> {
    Class::<Principal>::define(&ctx.globals())
}

#[cfg(test)]
mod tests {
    use crate::testing::{assert_script, error};

    /// The management canister, whose principal is short enough to spell out:
    /// text "aaaaa-aa", bytes 0x04 for the anonymous one below.
    const MANAGEMENT: &str = "aaaaa-aa";

    #[test]
    fn the_class_matches_icp_js_core() {
        assert_script(&[
            // Constructors.
            (
                "fromText",
                &format!("Principal.fromText('{MANAGEMENT}').toText() === '{MANAGEMENT}'"),
            ),
            (
                "from text",
                &format!("Principal.from('{MANAGEMENT}').toText() === '{MANAGEMENT}'"),
            ),
            (
                "from bytes",
                "Principal.from(new Uint8Array([4])).toText() === '2vxsx-fae'",
            ),
            (
                "from Principal",
                "Principal.from(Principal.anonymous()).isAnonymous()",
            ),
            (
                "fromUint8Array",
                "Principal.fromUint8Array(new Uint8Array([4])).toText() === '2vxsx-fae'",
            ),
            (
                "fromHex",
                "Principal.fromHex('04').toText() === '2vxsx-fae'",
            ),
            (
                "anonymous",
                "Principal.anonymous().toText() === '2vxsx-fae'",
            ),
            (
                "managementCanister",
                &format!("Principal.managementCanister().toText() === '{MANAGEMENT}'"),
            ),
            (
                "new",
                &format!("new Principal('{MANAGEMENT}').toText() === '{MANAGEMENT}'"),
            ),
            // Predicates.
            ("isPrincipal", "Principal.isPrincipal(canister)"),
            ("isPrincipal on text", "!Principal.isPrincipal(canisterId)"),
            ("_isPrincipal", "canister._isPrincipal === true"),
            ("isAnonymous", "Principal.anonymous().isAnonymous()"),
            ("isAnonymous on other", "!canister.isAnonymous()"),
            // Conversions.
            ("toText", "canister.toText() === canisterId"),
            ("toString", "`${canister}` === canisterId"),
            (
                "toUint8Array",
                "canister.toUint8Array() instanceof Uint8Array",
            ),
            (
                "toHex",
                "canister.toUint8Array().toHex() === canister.toHex()",
            ),
            (
                "round trip",
                "Principal.fromHex(canister.toHex()).toText() === canisterId",
            ),
            (
                "toJSON",
                "JSON.stringify(canister) === `{\"__principal__\":\"${canisterId}\"}`",
            ),
            // Ordering, which is byte-wise with a shorter prefix sorting first.
            ("compareTo eq", "canister.compareTo(canister) === 'eq'"),
            (
                "compareTo lt",
                &format!("Principal.fromText('{MANAGEMENT}').compareTo(canister) === 'lt'"),
            ),
            (
                "compareTo gt",
                &format!("canister.compareTo(Principal.fromText('{MANAGEMENT}')) === 'gt'"),
            ),
            (
                "ltEq",
                "canister.ltEq(canister) && !canister.gtEq(Principal.anonymous())",
            ),
            ("gtEq", "canister.gtEq(canister)"),
        ]);
    }

    #[test]
    fn an_invalid_principal_is_an_error() {
        assert!(error("Principal.fromText('nope');").contains("invalid principal 'nope'"));
        assert!(error("Principal.fromHex('zz');").contains("invalid principal hex"));
        let number = error("Principal.from(7);");
        assert!(
            number.contains("cannot convert '7' to a Principal"),
            "{number}"
        );
    }
}
