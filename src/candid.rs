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

use std::ops::Range;

use ::candid::types::Label;
use ::candid::types::value::{IDLArgs, IDLField, IDLValue, VariantValue};
use candid_parser::parse_idl_args;
use rquickjs::function::Rest;
use rquickjs::{
    Array, Class, Coerced, Ctx, FromJs, Function, Object, Result as JsResult, TypedArray, Value,
};

use crate::engine::{bytes_of, throw};
use crate::number;
use crate::principal::Principal;

/// Register the Candid template tag and the textual encode/decode helpers.
pub fn register(ctx: &Ctx<'_>) -> JsResult<()> {
    let globals = ctx.globals();
    globals.set("candid", Function::new(ctx.clone(), candid_template)?)?;
    globals.set("candidEncode", Function::new(ctx.clone(), candid_encode)?)?;
    globals.set("candidDecode", Function::new(ctx.clone(), candid_decode)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The template tag
// ---------------------------------------------------------------------------

/// `` candid`(…)` `` — Candid source with JavaScript values interpolated,
/// encoded to argument bytes. The template is an argument *list*, parenthesized
/// exactly as the text `candidEncode` takes is.
fn candid_template<'js>(
    ctx: Ctx<'js>,
    strings: Value<'js>,
    values: Rest<Value<'js>>,
) -> JsResult<TypedArray<'js, u8>> {
    let chunks = template_chunks(&ctx, strings)?;
    if chunks.len() != values.len() + 1 {
        let message = format!(
            "candid: {} template chunks for {} interpolated values; call it as a template tag: candid`(…)`",
            chunks.len(),
            values.len(),
        );
        return Err(throw(&ctx, &message));
    }

    let bytes = encode(&ctx, &chunks, &values).map_err(|e| throw(&ctx, &e))?;
    TypedArray::new(ctx, bytes)
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
/// values, graft those values onto the parsed tree, and encode the result.
fn encode<'js>(
    ctx: &Ctx<'js>,
    chunks: &[String],
    values: &[Value<'js>],
) -> Result<Vec<u8>, String> {
    let mut holes = Holes::new(chunks, values);
    let source = holes.source(chunks)?;

    let mut args = parse_idl_args(&source).map_err(|e| holes.explain(&source, &e))?;
    for arg in &mut args.args {
        holes.graft(ctx, arg)?;
    }
    holes.check_all_grafted()?;
    for (index, arg) in args.args.iter().enumerate() {
        check_uniform_vecs(arg, &format!("argument {}", index + 1))?;
    }

    args.to_bytes().map_err(|e| format!("candid: {e}"))
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

/// Reject a `vec` whose elements do not share one Candid type. Without a
/// declared type a vector takes the type of its first element and every element
/// is then written as its own, which yields bytes no decoder can read — so a
/// mixed array is caught here rather than sent.
fn check_uniform_vecs(value: &IDLValue, path: &str) -> Result<(), String> {
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
// JavaScript values as Candid values
// ---------------------------------------------------------------------------

/// The Candid value an interpolated JavaScript value stands for.
///
/// The mapping is the one Candid's own syntax gives the same literal: an
/// integer is a width-undetermined number (an `int` unless a type says
/// otherwise), a fractional number is a `float64`, a string is `text`, an array
/// is a `vec`, a `Uint8Array` is a `blob`, a `Principal` is a `principal`, and
/// any other object is a `record`. `null` and `undefined` are Candid's `null`.
///
/// `path` names the value in error messages: the hole it came from, extended
/// with the field or index the offending value sits at.
fn to_candid<'js>(ctx: &Ctx<'js>, value: &Value<'js>, path: &str) -> Result<IDLValue, String> {
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
    // A `Nat32` and friends, which say what an integer's width is.
    if let Some(number) = number::to_candid(value) {
        return Ok(number);
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
        "{path} is a {}, which has no Candid form",
        value.type_of(),
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
    let array =
        TypedArray::<u8>::from_object(object.clone()).map_err(|e| format!("{path}: {e}"))?;
    let bytes = array
        .as_bytes()
        .ok_or_else(|| format!("{path}: Uint8Array buffer is detached"))?;
    Ok(IDLValue::Blob(bytes.to_vec()))
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
            id: Label::Named(name),
            val,
        });
    }

    // A `Map`, a `Set` or a `Date` keeps its contents in internal slots, which
    // a record reads as nothing at all. An empty record is a plausible thing to
    // send and an implausible thing to mean by one of those, so say so.
    if fields.is_empty() && !is_plain_object(ctx, object) {
        return Err(format!(
            "{path} is a {} with no properties to read; pass a plain object, an array, a Uint8Array or a Principal",
            class_name(object).unwrap_or_else(|| "object".to_string()),
        ));
    }

    // Hashed-label order, the order the parser leaves a `record { … }` in.
    fields.sort_unstable_by_key(|field| field.id.get_id());
    Ok(IDLValue::Record(fields))
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

/// A value's JavaScript string form, for a hole inside a string literal — where
/// the interpolation is textual, and JavaScript's own coercion is the rule.
fn js_text<'js>(ctx: &Ctx<'js>, value: &Value<'js>, path: &str) -> Result<String, String> {
    Coerced::<String>::from_js(ctx, value.clone())
        .map(|coerced| coerced.0)
        .map_err(|e| format!("{path} cannot be read as text: {e}"))
}

// ---------------------------------------------------------------------------
// Textual encode/decode
// ---------------------------------------------------------------------------

/// Encode a Candid value in text format (e.g. `"(42 : nat64, \"hi\")"`) to
/// argument bytes. Number literals default to `int`/`nat`; annotate them
/// (`42 : nat64`) when the method signature needs a specific width.
fn candid_encode<'js>(ctx: Ctx<'js>, text: String) -> JsResult<TypedArray<'js, u8>> {
    let args =
        parse_idl_args(&text).map_err(|e| throw(&ctx, &format!("candidEncode failed: {e}")))?;
    let bytes = args
        .to_bytes()
        .map_err(|e| throw(&ctx, &format!("candidEncode failed: {e}")))?;
    TypedArray::new(ctx, bytes)
}

/// Decode Candid argument bytes back to their text representation. Without a
/// type it reconstructs a best-effort structural view, which is enough for
/// inspecting responses in a script.
fn candid_decode<'js>(ctx: Ctx<'js>, bytes: TypedArray<'js, u8>) -> JsResult<String> {
    let bytes = bytes_of(&ctx, "candidDecode", &bytes)?;
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
            const want = candidEncode(expected);
            if (toHex(actual) !== toHex(want)) {
                throw label + ": " + candidDecode(actual) + " is not " + candidDecode(want);
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
    fn a_hole_can_stand_anywhere_a_value_can() {
        eval(&format!(
            r#"{SAME}
            same("record field", candid`(record {{ a = ${{1}}; b = ${{"x"}} }})`, '(record {{ a = 1; b = "x" }})');
            same("vec element", candid`(vec {{ ${{1}}; ${{2}} }})`, "(vec {{ 1; 2 }})");
            same("opt", candid`(opt ${{5}})`, "(opt 5)");
            same("variant", candid`(variant {{ ok = ${{5}} }})`, "(variant {{ ok = 5 }})");
            same("two args", candid`(${{1}}, ${{"x"}})`, '(1, "x")');
            "#
        ))
        .unwrap();
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
}
