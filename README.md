# icp-js-plugin

An [icp-cli](https://github.com/dfinity/icp-cli) **sync plugin** that runs a
JavaScript script against the canister being synced. It implements the
`icp:sync-plugin` WIT world (see [`sync-plugin.wit`](sync-plugin.wit)) and
exposes to the script roughly the same capabilities a native sync plugin has —
calling the target canister, reading its metadata, the sync inputs, and read-only
filesystem access — plus Candid, principal, and encoding helpers convenient for
canister work.

Scripts run on [QuickJS](https://bellard.org/quickjs/) via
[rquickjs](https://crates.io/crates/rquickjs); it is a small ES2020-class engine
without Node or Web APIs, so no `require`/`import`, no `fetch`, no timers — just
the language plus the host functions documented below.

## Building

The plugin is a WebAssembly component targeting `wasm32-wasip2`:

```sh
rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2 --release
```

The component is emitted at `target/wasm32-wasip2/release/icp_js_plugin.wasm`.

The crate also builds for the host, so `cargo check` and `cargo test` run
without a WebAssembly runtime.

## The script

The entry script comes from one of two places, and declaring both is an error:

- A **`script` field**, whose value is the JavaScript source inline.
- A file declared under the **`script` key**, whose contents are the JavaScript
  source.

```yaml
sync:
  steps:
    - plugin: ./icp_js_plugin.wasm
      files:
        script: sync.js
        seed: [seed/users.json, seed/roles.json]
      dirs:
        assets: assets
```

Every declared file — the entry script included — is read by the host and handed
to the script via the `files` object, keyed by path. Directories declared in
`dirs` are preopened read-only and reachable with the filesystem functions below.
Declaring `files:`/`dirs:` as a map instead of a plain list tags each entry with
its key, which the script reads back through `fileKeys` / `dirKeys`.

A script runs to completion for a clean sync; throwing (or a runtime error)
fails the step with the thrown message.

## Scripting API

### Sync inputs (globals)

| Name          | Type                        | Description                                           |
| ------------- | --------------------------- | ----------------------------------------------------- |
| `canisterId`  | `string`                    | Textual principal of the target canister.             |
| `canister`    | `Principal`                 | The target canister as a `Principal`.                 |
| `environment` | `string`                    | Environment being synced (e.g. `"production"`).       |
| `identityId`  | `string`                    | Textual principal of the signing identity.            |
| `identity`    | `Principal`                 | The signing identity as a `Principal`.                |
| `proxy`       | `Principal` \| `null`       | Proxy canister if `--proxy` was set, else `null`.     |
| `dirs`        | `string[]`                  | Declared directory paths (preopened read-only).       |
| `dirKeys`     | `object` (key → `string[]`) | Manifest key → the directory paths declared under it. |
| `files`       | `object` (path → `string`)  | Contents of every declared file, by path.             |
| `fileKeys`    | `object` (key → `string[]`) | Manifest key → the file paths declared under it.      |
| `fields`      | `object` (name → `string`)  | Key-value fields declared in the step's `fields`.     |
| `canisterIds` | `object` (name → `string`)  | Every project canister's name → textual principal.    |

`dirKeys` and `fileKeys` cover only the entries declared under a map key; a
plain-list `dirs:`/`files:` has none, and appears only in `dirs`/`files`. One key
may name several paths, so each maps to an array:

```js
// Contents of every file declared under the `seed` key.
let seeds = fileKeys.seed.map((path) => files[path]);
```

`canisterIds` is informational: it maps each named canister in the project
(both `subproject:local` keys and bare local names for same-subproject siblings)
to its textual principal for the environment being synced. Being listed does not
grant permission to call a canister — that still requires declaring it in the
step's `canisters:` list. Wrap a value in `Principal.from(..)` for a `Principal`.

### Canister calls

By default a call targets the canister being synced (`canisterId`), which the
global `self` also names explicitly. A call may instead target any canister
listed in the sync step's `canisters:`, via the `target` field — **by name**,
spelled exactly as that list does. A principal is not a target: the host resolves
names so that the permission it checks is the one the manifest granted, and
passing one is an error that names the canister instead. Each call returns the
raw Candid-encoded response bytes as a `Uint8Array`, or throws with the host's
error message.

```js
// Shorthands: empty-arg style is just candid`()`.
// The first two always target the canister being synced; `callOther` makes an
// update call to a canister listed in `canisters:`, by name.
let resp = callQuery("get_count", candid`()`);
let resp = callUpdate("set_count", candid`(7 : nat64)`);
let resp = callOther("ledger", "set_count", candid`(7 : nat64)`);

// General form. Only `method` is required.
let resp = canisterCall({
    method: "transfer",
    arg: candid`(record { to = ${dest}; amount = ${10} })`,
    query: false,   // default false → update; true → query
    direct: false,  // default false → route update through the proxy if configured
    cycles: 0,      // attached to a proxied update call only
    // target: `self` (or omitted) → the canister being synced. Otherwise the
    // name of a canister listed in the step's `canisters:`.
    target: "ledger",
});
```

`arg` takes a `CandidArgs` — what the `candid` tag yields — or a `Uint8Array` of
bytes encoded some other way.

These are the raw calls: the script encodes the argument and decodes the
response itself. [Coerced calls](#coerced-calls) do both against the callee's own
interface instead.

### Metadata sections

```js
let did = canisterMetadata("candid:service"); // → Uint8Array, or null if absent
let text = decodeUtf8(did);

// General form. Only `name` is required.
let section = canisterMetadata({
    name: "candid:args",
    target: "ledger", // omitted → the canister being synced
    direct: false,    // default false → read through the proxy if configured
});
```

The section name is spelled as the wasm module's custom section does, minus the
`icp:public `/`icp:private ` prefix. `null` means the target provably has no
section by that name — including having no module installed at all; a section the
reader may not have is an error. A `direct` read is a certified `read_state`
signed by the sync identity, which reaches a private section only if that
identity controls the target; a proxied read reaches one private to the proxy's
control.

### Candid

An argument is written as Candid source with JavaScript values interpolated
into it, using the `candid` template tag:

```js
let args = candid`(record { to = ${dest}; amount = ${10} })`;
let args = candid`(${[1, 2, 3]}, "literal text")`; // → CandidArgs
```

The template is an argument *list*, parenthesized like the text
`new CandidArgs(text)` takes. Interpolation is not textual: each `${…}` is parsed as a placeholder and
the JavaScript value is grafted onto the parsed value afterwards, so a value can
never inject Candid syntax of its own — `${"1; b = 2"}` is one `text` value, not
a record field.

Each hole becomes the Candid value Candid's own syntax would give the same
literal:

| JavaScript                | Candid                                         |
| ------------------------- | ---------------------------------------------- |
| `null`, `undefined`       | `null`                                         |
| `true` / `false`          | `bool`                                         |
| an integer, or a `BigInt` | a width-undetermined number (`int` by default) |
| a fractional number       | `float64`                                      |
| a string                  | `text`                                         |
| a `Principal`             | `principal`                                    |
| a `Uint8Array`            | `blob`                                         |
| a number class            | that class's type (see below)                  |
| an exact-encoding class   | that class's value (see below)                 |
| an array                  | `vec` (its elements must share one type)       |
| any other object          | `record` of its own enumerable properties      |

A hole inside a string literal interpolates *text* instead, the way JavaScript's
own template literals do — `` candid`("hello ${name}")` `` is one `text` value.

What a hole cannot stand for is anything the parser resolves as it goes: a type
annotation (`${n} : nat64`), a field name, or the contents of a `principal` or
`blob` literal. Those are errors that name the offending `${…}`; spell the
literal out, or interpolate the whole value (`${Principal.from(id)}` rather than
`principal "${id}"`).

#### `CandidArgs`

An argument list, and what a call's `arg` takes. It carries the argument
*values*, not only the bytes they encode to untyped, which is what lets a
[coerced call](#coerced-calls) serialize them at the types the callee declares.

```js
let args = candid`(1, "x")`;
args.length;         // → 2
args.toUint8Array(); // → Uint8Array of encoded bytes
args.toValues();     // → [1n, "x"], the arguments as JavaScript values
`${args}`;           // → '(1, "x")', the list as Candid text

new CandidArgs('(42 : nat64)');    // parse an argument list from Candid source
CandidArgs.decode(bytes);          // read back encoded bytes, untyped
```

A one-value `` candid`…` `` also stands for that one value wherever a value is
wanted — a record field, or one argument of a coerced call.

An argument list can also be built from JavaScript values alone, with no source
to write: `candidEncode` takes one value per argument and converts each exactly
as a `${…}` hole is converted, by the table above.

```js
let args = candidEncode({ to: dest, amount: 10 }); // → CandidArgs, one record
let args = candidEncode(1, 'hi');                  // → CandidArgs, two arguments
candidEncode('(42)');                              // → ('(42)'), a text value —
                                                   //   source is CandidArgs' job
let text = candidDecode(args);                     // CandidArgs/Uint8Array → text
```

`candidDecode` reconstructs a structural view without type information, so
record fields appear as their numeric hashes — it is meant for inspection, not
round-tripping.

### Number types

A bare JavaScript number is width-undetermined and encodes as `int`, which is
wrong for a method that declares a fixed width. Since a hole cannot carry a type
annotation, the number classes say which type is meant:

```js
candid`(record { amount = ${new Nat64(10)}; rate = ${new Float32(0.5)} })`;
```

`Nat`, `Int`, `Nat8`, `Nat16`, `Nat32`, `Nat64`, `Int8`, `Int16`, `Int32`,
`Int64`, `Float32` and `Float64` each map to the Candid type of the same name.
Each takes a number, a `BigInt` or a string — so a `nat64` past 2^53 stays
exact, `new Nat64("18446744073709551615")` — and throws when the value does not
fit. They are wrappers for encoding: `toString()` reads one back, and in a
string a hole holding one interpolates its decimal.

### Exact-encoding classes

The literal mapping covers what JavaScript syntax denotes. What is left over —
a variant, a canister or function reference, an optional distinct from `null`,
and a tuple of mixed types — has a class that says it exactly:

```js
new Variant("ok");                       // variant { ok }
new Variant("member", { since: 1 });     // variant { member = record { since = 1 } }
new Opt(5);                              // opt 5
new Opt();                               // an absent optional
new Service(canister);                   // service "…"
new Func(canister, "transfer");          // func "…".transfer
new Tuple("x", new Nat32(7));            // record { "x"; 7 : nat32 }
```

`Variant` reads its tag the way a Candid field name is read: `_123_` is the hash
itself, anything else is hashed. `Opt` is for the two cases the coercion rules
cannot spell — an optional holding `null` (`new Opt(null)`) and an optional of an
optional. `Service` and `Func` take a `Principal` or its text. `Tuple` is a
record of numbered fields, which an array cannot be because a `vec` holds one
type.

Each carries the accessors its shape suggests (`.tag`, `.hasValue`, `.canister`,
`.method`, `.length`) and a `toString()` that renders the value as Candid text.

### Coerced calls

A script that knows what a method's arguments *are* need not write any Candid:
the types the callee declares decide how each JavaScript value encodes, and how
the response reads back. The interface comes from the receiver itself, out of its
`candid:service` metadata section.

```js
callTyped("ledger", "transfer", { to: dest, amount: 10 });

const balance = callTyped("ledger", "balance_of", identity); // → decoded JavaScript
```

The first argument is the receiver: a canister name as a call's `target` is, or
**`self`** for the canister being synced. A target is otherwise always a name,
and the canister being synced has none to give — so `self` is the global that
names it.

```js
callTyped(self, "increment");
```

The interface decides whether each call is a query or an update, and is read
once per receiver per run rather than once per call. A method with one result
returns it directly, one with none returns `undefined`, and one with several
returns an array.

The general form takes the options a raw `canisterCall` does, plus an
`interface` to use instead of the one read from the receiver:

```js
canisterCallTyped({
    method: "transfer",
    args: [{ to: dest, amount: 10 }], // the argument list; omitted → no arguments
    target: "ledger",                  // omitted → the canister being synced
    interface: files["ledger.did"],    // a CandidInterface, or `.did` source
    direct: false,
    cycles: 0,
});
```

`args` is the argument *list*, so a method taking one `vec` gets
`args: [[1, 2, 3]]`. A `` candid`…` `` may stand for the whole list instead of an
array.

`CandidInterface` is the parsed `.did` on its own, for encoding and decoding
without calling:

```js
const iface = new CandidInterface(files["backend.did"]);
const iface = CandidInterface.fromCanister("ledger"); // read from its metadata
const iface = CandidInterface.fromCanister(self);

iface.methods();                      // → string[]
iface.signature("get");               // → "(nat64) -> (text) query"
iface.isQuery("get");                 // → boolean
iface.encode("transfer", { … });      // → CandidArgs, at the declared types
iface.decodeResult("transfer", resp); // → decoded JavaScript
iface.decodeArgs("transfer", bytes);  // → an array, one per declared argument
```

#### What coerces to what

Every rule is there to make a JavaScript *literal* land on the type the method
declares. A value the declared type cannot account for is an error naming the
argument and the field it sits at — never a guess.

| Declared type                | JavaScript                                                  |
| ---------------------------- | ----------------------------------------------------------- |
| `bool`                       | a boolean                                                    |
| `text`                       | a string                                                     |
| `nat`, `int`, any fixed width | an integral number, or a `BigInt`                           |
| `float32`, `float64`         | a number                                                     |
| `principal`                  | a `Principal`, or a textual principal                        |
| `service`                    | a `Principal`, a textual principal, or a `Service`           |
| `func`                       | a `Func` — no literal denotes one                            |
| `opt T`                      | `null`/`undefined` for absent, anything else for present     |
| `vec T`                      | an array; a `Uint8Array` where `T` is `nat8`                 |
| `record`                     | an object keyed by field name; an array for a tuple record   |
| `variant`                    | `{ tag: value }`, or `"tag"` for a tag that carries nothing  |
| `null`                       | `null` or `undefined`                                        |
| `reserved`                   | anything                                                     |

An omitted record field is absent where the type says `opt`, `null` or
`reserved`, and an error otherwise; an unknown field is an error rather than
something dropped on the floor. A string is *not* a number — `BigInt` is how
JavaScript writes an integer too wide for a number, so there is nothing a decimal
string could add.

A value one of the exact-encoding or number classes holds passes through as it
stands, as does a `` candid`…` `` — which supplies the whole argument list when
it is a call's only argument, and one value where a single value is wanted.

Decoded results are the same mapping read backwards, so a response goes straight
back into another call: a record is an object keyed by field name, a variant is a
one-entry object, `principal` and `service` are `Principal`s, `func` is a `Func`,
`blob` is a `Uint8Array`, and an absent optional is `null`. `nat`, `int`, `nat64`
and `int64` come back as `BigInt`s, being wider than a number is exact for; the
narrower widths and the floats are numbers. `opt null` and nested optionals are
what this loses — `candidDecode` shows a response exactly.

### Principals

`Principal` is the class of
[icp-js-core](https://github.com/dfinity/icp-js-core), minus
`selfAuthenticating` (which would need a key the plugin has no way to reach), so
code written against `@icp-sdk/core` carries over:

```js
Principal.from("ryjl3-tyaaa-aaaaa-aaaba-cai"); // text, bytes or a Principal
Principal.fromText("ryjl3-tyaaa-aaaaa-aaaba-cai"); // throws if invalid
Principal.fromUint8Array(bytes);
Principal.fromHex("00000000000000020101"); // ryjl3-tyaaa-aaaaa-aaaba-cai
Principal.anonymous();          // 2vxsx-fae
Principal.managementCanister(); // aaaaa-aa
Principal.isPrincipal(value);

p.toText();       // → string
p.toUint8Array(); // → Uint8Array (raw bytes)
p.toHex();        // → hex string
p.isAnonymous();  // → boolean
p.toJSON();       // → { "__principal__": "<text>" }
`${p}`;           // string coercion → textual principal

p.compareTo(q);   // → "lt" | "eq" | "gt", byte-wise
p.ltEq(q);
p.gtEq(q);
```

`new Principal(..)` takes what `Principal.from` does; icp-js-core keeps its own
constructor protected.

### Encoding helpers

```js
toHex(bytes);          // Uint8Array → hex string
fromHex("deadbeef");   // hex string → Uint8Array
sha256(bytes);         // Uint8Array → 32-byte Uint8Array
encodeUtf8("hi");      // string → Uint8Array
decodeUtf8(bytes);     // Uint8Array → string (throws on invalid UTF-8)
```

QuickJS ships no `TextEncoder`/`TextDecoder`, hence the last two — the bytes a
metadata section comes back as are usually text.

JSON is built into JavaScript — use `JSON.parse` / `JSON.stringify` directly;
there is no `jsonDecode` / `jsonEncode` helper. Note that `JSON.stringify`
refuses a `BigInt`, which a decoded `nat`/`int`/`nat64`/`int64` is.

### Filesystem

Read-only access to the preopened `dirs`, backed by WASI. Paths are relative to
a preopened directory.

```js
let text  = readFile("assets/data.json");        // → string (UTF-8)
let bytes = readFileBytes("assets/logo.png");    // → Uint8Array
let names = readDir("assets");                    // → string[] of entry names
```

Writes are unavailable because the host preopens directories read-only.

### Output

`print(..)` and `console.log(..)` write to stdout, shown as transient progress
and discarded when the step ends. `eprint(..)`, `console.error(..)`,
`console.warn(..)` and `console.debug(..)` write to stderr, which is also printed
persistently after the step completes — use them for warnings and summaries the
user should still see. All accept multiple arguments and join them with spaces.

## Examples

Push a list of authorized principals from a JSON file to a sibling canister:

```js
// { "authorized": ["aaaaa-aa", "ryjl3-tyaaa-aaaaa-aaaba-cai"] }
const config = JSON.parse(files["config.json"]);

// `set_authorized : (vec principal) -> ()`. The strings out of the file are
// wrapped in `Principal.from(..)`, which both validates them and tells the encoder
// they are principals rather than text.
const authorized = config.authorized.map((p) => Principal.from(p));
callOther("example", "set_authorized", candid`(${authorized})`);
```

Or the same call written against the canister's own interface, which turns the
strings into principals itself:

```js
const config = JSON.parse(files["config.json"]);
callTyped("example", "set_authorized", config.authorized);
```

Bump a counter and report the new value:

```js
console.error("count before sync: " + callTyped(self, "get"));

callTyped(self, "increment");

console.error("count after sync: " + callTyped(self, "get"));
```
