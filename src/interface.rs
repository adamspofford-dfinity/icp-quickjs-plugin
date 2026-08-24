//! Calling a canister through its Candid interface.
//!
//! A script that knows what a method's arguments and results *are* need not
//! spell out any Candid: it writes ordinary JavaScript, and the declared types
//! decide how each value encodes. The interface comes from the canister itself,
//! read out of its `candid:service` metadata section:
//!
//! ```js
//! callTyped("ledger", "transfer", { to: dest, amount: 10 });
//! callTyped(self, "increment");
//! ```
//!
//! [`CandidInterface`] is the parsed `.did`, and [`SelfTarget`] the `self` a
//! call names the canister being synced with. Argument conversion is
//! [`crate::convert::coerce`] and result conversion [`crate::convert::to_js`].

use std::cell::RefCell;
use std::rc::Rc;

use ::candid::types::value::{IDLArgs, IDLValue};
use ::candid::types::{FuncMode, Function as CandidFunction, Type, TypeEnv};
use candid_parser::utils::CandidSource;
use rquickjs::class::{Trace, Tracer};
use rquickjs::function::{Opt as OptArg, Rest};
use rquickjs::{Class, Ctx, Function, JsLifetime, Object, Result as JsResult, TypedArray, Value};

use crate::candid::{CandidArgs, arg_bytes};
use crate::convert;
use crate::engine::{map_cycles, resolve_target, throw};
use crate::icp::sync_plugin::types::{CallTarget, CallType};
use crate::{
    CanisterCallRequest, MetadataSectionRequest, canister_call, canister_metadata_section,
};

/// The metadata section every canister built with the standard tooling carries
/// its own `.did` in.
const CANDID_SECTION: &str = "candid:service";

/// Register the coerced call functions, the `CandidInterface` class, the
/// `canisterMetadata` reader they are built on, and the `self` receiver.
pub fn register(ctx: &Ctx<'_>) -> JsResult<()> {
    let globals = ctx.globals();
    Class::<CandidInterface>::define(&globals)?;
    globals.set(
        "canisterMetadata",
        Function::new(ctx.clone(), canister_metadata_js)?,
    )?;
    globals.set("callTyped", Function::new(ctx.clone(), call_typed_js)?)?;
    globals.set(
        "canisterCallTyped",
        Function::new(ctx.clone(), canister_call_typed_js)?,
    )?;
    // The sentinel only, not its constructor: `self` is the one of it there is.
    globals.set("self", Class::instance(ctx.clone(), SelfTarget {})?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Metadata sections
// ---------------------------------------------------------------------------

/// `canisterMetadata("candid:service")`, or the general
/// `canisterMetadata({ name, target, direct })`.
///
/// Returns the section's raw bytes as a `Uint8Array`, or `null` when the target
/// provably has no section by that name — including when it has no module
/// installed at all.
fn canister_metadata_js<'js>(ctx: Ctx<'js>, spec: Value<'js>) -> JsResult<Value<'js>> {
    let (name, target, direct) = match spec.as_string() {
        Some(name) => (name.to_string()?, CallTarget::Host, false),
        None => {
            let Some(opts) = spec.as_object() else {
                return Err(throw(
                    &ctx,
                    "canisterMetadata: expected a section name or an options object",
                ));
            };
            let Some(name) = opts.get::<_, Option<String>>("name")? else {
                return Err(throw(&ctx, "canisterMetadata: missing required `name`"));
            };
            let (target, _) = target_of(&ctx, opts, "canisterMetadata")?;
            let direct = opts.get::<_, Option<bool>>("direct")?.unwrap_or(false);
            (name, target, direct)
        }
    };

    let req = MetadataSectionRequest {
        target,
        name,
        direct,
    };
    match canister_metadata_section(&req) {
        Ok(Some(bytes)) => Ok(TypedArray::new(ctx, bytes)?.into_value()),
        Ok(None) => Ok(Value::new_null(ctx)),
        Err(e) => Err(throw(&ctx, &format!("canisterMetadata failed: {e}"))),
    }
}

/// Resolve the optional `target` field of an options object.
fn target_of<'js>(
    ctx: &Ctx<'js>,
    opts: &Object<'js>,
    what: &str,
) -> JsResult<(CallTarget, String)> {
    let target = opts.get::<_, Option<Value<'_>>>("target")?;
    resolve_target(ctx, target.as_ref(), what)
}

// ---------------------------------------------------------------------------
// The interface
// ---------------------------------------------------------------------------

/// A parsed Candid service interface: the types a canister's methods take and
/// return, which is what lets a call be written without any Candid at all.
///
/// Cheap to clone — the parsed types are shared.
#[rquickjs::class(rename = "CandidInterface")]
#[derive(Clone)]
pub struct CandidInterface {
    inner: Rc<Parsed>,
}

struct Parsed {
    source: String,
    env: TypeEnv,
    actor: Type,
}

/// Holds no JS values, so its GC trace is empty.
impl<'js> Trace<'js> for CandidInterface {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

// No `'js`-bound state, so the lifetime brand is the identity. See
// [`crate::principal::Principal`] for why this is written by hand.
unsafe impl<'js> JsLifetime<'js> for CandidInterface {
    type Changed<'to> = CandidInterface;
}

impl CandidInterface {
    /// Parse a `.did` file's contents. The source must declare a service.
    pub fn parse(source: &str) -> Result<Self, String> {
        let (env, actor) = CandidSource::Text(source)
            .load()
            .map_err(|e| format!("invalid Candid interface: {e}"))?;
        let actor = actor.ok_or("Candid interface declares no service")?;
        Ok(Self {
            inner: Rc::new(Parsed {
                source: source.to_string(),
                env,
                actor,
            }),
        })
    }

    /// Read the interface a canister reports for itself, out of its
    /// `candid:service` metadata section.
    pub fn fetch(target: CallTarget, label: &str, direct: bool) -> Result<Self, String> {
        let req = MetadataSectionRequest {
            target,
            name: CANDID_SECTION.to_string(),
            direct,
        };
        let section = canister_metadata_section(&req)
            .map_err(|e| format!("reading the `{CANDID_SECTION}` metadata of {label} failed: {e}"))?
            .ok_or_else(|| {
                format!(
                    "{label} has no `{CANDID_SECTION}` metadata section; pass an interface built \
                     from a declared `.did` file instead",
                )
            })?;
        let source = String::from_utf8(section).map_err(|e| {
            format!("the `{CANDID_SECTION}` metadata of {label} is not valid UTF-8: {e}")
        })?;
        Self::parse(&source)
    }

    /// The names of every method the service exposes, in declaration order.
    fn method_names(&self) -> Result<Vec<String>, String> {
        let service = self
            .inner
            .env
            .as_service(&self.inner.actor)
            .map_err(|e| e.to_string())?;
        Ok(service.iter().map(|(name, _)| name.clone()).collect())
    }

    fn method<'a>(&'a self, name: &'a str) -> Result<&'a CandidFunction, String> {
        self.inner
            .env
            .get_method(&self.inner.actor, name)
            .map_err(|_| {
                format!(
                    "the interface has no method '{name}'; it has {}",
                    self.method_names()
                        .map(|names| names.join(", "))
                        .unwrap_or_else(|e| format!("<{e}>")),
                )
            })
    }

    /// Whether the method is a (possibly composite) query.
    fn query(&self, method: &str) -> Result<bool, String> {
        Ok(self
            .method(method)?
            .modes
            .iter()
            .any(|mode| matches!(mode, FuncMode::Query | FuncMode::CompositeQuery)))
    }

    /// Encode a call to `method`, converting each value against the type the
    /// method declares for it.
    ///
    /// A lone `` candid`…` `` supplies the whole argument list, already written
    /// as Candid: its values are serialized against the declared types rather
    /// than at the types they were written with, so a bare `${7}` still lands on
    /// a `nat64` where one is wanted.
    pub fn encode_call<'js>(
        &self,
        ctx: &Ctx<'js>,
        method: &str,
        args: &[Value<'js>],
    ) -> Result<CandidArgs, String> {
        let types = self.method(method)?.args.clone();

        if let [only] = args
            && let Ok(list) = Class::<CandidArgs>::from_value(only)
        {
            let values = list.borrow().values().to_vec();
            return self.serialize(method, values, &types);
        }

        if args.len() != types.len() {
            return Err(format!(
                "{method} takes {} argument(s), got {}",
                types.len(),
                args.len(),
            ));
        }
        let values = args
            .iter()
            .zip(&types)
            .enumerate()
            .map(|(index, (value, ty))| {
                let at = format!("{method} argument {}", index + 1);
                convert::coerce(ctx, &self.inner.env, ty, value, &at)
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.serialize(method, values, &types)
    }

    fn serialize(
        &self,
        method: &str,
        values: Vec<IDLValue>,
        types: &[Type],
    ) -> Result<CandidArgs, String> {
        if values.len() != types.len() {
            return Err(format!(
                "{method} takes {} argument(s), got {}",
                types.len(),
                values.len(),
            ));
        }
        let bytes = IDLArgs::new(&values)
            .to_bytes_with_types(&self.inner.env, types)
            .map_err(|e| format!("encoding a call to {method} failed: {e}"))?;
        Ok(CandidArgs::encoded(values, bytes))
    }

    /// Decode bytes against the types `method` declares, so records come back
    /// with their field names.
    fn decode<'js>(
        &self,
        ctx: &Ctx<'js>,
        method: &str,
        what: &str,
        bytes: &[u8],
        types: &[Type],
    ) -> Result<Vec<Value<'js>>, String> {
        let args = IDLArgs::from_bytes_with_types(bytes, &self.inner.env, types)
            .map_err(|e| format!("decoding the {what} of {method} failed: {e}"))?;
        args.args
            .iter()
            .map(|value| convert::to_js(ctx, value).map_err(|e| format!("{method}: {e}")))
            .collect()
    }
}

#[rquickjs::methods]
impl CandidInterface {
    /// `new CandidInterface(source)` parses a `.did` file's contents —
    /// typically straight out of the `files` object.
    #[qjs(constructor)]
    pub fn construct(ctx: Ctx<'_>, source: String) -> JsResult<Self> {
        Self::parse(&source).map_err(|e| throw(&ctx, &e))
    }

    /// The interface a canister reports for itself. `target` selects the
    /// canister the way a call's does; omitted, it is the canister being synced.
    /// `direct` reads the metadata section straight from the target instead of
    /// through the proxy.
    #[qjs(static, rename = "fromCanister")]
    pub fn from_canister<'js>(
        ctx: Ctx<'js>,
        target: OptArg<Value<'js>>,
        direct: OptArg<bool>,
    ) -> JsResult<Self> {
        let (target, label) = resolve_target(&ctx, target.0.as_ref(), "CandidInterface")?;
        Self::fetch(target, &label, direct.0.unwrap_or(false)).map_err(|e| throw(&ctx, &e))
    }

    /// The names of every method the service exposes.
    pub fn methods(&self, ctx: Ctx<'_>) -> JsResult<Vec<String>> {
        self.method_names().map_err(|e| throw(&ctx, &e))
    }

    /// The method's Candid signature, e.g. `(nat64) -> (text) query`.
    pub fn signature(&self, ctx: Ctx<'_>, method: String) -> JsResult<String> {
        self.method(&method)
            .map(CandidFunction::to_string)
            .map_err(|e| throw(&ctx, &e))
    }

    /// Whether the method is a (possibly composite) query.
    #[qjs(rename = "isQuery")]
    pub fn is_query(&self, ctx: Ctx<'_>, method: String) -> JsResult<bool> {
        self.query(&method).map_err(|e| throw(&ctx, &e))
    }

    /// `iface.encode("method", arg1, …)` — one JavaScript value per Candid
    /// argument, converted against the type the method declares.
    pub fn encode<'js>(
        &self,
        ctx: Ctx<'js>,
        method: String,
        args: Rest<Value<'js>>,
    ) -> JsResult<CandidArgs> {
        self.encode_call(&ctx, &method, &args)
            .map_err(|e| throw(&ctx, &e))
    }

    /// Decode a response to the method: its single result, `undefined` when it
    /// returns nothing, or an array when it returns several.
    #[qjs(rename = "decodeResult")]
    pub fn decode_result<'js>(
        &self,
        ctx: Ctx<'js>,
        method: String,
        bytes: Value<'js>,
    ) -> JsResult<Value<'js>> {
        let bytes = arg_bytes(&ctx, "decodeResult", &bytes)?;
        let types = self
            .method(&method)
            .map_err(|e| throw(&ctx, &e))?
            .rets
            .clone();
        let values = self
            .decode(&ctx, &method, "response", &bytes, &types)
            .map_err(|e| throw(&ctx, &e))?;
        results(&ctx, values)
    }

    /// Decode an argument sequence for the method, as an array of one value per
    /// declared argument.
    #[qjs(rename = "decodeArgs")]
    pub fn decode_args<'js>(
        &self,
        ctx: Ctx<'js>,
        method: String,
        bytes: Value<'js>,
    ) -> JsResult<Vec<Value<'js>>> {
        let bytes = arg_bytes(&ctx, "decodeArgs", &bytes)?;
        let types = self
            .method(&method)
            .map_err(|e| throw(&ctx, &e))?
            .args
            .clone();
        self.decode(&ctx, &method, "arguments", &bytes, &types)
            .map_err(|e| throw(&ctx, &e))
    }

    /// The `.did` source the interface was parsed from.
    #[qjs(rename = "toString")]
    pub fn to_string_js(&self) -> String {
        self.inner.source.clone()
    }
}

/// A method's results as one JavaScript value: the value itself for the usual
/// one-result method, `undefined` for a method that returns nothing, and an
/// array for one that returns several.
fn results<'js>(ctx: &Ctx<'js>, mut values: Vec<Value<'js>>) -> JsResult<Value<'js>> {
    match values.len() {
        0 => Ok(Value::new_undefined(ctx.clone())),
        1 => Ok(values.pop().expect("checked length")),
        _ => {
            let array = rquickjs::Array::new(ctx.clone())?;
            for (index, value) in values.into_iter().enumerate() {
                array.set(index, value)?;
            }
            Ok(array.into_value())
        }
    }
}

// ---------------------------------------------------------------------------
// Coerced calls
// ---------------------------------------------------------------------------

/// `callTyped(receiver, method, arg1, …)` — a call whose arguments and result
/// are converted against the types the receiver's interface declares.
///
/// The interface decides whether the call is a query or an update. It is read
/// from the receiver's `candid:service` metadata, once per receiver per run;
/// `canisterCallTyped` takes a declared one instead.
fn call_typed_js<'js>(
    ctx: Ctx<'js>,
    receiver: Value<'js>,
    method: String,
    args: Rest<Value<'js>>,
) -> JsResult<Value<'js>> {
    let (target, label) = resolve_target(&ctx, Some(&receiver), "callTyped")?;
    let interface = interface_for(&ctx, &target, &label, false).map_err(|e| throw(&ctx, &e))?;
    invoke(&ctx, &interface, &target, &label, &method, &args, false, 0)
}

/// The general form:
/// `canisterCallTyped({ method, args, target, interface, direct, cycles })`.
///
/// Only `method` is required. `args` is the argument *list*, as an array or a
/// `` candid`…` ``; `target` defaults to the canister being synced; `interface`
/// overrides the one read from the receiver; `direct` and `cycles` are as in
/// `canisterCall`.
fn canister_call_typed_js<'js>(ctx: Ctx<'js>, opts: Object<'js>) -> JsResult<Value<'js>> {
    let Some(method) = opts.get::<_, Option<String>>("method")? else {
        return Err(throw(&ctx, "canisterCallTyped: missing required `method`"));
    };
    let target = opts.get::<_, Option<Value<'js>>>("target")?;
    let (target, label) = resolve_target(&ctx, target.as_ref(), "canisterCallTyped")?;
    let direct = opts.get::<_, Option<bool>>("direct")?.unwrap_or(false);
    let cycles = map_cycles(&ctx, &opts, "canisterCallTyped")?;

    let interface = match opts.get::<_, Option<Value<'js>>>("interface")? {
        Some(declared) if !declared.is_null() && !declared.is_undefined() => {
            interface_of(&ctx, &declared)?
        }
        _ => interface_for(&ctx, &target, &label, direct).map_err(|e| throw(&ctx, &e))?,
    };
    let args = arg_list(&ctx, &opts)?;

    invoke(
        &ctx, &interface, &target, &label, &method, &args, direct, cycles,
    )
}

/// Encode a call against the method's declared types, make it, and decode the
/// response against them.
#[allow(clippy::too_many_arguments)]
fn invoke<'js>(
    ctx: &Ctx<'js>,
    interface: &CandidInterface,
    target: &CallTarget,
    label: &str,
    method: &str,
    args: &[Value<'js>],
    direct: bool,
    cycles: u64,
) -> JsResult<Value<'js>> {
    let arg = interface
        .encode_call(ctx, method, args)
        .map_err(|e| throw(ctx, &e))?;
    let call_type = match interface.query(method).map_err(|e| throw(ctx, &e))? {
        true => CallType::Query,
        false => CallType::Update,
    };

    let req = CanisterCallRequest {
        target: target.clone(),
        method: method.to_string(),
        arg: arg.bytes().to_vec(),
        call_type,
        direct,
        cycles,
    };
    let response = canister_call(&req)
        .map_err(|e| throw(ctx, &format!("calling {method} on {label} failed: {e}")))?;

    let types = interface
        .method(method)
        .map_err(|e| throw(ctx, &e))?
        .rets
        .clone();
    let values = interface
        .decode(ctx, method, "response", &response, &types)
        .map_err(|e| throw(ctx, &e))?;
    results(ctx, values)
}

/// Read the optional `args` field: an array of one value per Candid argument,
/// or a `` candid`…` `` standing for the whole list.
fn arg_list<'js>(ctx: &Ctx<'js>, opts: &Object<'js>) -> JsResult<Vec<Value<'js>>> {
    let Some(args) = opts.get::<_, Option<Value<'js>>>("args")? else {
        return Ok(Vec::new());
    };
    if args.is_null() || args.is_undefined() {
        return Ok(Vec::new());
    }
    if Class::<CandidArgs>::from_value(&args).is_ok() {
        return Ok(vec![args]);
    }
    let Some(array) = args.as_array() else {
        return Err(throw(
            ctx,
            &format!(
                "canisterCallTyped: `args` must be an array of arguments, or a candid`…` \
                 argument list, got {}",
                convert::type_name(&args),
            ),
        ));
    };
    array.iter::<Value<'js>>().collect()
}

/// The interfaces already read this run, keyed by the receiver they were read
/// from and whether the read was direct.
///
/// A metadata read is a network round trip, and a script calling one canister
/// several times should not pay for one per call.
struct InterfaceCache(RefCell<Vec<(String, bool, CandidInterface)>>);

// Holds no `'js`-bound state, so the lifetime brand is the identity.
unsafe impl<'js> JsLifetime<'js> for InterfaceCache {
    type Changed<'to> = InterfaceCache;
}

/// The receiver's interface, read from its metadata the first time it is asked
/// for and remembered after.
fn interface_for(
    ctx: &Ctx<'_>,
    target: &CallTarget,
    label: &str,
    direct: bool,
) -> Result<CandidInterface, String> {
    if let Some(remembered) = recall(ctx, label, direct) {
        return Ok(remembered);
    }
    let interface = CandidInterface::fetch(target.clone(), label, direct)?;
    remember(ctx, label, direct, &interface);
    Ok(interface)
}

fn recall(ctx: &Ctx<'_>, label: &str, direct: bool) -> Option<CandidInterface> {
    let cache = ctx.userdata::<InterfaceCache>()?;
    let entries = cache.0.borrow();
    entries
        .iter()
        .find(|(at, how, _)| at == label && *how == direct)
        .map(|(_, _, interface)| interface.clone())
}

fn remember(ctx: &Ctx<'_>, label: &str, direct: bool, interface: &CandidInterface) {
    if ctx.userdata::<InterfaceCache>().is_none() {
        // Both failures here mean another borrow of the cache is live, which a
        // missed entry only costs a second metadata read.
        let _ = ctx.store_userdata(InterfaceCache(RefCell::new(Vec::new())));
    }
    if let Some(cache) = ctx.userdata::<InterfaceCache>() {
        cache
            .0
            .borrow_mut()
            .push((label.to_string(), direct, interface.clone()));
    }
}

/// The receiver of a self-call, registered as the global `self`: the canister
/// being synced, the one target that is always permitted.
///
/// A target is otherwise a canister *name*, and the canister being synced has
/// none to give — so rather than spell it as an omitted argument, a call names
/// it with this.
#[rquickjs::class(rename = "Self")]
#[derive(Clone)]
pub struct SelfTarget {}

/// Holds no JS values, so its GC trace is empty.
impl<'js> Trace<'js> for SelfTarget {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

// No `'js`-bound state, so the lifetime brand is the identity. See
// [`crate::principal::Principal`] for why this is written by hand.
unsafe impl<'js> JsLifetime<'js> for SelfTarget {
    type Changed<'to> = SelfTarget;
}

#[rquickjs::methods]
impl SelfTarget {
    #[qjs(rename = "toString")]
    pub fn to_string_js(&self) -> String {
        "self".to_string()
    }
}

/// A `CandidInterface`, or the `.did` source to parse into one.
fn interface_of<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> JsResult<CandidInterface> {
    if let Ok(interface) = Class::<CandidInterface>::from_value(value) {
        return Ok(interface.borrow().clone());
    }
    if let Some(source) = value.as_string() {
        return CandidInterface::parse(&source.to_string()?).map_err(|e| throw(ctx, &e));
    }
    Err(throw(
        ctx,
        &format!(
            "`interface` must be a CandidInterface or the `.did` source of one, got {}",
            convert::type_name(value),
        ),
    ))
}

#[cfg(test)]
mod tests {
    use crate::testing::{error, eval};

    /// A test interface covering the shapes coercion has rules for, made
    /// available to a script as `iface`.
    const DID: &str = r#"
        type Role = variant { admin; member : record { since : nat64 }; guest };
        type User = record { name : text; id : principal; role : Role; note : opt text };
        service : {
          set_authorized : (vec principal) -> ();
          add_user : (User) -> (nat64);
          get_user : (nat64) -> (opt User) query;
          pair : (record { text; nat32 }) -> ();
          blobby : (blob) -> ();
          big : (nat) -> ();
          weigh : (float64) -> (float32);
          maybe : (opt nat8) -> ();
          two : (text, nat32) -> (text, nat32);
          nothing : () -> ();
          reachable : (func (nat) -> ()) -> ();
          somewhere : (service {}) -> ();
        }
    "#;

    /// A script with the test interface bound to `iface`, and a helper that
    /// re-decodes what a method's arguments encode to so a test can read them.
    fn with_iface(script: &str) -> String {
        format!(
            r#"const iface = new CandidInterface({DID:?});
            function encoded(method, ...args) {{
                return candidDecode(iface.encode(method, ...args));
            }}
            function roundTrip(method, ...args) {{
                return iface.decodeArgs(method, iface.encode(method, ...args));
            }}
            {script}"#
        )
    }

    fn assert_iface(checks: &[(&str, &str)]) {
        eval(&with_iface(&crate::testing::assertions(checks))).unwrap();
    }

    fn iface_error(script: &str) -> String {
        error(&with_iface(script))
    }

    #[test]
    fn the_interface_reports_what_it_declares() {
        assert_iface(&[
            (
                "methods",
                "iface.methods().includes('add_user') && iface.methods().length === 12",
            ),
            (
                "signature",
                "iface.signature('get_user') === '(nat64) -> (opt User) query'",
            ),
            ("isQuery", "iface.isQuery('get_user')"),
            ("not a query", "!iface.isQuery('add_user')"),
            ("toString", "`${iface}`.includes('type Role')"),
        ]);
    }

    #[test]
    fn a_declared_type_takes_the_annotations_out_of_a_call() {
        assert_iface(&[
            // A string where a principal is wanted, and a bare integer at the
            // declared width.
            (
                "principals",
                "encoded('set_authorized', ['aaaaa-aa', canisterId]) === \
                 `(vec { principal \"aaaaa-aa\"; principal \"${canisterId}\" })`",
            ),
            (
                "widths",
                "encoded('add_user', { name: 'a', id: 'aaaaa-aa', \
                     role: { member: { since: 1234567890123 } } }).includes('1_234_567_890_123 : nat64')",
            ),
            // A tag with no payload is just its name; one with a payload is an
            // object of a single field.
            (
                "unit tag",
                "'admin' in roundTrip('add_user', \
                     { name: 'a', id: 'aaaaa-aa', role: 'admin' })[0].role",
            ),
            // An omitted `opt` field is absent rather than missing.
            (
                "omitted opt",
                "roundTrip('add_user', { name: 'a', id: 'aaaaa-aa', role: 'guest' })[0].note === null",
            ),
            ("present opt", "roundTrip('maybe', 7)[0] === 7"),
            ("null opt", "roundTrip('maybe', null)[0] === null"),
            // A tuple record takes positional elements.
            (
                "tuple",
                "encoded('pair', ['x', 7]) === '(record { \"x\"; 7 : nat32 })'",
            ),
            (
                "blob",
                "encoded('blobby', new Uint8Array([222, 173])) === '(blob \"\\\\de\\\\ad\")'",
            ),
            // A BigInt is how JavaScript writes an integer too wide for a number.
            (
                "wide",
                "encoded('big', 123456789012345678901234567890n)\
                     .includes('123_456_789_012_345_678_901_234_567_890 : nat')",
            ),
            ("float", "encoded('weigh', 1) === '(1.0 : float64)'"),
            ("no arguments", "encoded('nothing') === '()'"),
        ]);
    }

    #[test]
    fn a_candid_template_supplies_the_whole_argument_list() {
        assert_iface(&[
            // The template's values are serialized at the declared types, so a
            // bare integer still lands on nat32.
            (
                "two arguments",
                "encoded('two', candid`(\"x\", 7)`) === '(\"x\", 7 : nat32)'",
            ),
            (
                "one argument",
                "encoded('pair', candid`(record { \"x\"; 7 })`) === '(record { \"x\"; 7 : nat32 })'",
            ),
            // And an exact-encoding class where a coercion rule would not reach.
            (
                "func",
                "encoded('reachable', new Func('aaaaa-aa', 'go')) === '(func \"aaaaa-aa\".go)'",
            ),
            (
                "service",
                "encoded('somewhere', 'aaaaa-aa') === '(service \"aaaaa-aa\")'",
            ),
        ]);
    }

    #[test]
    fn a_decoded_value_reads_as_javascript() {
        assert_iface(&[
            (
                "record and variant",
                "((u) => u.name === 'a' && u.id.toText() === 'aaaaa-aa' && \
                     u.role.member.since === 1n && u.note === 'n')\
                 (roundTrip('add_user', \
                     { name: 'a', id: 'aaaaa-aa', role: { member: { since: 1 } }, note: 'n' })[0])",
            ),
            (
                "principal",
                "Principal.isPrincipal(roundTrip('set_authorized', ['aaaaa-aa'])[0][0])",
            ),
            ("wide integers are BigInt", "roundTrip('big', 1)[0] === 1n"),
            (
                "narrow integers are numbers",
                "roundTrip('pair', ['x', 7])[0]._1_ === 7",
            ),
            (
                "blob",
                "roundTrip('blobby', new Uint8Array([1]))[0][0] === 1",
            ),
            (
                "func",
                "roundTrip('reachable', new Func('aaaaa-aa', 'go'))[0].method === 'go'",
            ),
            // Several results come back as an array, none as undefined.
            (
                "several results",
                "JSON.stringify(iface.decodeResult('two', iface.encode('two', 'x', 7))) === '[\"x\",7]'",
            ),
            (
                "no results",
                "iface.decodeResult('nothing', candid`()`) === undefined",
            ),
        ]);
    }

    #[test]
    fn errors_name_the_position_and_what_was_expected() {
        for (script, expected) in [
            (
                "iface.encode('add_user', { nam: 'a', id: 'aaaaa-aa', role: 'guest' });",
                "add_user argument 1: unknown field 'nam'",
            ),
            (
                "iface.encode('add_user', { name: 'a', role: 'guest' });",
                "add_user argument 1.id: missing field of type principal",
            ),
            (
                "iface.encode('set_authorized', ['nope']);",
                "argument 1[0]: 'nope' is not a principal",
            ),
            (
                "iface.encode('add_user');",
                "add_user takes 1 argument(s), got 0",
            ),
            (
                "iface.encode('add_user', { name: 'a', id: 'aaaaa-aa', \
                     role: { member: { since: 1.5 } } });",
                "since: expected nat64, got 1.5, which is not an integer",
            ),
            (
                "iface.encode('add_user', { name: 'a', id: 'aaaaa-aa', role: 'owner' });",
                "role: unknown tag 'owner'",
            ),
            // A tag that carries a payload cannot be named on its own.
            (
                "iface.encode('add_user', { name: 'a', id: 'aaaaa-aa', role: 'member' });",
                "write it as { member: value }",
            ),
            (
                "iface.encode('pair', ['x']);",
                "expected 2 tuple element(s), got 1",
            ),
            (
                "iface.encode('add_user', { name: 7, id: 'aaaaa-aa', role: 'guest' });",
                "name: expected text, got a number",
            ),
            (
                "iface.encode('reachable', 'aaaaa-aa');",
                "build one with new Func(canister, method)",
            ),
            ("iface.encode('nope');", "no method 'nope'"),
            ("iface.encode('big', '12');", "expected nat, got a string"),
            (
                "iface.encode('add_user', new Map());",
                "argument 1 is a Map with no properties",
            ),
        ] {
            let reported = iface_error(script);
            assert!(reported.contains(expected), "{script}\n{reported}");
        }
    }

    #[test]
    fn an_invalid_interface_is_an_error() {
        assert!(error("new CandidInterface('type X = ');").contains("invalid Candid interface"));
        assert!(
            error("new CandidInterface('type X = nat;');").contains("declares no service"),
            "a `.did` with no service has no methods to call",
        );
    }

    /// A coerced call needs a host to answer it, so what a test can reach is
    /// everything the call does before that: resolving the receiver, finding
    /// the interface, and coercing the arguments.
    #[test]
    fn a_call_fails_no_later_than_its_arguments() {
        let bad = iface_error(
            "canisterCallTyped({ target: self, method: 'add_user', args: [{ nam: 'a' }], \
             interface: iface });",
        );
        assert!(bad.contains("unknown field 'nam'"), "{bad}");

        // The `interface` option takes the `.did` source as well as a parsed one.
        let source = error(&format!(
            "canisterCallTyped({{ method: 'nope', interface: {DID:?} }});"
        ));
        assert!(source.contains("no method 'nope'"), "{source}");
    }

    #[test]
    fn arguments_are_a_list() {
        let scalar =
            iface_error("canisterCallTyped({ method: 'big', args: 1, interface: iface });");
        assert!(scalar.contains("`args` must be an array"), "{scalar}");
    }

    #[test]
    fn a_receiver_is_self_or_a_canister_name() {
        // `self` is the canister being synced, which has no name to give.
        eval("if (`${self}` !== 'self') throw 'toString';").unwrap();

        let principal = error("callTyped('ryjl3-tyaaa-aaaaa-aaaba-cai', 'go');");
        assert!(
            principal.contains("a target is `self` or the name of a canister listed"),
            "{principal}",
        );
        let value = error("callTyped(7, 'go');");
        assert!(value.contains("got a number"), "{value}");
    }
}
