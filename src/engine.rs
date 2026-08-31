//! Builds the QuickJS context the plugin runs scripts on, wiring in every
//! capability a sync plugin has: canister calls, environment variable updates,
//! the sync inputs, read-only filesystem access over WASI, and
//! Candid/principal/encoding helpers.

use ::candid::Principal as CandidPrincipal;
use rquickjs::function::{Opt as OptArg, Rest};
use rquickjs::{
    Class, Coerced, Ctx, Exception, FromJs, Function, Object, Persistent, Result as JsResult,
    TypedArray, Value,
};
use sha2::{Digest, Sha256};

use crate::candid::{self, arg_bytes};
use crate::convert;
use crate::icp::sync_plugin::types::{CallTarget, CallType};
use crate::interface::SelfTarget;
use crate::principal::{self, Principal};
use crate::{
    CanisterCallRequest, SetEnvironmentVariableRequest, SyncExecInput, canister_call,
    canister_set_environment_variable,
};
use crate::{exact, fs, interface, number};

/// Run the entry script with all capabilities wired in. Returns the plugin's
/// `exec` result: `Ok(())` on a clean run, or a human-readable error string on
/// any failure.
///
/// The script source comes from whichever of the two `script` declarations the
/// manifest step uses: the `script` field holds the source inline, and the file
/// declared under the `script` key holds it on disk. Declaring both is an error.
/// Every declared file — the entry script included — stays visible to the script
/// in the `files` object.
pub fn run(input: SyncExecInput) -> Result<(), String> {
    let (script_name, script_src) = entry_script(&input)?;

    let runtime =
        rquickjs::Runtime::new().map_err(|e| format!("failed to start JS runtime: {e}"))?;
    let context = rquickjs::Context::full(&runtime)
        .map_err(|e| format!("failed to build JS context: {e}"))?;

    let rejections = context
        .with(RejectionLog::install)
        .map_err(|e| format!("failed to install the promise rejection tracker: {e}"))?;
    let tracked = rejections.clone();
    runtime.set_host_promise_rejection_tracker(Some(Box::new(
        move |ctx, promise, reason, is_handled| tracked.track(&ctx, promise, reason, is_handled),
    )));

    context.with(|ctx| {
        install(&ctx, &input).map_err(|e| describe_error(&ctx, e))?;
        ctx.eval::<(), _>(script_src.as_bytes())
            .map_err(|e| format!("{script_name}: {}", describe_error(&ctx, e)))
    })?;

    run_jobs(&runtime, &script_name)?;
    context.with(|ctx| rejections.into_result(&ctx, &script_name))
}

// ---------------------------------------------------------------------------
// Promise jobs. The script runs to completion first, but the promises it made
// have not: QuickJS queues every `.then` callback, every `queueMicrotask` and
// every resumption of an `await` as a job, and runs none of them on its own.
// So the step is over only once the queue has drained — and a rejection nobody
// handled is the step's error, since the work it stood for did not happen.
// ---------------------------------------------------------------------------

/// Run every queued job, and every job those queue in turn, until none is left.
///
/// A job that throws outright — a `queueMicrotask` callback, say, which has no
/// promise to reject — leaves its exception pending on the context it ran in,
/// and fails the step with it.
fn run_jobs(runtime: &rquickjs::Runtime, script_name: &str) -> Result<(), String> {
    loop {
        match runtime.execute_pending_job() {
            Ok(true) => continue,
            Ok(false) => return Ok(()),
            Err(exception) => {
                let reported = exception
                    .0
                    .with(|ctx| describe_error(&ctx, rquickjs::Error::Exception));
                return Err(format!("{script_name}: {reported}"));
            }
        }
    }
}

/// The rejected promises nothing has handled, kept as a JS-side `Map` from the
/// promise to the reason it was rejected with.
///
/// QuickJS reports a rejection the moment it happens, before a handler that
/// arrives in a later job could have been attached, and reports that late
/// handler separately — so the two reports have to be matched up by promise,
/// and matching them means the engine's own notion of object identity. A `Map`
/// keyed by the promise is that identity, and holding the log in JavaScript
/// keeps it out of reach of the script, which never sees this object.
#[derive(Clone)]
struct RejectionLog(Persistent<Object<'static>>);

impl RejectionLog {
    fn install(ctx: Ctx<'_>) -> JsResult<Self> {
        let log: Object<'_> = ctx.eval(
            br#"(() => {
                const pending = new Map();
                return {
                    rejected: (promise, reason) => { pending.set(promise, reason); },
                    handled: (promise) => { pending.delete(promise); },
                    unhandled: () => [...pending.values()],
                };
            })()"#,
        )?;
        Ok(Self(Persistent::save(&ctx, log)))
    }

    /// Record a rejection, or forget one that turned out to be handled after
    /// all. The reason is rendered here rather than kept as a value, so the log
    /// holds what the error message will say and nothing that can change under
    /// it afterwards.
    fn track<'js>(&self, ctx: &Ctx<'js>, promise: Value<'js>, reason: Value<'js>, handled: bool) {
        let Ok(log) = self.0.clone().restore(ctx) else {
            return;
        };
        let recorded = if handled {
            log.get::<_, Function<'js>>("handled")
                .and_then(|f| f.call::<_, ()>((promise,)))
        } else {
            let reason = describe_value(ctx, reason);
            log.get::<_, Function<'js>>("rejected")
                .and_then(|f| f.call::<_, ()>((promise, reason)))
        };
        // The log is ours and its calls cannot throw, so a failure here means
        // the engine is already unwinding — and this callback has no way to
        // report it that would not itself be swallowed.
        let _ = recorded;
    }

    /// The step's result: an error naming what was rejected and never handled,
    /// or `Ok(())` when nothing was.
    fn into_result(self, ctx: &Ctx<'_>, script_name: &str) -> Result<(), String> {
        let reasons = self
            .0
            .restore(ctx)
            .and_then(|log| {
                log.get::<_, Function<'_>>("unhandled")?
                    .call::<_, Vec<String>>(())
            })
            .unwrap_or_default();

        match reasons.as_slice() {
            [] => Ok(()),
            [only] => Err(format!(
                "{script_name}: unhandled promise rejection: {only}"
            )),
            many => Err(format!(
                "{script_name}: {} unhandled promise rejections: {}",
                many.len(),
                many.join("; "),
            )),
        }
    }
}

/// Resolve the entry script to a `(name for error messages, source)` pair.
fn entry_script(input: &SyncExecInput) -> Result<(String, String), String> {
    let field = input.fields.iter().find(|f| f.name == "script");
    let mut files = input
        .files
        .iter()
        .filter(|f| f.key.as_deref() == Some("script"));
    let file = files.next();

    if let Some(extra) = files.next() {
        return Err(format!(
            "the `script` key maps to more than one file ('{}' and '{}'); it must name exactly one JavaScript script",
            file.expect("first file precedes the second").name,
            extra.name,
        ));
    }

    match (field, file) {
        (Some(field), None) => Ok(("<script field>".to_string(), field.value.clone())),
        (None, Some(file)) => Ok((file.name.clone(), file.content.clone())),
        (Some(_), Some(file)) => Err(format!(
            "the step declares both a `script` field and a `script` file ('{}'); use one or the other",
            file.name,
        )),
        (None, None) => Err(
            "no script provided: declare the JavaScript source in a `script` field, or point the `script` file key at a JavaScript script"
                .to_string(),
        ),
    }
}

/// Register every host-provided capability and inject the sync inputs as
/// script-visible globals.
fn install(ctx: &Ctx<'_>, input: &SyncExecInput) -> JsResult<()> {
    principal::register(ctx)?;
    number::register(ctx)?;
    exact::register(ctx)?;
    register_output(ctx)?;
    register_canister_calls(ctx)?;
    register_environment(ctx)?;
    candid::register(ctx)?;
    interface::register(ctx)?;
    register_encoding(ctx)?;
    register_random(ctx)?;
    fs::register(ctx)?;
    inject_inputs(ctx, input)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Output: `print`/`console.log` are transient stdout; `eprint`/`console.error`
// and `console.warn` are persistent stderr. Mirrors the plugin stdio contract
// in the WIT docs. QuickJS-as-a-library ships neither `print` nor `console`.
// ---------------------------------------------------------------------------

fn register_output(ctx: &Ctx<'_>) -> JsResult<()> {
    let globals = ctx.globals();
    globals.set("print", Function::new(ctx.clone(), print_stdout)?)?;
    globals.set("eprint", Function::new(ctx.clone(), print_stderr)?)?;

    let console = Object::new(ctx.clone())?;
    console.set("log", Function::new(ctx.clone(), print_stdout)?)?;
    console.set("info", Function::new(ctx.clone(), print_stdout)?)?;
    console.set("debug", Function::new(ctx.clone(), print_stderr)?)?;
    console.set("warn", Function::new(ctx.clone(), print_stderr)?)?;
    console.set("error", Function::new(ctx.clone(), print_stderr)?)?;
    globals.set("console", console)?;
    Ok(())
}

fn print_stdout(args: Rest<Coerced<String>>) {
    println!("{}", join_args(args));
}

fn print_stderr(args: Rest<Coerced<String>>) {
    eprintln!("{}", join_args(args));
}

fn join_args(args: Rest<Coerced<String>>) -> String {
    args.iter()
        .map(|c| c.0.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Canister calls
// ---------------------------------------------------------------------------

/// Register `canisterCall` and its `callUpdate` / `callQuery` shorthands.
fn register_canister_calls(ctx: &Ctx<'_>) -> JsResult<()> {
    let globals = ctx.globals();
    globals.set(
        "canisterCall",
        Function::new(ctx.clone(), canister_call_js)?,
    )?;
    globals.set("callUpdate", Function::new(ctx.clone(), call_update_js)?)?;
    globals.set("callQuery", Function::new(ctx.clone(), call_query_js)?)?;
    Ok(())
}

/// The general form:
/// `canisterCall({ method, arg, query, direct, cycles, target })`.
/// Only `method` is required; the rest default to an empty-arg update call to
/// the canister being synced, routed through the proxy (if configured) with no
/// cycles. `target` names a canister listed in the step's `canisters:` (see
/// [`resolve_target`]); omitted, it targets the canister being synced.
fn canister_call_js<'js>(ctx: Ctx<'js>, opts: Object<'js>) -> JsResult<TypedArray<'js, u8>> {
    let method: String = match opts.get::<_, Option<String>>("method")? {
        Some(m) => m,
        None => return Err(throw(&ctx, "canisterCall: missing required `method`")),
    };

    let arg = match opts.get::<_, Option<Value<'js>>>("arg")? {
        Some(a) if !a.is_null() && !a.is_undefined() => arg_bytes(&ctx, "canisterCall `arg`", &a)?,
        _ => Vec::new(),
    };

    let call_type = if opts.get::<_, Option<bool>>("query")?.unwrap_or(false) {
        CallType::Query
    } else {
        CallType::Update
    };
    let direct = opts.get::<_, Option<bool>>("direct")?.unwrap_or(false);
    let cycles = map_cycles(&ctx, &opts, "canisterCall")?;
    let target = opts.get::<_, Option<Value<'js>>>("target")?;
    let (target, _) = resolve_target(&ctx, target.as_ref(), "canisterCall")?;

    host_call(&ctx, target, method, arg, call_type, direct, cycles)
}

/// `callUpdate(receiver, method, arg)` — an update call to the receiver, which
/// is `self` or the name of a canister listed in the step's `canisters:`, as a
/// coerced call's is. `arg` is optional, and omitted is an empty argument list.
fn call_update_js<'js>(
    ctx: Ctx<'js>,
    receiver: Value<'js>,
    method: String,
    arg: OptArg<Value<'js>>,
) -> JsResult<TypedArray<'js, u8>> {
    shorthand_call(&ctx, "callUpdate", receiver, method, arg, CallType::Update)
}

/// `callQuery(receiver, method, arg)` — the query form of [`call_update_js`].
fn call_query_js<'js>(
    ctx: Ctx<'js>,
    receiver: Value<'js>,
    method: String,
    arg: OptArg<Value<'js>>,
) -> JsResult<TypedArray<'js, u8>> {
    shorthand_call(&ctx, "callQuery", receiver, method, arg, CallType::Query)
}

/// What the two shorthands share: resolve the receiver, read the argument, and
/// make the call with the defaults `canisterCall` would apply.
fn shorthand_call<'js>(
    ctx: &Ctx<'js>,
    what: &str,
    receiver: Value<'js>,
    method: String,
    arg: OptArg<Value<'js>>,
    call_type: CallType,
) -> JsResult<TypedArray<'js, u8>> {
    let (target, _) = resolve_target(ctx, Some(&receiver), what)?;
    let arg = match arg.0 {
        Some(arg) if !arg.is_null() && !arg.is_undefined() => {
            arg_bytes(ctx, &format!("{what} `arg`"), &arg)?
        }
        _ => Vec::new(),
    };
    host_call(ctx, target, method, arg, call_type, false, 0)
}

/// Invoke the host `canister-call` import, mapping its error string into a JS
/// exception so scripts can `try`/`catch` or let it abort the run.
fn host_call<'js>(
    ctx: &Ctx<'js>,
    target: CallTarget,
    method: String,
    arg: Vec<u8>,
    call_type: CallType,
    direct: bool,
    cycles: u64,
) -> JsResult<TypedArray<'js, u8>> {
    let req = CanisterCallRequest {
        target,
        method,
        arg,
        call_type,
        direct,
        cycles,
    };
    match canister_call(&req) {
        Ok(bytes) => TypedArray::new(ctx.clone(), bytes),
        Err(e) => Err(throw(ctx, &format!("canisterCall failed: {e}"))),
    }
}

/// Read the optional `cycles` field (a non-negative integer) from the options.
pub(crate) fn map_cycles(ctx: &Ctx<'_>, opts: &Object<'_>, what: &str) -> JsResult<u64> {
    match opts.get::<_, Option<i64>>("cycles")? {
        Some(n) => u64::try_from(n)
            .map_err(|_| throw(ctx, &format!("{what}: `cycles` must be non-negative"))),
        None => Ok(0),
    }
}

/// Resolve a script-provided target into a [`CallTarget`] and a description of
/// it for error messages.
///
/// `self`, a missing target and `null` are all the canister being synced.
/// Anything else names a canister, spelled exactly as the sync step's
/// `canisters:` list does — a bare local name for a canister in the same
/// subproject, or a `subproject:local` key otherwise. The host resolves that
/// name and rejects a target the step did not list.
///
/// A principal is not a target: the host takes names only, so that the
/// permission it checks is the one the manifest granted. When the principal is
/// one the project knows, the error says which name to write instead.
pub(crate) fn resolve_target<'js>(
    ctx: &Ctx<'js>,
    target: Option<&Value<'js>>,
    what: &str,
) -> JsResult<(CallTarget, String)> {
    let host = || (CallTarget::Host, "the canister being synced".to_string());
    let Some(value) = target else {
        return Ok(host());
    };
    if value.is_null() || value.is_undefined() {
        return Ok(host());
    }
    if Class::<SelfTarget>::from_value(value).is_ok() {
        return Ok(host());
    }

    let text = match value.as_string() {
        Some(text) => text.to_string()?,
        None => {
            let named = convert::principal_of(value)
                .ok()
                .map(|p| p.to_text())
                .unwrap_or_default();
            return Err(throw(
                ctx,
                &format!(
                    "{what}: a target is `self` or the name of a canister listed in the step's \
                     `canisters:`, got {}{}",
                    convert::type_name(value),
                    name_hint(ctx, &named),
                ),
            ));
        }
    };
    if CandidPrincipal::from_text(&text).is_ok() {
        return Err(throw(
            ctx,
            &format!(
                "{what}: a target is `self` or the name of a canister listed in the step's \
                 `canisters:`, not the principal '{text}'{}",
                name_hint(ctx, &text),
            ),
        ));
    }
    Ok((CallTarget::Name(text.clone()), format!("canister {text}")))
}

/// The name the project knows a principal by, if it knows one, so an error
/// about a principal target can say what to write instead.
fn name_hint(ctx: &Ctx<'_>, principal: &str) -> String {
    if principal.is_empty() {
        return String::new();
    }
    let Ok(ids) = ctx.globals().get::<_, Object<'_>>("canisterIds") else {
        return String::new();
    };
    let found = ids
        .props::<String, String>()
        .flatten()
        .find(|(_, id)| id == principal);
    match found {
        Some((name, _)) => format!("; the project calls that canister '{name}'"),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Canister environment variables
// ---------------------------------------------------------------------------

/// Register `canisterSetenv`.
fn register_environment(ctx: &Ctx<'_>) -> JsResult<()> {
    ctx.globals().set(
        "canisterSetenv",
        Function::new(ctx.clone(), canister_setenv_js)?,
    )
}

/// `canisterSetenv(receiver, name, value, options)` — the receiver first, the
/// way a call shorthand names its own.
///
/// Sets one of the receiver's runtime environment variables, leaving its other
/// variables — and the rest of its settings — as they are. The receiver is
/// `self` or the name of a canister listed in the step's `canisters:` (see
/// [`resolve_target`]). The options are `{ direct }` and may be omitted or
/// `null`; `direct` is false by default, which lets the proxy make the update
/// when one is configured.
fn canister_setenv_js<'js>(
    ctx: Ctx<'js>,
    receiver: Value<'js>,
    name: String,
    value: Value<'js>,
    options: OptArg<Value<'js>>,
) -> JsResult<()> {
    let (target, _) = resolve_target(&ctx, Some(&receiver), "canisterSetenv")?;
    let value = setenv_value(&ctx, &value)?;
    let direct = setenv_direct(&ctx, options)?;

    let req = SetEnvironmentVariableRequest {
        target,
        name,
        value,
        direct,
    };
    canister_set_environment_variable(&req)
        .map_err(|e| throw(&ctx, &format!("canisterSetenv failed: {e}")))
}

/// The trailing options, whose one field is `direct`. Everything else about the
/// update is positional, so a field this does not know is a mistake worth naming
/// rather than a setting silently dropped.
fn setenv_direct<'js>(ctx: &Ctx<'js>, options: OptArg<Value<'js>>) -> JsResult<bool> {
    let Some(options) = options.0.filter(|v| !v.is_null() && !v.is_undefined()) else {
        return Ok(false);
    };
    let Some(options) = options.as_object() else {
        return Err(throw(
            ctx,
            &format!(
                "canisterSetenv: options are an object with a `direct` field, got {}",
                convert::type_name(&options),
            ),
        ));
    };

    let unknown: Vec<String> = options
        .keys::<String>()
        .flatten()
        .filter(|key| key != "direct")
        .map(|key| format!("`{key}`"))
        .collect();
    if !unknown.is_empty() {
        return Err(throw(
            ctx,
            &format!(
                "canisterSetenv: `direct` is the only option, got {}",
                unknown.join(", "),
            ),
        ));
    }
    Ok(options.get::<_, Option<bool>>("direct")?.unwrap_or(false))
}

/// The value to set, which is a string: the canister reads the variable back
/// verbatim, so how a value that is not one renders is the script's to say
/// rather than a coercion's to guess.
fn setenv_value<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> JsResult<String> {
    match value.as_string() {
        Some(text) => text.to_string(),
        None => Err(throw(
            ctx,
            &format!(
                "canisterSetenv: a value is a string, got {}; convert it first — `String(x)`, or \
                 `x.toText()` for a Principal",
                convert::type_name(value),
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// Encoding helpers, for what the engine itself has no answer to. JSON is native
// (`JSON.parse`/`JSON.stringify`), and so are hex and base64 — a `Uint8Array`
// carries `toHex()`/`toBase64()`, with `Uint8Array.fromHex(..)`/`fromBase64(..)`
// to read them back.
// ---------------------------------------------------------------------------

fn register_encoding(ctx: &Ctx<'_>) -> JsResult<()> {
    let globals = ctx.globals();
    globals.set("sha256", Function::new(ctx.clone(), sha256)?)?;
    // QuickJS ships no TextEncoder/TextDecoder, and the bytes a metadata
    // section or a file comes back as are usually text.
    globals.set("encodeUtf8", Function::new(ctx.clone(), encode_utf8)?)?;
    globals.set("decodeUtf8", Function::new(ctx.clone(), decode_utf8)?)?;
    Ok(())
}

fn sha256<'js>(ctx: Ctx<'js>, bytes: TypedArray<'js, u8>) -> JsResult<TypedArray<'js, u8>> {
    let digest = Sha256::digest(bytes_of(&ctx, "sha256", &bytes)?);
    TypedArray::new(ctx, digest.to_vec())
}

fn encode_utf8<'js>(ctx: Ctx<'js>, text: String) -> JsResult<TypedArray<'js, u8>> {
    TypedArray::new(ctx, text.into_bytes())
}

fn decode_utf8(ctx: Ctx<'_>, bytes: TypedArray<'_, u8>) -> JsResult<String> {
    String::from_utf8(bytes_of(&ctx, "decodeUtf8", &bytes)?)
        .map_err(|e| throw(&ctx, &format!("decodeUtf8 failed: {e}")))
}

// ---------------------------------------------------------------------------
// Randomness, from the host's `wasi:random`. `Math.random` is QuickJS's own
// PRNG, seeded from the clock and fine for a sample or a jitter; this is the
// host's cryptographic randomness, for a nonce, a salt or a subaccount that
// has to be unguessable.
// ---------------------------------------------------------------------------

/// The most bytes one call will produce. Nothing a script needs randomness for
/// is anywhere near this large, so a bigger request is a mistake worth naming
/// rather than an allocation worth making.
const MAX_RANDOM_BYTES: f64 = 1024.0 * 1024.0;

fn register_random(ctx: &Ctx<'_>) -> JsResult<()> {
    ctx.globals()
        .set("randomBytes", Function::new(ctx.clone(), random_bytes)?)
}

/// `randomBytes(count)` — `count` cryptographically random bytes.
fn random_bytes<'js>(ctx: Ctx<'js>, count: f64) -> JsResult<TypedArray<'js, u8>> {
    if !count.is_finite() || count.fract() != 0.0 || count < 0.0 {
        return Err(throw(
            &ctx,
            &format!("randomBytes: expected a whole number of bytes, got {count}"),
        ));
    }
    if count > MAX_RANDOM_BYTES {
        return Err(throw(
            &ctx,
            &format!(
                "randomBytes: {count} bytes is more than the {MAX_RANDOM_BYTES} this returns at once"
            ),
        ));
    }

    let mut bytes = vec![0u8; count as usize];
    getrandom::fill(&mut bytes).map_err(|e| throw(&ctx, &format!("randomBytes failed: {e}")))?;
    TypedArray::new(ctx, bytes)
}

// ---------------------------------------------------------------------------
// Sync inputs, injected as script-visible globals.
// ---------------------------------------------------------------------------

fn inject_inputs(ctx: &Ctx<'_>, input: &SyncExecInput) -> JsResult<()> {
    let globals = ctx.globals();

    let canister = CandidPrincipal::from_text(&input.canister_id).map_err(|e| {
        throw(
            ctx,
            &format!(
                "host passed an invalid canister id '{}': {e}",
                input.canister_id
            ),
        )
    })?;
    let identity = CandidPrincipal::from_text(&input.identity_principal).map_err(|e| {
        throw(
            ctx,
            &format!(
                "host passed an invalid identity principal '{}': {e}",
                input.identity_principal
            ),
        )
    })?;

    globals.set("canisterId", input.canister_id.clone())?;
    globals.set("canister", Principal::from(canister))?;
    globals.set("environment", input.environment.clone())?;
    globals.set("identityId", input.identity_principal.clone())?;
    globals.set("identity", Principal::from(identity))?;

    match &input.proxy_canister_id {
        Some(text) => {
            let p = CandidPrincipal::from_text(text).map_err(|e| {
                throw(
                    ctx,
                    &format!("host passed an invalid proxy principal '{text}': {e}"),
                )
            })?;
            globals.set("proxy", Principal::from(p))?;
        }
        // Explicitly `null`, so an absent proxy reads as a value the host passed
        // rather than as the `undefined` of a global that was never set.
        None => globals.set("proxy", Value::new_null(ctx.clone()))?,
    }

    let dirs: Vec<&str> = input.dirs.iter().map(|d| d.path.as_str()).collect();
    globals.set("dirs", dirs)?;
    globals.set(
        "files",
        string_map(ctx, input.files.iter().map(|f| (&f.name, &f.content)))?,
    )?;
    // The manifest keys `dirs:`/`files:` were declared under, if any, grouped for
    // lookup: a key maps to every path declared beneath it, in declaration order.
    // Plain-list entries carry no key and appear only in `dirs`/`files`.
    globals.set(
        "dirKeys",
        group_by_key(ctx, input.dirs.iter().map(|d| (d.key.as_deref(), &d.path)))?,
    )?;
    globals.set(
        "fileKeys",
        group_by_key(ctx, input.files.iter().map(|f| (f.key.as_deref(), &f.name)))?,
    )?;
    globals.set(
        "fields",
        string_map(ctx, input.fields.iter().map(|f| (&f.name, &f.value)))?,
    )?;
    // Name → textual principal, as passed by the host. A script can wrap a value
    // in `Principal.from(..)`, or hand it straight to a `canisterCall` `target`.
    globals.set(
        "canisterIds",
        string_map(ctx, input.canister_ids.iter().map(|e| (&e.name, &e.id)))?,
    )?;

    Ok(())
}

/// Build a JS object from string key/value pairs.
fn string_map<'js, 'a>(
    ctx: &Ctx<'js>,
    entries: impl Iterator<Item = (&'a String, &'a String)>,
) -> JsResult<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    for (k, v) in entries {
        obj.set(k.as_str(), v.as_str())?;
    }
    Ok(obj)
}

/// Group declared paths by the manifest map key they were declared under,
/// dropping the entries that have none. Each key maps to an array of paths in
/// declaration order, since one key may name several paths.
fn group_by_key<'js, 'a>(
    ctx: &Ctx<'js>,
    entries: impl Iterator<Item = (Option<&'a str>, &'a String)>,
) -> JsResult<Object<'js>> {
    // Grouped in Rust first, so the keys are written to the object exactly once
    // and never read back through its prototype chain.
    let mut grouped: Vec<(&str, Vec<&str>)> = Vec::new();
    for (key, path) in entries {
        let Some(key) = key else { continue };
        match grouped.iter_mut().find(|(k, _)| *k == key) {
            Some((_, paths)) => paths.push(path),
            None => grouped.push((key, vec![path.as_str()])),
        }
    }

    let obj = Object::new(ctx.clone())?;
    for (key, paths) in grouped {
        obj.set(key, paths)?;
    }
    Ok(obj)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Copy a `Uint8Array`'s bytes, erroring if its backing buffer was detached.
pub(crate) fn bytes_of(ctx: &Ctx<'_>, name: &str, arr: &TypedArray<'_, u8>) -> JsResult<Vec<u8>> {
    arr.as_bytes()
        .map(<[u8]>::to_vec)
        .ok_or_else(|| throw(ctx, &format!("{name}: Uint8Array buffer is detached")))
}

/// Build a JS exception carrying `msg` and return it as an [`rquickjs::Error`]
/// suitable for `?`/`Err`.
pub(crate) fn throw(ctx: &Ctx<'_>, msg: &str) -> rquickjs::Error {
    Exception::throw_message(ctx, msg)
}

/// Render an error from eval or setup into a human-readable string, pulling the
/// thrown value's message and stack out of the context when one is pending.
fn describe_error(ctx: &Ctx<'_>, err: rquickjs::Error) -> String {
    if !err.is_exception() {
        return err.to_string();
    }
    describe_value(ctx, ctx.catch())
}

/// Render a thrown or rejected value: an `Error`'s message and stack when it is
/// one, and whatever the value coerces to otherwise, since a script may throw
/// or reject with anything at all.
fn describe_value<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> String {
    if let Some(exception) = value.clone().into_exception() {
        return exception.to_string();
    }
    match Coerced::<String>::from_js(ctx, value) {
        Ok(Coerced(s)) => s,
        Err(_) => "unknown JavaScript exception".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DirInput, FieldInput, FileInput};

    /// A step declaring the entry script under the `script` file key, two files
    /// under a shared `seed` key, and one keyed and one plain-list directory.
    fn input(script: &str) -> SyncExecInput {
        SyncExecInput {
            canister_id: "ryjl3-tyaaa-aaaaa-aaaba-cai".to_string(),
            environment: "local".to_string(),
            dirs: vec![
                DirInput {
                    key: Some("assets".into()),
                    path: "assets".into(),
                },
                DirInput {
                    key: None,
                    path: "plain".into(),
                },
            ],
            files: vec![
                FileInput {
                    key: Some("script".into()),
                    name: "sync.js".into(),
                    content: script.into(),
                },
                FileInput {
                    key: Some("seed".into()),
                    name: "a.json".into(),
                    content: "1".into(),
                },
                FileInput {
                    key: Some("seed".into()),
                    name: "b.json".into(),
                    content: "2".into(),
                },
            ],
            fields: vec![FieldInput {
                name: "mode".into(),
                value: "fast".into(),
            }],
            identity_principal: "aaaaa-aa".to_string(),
            proxy_canister_id: None,
            canister_ids: vec![],
        }
    }

    /// Run the shared `[description, condition]` assertions against a step
    /// that declares the script as a file, the way [`input`] builds it.
    fn assert_script(checks: &[(&str, &str)]) {
        run(input(&crate::testing::assertions(checks))).unwrap();
    }

    #[test]
    fn sync_inputs_are_visible() {
        assert_script(&[
            ("canisterId", "canisterId === `${canister}`"),
            ("identityId", "identityId === `${identity}`"),
            ("environment", "environment === 'local'"),
            ("proxy", "proxy === null"),
            ("fields", "fields.mode === 'fast'"),
            ("canisterIds", "Object.keys(canisterIds).length === 0"),
        ]);
    }

    #[test]
    fn declared_dirs_and_files_are_visible() {
        assert_script(&[
            // Plain-list entries appear in `dirs`/`files` but under no key.
            ("dirs", "JSON.stringify(dirs) === '[\"assets\",\"plain\"]'"),
            (
                "dirKeys",
                "JSON.stringify(dirKeys) === '{\"assets\":[\"assets\"]}'",
            ),
            // One key may name several files, and the entry script stays visible.
            (
                "fileKeys",
                "JSON.stringify(fileKeys.seed) === '[\"a.json\",\"b.json\"]'",
            ),
            (
                "seed contents",
                "fileKeys.seed.map((p) => files[p]).join() === '1,2'",
            ),
            ("script file", "typeof files['sync.js'] === 'string'"),
        ]);
    }

    #[test]
    fn script_field_is_an_alternative_to_a_script_file() {
        let mut input = input("");
        input.files.retain(|f| f.key.as_deref() != Some("script"));
        input.fields.push(FieldInput {
            name: "script".into(),
            value: "if (Object.keys(fileKeys).length !== 1) throw 'fileKeys';".into(),
        });
        run(input).unwrap();
    }

    #[test]
    fn declaring_both_script_kinds_is_an_error() {
        let mut input = input("");
        input.fields.push(FieldInput {
            name: "script".into(),
            value: String::new(),
        });
        assert!(run(input).unwrap_err().contains("use one or the other"));
    }

    #[test]
    fn a_script_key_naming_two_files_is_an_error() {
        let mut input = input("");
        input.files.push(FileInput {
            key: Some("script".into()),
            name: "other.js".into(),
            content: String::new(),
        });
        assert!(run(input).unwrap_err().contains("more than one file"));
    }

    #[test]
    fn declaring_no_script_is_an_error() {
        let mut input = input("");
        input.files.retain(|f| f.key.as_deref() != Some("script"));
        assert!(run(input).unwrap_err().contains("no script provided"));
    }

    /// A shorthand names its receiver first, the way a coerced call does. A
    /// call needs a host to answer it, so what a test can reach is what the
    /// shorthand does before that: resolving the receiver and reading the
    /// argument.
    #[test]
    fn a_shorthand_names_its_receiver_first() {
        for (script, expected) in [
            (
                "callUpdate('ryjl3-tyaaa-aaaaa-aaaba-cai', 'go', candid`()`);",
                "callUpdate: a target is `self` or the name of a canister listed",
            ),
            (
                "callQuery(7, 'go');",
                "callQuery: a target is `self` or the name of a canister listed",
            ),
            (
                "callUpdate(self, 'go', 'nope');",
                "callUpdate `arg`: expected a Uint8Array or a CandidArgs",
            ),
        ] {
            let reported = crate::testing::error(script);
            assert!(reported.contains(expected), "{script}\n{reported}");
        }
    }

    /// `canisterSetenv` names its receiver first too. The update needs a host
    /// to make it, so what a test reaches is the checking that precedes it.
    #[test]
    fn setenv_names_its_receiver_first() {
        for (script, expected) in [
            (
                "canisterSetenv('ryjl3-tyaaa-aaaaa-aaaba-cai', 'SEEDED_BY', 'local');",
                "canisterSetenv: a target is `self` or the name of a canister listed",
            ),
            (
                "canisterSetenv(self, 'SEEDED_BY', 7);",
                "canisterSetenv: a value is a string, got a number",
            ),
            (
                "canisterSetenv(self, 'SEEDED_BY', undefined);",
                "canisterSetenv: a value is a string, got undefined",
            ),
            (
                "canisterSetenv(self, 'SEEDED_BY', 'local', { target: 'ledger' });",
                "canisterSetenv: `direct` is the only option, got `target`",
            ),
        ] {
            let reported = crate::testing::error(script);
            assert!(reported.contains(expected), "{script}\n{reported}");
        }
    }

    #[test]
    fn a_thrown_value_becomes_the_step_error() {
        let err = run(input("throw 'nope';")).unwrap_err();
        assert!(err.contains("sync.js"), "{err}");
        assert!(err.contains("nope"), "{err}");
    }

    /// The step is not over when the script's last statement is: what a script
    /// queued has to run too, or the work it stands for silently never happens.
    #[test]
    fn queued_jobs_run_before_the_step_ends() {
        let reported = crate::testing::error(
            "queueMicrotask(() => { throw new Error('the microtask ran'); });",
        );
        assert!(reported.contains("the microtask ran"), "{reported}");
    }

    /// An `await` resumes in a job of its own, so the rest of an async function
    /// runs only if the queue drains to the end.
    ///
    /// The check has to be made from inside a job, and a job has no way to
    /// report success — so it reports which of the two outcomes it saw by
    /// throwing it, and the test reads that back as the step's error.
    #[test]
    fn an_await_resumes_before_the_step_ends() {
        let reported = crate::testing::error(
            "let resumed = false;
             (async () => { await null; resumed = true; })();
             queueMicrotask(() => { throw resumed ? 'resumed' : 'never resumed'; });",
        );
        assert!(reported.contains("resumed"), "{reported}");
        assert!(!reported.contains("never resumed"), "{reported}");
    }

    #[test]
    fn an_unhandled_rejection_fails_the_step() {
        for (script, expected) in [
            (
                "(async () => { await null; throw 'after the await'; })();",
                "unhandled promise rejection: after the await",
            ),
            (
                "Promise.reject(new Error('rejected outright'));",
                "rejected outright",
            ),
            (
                "Promise.resolve().then(() => { throw 'from a then'; });",
                "unhandled promise rejection: from a then",
            ),
            (
                "Promise.reject('one'); Promise.reject('two');",
                "2 unhandled promise rejections: one; two",
            ),
        ] {
            let reported = crate::testing::error(script);
            assert!(reported.contains(expected), "{script}\n{reported}");
        }
    }

    /// A rejection someone handles is not the step's problem — including one
    /// whose handler is attached in a later job, which QuickJS reports as
    /// unhandled first and thinks better of afterwards.
    #[test]
    fn a_handled_rejection_leaves_the_step_alone() {
        for script in [
            "Promise.reject('caught').catch(() => {});",
            "(async () => { try { await Promise.reject('caught'); } catch (e) {} })();",
            "Promise.resolve().then(() => { throw 'caught'; }).catch(() => {});",
            "const p = Promise.reject('late'); queueMicrotask(() => p.catch(() => {}));",
        ] {
            crate::testing::eval(script).unwrap_or_else(|e| panic!("{script}\n{e}"));
        }
    }

    #[test]
    fn random_bytes_are_random_bytes() {
        crate::testing::assert_script(&[
            ("length", "randomBytes(32).length === 32"),
            ("type", "randomBytes(1) instanceof Uint8Array"),
            ("none", "randomBytes(0).length === 0"),
            // Two 16-byte draws colliding is not something to plan around.
            (
                "distinct",
                "randomBytes(16).toHex() !== randomBytes(16).toHex()",
            ),
        ]);
    }

    #[test]
    fn random_bytes_takes_a_whole_count_it_can_answer() {
        for (script, expected) in [
            ("randomBytes(-1);", "expected a whole number of bytes"),
            ("randomBytes(1.5);", "expected a whole number of bytes"),
            ("randomBytes(NaN);", "expected a whole number of bytes"),
            ("randomBytes(2 ** 30);", "more than the"),
        ] {
            let reported = crate::testing::error(script);
            assert!(reported.contains(expected), "{script}\n{reported}");
        }
    }
}
