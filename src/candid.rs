//! Candid support for the scripting API, built around a template tag.
//!
//! A script writes a call argument as Candid source and interpolates
//! JavaScript values into it:
//!
//! ```js
//! candid`(record { to = ${dest}; amount = ${10} })`
//! ```
//!
//! Interpolation is never textual. Splicing a value into the source would let
//! whatever it stringifies to rewrite the argument around it, so instead each
//! `${…}` is spelled as a placeholder string the template cannot contain, the
//! source is parsed with those placeholders in place, and the JavaScript values
//! are grafted onto the parsed value tree afterwards — as a whole Candid value
//! where the placeholder stands alone, and as text where it sits inside a
//! string literal.
//!
//! What a placeholder cannot stand for is anything the parser resolves while
//! parsing: a type annotation, a field name, or the contents of a `principal`
//! or `blob` literal. Those are reported as errors naming the `${…}` at fault.
//!
//! The tag yields a [`CandidArgs`], which carries the argument *values* and not
//! just the bytes they encode to. A call that knows the method's declared types
//! can then serialize those values against them rather than re-reading bytes
//! that were written without them.

use std::ops::Range;

use ::candid::types::value::{IDLArgs, IDLValue, VariantValue};
use candid_parser::parse_idl_args;
use rquickjs::class::{Trace, Tracer};
use rquickjs::function::Rest;
use rquickjs::{Class, Ctx, FromJs, Function, JsLifetime, Result as JsResult, TypedArray, Value};

use crate::convert::{self, check_uniform_vecs, js_text, to_candid};
use crate::engine::{bytes_of, throw};

/// Register the Candid template tag, the `CandidArgs` class and the textual
/// encode/decode helpers.
pub fn register(ctx: &Ctx<'_>) -> JsResult<()> {
    let globals = ctx.globals();
    Class::<CandidArgs>::define(&globals)?;
    globals.set("candid", Function::new(ctx.clone(), candid_template)?)?;
    globals.set("candidEncode", Function::new(ctx.clone(), candid_encode)?)?;
    globals.set("candidDecode", Function::new(ctx.clone(), candid_decode)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The argument list
// ---------------------------------------------------------------------------

/// A Candid argument list: what the `candid` template tag, `candidEncode` and
/// `CandidInterface.encode` all produce, and what a call takes as its `arg`.
///
/// It holds the argument values, not only the bytes they encode to untyped, so
/// a call against a known interface can serialize them at the types the method
/// declares — a bare `${7}` lands on a `nat64` where one is wanted. `toString()`
/// renders the list as Candid text and `toUint8Array()` gives the encoded bytes.
#[rquickjs::class(rename = "CandidArgs")]
#[derive(Clone)]
pub struct CandidArgs {
    values: Vec<IDLValue>,
    /// The untyped encoding, computed up front so a malformed argument list is
    /// reported where it was written rather than where it is used.
    bytes: Vec<u8>,
}

/// Holds no JS values, so its GC trace is empty.
impl<'js> Trace<'js> for CandidArgs {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

// No `'js`-bound state, so the lifetime brand is the identity. See
// [`crate::principal::Principal`] for why this is written by hand.
unsafe impl<'js> JsLifetime<'js> for CandidArgs {
    type Changed<'to> = CandidArgs;
}

impl CandidArgs {
    /// Build an argument list from values, checking that it encodes at all.
    pub fn new(values: Vec<IDLValue>) -> Result<Self, String> {
        for (index, value) in values.iter().enumerate() {
            check_uniform_vecs(value, &format!("argument {}", index + 1))?;
        }
        let bytes = IDLArgs::new(&values)
            .to_bytes()
            .map_err(|e| format!("candid: {e}"))?;
        Ok(Self { values, bytes })
    }

    /// Build an argument list from bytes already encoded against known types.
    pub fn encoded(values: Vec<IDLValue>, bytes: Vec<u8>) -> Self {
        Self { values, bytes }
    }

    /// The argument values.
    pub fn values(&self) -> &[IDLValue] {
        &self.values
    }

    /// The encoded bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The one value the list holds, for a `` candid`…` `` written where a
    /// single value is wanted rather than a whole argument list.
    pub fn single(&self, path: &str) -> Result<IDLValue, String> {
        match self.values.as_slice() {
            [only] => Ok(only.clone()),
            values => Err(format!(
                "{path}: a candid`…` template stands for one value here, but this one holds {}; \
                 write the value on its own",
                values.len(),
            )),
        }
    }
}

#[rquickjs::methods]
impl CandidArgs {
    /// `new CandidArgs(text)` parses Candid source — an argument list written
    /// out in full, the textual counterpart of `candidEncode`'s values.
    #[qjs(constructor)]
    pub fn parse(ctx: Ctx<'_>, text: String) -> JsResult<Self> {
        let args = parse_idl_args(&text).map_err(|e| throw(&ctx, &format!("CandidArgs: {e}")))?;
        Self::new(args.args).map_err(|e| throw(&ctx, &e))
    }

    /// Read back an encoded argument list. Without type information this is a
    /// best-effort structural view — record fields come back as their numeric
    /// hashes — so it is for inspection, not round-tripping.
    #[qjs(static, rename = "decode")]
    pub fn decode<'js>(ctx: Ctx<'js>, bytes: Value<'js>) -> JsResult<Self> {
        let bytes = arg_bytes(&ctx, "CandidArgs.decode", &bytes)?;
        let args = IDLArgs::from_bytes(&bytes)
            .map_err(|e| throw(&ctx, &format!("CandidArgs.decode failed: {e}")))?;
        Ok(Self::encoded(args.args, bytes))
    }

    /// How many arguments the list holds.
    #[qjs(get)]
    pub fn length(&self) -> usize {
        self.values.len()
    }

    /// The encoded argument bytes.
    #[qjs(rename = "toUint8Array")]
    pub fn to_uint8_array<'js>(&self, ctx: Ctx<'js>) -> JsResult<TypedArray<'js, u8>> {
        TypedArray::new(ctx, self.bytes.clone())
    }

    /// The arguments as JavaScript values, one per argument.
    #[qjs(rename = "toValues")]
    pub fn to_values<'js>(&self, ctx: Ctx<'js>) -> JsResult<Vec<Value<'js>>> {
        self.values
            .iter()
            .map(|value| convert::to_js(&ctx, value))
            .collect()
    }

    /// The argument list as Candid text, e.g. `(42 : nat64, "hi")`.
    #[qjs(rename = "toString")]
    pub fn to_string_js(&self) -> String {
        IDLArgs::new(&self.values).to_string()
    }
}

/// The encoded bytes of a value a script offers as argument bytes: a
/// `CandidArgs`, or a `Uint8Array` of bytes it encoded some other way.
pub fn arg_bytes<'js>(ctx: &Ctx<'js>, what: &str, value: &Value<'js>) -> JsResult<Vec<u8>> {
    if let Ok(args) = Class::<CandidArgs>::from_value(value) {
        return Ok(args.borrow().bytes.clone());
    }
    match TypedArray::<u8>::from_value(value.clone()) {
        Ok(bytes) => bytes_of(ctx, what, &bytes),
        Err(_) => Err(throw(
            ctx,
            &format!(
                "{what}: expected a Uint8Array or a CandidArgs (e.g. from candid`…`), got {}",
                convert::type_name(value),
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// The template tag
// ---------------------------------------------------------------------------

/// `` candid`(…)` `` — Candid source with JavaScript values interpolated,
/// yielding an argument list. The template is an argument *list*, parenthesized
/// exactly as the text `new CandidArgs(text)` takes is.
fn candid_template<'js>(
    ctx: Ctx<'js>,
    strings: Value<'js>,
    values: Rest<Value<'js>>,
) -> JsResult<CandidArgs> {
    let chunks = template_chunks(&ctx, strings)?;
    if chunks.len() != values.len() + 1 {
        let message = format!(
            "candid: {} template chunks for {} interpolated values; call it as a template tag: candid`(…)`",
            chunks.len(),
            values.len(),
        );
        return Err(throw(&ctx, &message));
    }

    build(&ctx, &chunks, &values).map_err(|e| throw(&ctx, &e))
}

/// The literal chunks of the template, taken raw so a backslash escape means to
/// Candid what the script wrote: `\n`, `\"` and `\u{1f600}` are Candid's own
/// escapes, spelled the same way in both languages. A plain string is accepted
/// too, for calling the tag as an ordinary function.
fn template_chunks<'js>(ctx: &Ctx<'js>, strings: Value<'js>) -> JsResult<Vec<String>> {
    if let Some(text) = strings.as_string() {
        return Ok(vec![text.to_string()?]);
    }
    let Some(object) = strings.as_object() else {
        return Err(throw(ctx, "candid: expected a template literal"));
    };
    match object.get::<_, Option<Vec<String>>>("raw")? {
        Some(raw) => Ok(raw),
        None => Vec::<String>::from_js(ctx, strings),
    }
}

/// Parse the template with placeholders standing in for the interpolated
/// values, and graft those values onto the parsed tree.
fn build<'js>(
    ctx: &Ctx<'js>,
    chunks: &[String],
    values: &[Value<'js>],
) -> Result<CandidArgs, String> {
    let mut holes = Holes::new(chunks, values);
    let source = holes.source(chunks)?;

    let mut args = parse_idl_args(&source).map_err(|e| holes.explain(&source, &e))?;
    for arg in &mut args.args {
        holes.graft(ctx, arg)?;
    }
    holes.check_all_grafted()?;
    CandidArgs::new(args.args)
}

/// The interpolated values of one template, and the placeholders standing in
/// for them while its source is parsed.
struct Holes<'js> {
    /// The prefix every placeholder starts with, extended until no chunk of the
    /// template contains it: a literal that collided with it would otherwise be
    /// mistaken for a placeholder and replaced.
    tag: String,
    holes: Vec<Hole<'js>>,
}

struct Hole<'js> {
    value: Value<'js>,
    /// Whether the hole sits inside a string literal of the template, which is
    /// what decides between grafting the value and splicing its text. The shape
    /// of the parsed node cannot say: a string that is nothing but a hole parses
    /// to the placeholder either way.
    in_text: bool,
    /// Whether the placeholder turned up in the parsed value tree. One that did
    /// not was resolved away by the parser, which the script needs to hear about.
    grafted: bool,
}

impl<'js> Holes<'js> {
    fn new(chunks: &[String], values: &[Value<'js>]) -> Self {
        let mut tag = "\u{27e6}candid-hole-".to_string();
        while chunks.iter().any(|chunk| chunk.contains(&tag)) {
            tag.push('!');
        }
        let holes = values
            .iter()
            .map(|value| Hole {
                value: value.clone(),
                in_text: false,
                grafted: false,
            })
            .collect();
        Holes { tag, holes }
    }

    /// The placeholder standing in for hole `index`.
    fn placeholder(&self, index: usize) -> String {
        format!("{}{index}\u{27e7}", self.tag)
    }

    /// The Candid source the template stands for. A hole in a value position
    /// becomes a text literal — the one Candid value that can hold an arbitrary
    /// placeholder — and a hole already inside a string literal becomes the bare
    /// placeholder, to be spliced into that string after parsing. Which of the
    /// two each hole is gets recorded, since the parsed source no longer says.
    fn source(&mut self, chunks: &[String]) -> Result<String, String> {
        let mut source = String::new();
        let mut cursor = Cursor::Code;

        for (index, chunk) in chunks.iter().enumerate() {
            source.push_str(chunk);
            cursor = cursor.advance(chunk);
            if index >= self.holes.len() {
                break;
            }
            match cursor {
                Cursor::Code => {
                    source.push('"');
                    source.push_str(&self.placeholder(index));
                    source.push('"');
                }
                Cursor::Text => {
                    source.push_str(&self.placeholder(index));
                    self.holes[index].in_text = true;
                }
                Cursor::LineComment | Cursor::BlockComment => {
                    return Err(format!(
                        "candid: {} is inside a comment, where nothing can be interpolated",
                        hole_name(index),
                    ));
                }
            }
        }

        Ok(source)
    }

    /// Explain a parse failure in the script's own terms: placeholders the
    /// message quotes are named as the holes they came from, and a failure the
    /// parser located at a placeholder says which hole cannot go there.
    fn explain(&self, source: &str, error: &candid_parser::Error) -> String {
        let message = error.to_string();
        let mut rewritten = message.clone();
        for index in 0..self.holes.len() {
            rewritten = rewritten.replace(&self.placeholder(index), &hole_name(index));
        }

        // The message quotes the placeholder for some failures and only points
        // at it for others, so take a location over the text when there is one.
        let blamed = self
            .hole_at(source, error_span(error))
            .or_else(|| (0..self.holes.len()).find(|&i| message.contains(&self.placeholder(i))));

        match blamed {
            None => format!("candid: {rewritten}"),
            Some(index) => format!(
                "candid: {rewritten}\n\
                 {} cannot be interpolated there: a placeholder stands for a whole Candid value, \
                 or for text inside a string literal — not for a type, a field name, or the \
                 contents of a `principal` or `blob` literal",
                hole_name(index),
            ),
        }
    }

    /// The hole whose placeholder covers `span` in the generated source.
    fn hole_at(&self, source: &str, span: Option<Range<usize>>) -> Option<usize> {
        let span = span?;
        (0..self.holes.len()).find(|&index| {
            let placeholder = self.placeholder(index);
            source
                .match_indices(&placeholder)
                .any(|(at, _)| at < span.end && span.start < at + placeholder.len())
        })
    }

    /// Replace the placeholders in a parsed value with what the script
    /// interpolated. Only text can carry one, so the walk hunts for text nodes:
    /// text that *is* a placeholder becomes the value itself, and placeholders
    /// embedded in a longer string become their values' JavaScript text form.
    fn graft(&mut self, ctx: &Ctx<'js>, value: &mut IDLValue) -> Result<(), String> {
        match value {
            IDLValue::Text(text) => {
                if let Some(grafted) = self.graft_text(ctx, text)? {
                    *value = grafted;
                }
            }
            IDLValue::Opt(inner) => self.graft(ctx, inner)?,
            IDLValue::Vec(items) => {
                for item in items {
                    self.graft(ctx, item)?;
                }
            }
            IDLValue::Record(fields) => {
                for field in fields {
                    self.graft(ctx, &mut field.val)?;
                }
            }
            IDLValue::Variant(VariantValue(field, _)) => self.graft(ctx, &mut field.val)?,
            _ => {}
        }
        Ok(())
    }

    /// The value a parsed text node should be replaced by, or `None` when it
    /// holds no placeholder at all.
    fn graft_text(&mut self, ctx: &Ctx<'js>, text: &str) -> Result<Option<IDLValue>, String> {
        // A hole written in a value position is this whole text node, so the
        // node becomes the value itself.
        for index in 0..self.holes.len() {
            if !self.holes[index].in_text && text == self.placeholder(index) {
                let value = self.holes[index].value.clone();
                self.holes[index].grafted = true;
                return to_candid(ctx, &value, &hole_name(index)).map(Some);
            }
        }
        if !text.contains(&self.tag) {
            return Ok(None);
        }

        let mut spliced = String::new();
        let mut rest = text;
        while let Some((at, index)) = self.next_placeholder(rest) {
            spliced.push_str(&rest[..at]);
            let value = self.holes[index].value.clone();
            spliced.push_str(&js_text(ctx, &value, &hole_name(index))?);
            self.holes[index].grafted = true;
            rest = &rest[at + self.placeholder(index).len()..];
        }
        spliced.push_str(rest);
        Ok(Some(IDLValue::Text(spliced)))
    }

    /// The leftmost placeholder in `text`, as `(byte offset, hole index)`.
    fn next_placeholder(&self, text: &str) -> Option<(usize, usize)> {
        (0..self.holes.len())
            .filter_map(|index| text.find(&self.placeholder(index)).map(|at| (at, index)))
            .min()
    }

    /// Report a hole whose placeholder the parser swallowed — inside a `blob`
    /// literal, say — rather than encoding an argument the script never wrote.
    fn check_all_grafted(&self) -> Result<(), String> {
        match self.holes.iter().position(|hole| !hole.grafted) {
            None => Ok(()),
            Some(index) => Err(format!(
                "candid: {} did not survive parsing; a placeholder inside a `blob` literal, or in \
                 a type or field-name position, is not substituted — interpolate the whole value \
                 instead",
                hole_name(index),
            )),
        }
    }
}

/// How a hole is named in errors: by the position it was written at, since the
/// script's own `${…}` has no name to report.
fn hole_name(index: usize) -> String {
    format!("${{…}} #{}", index + 1)
}

/// The source range a parse error points at, which the parser reports as the
/// label of its diagnostic rather than as part of the message.
fn error_span(error: &candid_parser::Error) -> Option<Range<usize>> {
    error
        .report()
        .labels
        .into_iter()
        .next()
        .map(|label| label.range)
}

/// Where the scan of a template sits in the Candid source it is building.
/// Which of these a hole falls in decides how its placeholder is spelled, or
/// whether it can be interpolated at all.
#[derive(Clone, Copy)]
enum Cursor {
    Code,
    Text,
    LineComment,
    BlockComment,
}

impl Cursor {
    /// Scan one literal chunk of the template, returning where it leaves off.
    fn advance(self, chunk: &str) -> Self {
        let mut cursor = self;
        let mut chars = chunk.chars().peekable();

        while let Some(c) = chars.next() {
            cursor = match (cursor, c) {
                (Cursor::Code, '"') => Cursor::Text,
                (Cursor::Code, '/') if chars.peek() == Some(&'/') => {
                    chars.next();
                    Cursor::LineComment
                }
                (Cursor::Code, '/') if chars.peek() == Some(&'*') => {
                    chars.next();
                    Cursor::BlockComment
                }
                // An escape hides whatever follows it, a closing quote included.
                (Cursor::Text, '\\') => {
                    chars.next();
                    Cursor::Text
                }
                (Cursor::Text, '"') => Cursor::Code,
                (Cursor::LineComment, '\n') => Cursor::Code,
                (Cursor::BlockComment, '*') if chars.peek() == Some(&'/') => {
                    chars.next();
                    Cursor::Code
                }
                (cursor, _) => cursor,
            };
        }

        cursor
    }
}

// ---------------------------------------------------------------------------
// Textual encode/decode
// ---------------------------------------------------------------------------

/// `candidEncode(value, …)` — one JavaScript value per Candid argument,
/// converted with no type to go on, exactly as a `` candid`…` `` placeholder
/// is: an integer is width-undetermined, a fractional number is a `float64`, a
/// string is `text`, an array is a `vec`, a `Uint8Array` is a `blob`, and any
/// other object is a `record`. The wrappers of [`crate::exact`] and
/// [`crate::number`] say what no literal can.
///
/// Candid *source* is what `new CandidArgs(text)` takes; a string here is a
/// `text` value, not a list to parse.
fn candid_encode<'js>(ctx: Ctx<'js>, values: Rest<Value<'js>>) -> JsResult<CandidArgs> {
    let values = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            to_candid(&ctx, value, &format!("candidEncode argument {}", index + 1))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| throw(&ctx, &e))?;
    CandidArgs::new(values).map_err(|e| throw(&ctx, &e))
}

/// Decode Candid argument bytes back to their text representation. Without a
/// type it reconstructs a best-effort structural view, which is enough for
/// inspecting responses in a script.
fn candid_decode<'js>(ctx: Ctx<'js>, bytes: Value<'js>) -> JsResult<String> {
    let bytes = arg_bytes(&ctx, "candidDecode", &bytes)?;
    IDLArgs::from_bytes(&bytes)
        .map(|args| args.to_string())
        .map_err(|e| throw(&ctx, &format!("candidDecode failed: {e}")))
}

#[cfg(test)]
mod tests {
    use crate::testing::{error, eval};

    /// Asserts an interpolated template encodes to exactly what the same
    /// argument list spelled out as Candid source does.
    const SAME: &str = r#"
        function same(label, actual, expected) {
            const got = actual.toUint8Array().toHex();
            const want = new CandidArgs(expected).toUint8Array().toHex();
            if (got !== want) {
                throw label + ": " + actual + " is not " + expected;
            }
        }
    "#;

    #[test]
    fn a_hole_encodes_as_the_value_it_stands_for() {
        eval(&format!(
            r#"{SAME}
            same("integer", candid`(${{7}})`, "(7)");
            same("negative", candid`(${{-7}})`, "(-7)");
            same("fraction", candid`(${{1.5}})`, "(1.5)");
            same("bigint", candid`(${{10n ** 30n}})`, "(1000000000000000000000000000000)");
            same("text", candid`(${{"hi"}})`, '("hi")');
            same("bool", candid`(${{true}}, ${{false}})`, "(true, false)");
            same("null", candid`(${{null}}, ${{undefined}})`, "(null, null)");
            same("principal", candid`(${{Principal.fromText("aaaaa-aa")}})`, '(principal "aaaaa-aa")');
            same("blob", candid`(${{new Uint8Array([222, 173])}})`, '(blob "\\de\\ad")');
            same("vec", candid`(${{[1, 2, 3]}})`, "(vec {{ 1; 2; 3 }})");
            same("record", candid`(${{{{ b: 2, a: 1 }}}})`, "(record {{ a = 1; b = 2 }})");
            same("nested", candid`(${{{{ xs: [true], p: Principal.fromText("aaaaa-aa") }}}})`,
                 '(record {{ xs = vec {{ true }}; p = principal "aaaaa-aa" }})');
            "#
        ))
        .unwrap();
    }

    #[test]
    fn a_number_class_encodes_as_its_own_type() {
        eval(&format!(
            r#"{SAME}
            same("nat8", candid`(${{new Nat8(5)}})`, "(5 : nat8)");
            same("nat16", candid`(${{new Nat16(5)}})`, "(5 : nat16)");
            same("nat32", candid`(${{new Nat32(5)}})`, "(5 : nat32)");
            same("nat64", candid`(${{new Nat64(5)}})`, "(5 : nat64)");
            same("int8", candid`(${{new Int8(-5)}})`, "(-5 : int8)");
            same("int16", candid`(${{new Int16(-5)}})`, "(-5 : int16)");
            same("int32", candid`(${{new Int32(-5)}})`, "(-5 : int32)");
            same("int64", candid`(${{new Int64(-5)}})`, "(-5 : int64)");
            same("nat", candid`(${{new Nat(5)}})`, "(5 : nat)");
            same("int", candid`(${{new Int(-5)}})`, "(-5 : int)");
            same("float32", candid`(${{new Float32(1.5)}})`, "(1.5 : float32)");
            // An integral float, which a bare `${{1}}` could not have said.
            same("float64", candid`(${{new Float64(1)}})`, "(1.0 : float64)");

            // Anywhere a value goes, and at a width JavaScript numbers cannot
            // hold: a string or BigInt keeps the value exact.
            same("in a record", candid`(record {{ a = ${{new Nat32(5)}} }})`, "(record {{ a = 5 : nat32 }})");
            same("wide", candid`(${{new Nat64("18446744073709551615")}})`, "(18446744073709551615 : nat64)");
            same("bigint", candid`(${{new Int64(-9223372036854775808n)}})`, "(-9223372036854775808 : int64)");

            // In a string it interpolates its decimal, like any other value.
            same("in a string", candid`("${{new Nat64(7)}}")`, '("7")');
            if (`${{new Nat32(5)}}` !== "5") throw "toString";
            "#
        ))
        .unwrap();
    }

    #[test]
    fn a_number_outside_its_type_is_an_error() {
        for (script, expected) in [
            ("new Nat8(300);", "'300' is not a valid Nat8"),
            ("new Nat32(1.5);", "'1.5' is not a valid Nat32"),
            ("new Nat64(-1);", "'-1' is not a valid Nat64"),
            ("new Nat('abc');", "'abc' is not a valid Nat"),
            (
                "new Int32(2147483648);",
                "'2147483648' is not a valid Int32",
            ),
        ] {
            let reported = error(script);
            assert!(reported.contains(expected), "{script}: {reported}");
        }
    }

    #[test]
    fn an_exact_class_encodes_as_the_value_it_names() {
        eval(&format!(
            r#"{SAME}
            same("unit variant", candid`(${{new Variant("ok")}})`, "(variant {{ ok }})");
            same("payload variant", candid`(${{new Variant("ok", 5)}})`, "(variant {{ ok = 5 }})");
            same("hashed tag", candid`(${{new Variant("_24860_", 5)}})`, "(variant {{ ok = 5 }})");
            same("present opt", candid`(${{new Opt(5)}})`, "(opt 5)");
            same("absent opt", candid`(${{new Opt()}})`, "(null : opt empty)");
            same("opt null", candid`(${{new Opt(null)}})`, "(opt null)");
            same("nested opt", candid`(${{new Opt(new Opt("x"))}})`, '(opt opt "x")');
            same("service", candid`(${{new Service("aaaaa-aa")}})`, '(service "aaaaa-aa")');
            same("func", candid`(${{new Func(canister, "go")}})`, `(func "${{canisterId}}".go)`);
            same("tuple", candid`(${{new Tuple("x", new Nat32(7))}})`, '(record {{ "x"; 7 : nat32 }})');

            // Read back as Candid text, and the accessors each class carries.
            if (`${{new Variant("ok", 5)}}` !== "variant {{ ok = 5 }}") throw "variant toString";
            if (new Variant("ok").tag !== "ok") throw "tag";
            if (new Opt(1).hasValue !== true || new Opt().hasValue !== false) throw "hasValue";
            if (new Service("aaaaa-aa").canister.toText() !== "aaaaa-aa") throw "canister";
            if (new Func("aaaaa-aa", "go").method !== "go") throw "method";
            if (new Tuple(1, 2, 3).length !== 3) throw "length";
            "#
        ))
        .unwrap();
    }

    #[test]
    fn an_exact_class_rejects_what_it_cannot_hold() {
        let service = error("new Service(7);");
        assert!(
            service.contains("Service: expected a Principal or its text"),
            "{service}"
        );
        let func = error("new Func('nope', 'go');");
        assert!(func.contains("'nope' is not a principal"), "{func}");
        let variant = error("new Variant('ok', () => 1);");
        assert!(variant.contains("Variant('ok') is a function"), "{variant}");
    }

    #[test]
    fn a_hole_can_stand_anywhere_a_value_can() {
        eval(&format!(
            r#"{SAME}
            same("record field", candid`(record {{ a = ${{1}}; b = ${{"x"}} }})`, '(record {{ a = 1; b = "x" }})');
            same("vec element", candid`(vec {{ ${{1}}; ${{2}} }})`, "(vec {{ 1; 2 }})");
            same("opt", candid`(opt ${{5}})`, "(opt 5)");
            same("variant", candid`(variant {{ ok = ${{5}} }})`, "(variant {{ ok = 5 }})");
            same("two args", candid`(${{1}}, ${{"x"}})`, '(1, "x")');
            // A one-value template stands for that value where a value is wanted.
            same("nested template", candid`(record {{ a = ${{candid`(5)`}} }})`, "(record {{ a = 5 }})");
            "#
        ))
        .unwrap();
    }

    #[test]
    fn a_multi_value_template_cannot_stand_for_one_value() {
        let two = error("candid`(record { a = ${candid`(1, 2)`} })`;");
        assert!(two.contains("stands for one value here"), "{two}");
    }

    #[test]
    fn a_hole_inside_a_string_interpolates_text() {
        eval(&format!(
            r#"{SAME}
            same("text", candid`("hello ${{"world"}}")`, '("hello world")');
            // A string that is nothing but a hole is still a string.
            same("whole string", candid`("${{"world"}}")`, '("world")');
            same("number", candid`("count: ${{7}}")`, '("count: 7")');
            same("two holes", candid`("${{1}}/${{2}}")`, '("1/2")');
            same("principal", candid`("id ${{Principal.fromText("aaaaa-aa")}}")`, '("id aaaaa-aa")');
            same("escaped quote", candid`("a \" ${{"b"}}")`, '("a \\" b")');
            "#
        ))
        .unwrap();
    }

    #[test]
    fn an_interpolated_value_cannot_inject_candid() {
        eval(&format!(
            r#"{SAME}
            // The value stays one text value however it is spelled.
            same("value position", candid`(${{'1; b = 2'}})`, '("1; b = 2")');
            same("string position", candid`("${{'" ; extra = "'}}")`, '("\\" ; extra = \\"")');
            "#
        ))
        .unwrap();
    }

    #[test]
    fn a_hole_the_parser_resolves_is_an_error() {
        // A type annotation is applied while parsing, to the placeholder.
        let annotated = error("candid`(${7} : nat64)`;");
        assert!(annotated.contains("${…} #1"), "{annotated}");
        assert!(
            annotated.contains("cannot be interpolated there"),
            "{annotated}"
        );

        // A principal literal is parsed too, so the placeholder is not a value
        // the tag ever gets to replace.
        let principal = error(r#"candid`(principal "${'aaaaa-aa'}")`;"#);
        assert!(principal.contains("${…} #1"), "{principal}");

        // A blob literal parses to bytes, which swallows the placeholder whole.
        let blob = error(r#"candid`(blob "${'de'}")`;"#);
        assert!(blob.contains("${…} #1"), "{blob}");
        assert!(blob.contains("did not survive parsing"), "{blob}");

        // A comment is dropped by the lexer, so nothing can be interpolated there.
        let comment = error("candid`( // ${1}\n 1)`;");
        assert!(comment.contains("${…} #1"), "{comment}");
        assert!(comment.contains("inside a comment"), "{comment}");
    }

    #[test]
    fn a_value_with_no_candid_form_is_an_error() {
        let function = error("candid`(${() => 1})`;");
        assert!(function.contains("${…} #1 is a function"), "{function}");

        // Errors name the field the offending value sits at.
        let nested = error("candid`(${{ amount: () => 1 }})`;");
        assert!(nested.contains("${…} #1.amount is a function"), "{nested}");

        let infinite = error("candid`(${1 / 0})`;");
        assert!(infinite.contains("${…} #1 is inf"), "{infinite}");
    }

    #[test]
    fn a_mixed_array_is_an_error() {
        // Untyped encoding would produce bytes no decoder can read.
        let mixed = error("candid`(${[1, 'two']})`;");
        assert!(mixed.contains("argument 1[1] is text"), "{mixed}");
        assert!(mixed.contains("a vec holds one type"), "{mixed}");

        // Including where the vec is the template's and the holes are its
        // elements, or where it sits inside a record.
        let spelled = error("candid`(vec { ${1}; ${'two'} })`;");
        assert!(spelled.contains("a vec holds one type"), "{spelled}");
        let nested = error("candid`(record { xs = ${[1, 'two']} })`;");
        assert!(nested.contains("argument 1.xs[1] is text"), "{nested}");
    }

    #[test]
    fn an_object_with_nothing_to_read_is_an_error() {
        // A Map or a Date keeps its contents where a record cannot see them.
        let map = error("candid`(${new Map([['a', 1]])})`;");
        assert!(map.contains("${…} #1 is a Map with no properties"), "{map}");

        // An object literal, empty or not, is still a record.
        eval("candid`(${{}})`; candid`(${Object.create(null)})`;").unwrap();
    }

    #[test]
    fn an_argument_list_reads_back() {
        crate::testing::assert_script(&[
            ("length", "candid`(1, \"x\")`.length === 2"),
            ("toString", "`${candid`(1, \"x\")`}` === '(1, \"x\")'"),
            (
                "toUint8Array",
                "candid`()`.toUint8Array() instanceof Uint8Array",
            ),
            ("toValues", "candid`(1, \"x\")`.toValues()[1] === 'x'"),
            (
                "decode",
                "CandidArgs.decode(candid`(7 : nat64)`).toValues()[0] === 7n",
            ),
            (
                "constructor",
                "`${new CandidArgs('(7 : nat64)')}` === '(7 : nat64)'",
            ),
            (
                "candidDecode takes either",
                "candidDecode(candid`(1 : nat8)`) === candidDecode(candid`(1 : nat8)`.toUint8Array())",
            ),
        ]);
    }

    #[test]
    fn candid_encode_converts_its_arguments() {
        crate::testing::assert_script(&[
            // One argument per value, each converted as a hole standing in the
            // same position is.
            (
                "several values",
                "`${candidEncode(1, 'hi')}` === `${candid`(${1}, ${'hi'})`}`",
            ),
            (
                "a record",
                "`${candidEncode({ amount: 10 })}` === `${candid`(${{ amount: 10 }})`}`",
            ),
            (
                "a number class",
                "`${candidEncode(new Nat64(7))}` === `${candid`(7 : nat64)`}`",
            ),
            (
                "a one-value template",
                "`${candidEncode(candid`(7 : nat64)`)}` === `${candid`(7 : nat64)`}`",
            ),
            ("no values", "candidEncode().length === 0"),
            // Source is `new CandidArgs(text)`'s job: a string is a text value.
            (
                "a string is text",
                "candidEncode('(42)').toValues()[0] === '(42)'",
            ),
        ]);
    }

    #[test]
    fn candid_encode_names_the_argument_at_fault() {
        let function = error("candidEncode(1, () => 1);");
        assert!(
            function.contains("candidEncode argument 2 is a function"),
            "{function}"
        );

        let nested = error("candidEncode({ amount: () => 1 });");
        assert!(
            nested.contains("candidEncode argument 1.amount is a function"),
            "{nested}"
        );
    }
}
