# icp-js-plugin

An [icp-cli](https://github.com/dfinity/icp-cli) **sync plugin** that runs a
JavaScript script against the canister being synced. It implements the
`icp:sync-plugin` WIT world (see [`sync-plugin.wit`](sync-plugin.wit)) and
exposes to the script roughly the same capabilities a native sync plugin has —
calling the target canister, the sync inputs, and read-only filesystem access —
plus Candid, principal, and encoding helpers convenient for canister work.

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
step's `canisters:` list. Wrap a value in `Principal.from(..)` for a
`Principal`, or pass it straight to a call's `target`.

### Canister calls

By default a call targets the canister being synced (`canisterId`). A call may
instead target any canister declared as a dependency in the sync step's
`canisters:` list, via the `target` field (see below). Each call returns the raw
Candid-encoded response bytes as a `Uint8Array`, or throws with the host's error
message.

```js
// Shorthands: empty-arg style is just candid`()`.
// The first two always target the canister being synced; `callOther` makes an
// update call to a canister declared in `canisters:`, by name.
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
    // target: omitted → the canister being synced. A string that parses as a
    // principal (or a Principal value) targets by id; any other string targets
    // by canister name. The target must be declared in `canisters:`.
    target: "ledger",
});
```

### Candid

An argument is written as Candid source with JavaScript values interpolated
into it, using the `candid` template tag:

```js
let bytes = candid`(record { to = ${dest}; amount = ${10} })`;
let bytes = candid`(${[1, 2, 3]}, "literal text")`; // → Uint8Array of arguments
```

The template is an argument *list*, parenthesized like the text `candidEncode`
takes. Interpolation is not textual: each `${…}` is parsed as a placeholder and
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
| an array                  | `vec` (its elements must share one type)       |
| any other object          | `record` of its own enumerable properties      |

A hole inside a string literal interpolates *text* instead, the way JavaScript's
own template literals do — `` candid`("hello ${name}")` `` is one `text` value.

What a hole cannot stand for is anything the parser resolves as it goes: a type
annotation (`${n} : nat64`), a field name, or the contents of a `principal` or
`blob` literal. Those are errors that name the offending `${…}`; spell the
literal out, or interpolate the whole value (`${Principal.from(id)}` rather than
`principal "${id}"`).

Candid text can also be encoded and decoded directly:

```js
let bytes = candidEncode('(42 : nat64, "hi")'); // text → Uint8Array
let text  = candidDecode(bytes);                 // Uint8Array → text (best-effort)
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
toHex(bytes);         // Uint8Array → hex string
fromHex("deadbeef");  // hex string → Uint8Array
sha256(bytes);         // Uint8Array → 32-byte Uint8Array
```

JSON is built into JavaScript — use `JSON.parse` / `JSON.stringify` directly;
there is no `jsonDecode` / `jsonEncode` helper.

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

Bump a counter and report the new value:

```js
let before = candidDecode(callQuery("get", candid`()`));
console.error("count before sync: " + before);

callUpdate("increment", candid`()`);

let after = candidDecode(callQuery("get", candid`()`));
console.error("count after sync: " + after);
```
